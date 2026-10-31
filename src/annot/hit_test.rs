//! 标注引擎：命中测试与控制柄（纯逻辑，不依赖 Win32）。
//!
//! 容差与优先级见设计文档 5.2：控制柄 > 标注（倒序 z 序，取最高层）。

use super::geometry::{dist_point_segment, Pt, Rect};
use super::shape::Shape;

/// 标注命中容差（逻辑像素）。
pub const HIT_TOL_LOGICAL: i32 = 4;
/// 控制柄命中半径（逻辑像素；柄 7px 的命中区略大）。
pub const HANDLE_HIT_LOGICAL: i32 = 5;
/// 控制柄视觉边长（逻辑像素）。
pub const HANDLE_SIZE_LOGICAL: i32 = 7;

/// 八个控制柄方位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
    NW,
}

impl Handle {
    /// 全部控制柄（绘制与命中遍历用）。**角柄在前**：微小图形上边柄中心与角柄
    /// 间距可小于命中半径，`find` 取首个命中者——角柄优先保证角缩放总是可用。
    pub const ALL: [Handle; 8] = [
        Handle::NE,
        Handle::SE,
        Handle::SW,
        Handle::NW,
        Handle::N,
        Handle::E,
        Handle::S,
        Handle::W,
    ];

    /// 控制柄中心：bbox 的四角与四边中点。
    pub fn center(self, bbox: Rect) -> Pt {
        let (l, t, r, b) = (bbox.x, bbox.y, bbox.right(), bbox.bottom());
        let c = bbox.center();
        match self {
            Handle::N => Pt::new(c.x, t),
            Handle::NE => Pt::new(r, t),
            Handle::E => Pt::new(r, c.y),
            Handle::SE => Pt::new(r, b),
            Handle::S => Pt::new(c.x, b),
            Handle::SW => Pt::new(l, b),
            Handle::W => Pt::new(l, c.y),
            Handle::NW => Pt::new(l, t),
        }
    }

    /// 对角（相向）控制柄。
    pub fn opposite(self) -> Handle {
        match self {
            Handle::N => Handle::S,
            Handle::NE => Handle::SW,
            Handle::E => Handle::W,
            Handle::SE => Handle::NW,
            Handle::S => Handle::N,
            Handle::SW => Handle::NE,
            Handle::W => Handle::E,
            Handle::NW => Handle::SE,
        }
    }

    /// 缩放锚点：对角固定点（拖 E 柄 → 左边中点；拖 SE 柄 → 左上角，见 5.3）。
    pub fn anchor(self, bbox: Rect) -> Pt {
        self.opposite().center(bbox)
    }
}

/// 点是否命中标注（含容差）。
pub fn hit_shape(shape: &Shape, p: Pt, tol: i32) -> bool {
    match shape {
        Shape::Rect { bbox, style, .. } => bbox.inflate(style.width / 2 + tol).contains(p),
        // 椭圆：按外接盒 + 归一化椭圆方程判内部（含容差外扩）
        Shape::Ellipse { bbox, style } => {
            let b = bbox.inflate(style.width / 2 + tol);
            if !b.contains(p) {
                return false;
            }
            let (rx, ry) = ((b.w as f32) / 2.0, (b.h as f32) / 2.0);
            if rx <= 0.0 || ry <= 0.0 {
                return false;
            }
            let (cx, cy) = (b.x as f32 + rx, b.y as f32 + ry);
            let nx = (p.x as f32 - cx) / rx;
            let ny = (p.y as f32 - cy) / ry;
            nx * nx + ny * ny <= 1.0
        }
        // 线段：点到线段距离
        Shape::Line { from, to, style } => {
            dist_point_segment(p, *from, *to) <= (style.width as f32) / 2.0 + tol as f32
        }
        // 画笔：命中轨迹上任意一段（含线宽与容差）
        Shape::Pen { pts, style } => {
            (style.width as f32) / 2.0 + tol as f32
                >= pts
                    .windows(2)
                    .map(|w| dist_point_segment(p, w[0], w[1]))
                    .fold(f32::INFINITY, f32::min)
        }
        // 马赛克/高亮按 bbox 命中（区域填充类标注）
        Shape::Mosaic { bbox, .. } | Shape::Highlight { bbox, .. } => bbox.inflate(tol).contains(p),
        // 文字按估算 bbox 命中
        Shape::Text { bbox, .. } => bbox.inflate(tol).contains(p),
        // 水印按覆盖区域命中（全选区）
        Shape::Watermark { bbox, .. } => bbox.inflate(tol).contains(p),
        Shape::Arrow { from, to, style, .. } => {
            dist_point_segment(p, *from, *to) <= (style.width as f32) / 2.0 + tol as f32
        }
        Shape::StepNo {
            center, radius, ..
        } => {
            let dx = (p.x - center.x) as f32;
            let dy = (p.y - center.y) as f32;
            (dx * dx + dy * dy).sqrt() <= (*radius + tol) as f32
        }
    }
}

/// 点是否命中某个控制柄（StepNo / Text 不参与缩放，恒 `None`）。
pub fn hit_handle(shape: &Shape, p: Pt, tol: i32) -> Option<Handle> {
    if matches!(
        shape,
        Shape::StepNo { .. } | Shape::Text { .. } | Shape::Watermark { .. }
    ) {
        return None;
    }
    let bbox = shape.bbox();
    Handle::ALL.into_iter().find(|&h| {
        let c = h.center(bbox);
        let dx = p.x - c.x;
        let dy = p.y - c.y;
        dx * dx + dy * dy <= tol * tol
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::style::{DEF_BADGE, DEF_STROKE, StrokeStyle};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Shape {
        Shape::Rect {
            bbox: Rect::new(x, y, w, h),
            style: StrokeStyle {
                color: DEF_STROKE,
                width: 3,
            },
            fill: None,
        }
    }

    #[test]
    fn rect_hit_inside_and_tolerance() {
        let s = rect(10, 10, 100, 50);
        assert!(hit_shape(&s, Pt::new(50, 30), 4), "内部命中");
        assert!(hit_shape(&s, Pt::new(10 - 5, 30), 4), "容差内命中（宽/2+4=5.5）");
        assert!(!hit_shape(&s, Pt::new(10 - 7, 30), 4), "容差外不命中");
    }

    #[test]
    fn arrow_hit_uses_segment_distance() {
        let s = Shape::Arrow {
            from: Pt::new(0, 0),
            to: Pt::new(100, 0),
            style: StrokeStyle {
                color: DEF_STROKE,
                width: 3,
            },
            head_len: 12,
        };
        assert!(hit_shape(&s, Pt::new(50, 4), 4), "距线段 4 ≤ 1.5+4");
        assert!(!hit_shape(&s, Pt::new(50, 10), 4), "距线段 10 不命中");
    }

    #[test]
    fn stepno_hit_by_radius() {
        let s = Shape::StepNo {
            center: Pt::new(50, 50),
            radius: 14,
            number: 1,
            style: DEF_BADGE.clone(),
        };
        assert!(hit_shape(&s, Pt::new(60, 50), 4), "14+4 内");
        assert!(!hit_shape(&s, Pt::new(70, 50), 4), "14+4 外");
        assert_eq!(hit_handle(&s, Pt::new(50, 50), 5), None, "序号无控制柄");
    }

    #[test]
    fn handle_positions_and_opposite() {
        let b = Rect::new(0, 0, 100, 50);
        assert_eq!(Handle::NW.center(b), Pt::new(0, 0));
        assert_eq!(Handle::SE.center(b), Pt::new(100, 50));
        assert_eq!(Handle::N.center(b), Pt::new(50, 0));
        assert_eq!(Handle::E.center(b), Pt::new(100, 25));
        assert_eq!(Handle::SE.opposite(), Handle::NW);
        // anchor = 对角固定点
        assert_eq!(Handle::SE.anchor(b), Pt::new(0, 0), "拖 SE → 锚点左上角");
        assert_eq!(Handle::E.anchor(b), Pt::new(0, 25), "拖 E → 锚点左边中点");
    }

    #[test]
    fn handle_hit_finds_nearest_direction() {
        let s = rect(0, 0, 100, 50);
        assert_eq!(hit_handle(&s, Pt::new(100, 50), 5), Some(Handle::SE));
        assert_eq!(hit_handle(&s, Pt::new(0, 0), 5), Some(Handle::NW));
        assert_eq!(hit_handle(&s, Pt::new(50, 25), 5), None, "中央不命中控制柄");
    }

    #[test]
    fn mosaic_and_highlight_hit_by_bbox() {
        let m = Shape::Mosaic {
            bbox: Rect::new(10, 10, 40, 20),
            mode: super::super::style::MosaicMode::Pixelate,
        };
        assert!(hit_shape(&m, Pt::new(30, 20), 4), "内部命中");
        assert!(!hit_shape(&m, Pt::new(60, 20), 4), "外部不命中");
        // 与矩形一致可缩放：角上有控制柄
        assert_eq!(hit_handle(&m, Pt::new(50, 30), 5), Some(Handle::SE));

        let h = Shape::Highlight {
            bbox: Rect::new(10, 10, 40, 20),
            color: super::super::style::HIGHLIGHT,
            mode: super::super::style::HighlightMode::Marker,
        };
        assert!(hit_shape(&h, Pt::new(30, 20), 4));
    }

    #[test]
    fn ellipse_hit_inside_but_not_corner() {
        // 100×100 外接盒 → 内切圆，半径 50
        let e = Shape::Ellipse {
            bbox: Rect::new(0, 0, 100, 100),
            style: StrokeStyle {
                color: DEF_STROKE,
                width: 2,
            },
        };
        assert!(hit_shape(&e, Pt::new(50, 50), 0), "中心命中");
        // 左上角在盒内但在椭圆外
        assert!(!hit_shape(&e, Pt::new(3, 3), 0), "角落在椭圆外");
    }

    #[test]
    fn line_and_pen_hit_by_distance() {
        let line = Shape::Line {
            from: Pt::new(0, 0),
            to: Pt::new(100, 0),
            style: StrokeStyle {
                color: DEF_STROKE,
                width: 2,
            },
        };
        assert!(hit_shape(&line, Pt::new(50, 2), 4));
        assert!(!hit_shape(&line, Pt::new(50, 20), 4));

        let pen = Shape::Pen {
            pts: vec![Pt::new(0, 0), Pt::new(50, 0), Pt::new(50, 50)],
            style: super::super::style::PenStyle {
                color: DEF_STROKE,
                width: 2,
                kind: super::super::style::PenKind::Solid,
            },
        };
        assert!(hit_shape(&pen, Pt::new(25, 3), 4), "命中第一段附近");
        assert!(hit_shape(&pen, Pt::new(52, 30), 4), "命中第二段附近");
        assert!(!hit_shape(&pen, Pt::new(90, 90), 4), "远离轨迹不命中");
        // 单点轨迹不 panic 且不命中
        let dot = Shape::Pen {
            pts: vec![Pt::new(1, 1)],
            style: super::super::style::PenStyle {
                color: DEF_STROKE,
                width: 2,
                kind: super::super::style::PenKind::Solid,
            },
        };
        assert!(!hit_shape(&dot, Pt::new(1, 1), 4));
    }

    #[test]
    fn text_hits_by_est_bbox_and_has_no_handle() {
        let t = Shape::Text {
            origin: Pt::new(10, 10),
            text: "ab".into(),
            bbox: Rect::new(10, 10, 24, 29),
            style: super::super::style::TextStyle {
                color: super::super::style::DEF_STROKE,
                font_px: 20,
                bold: false,
                italic: false,
            },
            frame: super::super::style::TextFrame::None,
            attach: None,
        };
        assert!(hit_shape(&t, Pt::new(20, 20), 4), "内部命中");
        assert!(!hit_shape(&t, Pt::new(60, 20), 4), "外部不命中");
        assert_eq!(hit_handle(&t, Pt::new(34, 39), 5), None, "文字无缩放柄");
    }
}
