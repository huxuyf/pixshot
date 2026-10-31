//! 截图辅助的纯计算：取色、颜色格式化、放大镜/辅助线几何。
//!
//! 本模块**不依赖 Win32**（只借用 `capture::CapturedImage` 的数据结构），
//! 便于单元测试；overlay 负责把结果画到分层表面上。

use crate::annot::geometry::{Pt, Rect};
use crate::annot::style::Rgba;
use crate::capture::CapturedImage;

/// 颜色文本格式（对应配置 `assist_color_format`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorFormat {
    /// `#RRGGBB`（默认）。
    #[default]
    HexRgb,
    /// `#RRGGBBAA`。
    HexRgba,
    /// `rgb(r, g, b)`。
    Rgb,
    /// `hsl(h, s%, l%)`。
    Hsl,
    /// `hsv(h, s%, v%)`。
    Hsv,
}

impl ColorFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "hex" | "hex_rgb" | "rgb_hex" => Some(Self::HexRgb),
            "hex_rgba" | "rgba" => Some(Self::HexRgba),
            "rgb" => Some(Self::Rgb),
            "hsl" => Some(Self::Hsl),
            "hsv" | "hsb" => Some(Self::Hsv),
            _ => None,
        }
    }

    /// 循环切到下一个格式（Shift+C 使用）。
    pub fn next(self) -> Self {
        match self {
            Self::HexRgb => Self::HexRgba,
            Self::HexRgba => Self::Rgb,
            Self::Rgb => Self::Hsl,
            Self::Hsl => Self::Hsv,
            Self::Hsv => Self::HexRgb,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::HexRgb => "hex_rgb",
            Self::HexRgba => "hex_rgba",
            Self::Rgb => "rgb",
            Self::Hsl => "hsl",
            Self::Hsv => "hsv",
        }
    }
}

/// 采样快照中屏幕坐标 `(x, y)` 的像素；越界返回 `None`。
///
/// `CapturedImage` 的 `x/y` 是快照原点（虚拟屏坐标），内部做偏移换算。
pub fn color_at(img: &CapturedImage, x: i32, y: i32) -> Option<Rgba> {
    let lx = x - img.x;
    let ly = y - img.y;
    if lx < 0 || ly < 0 || lx >= img.width || ly >= img.height {
        return None;
    }
    let off = (ly as usize * img.width.max(1) as usize + lx as usize) * 4;
    if off + 4 > img.data.len() {
        return None;
    }
    Some(Rgba::new(
        img.data[off + 2],
        img.data[off + 1],
        img.data[off],
        img.data[off + 3],
    ))
}

/// 颜色 → 文本（`Rgba.a` 仅在 HexRgba 中使用）。
pub fn format_color(c: Rgba, f: ColorFormat) -> String {
    match f {
        ColorFormat::HexRgb => format!("#{:02X}{:02X}{:02X}", c.r, c.g, c.b),
        ColorFormat::HexRgba => format!("#{:02X}{:02X}{:02X}{:02X}", c.r, c.g, c.b, c.a),
        ColorFormat::Rgb => format!("rgb({}, {}, {})", c.r, c.g, c.b),
        ColorFormat::Hsl => {
            let (h, s, l) = rgb_to_hsl(c.r, c.g, c.b);
            format!("hsl({}, {}%, {}%)", h, s, l)
        }
        ColorFormat::Hsv => {
            let (h, s, v) = rgb_to_hsv(c.r, c.g, c.b);
            format!("hsv({}, {}%, {}%)", h, s, v)
        }
    }
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (u16, u8, u8) {
    let (rf, gf, bf) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0, 0, (l * 100.0).round() as u8);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == rf {
        ((gf - bf) / d + if gf < bf { 6.0 } else { 0.0 }) / 6.0
    } else if max == gf {
        ((bf - rf) / d + 2.0) / 6.0
    } else {
        ((rf - gf) / d + 4.0) / 6.0
    };
    (
        (h * 360.0).round() as u16 % 360,
        (s * 100.0).round().clamp(0.0, 100.0) as u8,
        (l * 100.0).round().clamp(0.0, 100.0) as u8,
    )
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (u16, u8, u8) {
    let (rf, gf, bf) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let d = max - min;
    let v = max;
    let s = if max <= f32::EPSILON { 0.0 } else { d / max };
    let h = if d <= f32::EPSILON {
        0.0
    } else if max == rf {
        ((gf - bf) / d + if gf < bf { 6.0 } else { 0.0 }) / 6.0
    } else if max == gf {
        ((bf - rf) / d + 2.0) / 6.0
    } else {
        ((rf - gf) / d + 4.0) / 6.0
    };
    (
        (h * 360.0).round() as u16 % 360,
        (s * 100.0).round().clamp(0.0, 100.0) as u8,
        (v * 100.0).round().clamp(0.0, 100.0) as u8,
    )
}

/// 放大镜窗口矩形：以光标为中心，`size` 为边长（物理像素），
/// 贴边时自动内收，保证完全落在 `screen`（虚拟屏）内。
pub fn lens_rect(cursor: Pt, size: i32, screen: Rect) -> Rect {
    let size = size.max(24);
    let sw = screen.w.max(1);
    let sh = screen.h.max(1);
    // 屏幕比镜头还小时退化为整屏
    let w = size.min(sw);
    let h = size.min(sh);
    let mut x = cursor.x - w / 2;
    let mut y = cursor.y - h / 2;
    x = x.clamp(screen.x, screen.right() - w);
    y = y.clamp(screen.y, screen.bottom() - h);
    Rect::new(x, y, w, h)
}

/// 放大镜内"光标像素"在镜头中的局部坐标（镜头几何固定，便于准星绘制）。
#[allow(dead_code)]
pub fn lens_local_point(cursor: Pt, lens: Rect) -> Pt {
    Pt::new(cursor.x - lens.x, cursor.y - lens.y)
}

/// 辅助线线段：四条（上/下/左/右）从光标到屏幕边缘。
/// `grid=true` 时额外返回选区内的两条纵向 + 两条横向三分线。
#[allow(dead_code)]
pub fn aux_segments(cursor: Pt, screen: Rect, sel: Option<Rect>, grid: bool) -> Vec<(Pt, Pt)> {
    let mut v = Vec::with_capacity(if grid { 8 } else { 4 });
    v.push((Pt::new(cursor.x, screen.y), Pt::new(cursor.x, cursor.y)));
    v.push((
        Pt::new(cursor.x, cursor.y),
        Pt::new(cursor.x, screen.bottom()),
    ));
    v.push((Pt::new(screen.x, cursor.y), Pt::new(cursor.x, cursor.y)));
    v.push((
        Pt::new(cursor.x, cursor.y),
        Pt::new(screen.right(), cursor.y),
    ));
    if grid {
        if let Some(s) = sel {
            let x1 = s.x + s.w / 3;
            let x2 = s.x + s.w * 2 / 3;
            let y1 = s.y + s.h / 3;
            let y2 = s.y + s.h * 2 / 3;
            v.push((Pt::new(x1, s.y), Pt::new(x1, s.bottom())));
            v.push((Pt::new(x2, s.y), Pt::new(x2, s.bottom())));
            v.push((Pt::new(s.x, y1), Pt::new(s.right(), y1)));
            v.push((Pt::new(s.x, y2), Pt::new(s.right(), y2)));
        }
    }
    v
}

/// 信息条文本：`"1234×567   X:812 Y:433   #3A7BD5"`（缺失项省略）。
pub fn info_text(sel: Option<Rect>, cursor: Pt, color: Option<Rgba>, f: ColorFormat) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if let Some(s) = sel {
        if s.w > 0 && s.h > 0 {
            parts.push(format!("{} × {}", s.w, s.h));
        }
    }
    parts.push(format!("X:{} Y:{}", cursor.x, cursor.y));
    if let Some(c) = color {
        parts.push(format_color(c, f));
    }
    parts.join("   ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn img(x: i32, y: i32, w: i32, h: i32) -> CapturedImage {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            data[i * 4] = 0x55; // B
            data[i * 4 + 1] = 0x66; // G
            data[i * 4 + 2] = 0x77; // R
            data[i * 4 + 3] = 0xFF;
        }
        CapturedImage {
            x,
            y,
            width: w,
            height: h,
            data: Arc::from(data),
        }
    }

    #[test]
    fn format_hex_rgb() {
        assert_eq!(
            format_color(Rgba::new(0x3A, 0x7B, 0xD5, 0xFF), ColorFormat::HexRgb),
            "#3A7BD5"
        );
    }

    #[test]
    fn format_hex_rgba() {
        assert_eq!(
            format_color(Rgba::new(0x3A, 0x7B, 0xD5, 0x80), ColorFormat::HexRgba),
            "#3A7BD580"
        );
    }

    #[test]
    fn format_hsl_roundtrip() {
        assert_eq!(
            format_color(Rgba::new(0xFF, 0, 0, 0xFF), ColorFormat::Hsl),
            "hsl(0, 100%, 50%)"
        );
        assert_eq!(
            format_color(Rgba::new(0xFF, 0xFF, 0xFF, 0xFF), ColorFormat::Hsl),
            "hsl(0, 0%, 100%)"
        );
    }

    #[test]
    fn format_hsv() {
        assert_eq!(
            format_color(Rgba::new(0xFF, 0, 0, 0xFF), ColorFormat::Hsv),
            "hsv(0, 100%, 100%)"
        );
    }

    #[test]
    fn color_at_oob() {
        let im = img(0, 0, 4, 4);
        assert_eq!(color_at(&im, -1, 0), None);
        assert_eq!(color_at(&im, 4, 0), None);
        assert_eq!(color_at(&im, 0, 4), None);
    }

    #[test]
    fn color_at_offset() {
        let im = img(-1920, 0, 100, 100);
        // 屏幕 (-1000,100) → 快照本地 (920,100)，超出 100×100 → None
        assert_eq!(color_at(&im, -1000, 100), None);
        // 屏幕 (-1900,50) → 本地 (20,50) 命中
        assert_eq!(
            color_at(&im, -1900, 50),
            Some(Rgba::new(0x77, 0x66, 0x55, 0xFF))
        );
    }

    #[test]
    fn lens_clamps_left_top_and_right_bottom() {
        let screen = Rect::new(-1920, 0, 3840, 1080);
        let l = lens_rect(Pt::new(-1920, 0), 121, screen);
        assert_eq!(l.x, -1920);
        assert_eq!(l.y, 0);
        let l = lens_rect(Pt::new(-1920, 1079), 121, screen);
        assert_eq!(l.x, -1920, "屏幕左缘内收");
        assert_eq!(l.y, 959, "屏幕下缘内收");
        let l = lens_rect(Pt::new(1919, 1079), 121, screen);
        assert_eq!(l.right(), screen.right());
        assert_eq!(l.bottom(), screen.bottom());
        assert!(l.w > 0 && l.h > 0);
    }

    #[test]
    fn lens_local_point_is_inside() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let cursor = Pt::new(100, 100);
        let lens = lens_rect(cursor, 121, screen);
        let p = lens_local_point(cursor, lens);
        assert!(lens.contains(p));
    }

    #[test]
    fn aux_segments_count() {
        let screen = Rect::new(0, 0, 1920, 1080);
        assert_eq!(aux_segments(Pt::new(100, 100), screen, None, false).len(), 4);
        let sel = Some(Rect::new(10, 10, 300, 200));
        assert_eq!(aux_segments(Pt::new(100, 100), screen, sel, true).len(), 8);
        // 有九宫格开关但没有选区：仍只有 4 条
        assert_eq!(aux_segments(Pt::new(100, 100), screen, None, true).len(), 4);
    }

    #[test]
    fn info_text_omits() {
        let s = info_text(None, Pt::new(1, 2), None, ColorFormat::HexRgb);
        assert!(!s.contains('#'));
        assert!(!s.contains('×'));
        let s = info_text(
            Some(Rect::new(0, 0, 9999, 9999)),
            Pt::new(1, 2),
            Some(Rgba::new(0x3A, 0x7B, 0xD5, 0xFF)),
            ColorFormat::HexRgb,
        );
        assert!(s.contains("9999 × 9999"));
        assert!(s.contains("#3A7BD5"));
        assert!(s.contains('\n') == false);
    }

    #[test]
    fn color_format_cycles_back_after_five() {
        let mut f = ColorFormat::HexRgb;
        for _ in 0..5 {
            f = f.next();
        }
        assert_eq!(f, ColorFormat::HexRgb);
        assert_eq!(ColorFormat::parse("hex_rgba"), Some(ColorFormat::HexRgba));
        assert_eq!(ColorFormat::parse("bogus"), None);
    }
}
