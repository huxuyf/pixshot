//! 标注引擎：颜色与样式（纯逻辑，不依赖 Win32）。
//!
//! 常量取自设计文档 3.2；`width` / `radius` 等尺寸字段一律为**物理像素**，
//! 用时由调用方以 `util::scaled(逻辑值, dpi)` 换算后写入。

/// 非预乘 RGBA 颜色（0x00–0xFF）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// 转 GDI+ 的 ARGB（`0xAARRGGBB`）。
    pub const fn to_argb(self) -> u32 {
        ((self.a as u32) << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }
}

/// 描边样式（矩形/箭头）。
#[derive(Debug, Clone, PartialEq)]
pub struct StrokeStyle {
    pub color: Rgba,
    /// 线宽（物理像素）。
    pub width: i32,
}

/// 步骤序号徽章样式。
#[derive(Debug, Clone, PartialEq)]
pub struct BadgeStyle {
    pub fill: Rgba,
    pub text: Rgba,
    /// 可选外圈（白色增对比）。
    pub outline: Option<Rgba>,
}

/// 默认描边色：红（96 DPI 基准，见设计文档 3.2）。
pub const DEF_STROKE: Rgba = Rgba {
    r: 0xE5,
    g: 0x39,
    b: 0x35,
    a: 0xFF,
};
/// 默认线宽（逻辑像素）。
pub const DEF_STROKE_W: i32 = 3;
/// 默认徽章样式：蓝底白字白圈。
pub const DEF_BADGE: BadgeStyle = BadgeStyle {
    fill: Rgba {
        r: 0x1E,
        g: 0x6F,
        b: 0xFF,
        a: 0xFF,
    },
    text: Rgba {
        r: 0xFF,
        g: 0xFF,
        b: 0xFF,
        a: 0xFF,
    },
    outline: Some(Rgba {
        r: 0xFF,
        g: 0xFF,
        b: 0xFF,
        a: 0xFF,
    }),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb_packing_is_aarrggbb() {
        assert_eq!(DEF_STROKE.to_argb(), 0xFFE5_3935);
        assert_eq!(DEF_BADGE.fill.to_argb(), 0xFF1E_6FFF);
        assert_eq!(Rgba::new(0x11, 0x22, 0x33, 0x44).to_argb(), 0x4411_2233);
    }

    #[test]
    fn defaults_match_design_doc() {
        assert_eq!(DEF_STROKE_W, 3);
        assert_eq!(DEF_STROKE, Rgba::new(0xE5, 0x39, 0x35, 0xFF));
        assert!(DEF_BADGE.outline.is_some());
    }
}
