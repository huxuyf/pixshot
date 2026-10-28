//! 标注引擎：交互状态机（纯逻辑，不依赖 Win32）。
//!
//! `AnnotSession` 汇集文档 + 模式 + 工具，事件入口返回 [`Action`]（脏区/光标/Esc 处理），
//! 由 Win32 集成层（[`super::session`] / `overlay.rs`）消费。状态迁移见设计文档 5.1。
//!
//! 说明：设计文档第 3 节明示为「Rust 示意」，故此处为可实现性补足了必要字段
//! （`dpi` 用于容差/尺寸换算、`shift` 记忆最近一次修饰键、`Moving/Resizing` 的 `cur`）。

use super::doc::AnnotDoc;
use super::geometry::{norm_rect, union_opt, Pt, Rect};
use super::hit_test::{self, Handle};
use super::shape::{arrow_head_len, text_est_size, Shape};
use super::style::{AnnotPrefs, PenStyle, StrokeStyle, TextStyle};

/// 工具种类。
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
    /// Add 命令落入文档（参照 SnowShot：Enter 换行、Esc / 点击别处完成）。
    Typing {
        anchor: Pt,
        text: String,
    },
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
            Mode::Typing { anchor, text } => Some((*anchor, text.clone(), self.text_style())),
            _ => None,
        }
    }

    /// 是否需要鼠标捕获（绘制/拖动/缩放期间，见 5.1 的 SetCapture）。
    pub fn capturing(&self) -> bool {
        matches!(
            self.mode,
            Mode::Drawing { .. }
                | Mode::Freehand { .. }
                | Mode::Moving { .. }
                | Mode::Resizing { .. }
        )
    }

    /// 拖动中的标注 id（渲染时需跳过其"旧位置"的提交副本）。
    pub fn hidden_id(&self) -> Option<u64> {
        match &self.mode {
            Mode::Moving { id, .. } | Mode::Resizing { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// 当前预览几何（绘制中的橡皮筋 / 自由轨迹 / 移动缩放中的临时位置 / 输入中的文字）。
    pub fn preview_shape(&self) -> Option<Shape> {
        match &self.mode {
            Mode::Drawing { tool, anchor, cur } => Some(self.new_shape(*tool, *anchor, *cur)),
            Mode::Freehand { pts } => Some(self.new_pen(pts.clone())),
            Mode::Typing { anchor, text } => {
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
                };
                return self.action(before, CursorKind::IBeam);
            }
            // 画笔：按下即开始累积轨迹
            Tool::Pen => {
                self.mode = Mode::Freehand { pts: vec![p] };
                return self.action(before, CursorKind::Cross);
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
            Mode::Moving { id, grab, orig, .. } => {
                let d = Pt::new(p.x - grab.x, p.y - grab.y);
                if d.x != 0 || d.y != 0 {
                    let new = orig.translate(d);
                    extra = self.doc.transform(id, orig, new);
                }
                self.mode = Mode::Selected { id };
            }            Mode::Resizing {
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
                if new != orig {
                    extra = self.doc.transform(id, orig, new);
                }
                self.mode = Mode::Selected { id };
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

    pub fn on_key(&mut self, vk: usize, ctrl: bool) -> Action {
        let before = self.transient_bbox();

        // 输入中：编辑键（换行/退格/正文）经 `on_char`；这里只处理 Esc
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

        // Ctrl+Z / Ctrl+Y：撤销重做（模式不变，见 5.1）
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
            _ => Action::default(),
        }
    }

    /// 输入中的字符（工具条经 `WM_APP_CHAR` 转发的 `WM_CHAR`）：
    /// `'\r'` = 换行、`'\u{8}'` = 退格、可打印字符 = 追加。仅 Typing 模式消费。
    pub fn on_char(&mut self, c: char) -> Action {
        if !matches!(self.mode, Mode::Typing { .. }) {
            return Action::default();
        }
        let before = self.transient_bbox();
        if let Mode::Typing { text, .. } = &mut self.mode {
            match c {
                '\r' | '\n' => text.push('\n'),
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
        let Mode::Typing { anchor, text } = std::mem::replace(&mut self.mode, Mode::Idle) else {
            return Action::default();
        };
        if text.trim().is_empty() {
            return Action::default();
        }
        // 挂线：落点靠近线段/箭头时吸附，并把文字居中到该线段的**中点**
        //（与 `AnnotDoc::transform` 的重居中口径一致，线一移动文字就跟着走）。
        let attach = if self.prefs.text_attach {
            self.find_attach(anchor).map(|(id, _)| id)
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
        let dirty = shape.bbox();
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
        match self.doc.find(id)? {
            Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => {
                Some(Pt::new((from.x + to.x) / 2, (from.y + to.y) / 2))
            }
            _ => None,
        }
    }

    /// 光标查询（供 `WM_SETCURSOR`）。
    pub fn cursor_at(&self, p: Pt) -> CursorKind {
        match &self.mode {
            Mode::Drawing { .. } | Mode::Freehand { .. } => CursorKind::Cross,
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

    /// 在落点附近寻找可挂线的线段/箭头，返回 `(id, 投影点)`（取最近的一条）。
    fn find_attach(&self, p: Pt) -> Option<(u64, Pt)> {
        let tol = self.s(self.prefs.attach_tol).max(1) as f32;
        let mut best: Option<(u64, Pt, f32)> = None;
        for a in self.doc.shapes() {
            let (from, to) = match &a.shape {
                Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => (*from, *to),
                _ => continue,
            };
            let d = super::geometry::dist_point_segment(p, from, to);
            if d <= tol && best.map(|(_, _, bd)| d < bd).unwrap_or(true) {
                best = Some((a.id, project_on_segment(p, from, to), d));
            }
        }
        best.map(|(id, proj, _)| (id, proj))
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
        let preview = self.preview_shape().map(|s| s.bbox());
        let chrome = self
            .chrome_shape()
            .map(|s| s.bbox().inflate(self.chrome_ext()));
        union_opt(preview, chrome)
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

fn sign(v: i32) -> i32 {
    if v < 0 {
        -1
    } else {
        1
    }
}

/// `p` 在线段 AB 上的投影点（用于挂线吸附）。
fn project_on_segment(p: Pt, a: Pt, b: Pt) -> Pt {
    let (ax, ay, bx, by) = (a.x as f32, a.y as f32, b.x as f32, b.y as f32);
    let (abx, aby) = (bx - ax, by - ay);
    let len2 = abx * abx + aby * aby;
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        (((p.x as f32 - ax) * abx + (p.y as f32 - ay) * aby) / len2).clamp(0.0, 1.0)
    };
    Pt::new((ax + t * abx).round() as i32, (ay + t * aby).round() as i32)
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
fn snap_45(from: Pt, to: Pt) -> Pt {    let dx = (to.x - from.x) as f32;
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
        let a = s.on_key(VK_Z, true);
        assert_eq!(s.doc.len(), 0);
        assert!(a.redraw.is_some());
        // 重做
        s.on_key(VK_Y, true);
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
        s.on_key(VK_DELETE, false);
        assert!(s.doc.is_empty());
        // 撤销恢复
        s.on_key(VK_Z, true);
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
        assert!(s.on_key(VK_ESCAPE, false).esc_passthrough);
        // 工具激活 → 第一次 Esc 回 None，不 passthrough
        s.set_tool(Tool::Rect);
        let a = s.on_key(VK_ESCAPE, false);
        assert!(!a.esc_passthrough);
        assert_eq!(s.tool(), Tool::None);
        // 再次 Esc → passthrough
        assert!(s.on_key(VK_ESCAPE, false).esc_passthrough);
    }

    #[test]
    fn drawing_esc_cancels_without_commit() {
        let mut s = sess();
        s.set_tool(Tool::Arrow);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_mouse_move(Pt::new(80, 80), false);
        let a = s.on_key(VK_ESCAPE, false);
        assert!(!a.esc_passthrough);
        assert!(matches!(s.mode, Mode::Idle));
        assert_eq!(s.doc.len(), 0);
        assert_eq!(s.tool(), Tool::Arrow, "取消绘制不改变工具");
    }

    #[test]
    fn toolbar_tool_key_toggles() {
        let mut s = sess();
        s.on_key(VK_R, false);
        assert_eq!(s.tool(), Tool::Rect);
        s.on_key(VK_R, false);
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
        s.on_key(VK_B, false);
        assert_eq!(s.tool(), Tool::Mosaic);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_mouse_move(Pt::new(60, 40), false);
        let a = s.on_l_up(Pt::new(60, 40), false);
        assert_eq!(s.doc.len(), 1);
        assert!(matches!(s.doc.shapes()[0].shape, Shape::Mosaic { .. }));
        assert!(a.redraw.is_some());
        s.on_key(VK_Z, true);
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
        let a = s.on_key(VK_ESCAPE, false);
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
        s.on_key(VK_Z, true);
        assert_eq!(s.doc.len(), 0);
    }

    #[test]
    fn text_enter_newline_backspace_and_empty_esc() {
        let mut s = sess();
        s.set_tool(Tool::Text);
        s.on_l_down(Pt::new(5, 5), false);
        s.on_char('a');
        s.on_char('b');
        s.on_char('\r'); // Enter 换行
        s.on_char('c');
        s.on_char('\u{8}'); // 退格
        s.on_key(VK_ESCAPE, false);
        match &s.doc.shapes()[0].shape {
            Shape::Text { text, .. } => assert_eq!(text, "ab\n"),
            other => panic!("应为文字，得到 {other:?}"),
        }
        // 空文字 Esc：退出输入、不产生标注、不透传。
        // （Esc 完成第一处文字后工具仍是 Text，直接再次落点开始第二处输入）
        s.on_l_down(Pt::new(9, 9), false);
        s.on_l_up(Pt::new(9, 9), false);
        let a = s.on_key(VK_ESCAPE, false);
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
            Mode::Typing { anchor, text } => {
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
        s.on_key(VK_Z, true);
        assert!(s.typing());
        assert_eq!(s.doc.len(), 0);
        // 工具键在输入中同样忽略（由工具条放行给正文输入）
        s.on_key(VK_B, false);
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
        s.on_key(VK_E, false);
        assert_eq!(s.tool(), Tool::Ellipse);
        s.on_l_down(Pt::new(10, 10), false);
        s.on_mouse_move(Pt::new(60, 40), false);
        s.on_l_up(Pt::new(60, 40), false);
        assert!(matches!(s.doc.shapes()[0].shape, Shape::Ellipse { .. }));
        // 线段
        s.on_key(VK_L, false);
        assert_eq!(s.tool(), Tool::Line);
        s.on_l_down(Pt::new(10, 80), false);
        s.on_l_up(Pt::new(90, 80), false);
        assert!(matches!(s.doc.shapes()[1].shape, Shape::Line { .. }));
        // 撤销两步
        s.on_key(VK_Z, true);
        s.on_key(VK_Z, true);
        assert!(s.doc.is_empty());
    }

    #[test]
    fn pen_accumulates_and_commits() {
        let mut s = sess();
        s.on_key(VK_P, false);
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
        s.on_key(VK_Z, true);
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
        s.on_key(VK_ESCAPE, false);
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
        s.on_key(VK_ESCAPE, false);
        match s.doc.shapes()[1].shape {
            Shape::Text { attach, .. } => assert_eq!(attach, None, "远处不应挂线"),
            ref other => panic!("应为文字，得到 {other:?}"),
        }
    }
}
