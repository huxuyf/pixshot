//! 标注引擎：几何与基础图形运算（纯逻辑，不依赖 Win32）。
//!
//! 全部坐标为**物理像素**，且是"成品图局部坐标"（= 冻结表面坐标 − 选区原点），
//! 与 [`crate::capture::CapturedImage`] 的像素坐标系一致（见设计文档 3.1）。

/// 点（物理像素，成品图局部坐标）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
}

impl Pt {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// 平移。
    pub const fn offset(self, dx: i32, dy: i32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
        }
    }
}

/// 规范化包围盒（w、h 恒 ≥ 0）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn right(self) -> i32 {
        self.x + self.w
    }

    pub const fn bottom(self) -> i32 {
        self.y + self.h
    }

    /// 中心点（整数除法，向零取整）。
    pub const fn center(self) -> Pt {
        Pt::new(self.x + self.w / 2, self.y + self.h / 2)
    }

    /// 四周外扩 `d`（负值收缩；结果仍规范化为 w、h ≥ 0）。
    pub fn inflate(self, d: i32) -> Self {
        Self {
            x: self.x - d,
            y: self.y - d,
            w: (self.w + 2 * d).max(0),
            h: (self.h + 2 * d).max(0),
        }
    }

    /// 点是否落在矩形内（含左/上边界，不含右/下边界）。
    pub const fn contains(self, p: Pt) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
}

/// 由两个对角点得到规范化矩形（支持反向拖动）。
pub fn norm_rect(a: Pt, b: Pt) -> Rect {
    Rect {
        x: a.x.min(b.x),
        y: a.y.min(b.y),
        w: (a.x - b.x).abs(),
        h: (a.y - b.y).abs(),
    }
}

/// 两矩形的最小包围并集。
pub fn rect_union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = a.right().max(b.right());
    let y1 = a.bottom().max(b.bottom());
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// 两个可选矩形的最小包围并集；都为空时返回 `None`。
pub fn union_opt(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(rect_union(a, b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 点到线段 AB 的距离（物理像素）。
/// `t = clamp(dot(AP,AB)/|AB|², 0, 1)`；`d = |AP − t·AB|`（见设计文档 5.2 / 7）。
pub fn dist_point_segment(p: Pt, a: Pt, b: Pt) -> f32 {
    let (px, py) = (p.x as f32, p.y as f32);
    let (ax, ay) = (a.x as f32, a.y as f32);
    let (bx, by) = (b.x as f32, b.y as f32);
    let (abx, aby) = (bx - ax, by - ay);
    let len2 = abx * abx + aby * aby;
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        (((px - ax) * abx + (py - ay) * aby) / len2).clamp(0.0, 1.0)
    };
    let cx = ax + t * abx;
    let cy = ay + t * aby;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: i32, y: i32) -> Pt {
        Pt::new(x, y)
    }

    #[test]
    fn norm_rect_forward_reverse_and_axes() {
        assert_eq!(norm_rect(p(10, 20), p(110, 120)), Rect::new(10, 20, 100, 100));
        assert_eq!(norm_rect(p(110, 120), p(10, 20)), Rect::new(10, 20, 100, 100));
        assert_eq!(norm_rect(p(110, 20), p(10, 120)), Rect::new(10, 20, 100, 100));
        assert_eq!(norm_rect(p(50, 50), p(50, 50)), Rect::new(50, 50, 0, 0));
    }

    #[test]
    fn rect_union_is_minimal_bounding() {
        assert_eq!(
            rect_union(Rect::new(10, 10, 5, 5), Rect::new(20, 30, 4, 4)),
            Rect::new(10, 10, 14, 24)
        );
        // 退化矩形不改变并集
        assert_eq!(
            rect_union(Rect::new(3, 4, 0, 0), Rect::new(3, 4, 7, 7)),
            Rect::new(3, 4, 7, 7)
        );
    }

    #[test]
    fn union_opt_handles_none() {
        assert_eq!(union_opt(None, None), None);
        assert_eq!(union_opt(Some(Rect::new(1, 2, 3, 4)), None), Some(Rect::new(1, 2, 3, 4)));
        assert_eq!(
            union_opt(Some(Rect::new(0, 0, 1, 1)), Some(Rect::new(2, 2, 1, 1))),
            Some(Rect::new(0, 0, 3, 3))
        );
    }

    #[test]
    fn dist_point_segment_matches_formula() {
        // 命中线段中段：垂距
        let d = dist_point_segment(p(5, 5), p(0, 0), p(10, 0));
        assert!((d - 5.0).abs() < 1e-3, "垂距应为 5，得到 {d}");
        // 线段外：退化为到最近端点距离
        let d = dist_point_segment(p(20, 0), p(0, 0), p(10, 0));
        assert!((d - 10.0).abs() < 1e-3, "端点外应退化为 10，得到 {d}");
        // 退化线段（a == b）：点到点距离
        let d = dist_point_segment(p(3, 4), p(0, 0), p(0, 0));
        assert!((d - 5.0).abs() < 1e-3, "退化线段应为点距 5，得到 {d}");
    }

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(2, 2, 4, 4);
        assert!(r.contains(p(2, 2)));
        assert!(r.contains(p(5, 5)));
        assert!(!r.contains(p(6, 6)));
        assert!(!r.contains(p(1, 3)));
    }

    #[test]
    fn inflate_clamps_negative_extent() {
        assert_eq!(Rect::new(10, 10, 4, 4).inflate(2), Rect::new(8, 8, 8, 8));
        assert_eq!(Rect::new(10, 10, 4, 4).inflate(-10), Rect::new(20, 20, 0, 0));
    }
}
