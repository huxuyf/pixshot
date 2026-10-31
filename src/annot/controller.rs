//! 标注引擎：交互状态机（纯逻辑，不依赖 Win32）。
//!
//! `AnnotSession` 汇集文档 + 模式 + 工具，事件入口返回 [`Action`]（脏区/光标/Esc 处理），
//! 由 Win32 集成层（[`super::session`] / `overlay.rs`）消费。状态迁移见设计文档 5.1。
//!
//! 说明：设计文档第 3 节明示为「Rust 示意」，故此处为可实现性补足了必要字段
//! （`dpi` 用于容差/尺寸换算、`shift` 记忆最近一次修饰键、`Moving/Resizing` 的 `cur`）。

use super::doc::AnnotDoc;
use super::geometry::{norm_rect, rect_union, union_opt, Pt, Rect};
use super::hit_test::{self, Handle};
use super::shape::{arrow_head_len, pen_bounds, text_est_size, Shape};
use super::style::{
    AnnotPrefs, BadgeStyle, HighlightMode, MosaicMode, PenKind, PenStyle, Rgba, StrokeStyle,
    TextFrame, TextStyle, WatermarkStyle,
};

/// 工具种类。判别值 1..9 保持稳定；新工具追加在末尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    None,
    Rect,
    Arrow,
    StepNo,
    Ellipse,
    Line,
    Pen,
    Mosaic,
    Highlight,
    Text,
    /// 对象橡皮：拖过即删，松手合并为一条撤销记录。
    Eraser,
    /// 文字水印：复用文字输入链路，提交为整选区水印。
    Watermark,
}

/// 文字输入类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Normal,
    Watermark,
}

/// Ctrl+滚轮可调整的标注尺寸类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeKind {
    /// 线宽（矩形/箭头/椭圆/线段/画笔）。
    StrokeWidth,
    /// 字号（文字/水印）。
    TextSize,
    /// 步骤序号半径。
    StepRadius,
    /// 橡皮命中容差。
    EraserTol,
}

impl SizeKind {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::StrokeWidth,
            2 => Self::TextSize,
            3 => Self::StepRadius,
            4 => Self::EraserTol,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::StrokeWidth => "线宽",
            Self::TextSize => "字号",
            Self::StepRadius => "序号半径",
            Self::EraserTol => "橡皮容差",
        }
    }

    /// 消息协议编码（与 [`Self::from_u32`] 对应）。
    pub fn code(self) -> u32 {
        match self {
            Self::StrokeWidth => 1,
            Self::TextSize => 2,
            Self::StepRadius => 3,
            Self::EraserTol => 4,
        }
    }

    /// 工具对应的可调尺寸；无尺寸概念的工具（马赛克/高亮/无）返回 `None`。
    pub fn for_tool(tool: Tool) -> Option<Self> {
        match tool {
            Tool::Rect | Tool::Arrow | Tool::Ellipse | Tool::Line | Tool::Pen => {
                Some(Self::StrokeWidth)
            }
            Tool::Text | Tool::Watermark => Some(Self::TextSize),
            Tool::StepNo => Some(Self::StepRadius),
            Tool::Eraser => Some(Self::EraserTol),
            Tool::None | Tool::Mosaic | Tool::Highlight => None,
        }
    }
}

/// 交互模式。
#[derive(Debug, Clone)]
pub enum Mode {
    /// Tool::None：点击 = 选中已有标注。
    Idle,
    /// 正在绘制（橡皮筋）。
    Drawing {
        tool: Tool,
        anchor: Pt,
        cur: Pt,
    },
    /// 自由绘制中（画笔）：累积轨迹点。
    Freehand {
        pts: Vec<Pt>,
    },
    /// 单选（MVP）。
    Selected {
        id: u64,
    },
    /// 拖动移动。
    Moving {
        id: u64,
        grab: Pt,
        orig: Shape,
        cur: Pt,
    },
    /// 缩放（`anchor` = 对角固定点）。
    Resizing {
        id: u64,
        handle: Handle,
        anchor: Pt,
        orig: Shape,
        cur: Pt,
    },
    /// 文字输入中（Text 工具单击落点后）：正文为瞬态，完成后才作为一条
    /// Add 命令落入文档（Enter 确认提交、Esc / 点击别处完成）。
    Typing {
        anchor: Pt,
        text: String,
        kind: TextKind,
    },
    /// 橡皮拖动中：`ids` 为预览删除集合（松手才落一条命令）。
    Erasing {
        ids: Vec<u64>,
        last: Pt,
    },
}

/// 样式补丁（UI 无关；面板 → 控制器）。
#[derive(Debug, Clone, Default)]
pub struct StylePatch {
    pub stroke_color: Option<Rgba>,
    pub stroke_width: Option<i32>,
    pub text_color: Option<Rgba>,
    pub text_size: Option<i32>,
    pub text_bold: Option<bool>,
    pub text_italic: Option<bool>,
    /// Some(true/false) = 打开/关闭文字描边。
    pub text_stroke: Option<bool>,
    pub pen_kind: Option<PenKind>,
    pub mosaic_mode: Option<MosaicMode>,
    pub highlight_mode: Option<HighlightMode>,
    pub text_frame: Option<TextFrame>,
    pub badge: Option<BadgeStyle>,
}

/// 光标种类（映射到系统 `IDC_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Arrow,
    Cross,
    SizeAll,
    SizeNS,
    SizeWE,
    SizeNESW,
    SizeNWSE,
    /// 文字输入（IDC_IBEAM）。
    IBeam,
}

/// 控制柄 → 缩放光标（见 5.2）。
pub fn cursor_for_handle(h: Handle) -> CursorKind {
    match h {
        Handle::N | Handle::S => CursorKind::SizeNS,
        Handle::E | Handle::W => CursorKind::SizeWE,
        Handle::NE | Handle::SW => CursorKind::SizeNESW,
        Handle::NW | Handle::SE => CursorKind::SizeNWSE,
    }
}

/// 事件处理结果。
#[derive(Debug, Default)]
pub struct Action {
    /// 需要重绘的脏区（局部坐标；无则 None）。
    pub redraw: Option<Rect>,
    /// 需要切换的光标。
    pub cursor: Option<CursorKind>,
    /// Esc 未被标注消费 → 交还工具条关闭会话。
    pub esc_passthrough: bool,
}

/// 选区外选中态外扩量 / 步骤序号半径 / 最小尺寸 / 画笔步长 / 挂线容差
/// 等交互尺寸现由配置文件提供（见 `config::AnnotPrefs`，逻辑像素，按 DPI 换算）。

const VK_ESCAPE: usize = 0x1B;
const VK_DELETE: usize = 0x2E;
const VK_Z: usize = 0x5A;
const VK_Y: usize = 0x59;
const VK_R: usize = 0x52;
const VK_A: usize = 0x41;
const VK_N: usize = 0x4E;
/// 标注工具键扩展：B=马赛克 / H=高亮 / T=文字（与 SnowShot 的编辑器按键一致）
const VK_B: usize = 0x42;
const VK_H: usize = 0x48;
const VK_T: usize = 0x54;
/// 新增图形工具键：E=椭圆 / L=线段 / P=画笔。
const VK_E: usize = 0x45;
const VK_L: usize = 0x4C;
const VK_P: usize = 0x50;
/// 橡皮 `X` / 水印 `M`（避开已占用的 E/L/P/B/H/T）。
const VK_X: usize = 0x58;
const VK_M: usize = 0x4D;

/// 标注会话（逻辑层）。
pub struct AnnotSession {
    pub doc: AnnotDoc,
    pub mode: Mode,
    pub tool: Tool,
    /// 选区尺寸（局部坐标 0,0,w,h）。
    bounds: Rect,
    dpi: i32,
    /// 最近一次事件的 Shift 状态（绘制约束用）。
    shift: bool,
    /// 默认偏好（马赛克/高亮/画笔形态、文字样式、挂线开关）。
    prefs: AnnotPrefs,
}

impl AnnotSession {
    pub fn new(bounds: Rect, dpi: i32) -> Self {
        Self::new_with_prefs(bounds, dpi, AnnotPrefs::default())
    }

    pub fn new_with_prefs(bounds: Rect, dpi: i32, prefs: AnnotPrefs) -> Self {
        Self {
            doc: AnnotDoc::new(),
            mode: Mode::Idle,
            tool: Tool::None,
            bounds,
            dpi,
            shift: false,
            prefs,
        }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// 是否处于"正在绘制"（橡皮筋或自由画笔轨迹；决定预览是否带选中态）。
    pub fn drawing(&self) -> bool {
        matches!(self.mode, Mode::Drawing { .. } | Mode::Freehand { .. })
    }

    /// 是否处于文字输入中（预览渲染 + 光标绘制用）。
    pub fn typing(&self) -> bool {
        matches!(self.mode, Mode::Typing { .. })
    }

    /// 输入中的 `(锚点, 正文, 文字样式)`：供光标绘制与工具栏查询。
    pub fn typing_state(&self) -> Option<(Pt, String, TextStyle)> {
        match &self.mode {
            Mode::Typing { anchor, text, .. } => Some((*anchor, text.clone(), self.text_style())),
            _ => None,
        }
    }

    /// 当前输入类型（普通文字 / 水印）。
    #[allow(dead_code)]
    pub fn typing_kind(&self) -> Option<TextKind> {
        match &self.mode {
            Mode::Typing { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// 是否需要鼠标捕获（绘制/拖动/缩放/橡皮期间，见 5.1 的 SetCapture）。
    pub fn capturing(&self) -> bool {
        matches!(
            self.mode,
            Mode::Drawing { .. }
                | Mode::Freehand { .. }
                | Mode::Moving { .. }
                | Mode::Resizing { .. }
                | Mode::Erasing { .. }
        )
    }

    /// 当前选中项 id（Selected / Moving / Resizing）。
    pub fn selected_id(&self) -> Option<u64> {
        match &self.mode {
            Mode::Selected { id } | Mode::Moving { id, .. } | Mode::Resizing { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// 当前选中项形状（只读）。
    pub fn selected_shape(&self) -> Option<&Shape> {
        self.selected_id().and_then(|id| self.doc.find(id))
    }

    /// 渲染主循环需跳过的形状 id 集合。
    ///
    /// - 拖动/缩放中的「旧位置」提交副本与**选中态**形状（由 chrome/预览分支绘制）；
    /// - 橡皮拖过但尚未落命令的形状（预览删除）。
    pub fn render_skip_ids(&self) -> Vec<u64> {
        match &self.mode {
            Mode::Moving { id, .. } | Mode::Resizing { id, .. } | Mode::Selected { id } => {
                vec![*id]
            }
            Mode::Erasing { ids, .. } => ids.clone(),
            _ => Vec::new(),
        }
    }

    /// 兼容旧调用点：单个跳过 id。
    #[allow(dead_code)]
    pub fn render_skip_id(&self) -> Option<u64> {
        self.render_skip_ids().into_iter().next()
    }

    /// 移动中：被拖形状 id 与预览位移（渲染层据此让挂线文字的**预览**跟随，
    /// 落文档仍发生在 mouseup 的 `doc.transform`）。缩放的中点联动不预览跟随。
    pub fn attach_preview_delta(&self) -> Option<(u64, Pt)> {
        match &self.mode {
            Mode::Moving { id, grab, cur, .. } => {
                Some((*id, Pt::new(cur.x - grab.x, cur.y - grab.y)))
            }
            _ => None,
        }
    }

    /// 会话偏好快照（渲染链下传，避免逐形状/逐线段读全局配置）。
    pub fn prefs(&self) -> &AnnotPrefs {
        &self.prefs
    }

    /// 当前预览几何（绘制中的橡皮筋 / 自由轨迹 / 移动缩放中的临时位置 / 输入中的文字）。
    pub fn preview_shape(&self) -> Option<Shape> {
        match &self.mode {
            Mode::Drawing { tool, anchor, cur } => Some(self.new_shape(*tool, *anchor, *cur)),
            Mode::Freehand { pts } => Some(self.new_pen(pts.clone())),
            Mode::Typing { anchor, text, .. } => {
                Some(self.new_text_shape(*anchor, text.clone(), None))
            }
            Mode::Moving { orig, grab, cur, .. } => {
                Some(orig.translate(Pt::new(cur.x - grab.x, cur.y - grab.y)))
            }
            Mode::Resizing {
                orig,
                handle,
                anchor,
                cur,
                ..
            } => Some(orig.remap_bbox(
                orig.bbox(),
                self.resize_bbox(orig.bbox(), *anchor, *handle, *cur),
            )),
            _ => None,
        }
    }

    /// 需要绘制选中态（虚线框/控制柄）的形状。绘制中不显示选中态。
    pub fn chrome_shape(&self) -> Option<Shape> {
        match &self.mode {
            Mode::Selected { id } => self.doc.find(*id).cloned(),
            Mode::Moving { .. } | Mode::Resizing { .. } => self.preview_shape(),
            _ => None,
        }
    }

    // ── 事件入口 ──────────────────────────────────────────────

    /// 切换工具。与当前工具相同则回到 `Tool::None`（见 5.5「再按同键」）。
    pub fn set_tool(&mut self, t: Tool) -> Action {
        // 输入中的文字先完成（参照 SnowShot：切换工具即完成当前文字）
        let committed = self.commit_text();
        let before = union_opt(self.transient_bbox(), committed.redraw);
        let target = if self.tool == t { Tool::None } else { t };
        self.mode = Mode::Idle;
        self.tool = target;
        let cursor = if target != Tool::None {
            CursorKind::Cross
        } else {
            CursorKind::Arrow
        };
        Action {
            redraw: before,
            cursor: Some(cursor),
            esc_passthrough: false,
        }
    }

    pub fn on_l_down(&mut self, p: Pt, shift: bool) -> Action {
        self.shift = shift;
        // 输入中再次按下：先完成当前文字（点击别处 = 完成，参照 SnowShot）
        let committed = self.commit_text();
        let before = union_opt(self.transient_bbox(), committed.redraw);
        // 绘制/拖拽中再次按下：防御性收尾
        if !matches!(self.mode, Mode::Idle | Mode::Selected { .. }) {
            self.mode = Mode::Idle;
        }

        match self.tool {
            // 文字工具：单击 = 落点进入输入（不进入橡皮筋）
            Tool::Text => {
                self.mode = Mode::Typing {
                    anchor: p,
                    text: String::new(),
                    kind: TextKind::Normal,
                };
                return self.action(before, CursorKind::IBeam);
            }
            // 水印：同样复用文字输入，提交时生成整选区水印
            Tool::Watermark => {
                self.mode = Mode::Typing {
                    anchor: p,
                    text: String::new(),
                    kind: TextKind::Watermark,
                };
                return self.action(before, CursorKind::IBeam);
            }
            // 画笔：按下即开始累积轨迹
            Tool::Pen => {
                self.mode = Mode::Freehand { pts: vec![p] };
                return self.action(before, CursorKind::Cross);
            }
            // 橡皮：按下即收集命中并进入预览删除
            Tool::Eraser => {
                let mut ids = Vec::new();
                self.collect_erase_hits(p, &mut ids);
                self.mode = Mode::Erasing {
                    ids: ids.clone(),
                    last: p,
                };
                let dirty = self.eraser_dirty(&ids);
                return Action {
                    redraw: union_opt(before, dirty),
                    cursor: Some(CursorKind::Arrow),
                    esc_passthrough: false,
                };
            }
            Tool::None => {}
            _ => {
                self.mode = Mode::Drawing {
                    tool: self.tool,
                    anchor: p,
                    cur: p,
                };
                return self.action(before, CursorKind::Cross);
            }
        }

        // 控制柄优先（仅当前选中项）
        if let Mode::Selected { id } = self.mode {
            if let Some(shape) = self.doc.find(id).cloned() {
                if let Some(h) = hit_test::hit_handle(&shape, p, self.handle_tol()) {
                    let anchor = h.anchor(shape.bbox());
                    self.mode = Mode::Resizing {
                        id,
                        handle: h,
                        anchor,
                        orig: shape,
                        cur: p,
                    };
                    return self.action(before, cursor_for_handle(h));
                }
            }
        }

        // 标注命中（倒序 z 序，取最高层）
        let tol = self.hit_tol();
        let hit = self
            .doc
            .shapes()
            .iter()
            .rev()
            .find(|a| hit_test::hit_shape(&a.shape, p, tol))
            .map(|a| (a.id, a.shape.clone()));
        match hit {
            Some((id, orig)) => {
                self.mode = Mode::Moving {
                    id,
                    grab: p,
                    orig,
                    cur: p,
                };
                self.action(before, CursorKind::SizeAll)
            }
            None => {
                self.mode = Mode::Idle;
                self.action(before, self.cursor_at(p))
            }
        }
    }

    pub fn on_mouse_move(&mut self, p: Pt, shift: bool) -> Action {
        self.shift = shift;
        // 文字输入中：光标固定在锚点，移动不产生预览（光标种类也保持 IBeam）
        if matches!(self.mode, Mode::Typing { .. }) {
            return Action::default();
        }
        let before = self.transient_bbox();
        // 橡皮：沿 (last, p) 插值采样，收集命中 id（预览删除，不落命令）。
        if matches!(self.mode, Mode::Erasing { .. }) {
            let tol = self.eraser_tol();
            let last = match &self.mode {
                Mode::Erasing { last, .. } => *last,
                _ => p,
            };
            let mut samples = Vec::new();
            let dx = (p.x - last.x).abs();
            let dy = (p.y - last.y).abs();
            let n = (dx.max(dy) / (tol / 2).max(1)).max(1);
            for i in 1..=n {
                samples.push(Pt::new(
                    last.x + (p.x - last.x) * i / n,
                    last.y + (p.y - last.y) * i / n,
                ));
            }
            let mut found: Vec<u64> = Vec::new();
            for q in samples {
                for a in self.doc.shapes().iter().rev() {
                    if hit_test::hit_shape(&a.shape, q, tol) && !found.contains(&a.id) {
                        found.push(a.id);
                    }
                }
            }
            if let Mode::Erasing { ids, last } = &mut self.mode {
                for id in found {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                *last = p;
            }
            let ids = match &self.mode {
                Mode::Erasing { ids, .. } => ids.clone(),
                _ => Vec::new(),
            };
            let dirty = self.eraser_dirty(&ids);
            return Action {
                redraw: union_opt(before, dirty),
                cursor: Some(CursorKind::Arrow),
                esc_passthrough: false,
            };
        }
        let step = self.s(self.prefs.pen_min_step).max(1);
        match &mut self.mode {
            Mode::Drawing { cur, .. } | Mode::Moving { cur, .. } | Mode::Resizing { cur, .. } => {
                *cur = p;
            }
            Mode::Freehand { pts } => {
                // 按最小间距抽稀，避免高频鼠标采样让点数暴涨
                let far = pts
                    .last()
                    .map(|last| (p.x - last.x).abs().max((p.y - last.y).abs()) >= step)
                    .unwrap_or(true);
                if far {
                    pts.push(p);
                }
            }
            _ => {}
        }
        let after = self.transient_bbox();
        Action {
            redraw: union_opt(before, after),
            cursor: Some(self.cursor_at(p)),
            esc_passthrough: false,
        }
    }

    pub fn on_l_up(&mut self, p: Pt, shift: bool) -> Action {
        self.shift = shift;
        // 文字输入中：松开鼠标不提交（提交发生在点击别处 / Esc / 工具栏动作），
        // mode 保持 Typing——这里必须在 mem::replace 之前返回
        if matches!(self.mode, Mode::Typing { .. }) {
            return Action::default();
        }
        let before = self.transient_bbox();
        let mut extra: Option<Rect> = None;

        match std::mem::replace(&mut self.mode, Mode::Idle) {
            Mode::Drawing { tool, anchor, .. } => {
                let mut shape = self.new_shape(tool, anchor, p);
                if self.is_committable(&shape, tool, anchor, p) {
                    // 序号在放置那一刻固化（计数器只增不减，见 5.4）
                    if let Shape::StepNo { number, .. } = &mut shape {
                        *number = self.doc.take_step_number();
                    }
                    extra = Some(shape.bbox());
                    self.doc.add(shape);
                }
            }
            Mode::Freehand { pts } => {
                // 画笔：末端补一个 u 点，够长才落笔（过短视为误触）
                let mut pts = pts;
                if pts.last().map(|last| *last != p).unwrap_or(true) {
                    pts.push(p);
                }
                let shape = self.new_pen(pts);
                if self.pen_is_committable(&shape) {
                    extra = Some(shape.bbox());
                    self.doc.add(shape);
                }
            }
            Mode::Erasing { ids, .. } => {
                // 一次拖动 = 一条 DeleteGroup 撤销记录
                if let Some(d) = self.doc.delete_many(&ids) {
                    extra = Some(d);
                }
            }
            Mode::Moving { id, grab, orig, .. } => {
                let d = Pt::new(p.x - grab.x, p.y - grab.y);
                if d.x != 0 || d.y != 0 {
                    let new = orig.translate(d);
                    // 文字按真实字形渲染、可超出估算 bbox：移动文字时脏区用保守外扩
                    let text_dirty = if matches!(orig, Shape::Text { .. }) {
                        Some(rect_union(orig.dirty_bbox(), new.dirty_bbox()))
                    } else {
                        None
                    };
                    match self.doc.transform(id, orig, new) {
                        Some(dirty) => {
                            extra = Some(match text_dirty {
                                Some(e) => rect_union(dirty, e),
                                None => dirty,
                            });
                            self.mode = Mode::Selected { id };
                        }
                        // 目标已不存在：保持 Idle，不留悬空选中 id
                        None => {}
                    }
                } else {
                    self.mode = Mode::Selected { id };
                }
            }
            Mode::Resizing {
                id,
                handle,
                anchor,
                orig,
                ..
            } => {
                let new = orig.remap_bbox(
                    orig.bbox(),
                    self.resize_bbox(orig.bbox(), anchor, handle, p),
                );
                // 0×0 的「幽灵」图形：不可见却仍可被命中/选中 —— 丢弃本次变换
                let ghost = {
                    let b = new.bbox();
                    b.w == 0 && b.h == 0
                };
                if !ghost && new != orig {
                    match self.doc.transform(id, orig, new) {
                        Some(dirty) => {
                            extra = Some(dirty);
                            self.mode = Mode::Selected { id };
                        }
                        None => {}
                    }
                } else {
                    self.mode = Mode::Selected { id };
                }
            }
            _ => {}
        }

        let after = self.transient_bbox();
        Action {
            redraw: union_opt(union_opt(before, after), extra),
            cursor: Some(self.cursor_at(p)),
            esc_passthrough: false,
        }
    }

    pub fn on_key(&mut self, vk: usize, ctrl: bool, shift: bool) -> Action {
        let before = self.transient_bbox();

        // 输入中：编辑键（回车提交/退格/正文）经 `on_char`；这里只处理 Esc
        // —— 有内容即完成（避免误删正文，参照 SnowShot），空则退出输入；
        // 其余按键（Ctrl+Z 等）在输入中一律忽略。Esc 永不直接关闭会话。
        if matches!(self.mode, Mode::Typing { .. }) {
            if vk == VK_ESCAPE {
                let mut a = self.commit_text();
                a.redraw = union_opt(before, a.redraw);
                a.esc_passthrough = false;
                return a;
            }
            return Action::default();
        }

        // Ctrl+Shift+Delete：清空全部标注（一条撤销记录）
        if ctrl && shift && vk == VK_DELETE {
            let d = self.doc.clear_all();
            self.mode = Mode::Idle;
            return Action {
                redraw: union_opt(before, d),
                cursor: Some(CursorKind::Arrow),
                esc_passthrough: false,
            };
        }

        // Ctrl+Z / Ctrl+Y：撤销重做（模式不变，见 5.1）。
        // 捕获中（绘制/拖动/缩放）忽略：瞬态与命令栈交织会污染历史
        // （如撤销掉被拖形状后 mouseup 留下悬空选中 id）。
        if ctrl && (vk == VK_Z || vk == VK_Y) && self.capturing() {
            return Action::default();
        }
        if ctrl && vk == VK_Z {
            let d = self.doc.undo();
            return Action {
                redraw: union_opt(before, d),
                cursor: Some(self.cursor_at_fallback()),
                esc_passthrough: false,
            };
        }
        if ctrl && vk == VK_Y {
            let d = self.doc.redo();
            return Action {
                redraw: union_opt(before, d),
                cursor: Some(self.cursor_at_fallback()),
                esc_passthrough: false,
            };
        }
        if ctrl {
            return Action::default();
        }

        match vk {
            VK_DELETE => {
                if let Mode::Selected { id } = self.mode {
                    let d = self.doc.delete(id);
                    self.mode = Mode::Idle;
                    return Action {
                        redraw: union_opt(before, d),
                        cursor: Some(CursorKind::Arrow),
                        esc_passthrough: false,
                    };
                }
                Action::default()
            }
            VK_ESCAPE => {
                // 有绘制/选中 → 取消它们；
                if !matches!(self.mode, Mode::Idle) {
                    self.mode = Mode::Idle;
                    return Action {
                        redraw: before,
                        cursor: Some(CursorKind::Arrow),
                        esc_passthrough: false,
                    };
                }
                // 无绘制/选中但工具激活 → 回到 Tool::None
                if self.tool != Tool::None {
                    self.tool = Tool::None;
                    return Action {
                        redraw: before,
                        cursor: Some(CursorKind::Arrow),
                        esc_passthrough: false,
                    };
                }
                // 无事可取消 → 交还工具条关闭会话
                Action {
                    redraw: None,
                    cursor: None,
                    esc_passthrough: true,
                }
            }
            VK_R => self.set_tool(Tool::Rect),
            VK_A => self.set_tool(Tool::Arrow),
            VK_N => self.set_tool(Tool::StepNo),
            VK_E => self.set_tool(Tool::Ellipse),
            VK_L => self.set_tool(Tool::Line),
            VK_P => self.set_tool(Tool::Pen),
            VK_B => self.set_tool(Tool::Mosaic),
            VK_H => self.set_tool(Tool::Highlight),
            VK_T => self.set_tool(Tool::Text),
            VK_X => self.set_tool(Tool::Eraser),
            VK_M => self.set_tool(Tool::Watermark),
            _ => Action::default(),
        }
    }

    /// 输入中的字符（工具条经 `WM_APP_CHAR` 转发的 `WM_CHAR`）：
    /// `'\r'/'\n'` = 确认提交、`'\u{8}'` = 退格、可打印字符 = 追加。仅 Typing 模式消费。
    pub fn on_char(&mut self, c: char) -> Action {
        if !matches!(self.mode, Mode::Typing { .. }) {
            return Action::default();
        }
        let before = self.transient_bbox();
        if matches!(c, '\r' | '\n') {
            // 回车即确认提交（与文字输入子进程的 Enter 语义一致）
            let mut a = self.commit_text();
            a.redraw = union_opt(before, a.redraw);
            return a;
        }
        if let Mode::Typing { text, .. } = &mut self.mode {
            match c {
                '\u{8}' => {
                    text.pop();
                }
                c if c >= ' ' && c != '\u{7f}' => text.push(c),
                _ => return Action::default(),
            }
        }
        let after = self.transient_bbox();
        Action {
            redraw: union_opt(before, after),
            cursor: Some(CursorKind::IBeam),
            esc_passthrough: false,
        }
    }

    /// 完成输入中的文字：非空则作为一条 Add 命令落入文档（整体撤销，
    /// 参照 SnowShot 的文字 op）。返回的事件结果含脏区；Esc 路径不透传
    /// （由调用方决定后续）。无输入中文字时为无操作（**不改当前模式**）。
    pub fn commit_text(&mut self) -> Action {
        if !matches!(self.mode, Mode::Typing { .. }) {
            return Action::default();
        }
        let Mode::Typing { anchor, text, kind } = std::mem::replace(&mut self.mode, Mode::Idle) else {
            return Action::default();
        };
        if text.trim().is_empty() {
            // 退出输入也要恢复光标（否则 IBeam 残留到下一次 WM_SETCURSOR）
            return Action {
                redraw: None,
                cursor: Some(self.cursor_at_fallback()),
                esc_passthrough: false,
            };
        }
        if kind == TextKind::Watermark {
            let shape = self.new_watermark_shape(text);
            let dirty = shape.dirty_bbox();
            self.doc.add(shape);
            return Action {
                redraw: Some(dirty),
                cursor: Some(CursorKind::Cross),
                esc_passthrough: false,
            };
        }
        // 挂线：落点靠近线段/箭头时吸附，并把文字居中到该线段的**中点**
        //（与 `AnnotDoc::transform` 的重居中口径一致，线一移动文字就跟着走）。
        let attach = if self.prefs.text_attach {
            self.find_attach(anchor)
        } else {
            None
        };
        let mut shape = self.new_text_shape(anchor, text, attach);
        if let Some(id) = attach {
            if let Shape::Text { bbox, .. } = &shape {
                let mid = self.attach_mid(id).unwrap_or(anchor);
                let (ox, oy) = (mid.x - bbox.w / 2, mid.y - bbox.h / 2);
                shape = center_shape_text(shape, Pt::new(ox, oy));
            }
        }
        // 文字按真实字形渲染，可超出估算 bbox：脏区取保守外扩（宁大勿小）
        let dirty = shape.dirty_bbox();
        self.doc.add(shape);
        Action {
            redraw: Some(dirty),
            cursor: Some(CursorKind::Cross),
            esc_passthrough: false,
        }
    }

    /// 把"输入中的正文"整段替换为 `text`（文字输入子进程每次变化回传）。
    /// 非输入中时无操作、无重绘（正文显示在子进程的输入窗内）。
    pub fn set_typing_text(&mut self, text: String) -> Action {
        if let Mode::Typing { text: t, .. } = &mut self.mode {
            *t = text;
        }
        Action::default()
    }

    /// 用整段文本结束输入（文字输入子进程提交/结束时回传）：等价于把正文设为
    /// `text` 再 [`commit_text`](Self::commit_text)；脏区含输入前的光标区域。
    pub fn commit_typing_with(&mut self, text: String) -> Action {
        if !matches!(self.mode, Mode::Typing { .. }) {
            return Action::default();
        }
        let before = self.transient_bbox();
        if let Mode::Typing { text: t, .. } = &mut self.mode {
            *t = text;
        }
        let a = self.commit_text();
        Action {
            redraw: union_opt(before, a.redraw),
            cursor: a.cursor,
            esc_passthrough: false,
        }
    }

    /// 挂线目标的中点（线段/箭头）；找不到返回 `None`。
    fn attach_mid(&self, id: u64) -> Option<Pt> {
        super::doc::seg_mid(self.doc.find(id)?)
    }

    /// 光标查询（供 `WM_SETCURSOR`）。
    pub fn cursor_at(&self, p: Pt) -> CursorKind {
        match &self.mode {
            Mode::Drawing { .. } | Mode::Freehand { .. } => CursorKind::Cross,
            Mode::Erasing { .. } => CursorKind::Arrow,
            Mode::Typing { .. } => CursorKind::IBeam,
            Mode::Resizing { handle, .. } => cursor_for_handle(*handle),
            Mode::Moving { .. } => CursorKind::SizeAll,
            Mode::Selected { id } => {
                if let Some(shape) = self.doc.find(*id) {
                    if let Some(h) = hit_test::hit_handle(shape, p, self.handle_tol()) {
                        return cursor_for_handle(h);
                    }
                }
                CursorKind::SizeAll
            }
            Mode::Idle => {
                if self.tool != Tool::None {
                    CursorKind::Cross
                } else if self
                    .doc
                    .shapes()
                    .iter()
                    .rev()
                    .any(|a| hit_test::hit_shape(&a.shape, p, self.hit_tol()))
                {
                    CursorKind::SizeAll
                } else {
                    CursorKind::Arrow
                }
            }
        }
    }

    // ── 内部 ──────────────────────────────────────────────────

    fn s(&self, v: i32) -> i32 {
        crate::util::scaled(v, self.dpi)
    }

    fn hit_tol(&self) -> i32 {
        self.s(self.prefs.hit_tol)
    }

    fn handle_tol(&self) -> i32 {
        self.s(self.prefs.handle_hit)
    }

    fn chrome_ext(&self) -> i32 {
        self.s(self.prefs.chrome_ext)
    }

    fn step_radius(&self) -> i32 {
        self.s(self.prefs.step_radius)
    }

    fn min_size(&self) -> i32 {
        self.s(self.prefs.min_size)
    }

    /// 箭头头部长度（按配置的比与上下限，物理像素）。
    fn arrow_head(&self, width: i32) -> i32 {
        arrow_head_len(
            width,
            self.prefs.arrow_width_ratio,
            self.s(self.prefs.arrow_len_min),
            self.s(self.prefs.arrow_len_max),
        )
    }

    /// 绘制中的形状（`cur` 已含 Shift 约束）。
    fn new_shape(&self, tool: Tool, anchor: Pt, cur: Pt) -> Shape {
        match tool {
            Tool::Rect => Shape::Rect {
                bbox: if self.shift {
                    square_rect(anchor, cur)
                } else {
                    norm_rect(anchor, cur)
                },
                style: StrokeStyle {
                    color: self.prefs.stroke_color,
                    width: self.s(self.prefs.stroke_width),
                },
                fill: None,
            },
            Tool::Arrow => {
                let to = if self.shift { snap_45(anchor, cur) } else { cur };
                let width = self.s(self.prefs.stroke_width);
                Shape::Arrow {
                    from: anchor,
                    to,
                    style: StrokeStyle {
                        color: self.prefs.stroke_color,
                        width,
                    },
                    head_len: self.arrow_head(width),
                }
            }
            Tool::StepNo => Shape::StepNo {
                center: cur,
                radius: self.step_radius(),
                number: self.doc.peek_step_number(),
                style: self.prefs.badge.clone(),
            },
            Tool::Ellipse => Shape::Ellipse {
                bbox: if self.shift {
                    square_rect(anchor, cur)
                } else {
                    norm_rect(anchor, cur)
                },
                style: StrokeStyle {
                    color: self.prefs.stroke_color,
                    width: self.s(self.prefs.stroke_width),
                },
            },
            Tool::Line => {
                let to = if self.shift { snap_45(anchor, cur) } else { cur };
                Shape::Line {
                    from: anchor,
                    to,
                    style: StrokeStyle {
                        color: self.prefs.stroke_color,
                        width: self.s(self.prefs.stroke_width),
                    },
                }
            }
            Tool::Pen => self.new_pen(vec![anchor, cur]),
            Tool::Mosaic => Shape::Mosaic {
                bbox: if self.shift {
                    square_rect(anchor, cur)
                } else {
                    norm_rect(anchor, cur)
                },
                mode: self.prefs.mosaic_mode,
            },
            Tool::Highlight => Shape::Highlight {
                bbox: if self.shift {
                    square_rect(anchor, cur)
                } else {
                    norm_rect(anchor, cur)
                },
                color: self.prefs.highlight_color,
                mode: self.prefs.highlight_mode,
            },
            Tool::Text => self.new_text_shape(anchor, String::new(), None),
            Tool::Watermark | Tool::Eraser => Shape::Rect {
                bbox: norm_rect(anchor, cur),
                style: StrokeStyle {
                    color: self.prefs.stroke_color,
                    width: self.s(self.prefs.stroke_width),
                },
                fill: None,
            },
            Tool::None => Shape::Rect {
                bbox: norm_rect(anchor, cur),
                style: StrokeStyle {
                    color: self.prefs.stroke_color,
                    width: self.s(self.prefs.stroke_width),
                },
                fill: None,
            },
        }
    }

    /// 画笔形状：轨迹 + 基础样式（形态取自偏好）。
    fn new_pen(&self, pts: Vec<Pt>) -> Shape {
        Shape::Pen {
            pts,
            style: PenStyle {
                color: self.prefs.stroke_color,
                width: self.s(self.prefs.stroke_width),
                kind: self.prefs.pen_kind,
            },
        }
    }

    /// 文字样式：按偏好构造（颜色/字号来自配置、粗斜体取自偏好）。
    fn text_style(&self) -> TextStyle {
        TextStyle {
            color: self.prefs.text_color,
            font_px: self.s(self.prefs.text_size),
            bold: self.prefs.text_bold,
            italic: self.prefs.text_italic,
            outline: self.prefs.text_stroke,
        }
    }

    /// 文字形状：估算包围盒（命中/移动用）+ 样式 + 外框 + 挂线 id。
    fn new_text_shape(&self, origin: Pt, text: String, attach: Option<u64>) -> Shape {
        let style = self.text_style();
        let (w, h) = text_est_size(&text, style.font_px);
        Shape::Text {
            origin,
            text,
            bbox: Rect::new(origin.x, origin.y, w.max(2), h.max(8)),
            style,
            frame: self.prefs.text_frame,
            attach,
        }
    }

    /// 水印形状：包围盒恒为整个选区（平铺/居中由渲染层按 `wm.tile` 决定）。
    fn new_watermark_shape(&self, text: String) -> Shape {
        let style = self.text_style();
        let wm = WatermarkStyle {
            angle_deg: self.prefs.watermark_angle,
            opacity: self.prefs.watermark_opacity,
            tile: self.prefs.watermark_tile,
            gap: self.prefs.watermark_gap,
        }
        .clamped(self.dpi);
        Shape::Watermark {
            bbox: self.bounds,
            text,
            style,
            wm,
        }
    }

    /// 应用样式补丁：作用于选中项（有则 restyle），否则更新默认偏好。
    pub fn apply_style(&mut self, patch: StylePatch) -> Action {
        let before = self.transient_bbox();
        if let Some(id) = self.selected_id() {
            if let Some(cur) = self.doc.find(id).cloned() {
                let new = restyle_shape(&cur, &patch, self.dpi);
                if new != cur {
                    if let Some(d) = self.doc.restyle(id, new) {
                        let cursor = self.cursor_at_fallback();
                        return Action {
                            redraw: union_opt(union_opt(before, self.transient_bbox()), Some(d)),
                            cursor: Some(cursor),
                            esc_passthrough: false,
                        };
                    }
                }
            }
        }
        self.prefs = apply_prefs(self.prefs, &patch);
        let cursor = self.cursor_at_fallback();
        self.action(before, cursor)
    }

    /// 直接设置默认偏好（颜色面板同步/配置下发）。
    #[allow(dead_code)]
    pub fn set_prefs(&mut self, prefs: AnnotPrefs) -> Action {
        let before = self.transient_bbox();
        self.prefs = prefs;
        let cursor = self.cursor_at_fallback();
        self.action(before, cursor)
    }

    /// Ctrl+滚轮调整标注默认尺寸（线宽/字号/序号半径/橡皮容差）。
    pub fn adjust_size(&mut self, kind: SizeKind, delta: i32) -> Action {
        let before = self.transient_bbox();
        match kind {
            SizeKind::StrokeWidth => {
                self.prefs.stroke_width = (self.prefs.stroke_width + delta).clamp(1, 200);
            }
            SizeKind::TextSize => {
                self.prefs.text_size = (self.prefs.text_size + delta).clamp(4, 400);
            }
            SizeKind::StepRadius => {
                self.prefs.step_radius = (self.prefs.step_radius + delta).clamp(2, 200);
            }
            SizeKind::EraserTol => {
                self.prefs.eraser_tol = (self.prefs.eraser_tol + delta).clamp(0, 200);
            }
        }
        let cursor = self.cursor_at_fallback();
        self.action(before, cursor)
    }

    /// 当前尺寸值（供工具条提示）。
    pub fn size_value(&self, kind: SizeKind) -> i32 {
        match kind {
            SizeKind::StrokeWidth => self.prefs.stroke_width,
            SizeKind::TextSize => self.prefs.text_size,
            SizeKind::StepRadius => self.prefs.step_radius,
            SizeKind::EraserTol => self.prefs.eraser_tol,
        }
    }

    /// 清空全部（一条撤销记录）。Esc 语义不变。
    pub fn clear_all(&mut self) -> Action {
        let before = self.transient_bbox();
        self.mode = Mode::Idle;
        let d = self.doc.clear_all();
        Action {
            redraw: union_opt(before, d),
            cursor: Some(CursorKind::Arrow),
            esc_passthrough: false,
        }
    }

    fn eraser_tol(&self) -> i32 {
        self.s(self.prefs.eraser_tol).max(1)
    }

    /// 收集 `p` 处命中的标注 id（倒序 z 序，去重）。
    fn collect_erase_hits(&self, p: Pt, ids: &mut Vec<u64>) {
        let tol = self.eraser_tol();
        for a in self.doc.shapes().iter().rev() {
            if hit_test::hit_shape(&a.shape, p, tol) && !ids.contains(&a.id) {
                ids.push(a.id);
            }
        }
    }

    /// 橡皮预览脏区（被擦形状 bbox 并集 + 外扩余量）。
    fn eraser_dirty(&self, ids: &[u64]) -> Option<Rect> {
        let mut dirty: Option<Rect> = None;
        for id in ids {
            if let Some(s) = self.doc.find(*id) {
                let r = s.bbox().inflate(self.chrome_ext());
                dirty = union_opt(dirty, Some(r));
            }
        }
        dirty
    }

    /// 在落点附近寻找可挂线的线段/箭头，返回其 id（取最近的一条）。
    fn find_attach(&self, p: Pt) -> Option<u64> {
        let tol = self.s(self.prefs.attach_tol).max(1) as f32;
        let mut best: Option<(u64, f32)> = None;
        for a in self.doc.shapes() {
            let (from, to) = match &a.shape {
                Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => (*from, *to),
                _ => continue,
            };
            let d = super::geometry::dist_point_segment(p, from, to);
            if d <= tol && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((a.id, d));
            }
        }
        best.map(|(id, _)| id)
    }

    /// 缩放：锚点固定。角柄用对角固定点 `anchor` + 当前点规范化；边柄只移动该边、
    /// 正交方向保持原尺寸（见 5.3）。
    fn resize_bbox(&self, bbox0: Rect, anchor: Pt, handle: Handle, cur: Pt) -> Rect {
        let (l, t, r, b) = (bbox0.x, bbox0.y, bbox0.right(), bbox0.bottom());
        match handle {
            // 边柄：只移动该边，正交方向保持原尺寸
            Handle::E => Rect::new(bbox0.x.min(cur.x), t, (cur.x - bbox0.x).abs(), bbox0.h),
            Handle::W => Rect::new(r.min(cur.x), t, (r - cur.x).abs(), bbox0.h),
            Handle::S => Rect::new(l, bbox0.y.min(cur.y), bbox0.w, (cur.y - bbox0.y).abs()),
            Handle::N => Rect::new(l, b.min(cur.y), bbox0.w, (b - cur.y).abs()),
            // 角柄：对角固定点 + 当前点规范化（拖过 anchor 自然翻转）
            Handle::NE | Handle::SE | Handle::SW | Handle::NW => norm_rect(anchor, cur),
        }
    }

    /// 是否满足最小尺寸（否则视为误触丢弃）。StepNo / Text 单击即有效。
    fn is_committable(&self, shape: &Shape, tool: Tool, anchor: Pt, cur: Pt) -> bool {
        let min = self.min_size();
        match tool {
            Tool::StepNo | Tool::Text | Tool::Pen => true,
            Tool::Eraser | Tool::Watermark => false,
            Tool::Arrow | Tool::Line => {
                let dx = (cur.x - anchor.x) as f32;
                let dy = (cur.y - anchor.y) as f32;
                (dx * dx + dy * dy).sqrt() >= min as f32
            }
            Tool::Rect | Tool::Ellipse | Tool::Mosaic | Tool::Highlight => {
                let b = shape.bbox();
                b.w >= min && b.h >= min
            }
            Tool::None => false,
        }
    }

    /// 画笔是否够长可落笔（≥2 个点且轨迹总长 ≥ 最小尺寸）。
    fn pen_is_committable(&self, shape: &Shape) -> bool {
        let Shape::Pen { pts, .. } = shape else {
            return false;
        };
        if pts.len() < 2 {
            return false;
        }
        let total: f32 = pts
            .windows(2)
            .map(|w| {
                let dx = (w[1].x - w[0].x) as f32;
                let dy = (w[1].y - w[0].y) as f32;
                (dx * dx + dy * dy).sqrt()
            })
            .sum();
        total >= self.min_size() as f32
    }

    /// 当前瞬时可视（预览 + 选中态）区域。
    fn transient_bbox(&self) -> Option<Rect> {
        // 画笔轨迹直接算包围盒，不为预览克隆整条 Vec<Pt>（mousemove 高频路径）
        let preview = match &self.mode {
            Mode::Freehand { pts } => {
                Some(pen_bounds(pts).inflate((self.s(self.prefs.stroke_width) / 2).max(1)))
            }
            Mode::Erasing { ids, .. } => self.eraser_dirty(ids),
            // 文字预览用保守脏区（真实字形可超出估算 bbox）
            _ => self.preview_shape().map(|s| s.dirty_bbox()),
        };
        let chrome = self
            .chrome_shape()
            .map(|s| s.bbox().inflate(self.chrome_ext()));
        // 挂线文字的预览跟随（渲染层按 attach_preview_delta 平移绘制）：
        // 其旧位置与新位置都必须进脏区，否则旧位置残留、新位置被裁剪
        let mut tethered: Option<Rect> = None;
        if let Some((hid, d)) = self.attach_preview_delta() {
            for a in self.doc.shapes() {
                if let Shape::Text { attach: Some(x), bbox, .. } = &a.shape {
                    if *x == hid {
                        let moved = Rect::new(bbox.x + d.x, bbox.y + d.y, bbox.w, bbox.h);
                        let r = rect_union(*bbox, moved);
                        tethered = union_opt(tethered, Some(r));
                    }
                }
            }
        }
        union_opt(union_opt(preview, chrome), tethered)
    }

    fn action(&self, before: Option<Rect>, cursor: CursorKind) -> Action {
        Action {
            redraw: union_opt(before, self.transient_bbox()),
            cursor: Some(cursor),
            esc_passthrough: false,
        }
    }

    /// 无鼠标位置时的光标兜底（键盘事件用）。
    fn cursor_at_fallback(&self) -> CursorKind {
        match &self.mode {
            Mode::Drawing { .. } | Mode::Freehand { .. } => CursorKind::Cross,
            Mode::Erasing { .. } => CursorKind::Arrow,
            Mode::Typing { .. } => CursorKind::IBeam,
            Mode::Resizing { handle, .. } => cursor_for_handle(*handle),
            Mode::Moving { .. } | Mode::Selected { .. } => CursorKind::SizeAll,
            Mode::Idle => {
                if self.tool != Tool::None {
                    CursorKind::Cross
                } else {
                    CursorKind::Arrow
                }
            }
        }
    }
}

/// 样式补丁应用到单个形状（**绝不**解绑挂线文字）。
/// 注意：补丁里的 `stroke_width` / `text_size` 来自颜色面板的**当前实际值**
///（选中形状时是物理像素，无选中时是逻辑像素偏好），因此这里不再二次缩放。
fn restyle_shape(cur: &Shape, p: &StylePatch, _dpi: i32) -> Shape {
    let s = |v: i32| v;
    match cur {
        Shape::Rect { bbox, style, fill } => Shape::Rect {
            bbox: *bbox,
            style: StrokeStyle {
                color: p.stroke_color.unwrap_or(style.color),
                width: p.stroke_width.map(s).unwrap_or(style.width),
            },
            fill: *fill,
        },
        Shape::Ellipse { bbox, style } => Shape::Ellipse {
            bbox: *bbox,
            style: StrokeStyle {
                color: p.stroke_color.unwrap_or(style.color),
                width: p.stroke_width.map(s).unwrap_or(style.width),
            },
        },
        Shape::Line { from, to, style } => Shape::Line {
            from: *from,
            to: *to,
            style: StrokeStyle {
                color: p.stroke_color.unwrap_or(style.color),
                width: p.stroke_width.map(s).unwrap_or(style.width),
            },
        },
        Shape::Arrow {
            from,
            to,
            style,
            head_len,
        } => Shape::Arrow {
            from: *from,
            to: *to,
            style: StrokeStyle {
                color: p.stroke_color.unwrap_or(style.color),
                width: p.stroke_width.map(s).unwrap_or(style.width),
            },
            head_len: *head_len,
        },
        Shape::Pen { pts, style } => Shape::Pen {
            pts: pts.clone(),
            style: PenStyle {
                color: p.stroke_color.unwrap_or(style.color),
                width: p.stroke_width.map(s).unwrap_or(style.width),
                kind: p.pen_kind.unwrap_or(style.kind),
            },
        },
        Shape::Mosaic { bbox, mode } => Shape::Mosaic {
            bbox: *bbox,
            mode: p.mosaic_mode.unwrap_or(*mode),
        },
        Shape::Highlight { bbox, color, mode } => Shape::Highlight {
            bbox: *bbox,
            color: p.stroke_color.unwrap_or(*color),
            mode: p.highlight_mode.unwrap_or(*mode),
        },
        Shape::StepNo {
            center,
            radius,
            number,
            style,
        } => Shape::StepNo {
            center: *center,
            radius: *radius,
            number: *number,
            style: p.badge.unwrap_or(*style),
        },
        // 文字：保留 origin 旁的挂线关系；改字号时以原 bbox 中心重排，避免脱离线段。
        Shape::Text {
            origin: _,
            text,
            bbox,
            style,
            frame,
            attach,
        } => {
            let mut st = TextStyle {
                color: p.text_color.unwrap_or(style.color),
                font_px: p.text_size.map(s).unwrap_or(style.font_px),
                bold: p.text_bold.unwrap_or(style.bold),
                italic: p.text_italic.unwrap_or(style.italic),
                outline: p.text_stroke.unwrap_or(style.outline),
            };
            st.font_px = st.font_px.max(1);
            let (w, h) = text_est_size(text, st.font_px);
            let c = bbox.center();
            Shape::Text {
                origin: Pt::new(c.x - w.max(2) / 2, c.y - h.max(8) / 2),
                text: text.clone(),
                bbox: Rect::new(c.x - w.max(2) / 2, c.y - h.max(8) / 2, w.max(2), h.max(8)),
                style: st,
                frame: p.text_frame.unwrap_or(*frame),
                attach: *attach,
            }
        }
        Shape::Watermark {
            bbox,
            text,
            style,
            wm,
        } => Shape::Watermark {
            bbox: *bbox,
            text: text.clone(),
            style: TextStyle {
                color: p.text_color.unwrap_or(style.color),
                font_px: p.text_size.map(s).unwrap_or(style.font_px).max(1),
                bold: p.text_bold.unwrap_or(style.bold),
                italic: p.text_italic.unwrap_or(style.italic),
                outline: p.text_stroke.unwrap_or(style.outline),
            },
            wm: *wm,
        },
    }
}

/// 无选中项时，把补丁写入默认偏好（影响后续新建标注）。
fn apply_prefs(mut prefs: AnnotPrefs, p: &StylePatch) -> AnnotPrefs {
    if let Some(v) = p.stroke_color {
        prefs.stroke_color = v;
    }
    if let Some(v) = p.stroke_width {
        prefs.stroke_width = v.max(1);
    }
    if let Some(v) = p.text_color {
        prefs.text_color = v;
    }
    if let Some(v) = p.text_size {
        prefs.text_size = v.max(4);
    }
    if let Some(v) = p.text_bold {
        prefs.text_bold = v;
    }
    if let Some(v) = p.text_italic {
        prefs.text_italic = v;
    }
    if let Some(v) = p.text_stroke {
        prefs.text_stroke = v;
    }
    if let Some(v) = p.pen_kind {
        prefs.pen_kind = v;
    }
    if let Some(v) = p.mosaic_mode {
        prefs.mosaic_mode = v;
    }
    if let Some(v) = p.highlight_mode {
        prefs.highlight_mode = v;
    }
    if let Some(v) = p.text_frame {
        prefs.text_frame = v;
    }
    if let Some(v) = p.badge {
        prefs.badge = v;
    }
    prefs
}

fn sign(v: i32) -> i32 {
    if v < 0 {
        -1
    } else {
        1
    }
}

/// 把文字形状的 origin/bbox 移到 `origin`（左上角）；非文字原样返回。
fn center_shape_text(shape: Shape, origin: Pt) -> Shape {
    match shape {
        Shape::Text {
            text,
            bbox,
            style,
            frame,
            attach,
            ..
        } => Shape::Text {
            origin,
            text,
            bbox: Rect::new(origin.x, origin.y, bbox.w, bbox.h),
            style,
            frame,
            attach,
        },
        other => other,
    }
}

/// Shift 约束正方形（绘制期，见 7）：边长取 |dx|、|dy| 的较大者，方向按 dx/dy 符号延伸。
fn square_rect(a: Pt, b: Pt) -> Rect {
    let side = (b.x - a.x).abs().max((b.y - a.y).abs());
    let ex = a.x + side * sign(b.x - a.x);
    let ey = a.y + side * sign(b.y - a.y);
    norm_rect(a, Pt::new(ex, ey))
}

/// Shift 约束箭头 45° 步进（见 7）。
fn snap_45(from: Pt, to: Pt) -> Pt {
    let dx = (to.x - from.x) as f32;
    let dy = (to.y - from.y) as f32;
    let hyp = (dx * dx + dy * dy).sqrt();
    if hyp < 0.5 {
        return to;
    }
    let step = std::f32::consts::FRAC_PI_4;
    let theta = dy.atan2(dx);
    let snapped = (theta / step).round() * step;
    Pt::new(
        from.x + (hyp * snapped.cos()).round() as i32,
        from.y + (hyp * snapped.sin()).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess() -> AnnotSession {
        AnnotSession::new(Rect::new(0, 0, 400, 300), 96)
    }

    #[test]
    fn square_rect_constrains_side() {
        assert_eq!(square_rect(Pt::new(10, 10), Pt::new(50, 30)), Rect::new(10, 10, 40, 40));
        // 反向拖动：从锚点朝左上延伸，方向按 dx/dy 符号
        assert_eq!(square_rect(Pt::new(50, 30), Pt::new(10, 10)), Rect::new(10, -10, 40, 40));
    }

    #[test]
    fn snap_45_produces_axis_or_diagonal() {
        // 近似水平 → 吸附到 0°
        let p = snap_45(Pt::new(0, 0), Pt::new(100, 5));
        assert_eq!(p, Pt::new(100, 0));
        // 近似 45° → 吸附到 45°
        let p = snap_45(Pt::new(0, 0), Pt::new(100, 90));
        assert!((p.x - p.y).abs() <= 1, "应吸附到对角：{p:?}");
    }

    #[test]
    fn draw_rect_commit_and_undo() {
        let mut s = sess();
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_mouse_move(Pt::new(60, 40), false);
        let a = s.on_l_up(Pt::new(60, 40), false);
        assert_eq!(s.doc.len(), 1);
        assert!(a.redraw.is_some());
        // 撤销
        let a = s.on_key(VK_Z, true, false);
        assert_eq!(s.doc.len(), 0);
        assert!(a.redraw.is_some());
        // 重做
        s.on_key(VK_Y, true, false);
        assert_eq!(s.doc.len(), 1);
    }

    #[test]
    fn tiny_drag_is_discarded() {
        let mut s = sess();
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(11, 11), false); // 1px < 3px 阈值
        assert_eq!(s.doc.len(), 0, "误触不应产生标注");
    }

    #[test]
    fn stepno_click_places_and_increments() {
        let mut s = sess();
        s.set_tool(Tool::StepNo);
        for (i, at) in [(20, 20), (80, 20), (140, 20)].iter().enumerate() {
            s.on_l_down(Pt::new(at.0, at.1), false);
            s.on_l_up(Pt::new(at.0, at.1), false);
            assert_eq!(s.doc.len(), i + 1);
        }
        let nums: Vec<u32> = s
            .doc
            .shapes()
            .iter()
            .filter_map(|a| match a.shape {
                Shape::StepNo { number, .. } => Some(number),
                _ => None,
            })
            .collect();
        assert_eq!(nums, vec![1, 2, 3]);
    }

    #[test]
    fn select_move_and_delete_flow() {
        let mut s = sess();
        // 先画一个矩形
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(110, 60), false);
        let id = s.doc.shapes()[0].id;

        // 关工具 → 点选（不移动 → Selected）
        s.set_tool(Tool::Rect); // 再按同键 → None
        assert_eq!(s.tool(), Tool::None);
        s.on_l_down(Pt::new(60, 30), false);
        s.on_l_up(Pt::new(60, 30), false);
        assert!(matches!(s.mode, Mode::Selected { id: x } if x == id));

        // 拖动移动
        s.on_l_down(Pt::new(60, 30), false);
        s.on_mouse_move(Pt::new(70, 40), false);
        s.on_l_up(Pt::new(70, 40), false);
        match s.doc.find(id) {
            Some(Shape::Rect { bbox, .. }) => assert_eq!(*bbox, Rect::new(20, 20, 100, 50)),
            other => panic!("应为平移后的矩形，得到 {other:?}"),
        }

        // Delete 删除
        s.on_key(VK_DELETE, false, false);
        assert!(s.doc.is_empty());
        // 撤销恢复
        s.on_key(VK_Z, true, false);
        assert_eq!(s.doc.len(), 1);
    }

    #[test]
    fn resize_se_handle_uses_opposite_anchor() {
        let mut s = sess();
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(110, 60), false);
        let id = s.doc.shapes()[0].id;
        s.set_tool(Tool::Rect); // -> None

        // 选中
        s.on_l_down(Pt::new(60, 30), false);
        s.on_l_up(Pt::new(60, 30), false);
        // 拖 SE 控制柄 (110,60) → (150,100)
        s.on_l_down(Pt::new(110, 60), false);
        assert!(matches!(s.mode, Mode::Resizing { handle: Handle::SE, .. }));
        s.on_mouse_move(Pt::new(150, 100), false);
        s.on_l_up(Pt::new(150, 100), false);
        match s.doc.find(id) {
            Some(Shape::Rect { bbox, .. }) => assert_eq!(*bbox, Rect::new(10, 10, 140, 90)),
            other => panic!("应为缩放后的矩形，得到 {other:?}"),
        }
    }

    #[test]
    fn resize_e_handle_keeps_height() {
        let mut s = sess();
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(110, 60), false);
        let id = s.doc.shapes()[0].id;
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(60, 30), false);
        s.on_l_up(Pt::new(60, 30), false);
        // 拖 E 边中点 (110,35) → (180, 999)：高度应保持 50
        s.on_l_down(Pt::new(110, 35), false);
        assert!(matches!(s.mode, Mode::Resizing { handle: Handle::E, .. }));
        s.on_l_up(Pt::new(180, 999), false);
        match s.doc.find(id) {
            Some(Shape::Rect { bbox, .. }) => assert_eq!(*bbox, Rect::new(10, 10, 170, 50)),
            other => panic!("应为仅改宽度的矩形，得到 {other:?}"),
        }
    }

    #[test]
    fn esc_passthrough_only_when_nothing_to_cancel() {
        let mut s = sess();
        // 空会话 → Esc 交还工具条
        assert!(s.on_key(VK_ESCAPE, false, false).esc_passthrough);
        // 工具激活 → 第一次 Esc 回 None，不 passthrough
        s.set_tool(Tool::Rect);
        let a = s.on_key(VK_ESCAPE, false, false);
        assert!(!a.esc_passthrough);
        assert_eq!(s.tool(), Tool::None);
        // 再次 Esc → passthrough
        assert!(s.on_key(VK_ESCAPE, false, false).esc_passthrough);
    }

    #[test]
    fn drawing_esc_cancels_without_commit() {
        let mut s = sess();
        s.set_tool(Tool::Arrow);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_mouse_move(Pt::new(80, 80), false);
        let a = s.on_key(VK_ESCAPE, false, false);
        assert!(!a.esc_passthrough);
        assert!(matches!(s.mode, Mode::Idle));
        assert_eq!(s.doc.len(), 0);
        assert_eq!(s.tool(), Tool::Arrow, "取消绘制不改变工具");
    }

    #[test]
    fn toolbar_tool_key_toggles() {
        let mut s = sess();
        s.on_key(VK_R, false, false);
        assert_eq!(s.tool(), Tool::Rect);
        s.on_key(VK_R, false, false);
        assert_eq!(s.tool(), Tool::None);
    }

    #[test]
    fn cursor_reflects_state() {
        let mut s = sess();
        assert_eq!(s.cursor_at(Pt::new(1, 1)), CursorKind::Arrow);
        s.set_tool(Tool::Rect);
        assert_eq!(s.cursor_at(Pt::new(1, 1)), CursorKind::Cross);
    }

    #[test]
    fn mosaic_draw_commit_and_undo() {
        let mut s = sess();
        s.on_key(VK_B, false, false);
        assert_eq!(s.tool(), Tool::Mosaic);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_mouse_move(Pt::new(60, 40), false);
        let a = s.on_l_up(Pt::new(60, 40), false);
        assert_eq!(s.doc.len(), 1);
        assert!(matches!(s.doc.shapes()[0].shape, Shape::Mosaic { .. }));
        assert!(a.redraw.is_some());
        s.on_key(VK_Z, true, false);
        assert_eq!(s.doc.len(), 0);
    }

    #[test]
    fn mosaic_tiny_drag_discarded() {
        let mut s = sess();
        s.set_tool(Tool::Mosaic);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(11, 11), false);
        assert_eq!(s.doc.len(), 0, "小于阈值不产生马赛克");
    }

    #[test]
    fn highlight_draw_commit_and_move() {
        let mut s = sess();
        s.set_tool(Tool::Highlight);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(110, 40), false);
        assert_eq!(s.doc.len(), 1);
        match &s.doc.shapes()[0].shape {
            Shape::Highlight { bbox, color, .. } => {
                assert_eq!(*bbox, Rect::new(10, 10, 100, 30));
                assert_eq!(*color, crate::annot::style::HIGHLIGHT);
            }
            _ => panic!("应为高亮"),
        }
        // 关工具（再按同键 → None）→ 点选拖动移动
        s.set_tool(Tool::Highlight);
        assert_eq!(s.tool(), Tool::None);
        let id = s.doc.shapes()[0].id;
        s.on_l_down(Pt::new(60, 20), false);
        s.on_mouse_move(Pt::new(80, 30), false);
        s.on_l_up(Pt::new(80, 30), false);
        assert_eq!(s.doc.find(id).unwrap().bbox(), Rect::new(30, 20, 100, 30));
    }

    #[test]
    fn text_typing_flow_commit_on_esc() {
        let mut s = sess();
        s.set_tool(Tool::Text);
        assert_eq!(s.tool(), Tool::Text);
        // 单击落点进入输入
        s.on_l_down(Pt::new(10, 10), false);
        assert!(s.typing());
        // 松开鼠标不提交、不退出
        s.on_l_up(Pt::new(10, 10), false);
        assert!(s.typing());
        assert_eq!(s.doc.len(), 0);
        // 逐字输入（WM_CHAR）
        for c in "Hi!".chars() {
            s.on_char(c);
        }
        assert!(s.typing());
        assert_eq!(s.doc.len(), 0, "输入中不落入文档");
        // Esc = 完成（非空）
        let a = s.on_key(VK_ESCAPE, false, false);
        assert!(!s.typing());
        assert_eq!(s.doc.len(), 1);
        assert!(!a.esc_passthrough, "输入中 Esc 不关闭会话");
        match &s.doc.shapes()[0].shape {
            Shape::Text { origin, text, .. } => {
                assert_eq!(*origin, Pt::new(10, 10));
                assert_eq!(text, "Hi!");
            }
            _ => panic!("应为文字"),
        }
        // 整体撤销
        s.on_key(VK_Z, true, false);
        assert_eq!(s.doc.len(), 0);
    }

    #[test]
    fn text_enter_commits_and_backspace_edits() {
        let mut s = sess();
        s.set_tool(Tool::Text);
        s.on_l_down(Pt::new(5, 5), false);
        s.on_char('a');
        s.on_char('b');
        s.on_char('\u{8}'); // 退格 → "a"
        let a = s.on_char('\r'); // Enter 即确认提交
        assert!(!s.typing(), "回车后应退出输入");
        assert_eq!(s.doc.len(), 1, "回车提交后应落入文档");
        assert!(a.redraw.is_some(), "提交需重绘脏区");
        match &s.doc.shapes()[0].shape {
            Shape::Text { text, .. } => assert_eq!(text, "a"),
            other => panic!("应为文字，得到 {other:?}"),
        }
        // 空文字 Esc：退出输入、不产生标注、不透传。
        // （Enter 完成第一处文字后工具仍是 Text，直接再次落点开始第二处输入）
        s.on_l_down(Pt::new(9, 9), false);
        s.on_l_up(Pt::new(9, 9), false);
        let a = s.on_key(VK_ESCAPE, false, false);
        assert!(!s.typing());
        assert_eq!(s.doc.len(), 1, "空文字不产生标注");
        assert!(!a.esc_passthrough);
        assert_eq!(s.tool(), Tool::Text, "退出输入不改变工具");
    }

    #[test]
    fn text_click_elsewhere_commits_then_new_typing() {
        let mut s = sess();
        s.set_tool(Tool::Text);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_char('x');
        // 点击别处：完成当前文字并开始新输入
        s.on_l_down(Pt::new(100, 50), false);
        assert_eq!(s.doc.len(), 1);
        assert!(s.typing());
        match &s.mode {
            Mode::Typing { anchor, text, .. } => {
                assert_eq!(*anchor, Pt::new(100, 50));
                assert!(text.is_empty());
            }
            _ => panic!("应处于输入中"),
        }
    }

    #[test]
    fn text_tool_switch_commits_pending() {
        let mut s = sess();
        s.set_tool(Tool::Text);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_char('x');
        s.set_tool(Tool::Rect);
        assert_eq!(s.doc.len(), 1, "切换工具时先完成文字");
        assert!(!s.typing());
    }

    #[test]
    fn typing_ignores_non_edit_keys() {
        let mut s = sess();
        s.set_tool(Tool::Text);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_char('a');
        // Ctrl+Z 在输入中忽略（正文编辑只经 on_char）
        s.on_key(VK_Z, true, false);
        assert!(s.typing());
        assert_eq!(s.doc.len(), 0);
        // 工具键在输入中同样忽略（由工具条放行给正文输入）
        s.on_key(VK_B, false, false);
        assert!(s.typing());
        assert_eq!(s.tool(), Tool::Text);
    }

    #[test]
    fn char_input_ignored_outside_typing() {
        let mut s = sess();
        assert_eq!(s.on_char('x').redraw, None, "非输入模式不消费字符");
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(0, 0), false);
        assert_eq!(s.on_char('x').redraw, None, "绘制中同样不消费字符");
    }

    #[test]
    fn ellipse_and_line_commit() {
        let mut s = sess();
        // 椭圆
        s.on_key(VK_E, false, false);
        assert_eq!(s.tool(), Tool::Ellipse);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_mouse_move(Pt::new(60, 40), false);
        s.on_l_up(Pt::new(60, 40), false);
        assert!(matches!(s.doc.shapes()[0].shape, Shape::Ellipse { .. }));
        // 线段
        s.on_key(VK_L, false, false);
        assert_eq!(s.tool(), Tool::Line);
        s.on_l_down(Pt::new(10, 80), false);
        s.on_l_up(Pt::new(90, 80), false);
        assert!(matches!(s.doc.shapes()[1].shape, Shape::Line { .. }));
        // 撤销两步
        s.on_key(VK_Z, true, false);
        s.on_key(VK_Z, true, false);
        assert!(s.doc.is_empty());
    }

    #[test]
    fn pen_accumulates_and_commits() {
        let mut s = sess();
        s.on_key(VK_P, false, false);
        assert_eq!(s.tool(), Tool::Pen);
        s.on_l_down(Pt::new(10, 10), false);
        for p in [(20, 15), (30, 25), (40, 20)] {
            s.on_mouse_move(Pt::new(p.0, p.1), false);
        }
        s.on_l_up(Pt::new(50, 30), false);
        match &s.doc.shapes()[0].shape {
            Shape::Pen { pts, .. } => assert!(pts.len() >= 3, "轨迹点应累积：{pts:?}"),
            other => panic!("应为画笔，得到 {other:?}"),
        }
        // 整体撤销
        s.on_key(VK_Z, true, false);
        assert!(s.doc.is_empty());
    }

    #[test]
    fn pen_is_drawing_while_dragging() {
        let mut s = sess();
        s.set_tool(Tool::Pen);
        s.on_l_down(Pt::new(10, 10), false);
        // 拖动中必须视为"绘制中"，否则渲染层跳过预览 → 松手前看不到轨迹
        assert!(s.drawing(), "自由画笔拖动中应处于绘制中");
        assert!(s.preview_shape().is_some(), "拖动中应有预览轨迹");
        s.on_l_up(Pt::new(60, 40), false);
        assert!(!s.drawing(), "松手后不再处于绘制中");
    }

    #[test]
    fn pen_dot_is_discarded() {
        let mut s = sess();
        s.set_tool(Tool::Pen);
        // 单击（无位移）→ 过短，丢弃
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(10, 10), false);
        assert_eq!(s.doc.len(), 0, "单击不应落笔");
    }

    #[test]
    fn text_near_line_attaches_to_it() {
        let mut s = sess();
        // 先画一条水平线（中点 (50, 50)）
        s.set_tool(Tool::Line);
        s.on_l_down(Pt::new(0, 50), false);
        s.on_l_up(Pt::new(100, 50), false);
        let lid = s.doc.shapes()[0].id;
        // 在线上方 5px 处落文字 → 应挂线，并居中到中点
        s.set_tool(Tool::Text);
        s.on_l_down(Pt::new(50, 45), false);
        s.on_char('x');
        s.on_key(VK_ESCAPE, false, false);
        match s.doc.shapes()[1].shape {
            Shape::Text { attach, bbox, .. } => {
                assert_eq!(attach, Some(lid), "应挂到线段");
                assert_eq!(bbox.center().x, 50, "水平居中到线段中点");
            }
            ref other => panic!("应为文字，得到 {other:?}"),
        }
    }

    #[test]
    fn text_far_from_line_does_not_attach() {
        let mut s = sess();
        s.set_tool(Tool::Line);
        s.on_l_down(Pt::new(0, 50), false);
        s.on_l_up(Pt::new(100, 50), false);
        s.set_tool(Tool::Text);
        s.on_l_down(Pt::new(50, 200), false); // 远离线段
        s.on_char('x');
        s.on_key(VK_ESCAPE, false, false);
        match s.doc.shapes()[1].shape {
            Shape::Text { attach, .. } => assert_eq!(attach, None, "远处不应挂线"),
            ref other => panic!("应为文字，得到 {other:?}"),
        }
    }

    #[test]
    fn eraser_deletes_multiple_in_one_undo_record() {
        let mut s = sess();
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(60, 40), false);
        s.on_l_down(Pt::new(100, 10), false);
        s.on_l_up(Pt::new(150, 40), false);
        assert_eq!(s.doc.len(), 2);
        s.set_tool(Tool::Eraser);
        // 从左到右横穿两个矩形
        s.on_l_down(Pt::new(20, 20), false);
        s.on_mouse_move(Pt::new(80, 20), false);
        s.on_mouse_move(Pt::new(140, 20), false);
        s.on_l_up(Pt::new(160, 20), false);
        assert_eq!(s.doc.len(), 0, "橡皮应删除两个矩形");
        s.on_key(VK_Z, true, false);
        assert_eq!(s.doc.len(), 2, "一次撤销恢复全部（橡皮只占一条记录）");
    }

    #[test]
    fn watermark_commits_to_full_bounds() {
        let mut s = sess();
        s.set_tool(Tool::Watermark);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_char('水');
        s.on_char('印');
        s.on_key(VK_ESCAPE, false, false);
        let Shape::Watermark { bbox, text, wm, .. } = &s.doc.shapes()[0].shape else {
            panic!("应为水印");
        };
        assert_eq!(*bbox, Rect::new(0, 0, 400, 300), "水印覆盖整个选区");
        assert_eq!(text, "水印");
        assert_eq!(wm.angle_deg, -30);
        assert!(wm.opacity > 0);
        s.on_key(VK_Z, true, false);
        assert!(s.doc.is_empty());
    }

    #[test]
    fn apply_style_to_selected_rect() {
        let mut s = sess();
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_l_up(Pt::new(60, 40), false);
        let id = s.doc.shapes()[0].id;
        s.set_tool(Tool::Rect); // -> None
        s.on_l_down(Pt::new(30, 20), false);
        s.on_l_up(Pt::new(30, 20), false);
        assert_eq!(s.selected_id(), Some(id));
        let a = s.apply_style(StylePatch {
            stroke_width: Some(8),
            ..Default::default()
        });
        assert!(a.redraw.is_some());
        match s.doc.find(id).unwrap() {
            Shape::Rect { style, .. } => assert_eq!(style.width, 8, "96 DPI 下 8 逻辑像素 = 8 物理像素"),
            other => panic!("应为矩形，得到 {other:?}"),
        }
        s.on_key(VK_Z, true, false);
        match s.doc.find(id).unwrap() {
            Shape::Rect { style, .. } => assert_eq!(style.width, 3, "撤销恢复样式"),
            other => panic!("应为矩形，得到 {other:?}"),
        }
    }

    #[test]
    fn apply_style_without_selection_updates_prefs() {
        let mut s = sess();
        s.apply_style(StylePatch {
            stroke_width: Some(6),
            ..Default::default()
        });
        s.set_tool(Tool::Rect);
        s.on_l_down(Pt::new(0, 0), false);
        s.on_l_up(Pt::new(40, 30), false);
        match s.doc.find(s.doc.shapes()[0].id).unwrap() {
            Shape::Rect { style, .. } => assert_eq!(style.width, 6),
            other => panic!("应为矩形，得到 {other:?}"),
        }
    }

    #[test]
    fn ctrl_wheel_size_adjustments_clamp() {
        let mut s = sess();
        s.adjust_size(SizeKind::StrokeWidth, 1);
        s.adjust_size(SizeKind::TextSize, 1);
        s.adjust_size(SizeKind::StepRadius, 1);
        s.adjust_size(SizeKind::EraserTol, 1);
        assert_eq!(s.size_value(SizeKind::StrokeWidth), 4);
        assert_eq!(s.size_value(SizeKind::TextSize), 17);
        assert_eq!(s.size_value(SizeKind::StepRadius), 15);
        assert_eq!(s.size_value(SizeKind::EraserTol), 7);
        // 下限钳制
        for _ in 0..20 {
            s.adjust_size(SizeKind::StrokeWidth, -1);
            s.adjust_size(SizeKind::TextSize, -1);
            s.adjust_size(SizeKind::StepRadius, -1);
            s.adjust_size(SizeKind::EraserTol, -1);
        }
        assert_eq!(s.size_value(SizeKind::StrokeWidth), 1);
        assert_eq!(s.size_value(SizeKind::TextSize), 4);
        assert_eq!(s.size_value(SizeKind::StepRadius), 2);
        assert_eq!(s.size_value(SizeKind::EraserTol), 0);
    }

    #[test]
    fn size_kind_maps_every_tool() {
        for tool in [
            Tool::Rect,
            Tool::Arrow,
            Tool::Ellipse,
            Tool::Line,
            Tool::Pen,
        ] {
            assert_eq!(SizeKind::for_tool(tool), Some(SizeKind::StrokeWidth));
        }
        for tool in [Tool::Text, Tool::Watermark] {
            assert_eq!(SizeKind::for_tool(tool), Some(SizeKind::TextSize));
        }
        assert_eq!(SizeKind::for_tool(Tool::StepNo), Some(SizeKind::StepRadius));
        assert_eq!(SizeKind::for_tool(Tool::Eraser), Some(SizeKind::EraserTol));
        assert_eq!(SizeKind::for_tool(Tool::Mosaic), None);
        assert_eq!(SizeKind::for_tool(Tool::Highlight), None);
        assert_eq!(SizeKind::for_tool(Tool::None), None);
        // 编码与反解一一对应
        for k in [
            SizeKind::StrokeWidth,
            SizeKind::TextSize,
            SizeKind::StepRadius,
            SizeKind::EraserTol,
        ] {
            assert_eq!(SizeKind::from_u32(k.code()), Some(k));
        }
    }
}
