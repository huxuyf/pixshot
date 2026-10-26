//! 标注引擎：交互状态机（纯逻辑，不依赖 Win32）。
//!
//! `AnnotSession` 汇集文档 + 模式 + 工具，事件入口返回 [`Action`]（脏区/光标/Esc 处理），
//! 由 Win32 集成层（[`super::session`] / `overlay.rs`）消费。状态迁移见设计文档 5.1。
//!
//! 说明：设计文档第 3 节明示为「Rust 示意」，故此处为可实现性补足了必要字段
//! （`dpi` 用于容差/尺寸换算、`shift` 记忆最近一次修饰键、`Moving/Resizing` 的 `cur`）。

use super::doc::AnnotDoc;
use super::geometry::{norm_rect, union_opt, Pt, Rect};
use super::hit_test::{self, Handle, HANDLE_HIT_LOGICAL};
use super::shape::{arrow_head_len, Shape};
use super::style::{DEF_BADGE, DEF_STROKE, DEF_STROKE_W, StrokeStyle};

/// 工具种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    None,
    Rect,
    Arrow,
    StepNo,
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

/// 选区外选中态外扩量（逻辑像素）：虚线框 2 + 柄 7px 的一半 ≈ 6（见 4.4）。
const HANDLE_CHROME_LOGICAL: i32 = 6;
/// 步骤序号默认半径（逻辑像素，见 4.3）。
const STEP_RADIUS_LOGICAL: i32 = 14;
/// 最小尺寸阈值（逻辑像素，小于则视为误触丢弃，见 5.1 / 7）。
const MIN_SIZE_LOGICAL: i32 = 3;

const VK_ESCAPE: usize = 0x1B;
const VK_DELETE: usize = 0x2E;
const VK_Z: usize = 0x5A;
const VK_Y: usize = 0x59;
const VK_R: usize = 0x52;
const VK_A: usize = 0x41;
const VK_N: usize = 0x4E;

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
}

impl AnnotSession {
    pub fn new(bounds: Rect, dpi: i32) -> Self {
        Self {
            doc: AnnotDoc::new(),
            mode: Mode::Idle,
            tool: Tool::None,
            bounds,
            dpi,
            shift: false,
        }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// 是否处于"正在绘制橡皮筋"（决定预览是否带选中态）。
    pub fn drawing(&self) -> bool {
        matches!(self.mode, Mode::Drawing { .. })
    }

    /// 是否需要鼠标捕获（绘制/拖动/缩放期间，见 5.1 的 SetCapture）。
    pub fn capturing(&self) -> bool {
        matches!(
            self.mode,
            Mode::Drawing { .. } | Mode::Moving { .. } | Mode::Resizing { .. }
        )
    }

    /// 拖动中的标注 id（渲染时需跳过其"旧位置"的提交副本）。
    pub fn hidden_id(&self) -> Option<u64> {
        match &self.mode {
            Mode::Moving { id, .. } | Mode::Resizing { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// 当前预览几何（绘制中的橡皮筋 / 移动缩放中的临时位置）。
    pub fn preview_shape(&self) -> Option<Shape> {
        match &self.mode {
            Mode::Drawing { tool, anchor, cur } => Some(self.new_shape(*tool, *anchor, *cur)),
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
        let before = self.transient_bbox();
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
        let before = self.transient_bbox();
        // 绘制/拖拽中再次按下：防御性收尾
        if !matches!(self.mode, Mode::Idle | Mode::Selected { .. }) {
            self.mode = Mode::Idle;
        }

        if self.tool != Tool::None {
            self.mode = Mode::Drawing {
                tool: self.tool,
                anchor: p,
                cur: p,
            };
            return self.action(before, CursorKind::Cross);
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
        let before = self.transient_bbox();
        match &mut self.mode {
            Mode::Drawing { cur, .. } | Mode::Moving { cur, .. } | Mode::Resizing { cur, .. } => {
                *cur = p;
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
            Mode::Moving { id, grab, orig, .. } => {
                let d = Pt::new(p.x - grab.x, p.y - grab.y);
                if d.x != 0 || d.y != 0 {
                    let new = orig.translate(d);
                    extra = self.doc.transform(id, orig, new);
                }
                self.mode = Mode::Selected { id };
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
            _ => Action::default(),
        }
    }

    /// 光标查询（供 `WM_SETCURSOR`）。
    pub fn cursor_at(&self, p: Pt) -> CursorKind {
        match &self.mode {
            Mode::Drawing { .. } => CursorKind::Cross,
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
        self.s(hit_test::HIT_TOL_LOGICAL)
    }

    fn handle_tol(&self) -> i32 {
        self.s(HANDLE_HIT_LOGICAL)
    }

    fn chrome_ext(&self) -> i32 {
        self.s(HANDLE_CHROME_LOGICAL)
    }

    fn step_radius(&self) -> i32 {
        self.s(STEP_RADIUS_LOGICAL)
    }

    fn min_size(&self) -> i32 {
        self.s(MIN_SIZE_LOGICAL)
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
                    color: DEF_STROKE,
                    width: self.s(DEF_STROKE_W),
                },
                fill: None,
            },
            Tool::Arrow => {
                let to = if self.shift { snap_45(anchor, cur) } else { cur };
                let width = self.s(DEF_STROKE_W);
                Shape::Arrow {
                    from: anchor,
                    to,
                    style: StrokeStyle {
                        color: DEF_STROKE,
                        width,
                    },
                    head_len: arrow_head_len(width),
                }
            }
            Tool::StepNo => Shape::StepNo {
                center: cur,
                radius: self.step_radius(),
                number: self.doc.peek_step_number(),
                style: DEF_BADGE.clone(),
            },
            Tool::None => Shape::Rect {
                bbox: norm_rect(anchor, cur),
                style: StrokeStyle {
                    color: DEF_STROKE,
                    width: self.s(DEF_STROKE_W),
                },
                fill: None,
            },
        }
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

    /// 是否满足最小尺寸（否则视为误触丢弃）。StepNo 单击即有效。
    fn is_committable(&self, shape: &Shape, tool: Tool, anchor: Pt, cur: Pt) -> bool {
        let min = self.min_size();
        match tool {
            Tool::StepNo => true,
            Tool::Arrow => {
                let dx = (cur.x - anchor.x) as f32;
                let dy = (cur.y - anchor.y) as f32;
                (dx * dx + dy * dy).sqrt() >= min as f32
            }
            Tool::Rect => {
                let b = shape.bbox();
                b.w >= min && b.h >= min
            }
            Tool::None => false,
        }
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
            Mode::Drawing { .. } => CursorKind::Cross,
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
}
