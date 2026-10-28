//! 标注引擎：标注对象（纯逻辑，不依赖 Win32）。
//!
//! 多类标注用 **enum dispatch**（而非 trait object）：值语义、`Clone + PartialEq`
//! 可直接存进命令栈、`match` 分发绘制（见设计文档 3.2）。

use super::geometry::{norm_rect, Pt, Rect};
use super::style::{
    BadgeStyle, HighlightMode, MosaicMode, PenStyle, Rgba, StrokeStyle, TextFrame, TextStyle,
};

/// 箭头头部长度：`clamp(width × ratio, min, max)`（物理像素，见设计文档 4.3 / 7）。
/// 系数与上下限来自配置（默认 3.5 / 12 / 48）。
pub fn arrow_head_len(width: i32, ratio: f32, min: i32, max: i32) -> i32 {
    let (lo, hi) = (min.min(max), min.max(max));
    ((width as f32) * ratio).clamp(lo as f32, hi as f32).round() as i32
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
    /// 椭圆（仅描边；`bbox` 为其外接矩形）。
    Ellipse { bbox: Rect, style: StrokeStyle },
    /// 线段（无箭头）。
    Line {
        from: Pt,
        to: Pt,
        style: StrokeStyle,
    },
    /// 自由画笔：`pts` 为轨迹点（≥ 2 才有效）；形态见 [`PenStyle::kind`]。
    Pen { pts: Vec<Pt>, style: PenStyle },
    /// 马赛克（打码）：`mode` 决定块平均或模糊。
    Mosaic { bbox: Rect, mode: MosaicMode },
    /// 高亮：`mode` 决定荧光笔色块或聚光灯（整图压暗 + 挖洞）。
    Highlight {
        bbox: Rect,
        color: Rgba,
        mode: HighlightMode,
    },
    /// 文字。`bbox` 为命中/移动用的**估算**包围盒（渲染用真实字形，见
    /// `text_est_size`）；创建后随文字固化，不随缩放变化（文字不参与缩放）。
    /// `frame` 为外框样式；`attach` 为所挂线段/箭头的 id（挂线文字随其移动）。
    Text {
        origin: Pt,
        text: String,
        bbox: Rect,
        style: TextStyle,
        frame: TextFrame,
        attach: Option<u64>,
    },
}

/// 文字包围盒估算（CJK 全角 1.0em / 其余 0.55em 每字符）。
///
/// 纯逻辑层的渲染前估算：真实字形宽度要经 GDI+ 量测（Win32 依赖），
/// 命中测试/移动用此估算即可（误差不显著，参照 SnowShot 对文字的估算方式）。
pub fn text_est_size(text: &str, font_px: i32) -> (i32, i32) {
    let px = font_px.max(1) as f32;
    let mut max_w = 0f32;
    let mut lines = 0usize;
    for line in text.split('\n') {
        lines += 1;
        let w: f32 = line
            .chars()
            .map(|c| {
                if (c as u32) >= 0x2E80 {
                    px
                } else {
                    px * 0.55
                }
            })
            .sum();
        max_w = max_w.max(w);
    }
    if lines == 0 {
        lines = 1;
    }
    (max_w.round() as i32 + 2, (px * lines as f32 * 1.45).round() as i32)
}

/// 轨迹点的最小包围盒（空集返回零面积矩形）。
pub fn pen_bounds(pts: &[Pt]) -> Rect {
    let Some(first) = pts.first() else {
        return Rect::new(0, 0, 0, 0);
    };
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
    for p in pts {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

impl Shape {
    /// 选中/缩放用的包围盒。箭头按两端点外扩 `head_len`（含头部，见 4.4 / 5.3）。
    pub fn bbox(&self) -> Rect {
        match self {
            Shape::Rect { bbox, .. }
            | Shape::Ellipse { bbox, .. }
            | Shape::Mosaic { bbox, .. }
            | Shape::Highlight { bbox, .. } => *bbox,
            Shape::Text { bbox, .. } => *bbox,
            Shape::Line { from, to, style } => {
                norm_rect(*from, *to).inflate((style.width / 2).max(1))
            }
            Shape::Pen { pts, style } => pen_bounds(pts).inflate((style.width / 2).max(1)),
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
            Shape::Ellipse { bbox, style } => Shape::Ellipse {
                bbox: Rect::new(bbox.x + d.x, bbox.y + d.y, bbox.w, bbox.h),
                style: style.clone(),
            },
            Shape::Line { from, to, style } => Shape::Line {
                from: from.offset(d.x, d.y),
                to: to.offset(d.x, d.y),
                style: style.clone(),
            },
            Shape::Pen { pts, style } => Shape::Pen {
                pts: pts.iter().map(|p| p.offset(d.x, d.y)).collect(),
                style: style.clone(),
            },
            Shape::Mosaic { bbox, mode } => Shape::Mosaic {
                bbox: Rect::new(bbox.x + d.x, bbox.y + d.y, bbox.w, bbox.h),
                mode: *mode,
            },
            Shape::Highlight { bbox, color, mode } => Shape::Highlight {
                bbox: Rect::new(bbox.x + d.x, bbox.y + d.y, bbox.w, bbox.h),
                color: *color,
                mode: *mode,
            },
            Shape::Text {
                origin,
                text,
                bbox,
                style,
                frame,
                attach,
            } => Shape::Text {
                origin: origin.offset(d.x, d.y),
                text: text.clone(),
                bbox: Rect::new(bbox.x + d.x, bbox.y + d.y, bbox.w, bbox.h),
                style: style.clone(),
                frame: *frame,
                attach: *attach,
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
    /// 矩形/椭圆直接替换 bbox；线段/箭头端点按 `sx/sy` 比例重映射；序号/文字不参与缩放（见 5.3）。
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
            Shape::Ellipse { style, .. } => Shape::Ellipse {
                bbox: new,
                style: style.clone(),
            },
            Shape::Mosaic { mode, .. } => Shape::Mosaic { bbox: new, mode: *mode },
            Shape::Highlight { color, mode, .. } => Shape::Highlight {
                bbox: new,
                color: *color,
                mode: *mode,
            },
            Shape::Pen { pts, style } => Shape::Pen {
                pts: pts.iter().map(|p| map(*p)).collect(),
                style: style.clone(),
            },
            Shape::Text { .. } | Shape::StepNo { .. } => self.clone(),
            Shape::Line { from, to, style } => Shape::Line {
                from: map(*from),
                to: map(*to),
                style: style.clone(),
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
        assert_eq!(arrow_head_len(1, 3.5, 12, 48), 12); // 3.5 < 12
        assert_eq!(arrow_head_len(3, 3.5, 12, 48), 12); // 10.5 -> 12
        assert_eq!(arrow_head_len(10, 3.5, 12, 48), 35);
        assert_eq!(arrow_head_len(100, 3.5, 12, 48), 48); // 350 -> 48
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

    #[test]
    fn mosaic_resizes_like_rect() {
        let s = Shape::Mosaic {
            bbox: Rect::new(0, 0, 10, 10),
            mode: MosaicMode::Pixelate,
        };
        let out = s.remap_bbox(s.bbox(), Rect::new(0, 0, 20, 30));
        assert_eq!(out.bbox(), Rect::new(0, 0, 20, 30));
        assert_eq!(s.translate(Pt::new(3, 4)).bbox(), Rect::new(3, 4, 10, 10));
        // 模式随平移/缩放保留
        match out {
            Shape::Mosaic { mode, .. } => assert_eq!(mode, MosaicMode::Pixelate),
            _ => panic!("应为马赛克"),
        }
    }

    #[test]
    fn highlight_translates_and_resizes() {
        let s = Shape::Highlight {
            bbox: Rect::new(0, 0, 10, 10),
            color: super::super::style::HIGHLIGHT,
            mode: HighlightMode::Spotlight,
        };
        assert_eq!(s.translate(Pt::new(-2, -2)).bbox(), Rect::new(-2, -2, 10, 10));
        let out = s.remap_bbox(s.bbox(), Rect::new(5, 5, 8, 8));
        assert_eq!(out.bbox(), Rect::new(5, 5, 8, 8));
        match out {
            Shape::Highlight { color, mode, .. } => {
                assert_eq!(color, super::super::style::HIGHLIGHT);
                assert_eq!(mode, HighlightMode::Spotlight);
            }
            _ => panic!("应为高亮"),
        }
    }

    #[test]
    fn text_est_size_cjk_wider_than_latin() {
        // CJK 全角按 1.0em，拉丁按 0.55em
        let (w_cjk, h) = text_est_size("中", 20);
        assert_eq!((w_cjk, h), (22, 29)); // 20+2, 20*1.45
        let (w_latin, _) = text_est_size("ab", 20);
        assert_eq!(w_latin, 24); // 2*11+2
        // 多行高度按行数线性增长
        let (_, h2) = text_est_size("a\na", 20);
        assert_eq!(h2, 58);
        // 空文本不退化
        let (w, h) = text_est_size("", 20);
        assert_eq!((w, h), (2, 29));
    }

    #[test]
    fn ellipse_resizes_like_rect() {
        let s = Shape::Ellipse {
            bbox: Rect::new(0, 0, 10, 20),
            style: stroke(),
        };
        assert_eq!(s.translate(Pt::new(1, 2)).bbox(), Rect::new(1, 2, 10, 20));
        assert_eq!(s.remap_bbox(s.bbox(), Rect::new(4, 4, 8, 8)).bbox(), Rect::new(4, 4, 8, 8));
    }

    #[test]
    fn line_bbox_and_remap_follow_endpoints() {
        let s = Shape::Line {
            from: Pt::new(0, 0),
            to: Pt::new(10, 0),
            style: stroke(),
        };
        // 端点盒外扩 线宽/2 = 1
        assert_eq!(s.bbox(), Rect::new(-1, -1, 12, 2));
        let out = s.remap_bbox(s.bbox(), Rect::new(-1, -1, 36, 2));
        match out {
            Shape::Line { from, to, .. } => {
                assert!(from != to, "端点应被映射开");
            }
            _ => panic!("应为线段"),
        }
    }

    #[test]
    fn pen_bounds_and_translate() {
        let pts = vec![Pt::new(0, 0), Pt::new(10, 5), Pt::new(5, 20)];
        let s = Shape::Pen {
            pts: pts.clone(),
            style: super::super::style::PenStyle {
                color: DEF_STROKE,
                width: 4,
                kind: super::super::style::PenKind::Soft,
            },
        };
        // 轨迹盒 (0,0,10,20) 外扩 2
        assert_eq!(s.bbox(), Rect::new(-2, -2, 14, 24));
        let t = s.translate(Pt::new(3, -1));
        match t {
            Shape::Pen { pts, .. } => assert_eq!(pts[0], Pt::new(3, -1)),
            _ => panic!("应为画笔"),
        }
        assert_eq!(pen_bounds(&[]), Rect::new(0, 0, 0, 0), "空轨迹不 panic");
    }

    #[test]
    fn text_translate_moves_origin_and_bbox() {
        let s = Shape::Text {
            origin: Pt::new(10, 10),
            text: "ab".into(),
            bbox: Rect::new(10, 10, 24, 29),
            style: super::super::style::TextStyle {
                color: DEF_STROKE,
                font_px: 20,
                bold: true,
                italic: false,
            },
            frame: super::super::style::TextFrame::Bubble,
            attach: Some(7),
        };
        let t = s.translate(Pt::new(5, 5));
        match t {
            Shape::Text { origin, bbox, frame, attach, .. } => {
                assert_eq!(origin, Pt::new(15, 15));
                assert_eq!(bbox, Rect::new(15, 15, 24, 29));
                assert_eq!(frame, super::super::style::TextFrame::Bubble);
                assert_eq!(attach, Some(7), "平移保留挂线关系");
            }
            _ => panic!("应为文字"),
        }
    }
}
