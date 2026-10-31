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

/// 文字样式（文字标注；`font_px` 为物理像素的 em 高）。
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub color: Rgba,
    pub font_px: i32,
    /// 加粗（GDI+ `FontStyleBold`）。
    pub bold: bool,
    /// 斜体（GDI+ `FontStyleItalic`）。
    pub italic: bool,
    /// 文字描边：在字形四周绘制一圈对比色轮廓，提升复杂背景上的可读性。
    pub outline: bool,
}

/// 水印样式（文字水印；参与合成导出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatermarkStyle {
    /// 旋转角度（-90..90，默认 -30）。
    pub angle_deg: i32,
    /// 不透明度（0..255）。
    pub opacity: u8,
    /// 是否平铺覆盖整个选区。
    pub tile: bool,
    /// 平铺间距（逻辑像素）。
    pub gap: i32,
}

impl Default for WatermarkStyle {
    fn default() -> Self {
        Self {
            angle_deg: -30,
            opacity: 64,
            tile: true,
            gap: 240,
        }
    }
}

impl WatermarkStyle {
    /// 角度/透明度/间距钳制（配置与颜色面板的脏值都被收敛到合法范围）。
    pub fn clamped(mut self, dpi: i32) -> Self {
        self.angle_deg = self.angle_deg.clamp(-90, 90);
        self.opacity = self.opacity.min(255);
        self.gap = crate::util::scaled(self.gap, dpi).max(80);
        self
    }
}

/// 画笔形态（自由绘制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PenKind {
    /// 硬笔：等宽圆头（默认）。
    #[default]
    Solid,
    /// 软笔：按笔速渐细——轨迹点稀疏处（画得快）收细，形成提按感。
    Soft,
    /// 渐隐笔：沿轨迹从半透明渐到不透明（彗尾）。
    Fade,
}

impl PenKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "solid" | "hard" => Some(Self::Solid),
            "soft" => Some(Self::Soft),
            "fade" | "laser" => Some(Self::Fade),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Solid => "solid",
            Self::Soft => "soft",
            Self::Fade => "fade",
        }
    }
}

/// 画笔样式。
#[derive(Debug, Clone, PartialEq)]
pub struct PenStyle {
    pub color: Rgba,
    /// 基础线宽（物理像素）。
    pub width: i32,
    pub kind: PenKind,
}

/// 文字外框（气泡 / 爆炸云）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextFrame {
    /// 无外框（默认）。
    #[default]
    None,
    /// 对话气泡：圆角矩形 + 尾巴。
    Bubble,
    /// 爆炸云：波浪形闭合轮廓。
    Burst,
}

impl TextFrame {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "0" => Some(Self::None),
            "bubble" => Some(Self::Bubble),
            "burst" | "cloud" => Some(Self::Burst),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bubble => "bubble",
            Self::Burst => "burst",
        }
    }
}

/// 马赛克渲染模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MosaicMode {
    /// 块平均（像素化，默认）。
    #[default]
    Pixelate,
    /// 盒式模糊（高斯近似），观感更自然。
    Blur,
}

impl MosaicMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "pixelate" | "pixel" | "mosaic" => Some(Self::Pixelate),
            "blur" | "gaussian" => Some(Self::Blur),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pixelate => "pixelate",
            Self::Blur => "blur",
        }
    }
}

/// 高亮渲染模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HighlightMode {
    /// 荧光笔色块：按 bbox 乘法混合（默认）。
    #[default]
    Marker,
    /// 聚光灯：整图压暗，仅 bbox 内保留原亮度（圈重点/遮除干扰）。
    Spotlight,
}

impl HighlightMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "marker" | "pen" => Some(Self::Marker),
            "spotlight" | "focus" | "dim" => Some(Self::Spotlight),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Marker => "marker",
            Self::Spotlight => "spotlight",
        }
    }
}

/// 标注默认偏好（由配置文件一次性读入，注入 [`super::controller::AnnotSession`]）。
///
/// 纯数据、无 Win32 依赖：单元测试用 [`AnnotPrefs::default`]，运行时由 overlay
/// 从 `config::get()` 构造。这样纯逻辑层不必依赖配置文件，测试保持确定性。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnnotPrefs {
    pub mosaic_mode: MosaicMode,
    pub highlight_mode: HighlightMode,
    pub pen_kind: PenKind,
    pub text_bold: bool,
    pub text_italic: bool,
    /// 文字是否带描边。
    pub text_stroke: bool,
    pub text_frame: TextFrame,
    /// 文字落点靠近线段/箭头时自动挂到线上（见 `AnnotDoc` 的挂线处理）。
    pub text_attach: bool,
    /// 默认描边色（矩形/箭头/椭圆/线段/画笔/文字）。
    pub stroke_color: Rgba,
    /// 默认线宽（逻辑像素）。
    pub stroke_width: i32,
    /// 文字颜色。
    pub text_color: Rgba,
    /// 文字字号（逻辑像素）。
    pub text_size: i32,
    /// 步骤序号徽章样式。
    pub badge: BadgeStyle,
    /// 荧光笔高亮颜色。
    pub highlight_color: Rgba,
    /// 高亮乘法混合保留系数（0–1）。
    pub highlight_keep: f32,
    /// 聚光灯压暗区保留系数（0–1）。
    pub spotlight_dim: f32,
    /// 渐隐笔起点透明度下限（0–1）。
    pub fade_min_alpha: f32,
    /// 步骤序号默认半径（逻辑像素）。
    pub step_radius: i32,
    /// 最小尺寸阈值（逻辑像素，更小的拖动/绘制丢弃）。
    pub min_size: i32,
    /// 画笔轨迹最小采样步长（逻辑像素）。
    pub pen_min_step: i32,
    /// 文字自动挂线容差（逻辑像素）。
    pub attach_tol: i32,
    /// 标注命中容差（逻辑像素）。
    pub hit_tol: i32,
    /// 控制柄命中半径（逻辑像素）。
    pub handle_hit: i32,
    /// 控制柄绘制边长（逻辑像素）。
    pub handle_size: i32,
    /// 选中态外扩量（逻辑像素）。
    pub chrome_ext: i32,
    /// 箭头头部长度与线宽的比。
    pub arrow_width_ratio: f32,
    /// 箭头头部长度下限（逻辑像素）。
    pub arrow_len_min: i32,
    /// 箭头头部长度上限（逻辑像素）。
    pub arrow_len_max: i32,
    /// 橡皮擦除命中容差（逻辑像素）。
    pub eraser_tol: i32,
    /// 水印默认角度（-90..90）。
    pub watermark_angle: i32,
    /// 水印默认透明度（0..255）。
    pub watermark_opacity: u8,
    /// 水印默认平铺。
    pub watermark_tile: bool,
    /// 水印平铺间距（逻辑像素）。
    pub watermark_gap: i32,
}

impl Default for AnnotPrefs {
    fn default() -> Self {
        Self {
            mosaic_mode: MosaicMode::default(),
            highlight_mode: HighlightMode::default(),
            pen_kind: PenKind::default(),
            text_bold: false,
            text_italic: false,
            text_stroke: false,
            text_frame: TextFrame::default(),
            text_attach: true,
            stroke_color: DEF_STROKE,
            stroke_width: DEF_STROKE_W,
            text_color: DEF_STROKE,
            text_size: DEF_TEXT_PX,
            badge: DEF_BADGE,
            highlight_color: HIGHLIGHT,
            highlight_keep: HIGHLIGHT_KEEP,
            spotlight_dim: SPOTLIGHT_DIM,
            fade_min_alpha: FADE_MIN_ALPHA,
            step_radius: 14,
            min_size: 3,
            pen_min_step: 2,
            attach_tol: 10,
            hit_tol: 4,
            handle_hit: 5,
            handle_size: 7,
            chrome_ext: 6,
            arrow_width_ratio: 3.5,
            arrow_len_min: 12,
            arrow_len_max: 48,
            eraser_tol: 6,
            watermark_angle: -30,
            watermark_opacity: 64,
            watermark_tile: true,
            watermark_gap: 240,
        }
    }
}

/// 高亮（荧光笔）颜色：荧光黄。
///
/// 渲染为**乘法混合**（见 `session::apply_highlight`）：底层深色文字保持可读。
pub const HIGHLIGHT: Rgba = Rgba {
    r: 0xFF,
    g: 0xCC,
    b: 0x00,
    a: 0xFF,
};

/// 高亮的乘法系数（参照 SnowShot `drawHighlight`：保留 55% 反差，
/// 使底层文字在荧光色下依然可读）。
pub const HIGHLIGHT_KEEP: f32 = 0.55;

/// 聚光灯模式：非高亮区（"压暗"区）保留的亮度系数（参照 SnowAir：压暗 45%）。
pub const SPOTLIGHT_DIM: f32 = 0.55;

/// 渐隐笔起点透明度下限（沿轨迹 0→1 渐变，1/255 视觉不可察觉的起点过淡，故抬高）。
pub const FADE_MIN_ALPHA: f32 = 0.12;

/// 文字标注默认字号（逻辑像素，96 DPI 基准；按 DPI 换算后写入 shape）。
pub const DEF_TEXT_PX: i32 = 16;

/// 步骤序号徽章样式。
#[derive(Debug, Clone, Copy, PartialEq)]
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

    #[test]
    fn highlight_is_opaque_yellow() {
        assert_eq!(HIGHLIGHT.to_argb(), 0xFFFF_CC00);
        assert!((0.0..=1.0).contains(&HIGHLIGHT_KEEP));
        assert_eq!(DEF_TEXT_PX, 16);
    }

    #[test]
    fn mode_enums_parse_and_label() {
        assert_eq!(MosaicMode::parse("blur"), Some(MosaicMode::Blur));
        assert_eq!(MosaicMode::parse(""), Some(MosaicMode::Pixelate));
        assert_eq!(MosaicMode::parse("bad"), None);
        assert_eq!(MosaicMode::default().label(), "pixelate");

        assert_eq!(HighlightMode::parse("spotlight"), Some(HighlightMode::Spotlight));
        assert_eq!(HighlightMode::default(), HighlightMode::Marker);

        assert_eq!(PenKind::parse("soft"), Some(PenKind::Soft));
        assert_eq!(PenKind::parse("laser"), Some(PenKind::Fade));
        assert_eq!(PenKind::default(), PenKind::Solid);

        assert_eq!(TextFrame::parse("none"), Some(TextFrame::None));
        assert_eq!(TextFrame::parse("cloud"), Some(TextFrame::Burst));
        assert_eq!(TextFrame::default(), TextFrame::None);
    }

    #[test]
    fn annot_prefs_default_attaches_text() {
        let p = AnnotPrefs::default();
        assert!(p.text_attach, "默认自动挂线");
        assert_eq!(p.pen_kind, PenKind::Solid);
        assert!(!p.text_bold && !p.text_italic && !p.text_stroke);
    }
}
