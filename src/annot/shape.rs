//! 标注引擎：标注对象（纯逻辑，不依赖 Win32）。
//!
//! 三类标注用 **enum dispatch**（而非 trait object）：值语义、`Clone + PartialEq`
//! 可直接存进命令栈、`match` 分发绘制（见设计文档 3.2）。

use super::geometry::{norm_rect, Pt, Rect};
use super::style::{BadgeStyle, Rgba, StrokeStyle};

/// 箭头头部长度：`clamp(width × 3.5, 12, 48)`（物理像素，见设计文档 4.3 / 7）。
pub fn arrow_head_len(width: i32) -> i32 {
    ((width as f32) * 3.5).clamp(12.0, 48.0).round() as i32
}

/// 箭头半张角：`tan(22°)`。
pub const ARROW_HALF_ANGLE_TAN: f32 = 0.404;

/// 一类标注的几何与样式。
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// 矩形（`fill` 预留，MVP 恒 `None`）。
    Rect {
        bbox: Rect,
        style: StrokeStyle,
        fill: Option<Rgba>,
    },
    /// 箭头（`head_len` 物理像素，创建时固化，不随缩放变化）。
    Arrow {
        from: Pt,
        to: Pt,
        style: StrokeStyle,
        head_len: i32,
    },
    /// 步骤序号徽章。
    StepNo {
        center: Pt,
        radius: i32,
        number: u32,
        style: BadgeStyle,
    },
}

impl Shape {
    /// 选中/缩放用的包围盒。箭头按两端点外扩 `head_len`（含头部，见 4.4 / 5.3）。
    pub fn bbox(&self) -> Rect {
        match self {
            Shape::Rect { bbox, .. } => *bbox,
            Shape::Arrow {
                from,
                to,
                head_len,
                ..
            } => norm_rect(*from, *to).inflate(*head_len),
            Shape::StepNo {
                center, radius, ..
            } => Rect::new(
                center.x - *radius,
                center.y - *radius,
                radius * 2,
                radius * 2,
            ),
        }
    }

    /// 平移（移动）。
    pub fn translate(&self, d: Pt) -> Shape {
        match self {
            Shape::Rect { bbox, style, fill } => Shape::Rect {
                bbox: Rect::new(bbox.x + d.x, bbox.y + d.y, bbox.w, bbox.h),
                style: style.clone(),
                fill: *fill,
            },
            Shape::Arrow {
                from,
                to,
                style,
                head_len,
            } => Shape::Arrow {
                from: from.offset(d.x, d.y),
                to: to.offset(d.x, d.y),
                style: style.clone(),
                head_len: *head_len,
            },
            Shape::StepNo {
                center,
                radius,
                number,
                style,
            } => Shape::StepNo {
                center: center.offset(d.x, d.y),
                radius: *radius,
                number: *number,
                style: style.clone(),
            },
        }
    }

    /// 按包围盒映射（缩放）：`bbox0` 为原包围盒、`new` 为新包围盒。
    /// 矩形直接替换 bbox；箭头两端点按 `sx/sy` 比例重映射；序号不参与缩放（见 5.3）。
    pub fn remap_bbox(&self, bbox0: Rect, new: Rect) -> Shape {
        let sx = if bbox0.w != 0 {
            new.w as f32 / bbox0.w as f32
        } else {
            1.0
        };
        let sy = if bbox0.h != 0 {
            new.h as f32 / bbox0.h as f32
        } else {
            1.0
        };
        let map = |p: Pt| {
            Pt::new(
                new.x + ((p.x - bbox0.x) as f32 * sx).round() as i32,
                new.y + ((p.y - bbox0.y) as f32 * sy).round() as i32,
            )
        };
        match self {
            Shape::Rect { style, fill, .. } => Shape::Rect {
                bbox: new,
                style: style.clone(),
                fill: *fill,
            },
            Shape::Arrow {
                from,
                to,
                style,
                head_len,
            } => Shape::Arrow {
                from: map(*from),
                to: map(*to),
                style: style.clone(),
                head_len: *head_len,
            },
            Shape::StepNo { .. } => self.clone(),
        }
    }
}

/// 带 id 的标注（id 由文档分配，稳定标识）。
#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub id: u64,
    pub shape: Shape,
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::style::{DEF_BADGE, DEF_STROKE};

    fn stroke() -> StrokeStyle {
        StrokeStyle {
            color: DEF_STROKE,
            width: 3,
        }
    }

    #[test]
    fn arrow_head_len_clamps() {
        assert_eq!(arrow_head_len(1), 12); // 3.5 < 12
        assert_eq!(arrow_head_len(3), 12); // 10.5 -> 12
        assert_eq!(arrow_head_len(10), 35);
        assert_eq!(arrow_head_len(100), 48); // 350 -> 48
    }

    #[test]
    fn arrow_bbox_includes_head() {
        let s = Shape::Arrow {
            from: Pt::new(10, 10),
            to: Pt::new(60, 10),
            style: stroke(),
            head_len: 12,
        };
        // 端点包围盒 (10,10,50,0) 外扩 12 -> (—2,−2,74,24)
        assert_eq!(s.bbox(), Rect::new(-2, -2, 74, 24));
    }

    #[test]
    fn translate_moves_all_variants() {
        let d = Pt::new(5, -3);
        let r = Shape::Rect {
            bbox: Rect::new(1, 2, 3, 4),
            style: stroke(),
            fill: None,
        };
        assert_eq!(r.translate(d).bbox(), Rect::new(6, -1, 3, 4));
    }

    #[test]
    fn remap_scales_arrow_endpoints() {
        let s = Shape::Arrow {
            from: Pt::new(0, 0),
            to: Pt::new(10, 0),
            style: stroke(),
            head_len: 12,
        };
        // bbox0 = 端点盒 (0,0,10,0) 外扩 head_len(12) -> (-12,-12,34,24)
        let bbox0 = s.bbox();
        assert_eq!(bbox0, Rect::new(-12, -12, 34, 24));
        // 宽度翻倍；端点按 bbox0→new 的比例重映射
        let new = Rect::new(bbox0.x, bbox0.y, bbox0.w * 2, bbox0.h);
        let out = s.remap_bbox(bbox0, new);
        match out {
            Shape::Arrow { from, to, head_len, .. } => {
                assert_eq!(head_len, 12, "head_len 保持恒定");
                assert_eq!(from, Pt::new(12, 0));
                assert_eq!(to, Pt::new(32, 0));
            }
            _ => panic!("应为箭头"),
        }
    }

    #[test]
    fn remap_stepno_is_identity() {
        let s = Shape::StepNo {
            center: Pt::new(5, 5),
            radius: 14,
            number: 1,
            style: DEF_BADGE.clone(),
        };
        assert_eq!(s.remap_bbox(Rect::new(0, 0, 10, 10), Rect::new(0, 0, 20, 20)), s);
    }
}
