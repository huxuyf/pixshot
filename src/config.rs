//! 配置：`config.ini`（与 PixShot.exe 同目录，`[section]` + `key = value` 纯文本，零依赖）。
//!
//! 设计约束：
//! - 手写解析，不引第三方 TOML/YAML 库（保持"零额外依赖"路线）；
//! - 解析失败一律回退默认值 —— 配置文件绝不能让程序起不来；
//! - 文件不存在时写入带注释的模板，用户可自行编辑。
//!
//! 结构：按用途分节（`[hotkey]` / `[general]` / `[ocr]` / `[annot]` / `[ui]` /
//! `[pin]` / `[textimg]` / `[rtfimg]` / `[imageio]` / `[tray]`）。节名仅用于组织文件，
//! 解析时按 **叶子键名** 匹配（键名在全文件内唯一），因此旧版无分节的扁平配置
//! （如 `ocr_lang = ...`）依然可用；重命名过的项额外保留旧名作为别名。

use std::path::PathBuf;
use std::sync::Mutex;

use crate::annot::style::{
    AnnotPrefs, BadgeStyle, HighlightMode, MosaicMode, PenKind, Rgba, TextFrame, DEF_BADGE,
    DEF_STROKE, DEF_STROKE_W, DEF_TEXT_PX, HIGHLIGHT,
};

// 修饰键位（与 Win32 `MOD_*` 数值一致）。此处用裸 u32，
// 使热键解析成为不依赖 Windows API 的纯函数，便于单元测试。
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_NOREPEAT: u32 = 0x4000;

/// 保存格式（WIC 容器）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaveFormat {
    Png,
    Jpeg,
    Bmp,
}

impl SaveFormat {
    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Bmp => "bmp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG 图片",
            Self::Jpeg => "JPEG 图片",
            Self::Bmp => "BMP 位图",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "bmp" => Some(Self::Bmp),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    // ── [hotkey] 全局热键 ────────────────────────────────────────
    /// 截图热键，如 `F1`、`Ctrl+Shift+S`、`Alt+PrintScreen`。
    pub hotkey: String,
    /// 剪贴板贴图热键（把剪贴板内容以图片置顶），如 `F3`。
    pub pin_hotkey: String,

    // ── [general] 通用 ──────────────────────────────────────────
    /// 默认保存格式。
    pub format: SaveFormat,
    /// 上次保存目录（下次对话框默认打开它）。
    pub save_dir: Option<String>,
    /// 轻提示停留时长（毫秒）。
    pub toast_ms: u32,
    /// 截图是否包含鼠标指针（`BitBlt` 默认不含，需手动合成，见 `cursor` 模块）。
    pub capture_cursor: bool,

    // ── [ocr] 文字识别 ──────────────────────────────────────────
    /// 首选 OCR 语言标签（如 `zh-Hans-CN`）；空 = 按系统语言自动回退。
    pub ocr_lang: Option<String>,
    /// OCR 版面类型（对应 PixPin `Ocr.LayoutType`）。
    pub ocr_layout: OcrLayout,
    /// OCR 标点转换（对应 PixPin `Ocr.PunctuationType`）。
    pub ocr_punctuation: OcrPunctuation,
    /// 是否删除空格（对应 PixPin `Ocr.RemoveSpaces`，保留换行）。
    pub ocr_remove_spaces: bool,
    /// 行置信度下限（0.0–1.0）：低于它的行不输出（仅对自带置信度的 ONNX 引擎生效）。
    pub ocr_confidence: f32,
    /// OCR 前放大目标：长边小于该值时按比例放大（提升小字识别率）。
    pub ocr_upsample_target: i32,
    /// 放大倍数上限（避免超大图过度放大）。
    pub ocr_max_upscale: f32,
    /// 深底判定的平均亮度阈值（0–255）：低于它视为深底，尝试反色识别。
    pub ocr_dark_luma: f32,
    /// 检测模型输入长边（越大越准越慢；会对齐到 32 的倍数）。
    pub ocr_det_limit: usize,
    /// 文本区域置信度阈值（低于它的候选框被丢弃）。
    pub ocr_det_thresh: f32,
    /// 文本框（二值化后连通域）分数阈值。
    pub ocr_box_thresh: f32,
    /// 文本框扩张系数（DB 后处理的 unclip ratio）。
    pub ocr_unclip_ratio: f32,

    // ── [annot] 标注默认样式与手感 ──────────────────────────────
    /// 马赛克渲染模式：块平均 / 盒式模糊。
    pub mosaic_mode: MosaicMode,
    /// 高亮渲染模式：荧光笔色块 / 聚光灯（整图压暗 + 挖洞）。
    pub highlight_mode: HighlightMode,
    /// 画笔形态：硬笔 / 软笔 / 渐隐笔。
    pub pen_kind: PenKind,
    /// 文字标注是否加粗。
    pub text_bold: bool,
    /// 文字标注是否斜体。
    pub text_italic: bool,
    /// 文字标注是否描边。
    pub text_stroke: bool,
    /// 文字外框：无 / 气泡 / 爆炸云。
    pub text_frame: TextFrame,
    /// 文字落点靠近线段/箭头时是否自动挂线（随线段移动）。
    pub text_attach: bool,
    /// 默认描边色（矩形/箭头/椭圆/线段/画笔/文字）。
    pub stroke_color: Rgba,
    /// 默认线宽（逻辑像素，96 DPI 基准）。
    pub stroke_width: i32,
    /// 文字标注颜色。
    pub text_color: Rgba,
    /// 文字标注字号（逻辑像素）。
    pub text_size: i32,
    /// 步骤序号徽章底色。
    pub badge_fill: Rgba,
    /// 步骤序号徽章文字色。
    pub badge_text: Rgba,
    /// 步骤序号徽章外圈色；空 = 无外圈。
    pub badge_outline: Option<Rgba>,
    /// 荧光笔高亮颜色。
    pub highlight_color: Rgba,
    /// 高亮乘法混合保留系数（0–1，越小底色越亮）。
    pub highlight_keep: f32,
    /// 聚光灯模式压暗区保留系数（0–1）。
    pub spotlight_dim: f32,
    /// 渐隐笔起点透明度下限（0–1）。
    pub fade_min_alpha: f32,
    /// 步骤序号默认半径（逻辑像素）。
    pub step_radius: i32,
    /// 最小尺寸阈值（逻辑像素，小于此值的拖动/绘制视为误触丢弃）。
    pub min_size: i32,
    /// 画笔轨迹最小采样步长（逻辑像素）。
    pub pen_min_step: i32,
    /// 文字自动挂线的距离容差（逻辑像素）。
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
    /// 文字/图标字体族（标注文字与步骤数字）。
    pub text_font: String,
    /// 爆炸云外框的波峰数。
    pub burst_bumps: f32,
    /// 爆炸云外框的波幅比（相对短边）。
    pub burst_amp: f32,
    /// 控制柄填充色。
    pub handle_fill: Rgba,
    /// 控制柄描边色。
    pub handle_stroke: Rgba,
    /// 选区虚线框颜色。
    pub chrome_color: Rgba,
    /// 橡皮擦除命中容差（逻辑像素）。
    pub eraser_tol: i32,
    /// 水印默认角度（-90..90）。
    pub watermark_angle: i32,
    /// 水印默认透明度（0..255）。
    pub watermark_opacity: u8,
    /// 水印是否平铺。
    pub watermark_tile: bool,
    /// 水印平铺间距（逻辑像素）。
    pub watermark_gap: i32,
    /// 是否启用标注颜色面板。
    pub style_panel: bool,
    /// 颜色面板预设色（8 色：红 / 绿 / 蓝 / 青 / 品红 / 黄 / 黑 / 白）。
    pub style_palette: [Rgba; 8],

    // ── [pick] 智能选区 ────────────────────────────────────────
    /// 拾取级别：none / region / window / element。
    pub pick_level: crate::pick::PickLevel,
    /// 候选框额外外扩（逻辑像素）。
    pub pick_padding: i32,
    /// 客户区矩形自动扩充到窗口矩形。
    pub pick_auto_expand: bool,
    /// 探测节流（毫秒，不得低于 16）。
    pub pick_probe_interval_ms: u32,
    /// 是否在候选框下方显示标题标签。
    pub pick_show_label: bool,
    /// 元素级最大上探深度。
    pub pick_max_depth: u32,

    // ── [assist] 截图辅助 ──────────────────────────────────────
    /// 放大镜开关。
    pub assist_magnifier: bool,
    /// 放大镜边长（逻辑像素）。
    pub assist_magnifier_size: i32,
    /// 放大倍数。
    pub assist_magnifier_zoom: i32,
    /// 辅助线开关。
    pub assist_aux_lines: bool,
    /// 选区内九宫格。
    pub assist_aux_grid: bool,
    /// 信息条开关。
    pub assist_show_info: bool,
    /// 取色文本格式。
    pub assist_color_format: crate::assist::ColorFormat,
    /// 标注阶段是否保留辅助层。
    pub assist_in_annot: bool,

    // ── [ui] 界面尺寸与配色 ─────────────────────────────────────
    /// 工具栏图标按钮边长（逻辑像素）。
    pub ui_toolbar_btn_size: i32,
    /// 工具栏按钮间隔（逻辑像素）。
    pub ui_toolbar_gap: i32,
    /// 工具栏上下内边距（逻辑像素）。
    pub ui_toolbar_pad_v: i32,
    /// 工具栏左右内边距（逻辑像素）。
    pub ui_toolbar_pad_h: i32,
    /// 工具栏窗口圆角（逻辑像素）。
    pub ui_toolbar_radius: i32,
    /// 悬停提示延时（毫秒）。
    pub ui_tooltip_delay_ms: u32,
    /// 提示气泡水平内边距（逻辑像素）。
    pub ui_tooltip_pad_x: i32,
    /// 提示气泡垂直内边距（逻辑像素）。
    pub ui_tooltip_pad_y: i32,
    /// 提示气泡与按钮的间距（逻辑像素）。
    pub ui_tooltip_gap: i32,
    /// 提示气泡最大宽度（逻辑像素）。
    pub ui_tooltip_max_width: i32,
    /// 选区遮罩暗化系数（0–1，越小越暗）。
    pub ui_overlay_dim: f32,
    /// 选区框线颜色。
    pub ui_overlay_frame_color: Rgba,
    /// 选区框线粗细（逻辑像素）。
    pub ui_overlay_frame_width: i32,
    /// 选区尺寸标签字号（逻辑像素）。
    pub ui_overlay_label_px: i32,
    /// 选区尺寸标签颜色。
    pub ui_overlay_label_color: Rgba,
    /// 有效框选最小边长（逻辑像素，更小的拖动忽略）。
    pub ui_overlay_min_drag: i32,
    /// Toast 水平内边距（逻辑像素）。
    pub ui_toast_pad_x: i32,
    /// Toast 垂直内边距（逻辑像素）。
    pub ui_toast_pad_y: i32,
    /// Toast 与光标间距（逻辑像素）。
    pub ui_toast_gap: i32,
    /// Toast 圆角（逻辑像素）。
    pub ui_toast_radius: i32,
    /// Toast 字号（磅）。
    pub ui_toast_font_pt: i32,
    /// Toast 底色。
    pub ui_toast_bg: Rgba,
    /// Toast 描边色。
    pub ui_toast_border: Rgba,
    /// Toast 文字色。
    pub ui_toast_text: Rgba,
    /// 右键菜单项高度（逻辑像素）。
    pub ui_menu_item_h: i32,
    /// 右键菜单分隔线高度（逻辑像素）。
    pub ui_menu_sep_h: i32,
    /// 右键菜单上下内边距（逻辑像素）。
    pub ui_menu_pad: i32,
    /// 右键菜单最小宽度（逻辑像素）。
    pub ui_menu_min_w: i32,
    /// 右键菜单右侧留白（逻辑像素，给快捷键文本）。
    pub ui_menu_right_pad: i32,
    /// 右键菜单圆角（逻辑像素）。
    pub ui_menu_radius: i32,
    /// 右键菜单悬停高亮圆角（逻辑像素）。
    pub ui_menu_hover_radius: i32,
    /// 右键菜单文本左缩进（逻辑像素）。
    pub ui_menu_text_left: i32,
    /// 右键菜单快捷键文本右缩进（逻辑像素）。
    pub ui_menu_text_right: i32,
    /// 右键菜单分隔线左右缩进（逻辑像素）。
    pub ui_menu_sep_inset: i32,
    /// 右键菜单字号（磅）。
    pub ui_menu_font_pt: i32,
    /// 右键菜单快捷键文本与说明文本的间隔（逻辑像素）。
    pub ui_menu_accel_gap: i32,
    /// 右键菜单禁用项文字色。
    pub ui_menu_disabled_color: Rgba,
    /// 是否启用 Mica 背景（关闭则用实色表面）。
    pub ui_mica: bool,

    // ── [pin] 贴图窗口 ──────────────────────────────────────────
    /// 贴图最小缩放。
    pub pin_min_scale: f32,
    /// 贴图最大缩放。
    pub pin_max_scale: f32,
    /// 贴图阴影留白（逻辑像素）。
    pub pin_shadow_margin: i32,
    /// 贴图阴影峰值不透明度（0–1）。
    pub pin_shadow_peak: f32,
    /// 滚轮缩放步进（每格乘/除该系数）。
    pub pin_zoom_step: f32,
    /// 首次显示时贴合屏幕的比例（相对工作区）。
    pub pin_fit_ratio: f32,
    /// 新贴图默认不透明度（alpha 26..255，即 10%..100%）。
    pub pin_opacity: u8,
    /// 新贴图默认锁定位置。
    pub pin_locked: bool,
    /// 新贴图默认显示阴影。
    pub pin_shadow: bool,
    /// 新贴图默认置顶。
    pub pin_topmost: bool,
    /// 边缘缩放热区（逻辑像素）。
    pub pin_edge_hit: i32,
    /// 缩放后窗口至少保留的可见像素。
    pub pin_min_visible_px: i32,

    // ── [textimg] 文本贴图渲染 ──────────────────────────────────
    /// 文本贴图字体族。
    pub textimg_font_family: String,
    /// 文本贴图字号（像素）。
    pub textimg_font_px: i32,
    /// 文本贴图页边距（像素）。
    pub textimg_margin: i32,
    /// 文本贴图最大宽度（像素）。
    pub textimg_max_width: i32,
    /// 文本贴图行距倍数。
    pub textimg_line_spacing: f32,
    /// 文本贴图前景色（文字）。
    pub textimg_fg: Rgba,
    /// 文本贴图背景色。
    pub textimg_bg: Rgba,

    // ── [rtfimg] 富文本贴图渲染 ─────────────────────────────────
    /// 富文本离屏渲染宽度（像素）。
    pub rtfimg_render_width: i32,
    /// 富文本渲染后裁边留白（像素）。
    pub rtfimg_render_margin: i32,

    // ── [imageio] 图像保存 ──────────────────────────────────────
    /// 自动保存目录（空 = 默认 `%USERPROFILE%\Pictures\PixShot`）。
    pub imageio_autosave_dir: Option<String>,
    /// 自动保存文件名前缀。
    pub imageio_prefix: String,
    /// 自动保存文件名序号上限。
    pub imageio_max_seq: u32,

    // ── [tray] 托盘与预捕获 ─────────────────────────────────────
    /// 热键预捕获快照的有效期（毫秒）。
    pub tray_precapture_ttl_ms: u64,
}

/// OCR 版面类型（`ocr_layout_type`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OcrLayout {
    /// 自动：多行时按段落输出。
    #[default]
    Auto,
    /// 单行：所有内容拼成一行。
    Single,
    /// 多行：每行一行文本，段落间空行分隔。
    Multi,
}

impl OcrLayout {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "single" | "one" => Some(Self::Single),
            "multi" | "multiple" => Some(Self::Multi),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Single => "single",
            Self::Multi => "multi",
        }
    }
}

/// OCR 标点转换（`ocr_punctuation_type`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OcrPunctuation {
    /// 不转换。
    #[default]
    None,
    /// 半角 → 全角。
    Full,
    /// 全角 → 半角。
    Half,
}

impl OcrPunctuation {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "none" => Some(Self::None),
            "full" | "fullwidth" => Some(Self::Full),
            "half" | "halfwidth" => Some(Self::Half),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::Half => "half",
        }
    }
}

// 默认值单源于 `annot::style` 的历史常量，保证行为不变。
const WHITE: Rgba = Rgba::new(0xFF, 0xFF, 0xFF, 0xFF);

/// 颜色面板默认 8 色：红 / 绿 / 蓝 / 青 / 品红 / 黄 / 黑 / 白（顺序固定）。
const DEF_STYLE_PALETTE: [Rgba; 8] = [
    Rgba::new(0xFF, 0x00, 0x00, 0xFF), // 红
    Rgba::new(0x00, 0xFF, 0x00, 0xFF), // 绿
    Rgba::new(0x00, 0x00, 0xFF, 0xFF), // 蓝
    Rgba::new(0x00, 0xFF, 0xFF, 0xFF), // 青
    Rgba::new(0xFF, 0x00, 0xFF, 0xFF), // 品红
    Rgba::new(0xFF, 0xFF, 0x00, 0xFF), // 黄
    Rgba::new(0x00, 0x00, 0x00, 0xFF), // 黑
    Rgba::new(0xFF, 0xFF, 0xFF, 0xFF), // 白
];


impl Default for Config {
    fn default() -> Self {
        Self {
            // [hotkey]
            hotkey: "F1".into(),
            pin_hotkey: "F3".into(),
            // [general]
            format: SaveFormat::Png,
            save_dir: None,
            toast_ms: 1600,
            capture_cursor: false,
            // [ocr]
            ocr_lang: None,
            ocr_layout: OcrLayout::Auto,
            ocr_punctuation: OcrPunctuation::None,
            ocr_remove_spaces: false,
            ocr_confidence: 0.0,
            ocr_upsample_target: 2400,
            ocr_max_upscale: 3.0,
            ocr_dark_luma: 110.0,
            ocr_det_limit: 960,
            ocr_det_thresh: 0.3,
            ocr_box_thresh: 0.6,
            ocr_unclip_ratio: 1.5,
            // [annot]
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
            badge_fill: DEF_BADGE.fill,
            badge_text: DEF_BADGE.text,
            badge_outline: DEF_BADGE.outline,
            highlight_color: HIGHLIGHT,
            highlight_keep: 0.55,
            spotlight_dim: 0.55,
            fade_min_alpha: 0.12,
            step_radius: 14,
            min_size: 3,
            pen_min_step: 2,
            attach_tol: 10,
            hit_tol: crate::annot::hit_test::HIT_TOL_LOGICAL,
            handle_hit: crate::annot::hit_test::HANDLE_HIT_LOGICAL,
            handle_size: crate::annot::hit_test::HANDLE_SIZE_LOGICAL,
            chrome_ext: 6,
            arrow_width_ratio: 3.5,
            arrow_len_min: 12,
            arrow_len_max: 48,
            text_font: "Microsoft YaHei UI".into(),
            burst_bumps: 7.0,
            burst_amp: 0.09,
            handle_fill: WHITE,
            handle_stroke: Rgba::new(0x55, 0x55, 0x55, 0xFF),
            chrome_color: Rgba::new(0x88, 0x88, 0x88, 0xFF),
            eraser_tol: 6,
            watermark_angle: -30,
            watermark_opacity: 64,
            watermark_tile: true,
            watermark_gap: 240,
            style_panel: true,
            style_palette: DEF_STYLE_PALETTE,
            // [pick]
            pick_level: crate::pick::PickLevel::Region,
            pick_padding: 0,
            pick_auto_expand: true,
            pick_probe_interval_ms: 40,
            pick_show_label: true,
            pick_max_depth: 12,
            // [assist]
            assist_magnifier: true,
            assist_magnifier_size: 121,
            assist_magnifier_zoom: 4,
            assist_aux_lines: true,
            assist_aux_grid: false,
            assist_show_info: true,
            assist_color_format: crate::assist::ColorFormat::HexRgb,
            assist_in_annot: false,
            // [ui]
            ui_toolbar_btn_size: 32,
            ui_toolbar_gap: 2,
            ui_toolbar_pad_v: 6,
            ui_toolbar_pad_h: 6,
            ui_toolbar_radius: 8,
            ui_tooltip_delay_ms: 450,
            ui_tooltip_pad_x: 10,
            ui_tooltip_pad_y: 6,
            ui_tooltip_gap: 8,
            ui_tooltip_max_width: 420,
            ui_overlay_dim: 0.5,
            ui_overlay_frame_color: Rgba::new(0xFF, 0x00, 0x00, 0xFF),
            ui_overlay_frame_width: 2,
            ui_overlay_label_px: 13,
            ui_overlay_label_color: WHITE,
            ui_overlay_min_drag: 3,
            ui_toast_pad_x: 16,
            ui_toast_pad_y: 9,
            ui_toast_gap: 18,
            ui_toast_radius: 8,
            ui_toast_font_pt: 10,
            ui_toast_bg: Rgba::new(0x2B, 0x2B, 0x2B, 0xFF),
            ui_toast_border: Rgba::new(0x45, 0x45, 0x45, 0xFF),
            ui_toast_text: Rgba::new(0xF5, 0xF5, 0xF5, 0xFF),
            ui_menu_item_h: 34,
            ui_menu_sep_h: 9,
            ui_menu_pad: 6,
            ui_menu_min_w: 72,
            ui_menu_right_pad: 44,
            ui_menu_radius: 12,
            ui_menu_hover_radius: 10,
            ui_menu_text_left: 18,
            ui_menu_text_right: 12,
            ui_menu_sep_inset: 12,
            ui_menu_font_pt: 10,
            ui_menu_accel_gap: 20,
            ui_menu_disabled_color: Rgba::new(0x80, 0x80, 0x80, 0xFF),
            ui_mica: true,
            // [pin]
            pin_min_scale: 0.05,
            pin_max_scale: 8.0,
            pin_shadow_margin: 16,
            pin_shadow_peak: 0.55,
            pin_zoom_step: 1.1,
            pin_fit_ratio: 0.85,
            pin_opacity: 255,
            pin_locked: false,
            pin_shadow: true,
            pin_topmost: true,
            pin_edge_hit: 6,
            pin_min_visible_px: 24,
            // [textimg]
            textimg_font_family: "Microsoft YaHei UI".into(),
            textimg_font_px: 20,
            textimg_margin: 16,
            textimg_max_width: 960,
            textimg_line_spacing: 1.4,
            textimg_fg: Rgba::new(0x1F, 0x1F, 0x1F, 0xFF),
            textimg_bg: WHITE,
            // [rtfimg]
            rtfimg_render_width: 1000,
            rtfimg_render_margin: 12,
            // [imageio]
            imageio_autosave_dir: None,
            imageio_prefix: "截图".into(),
            imageio_max_seq: 99,
            // [tray]
            tray_precapture_ttl_ms: 1000,
        }
    }
}

impl Config {
    /// 界面展示用：热键文本（原样，用户在配置文件里写什么就显示什么）。
    pub fn hotkey_label(&self) -> &str {
        &self.hotkey
    }

    /// 界面展示用：剪贴板贴图热键文本。
    pub fn pin_hotkey_label(&self) -> &str {
        &self.pin_hotkey
    }

    /// 标注默认偏好（注入 [`crate::annot::AnnotSession`]）。
    pub fn annot_prefs(&self) -> AnnotPrefs {
        AnnotPrefs {
            mosaic_mode: self.mosaic_mode,
            highlight_mode: self.highlight_mode,
            pen_kind: self.pen_kind,
            text_bold: self.text_bold,
            text_italic: self.text_italic,
            text_stroke: self.text_stroke,
            text_frame: self.text_frame,
            text_attach: self.text_attach,
            stroke_color: self.stroke_color,
            stroke_width: self.stroke_width,
            text_color: self.text_color,
            text_size: self.text_size,
            badge: BadgeStyle {
                fill: self.badge_fill,
                text: self.badge_text,
                outline: self.badge_outline,
            },
            highlight_color: self.highlight_color,
            highlight_keep: self.highlight_keep,
            spotlight_dim: self.spotlight_dim,
            fade_min_alpha: self.fade_min_alpha,
            step_radius: self.step_radius,
            min_size: self.min_size,
            pen_min_step: self.pen_min_step,
            attach_tol: self.attach_tol,
            hit_tol: self.hit_tol,
            handle_hit: self.handle_hit,
            handle_size: self.handle_size,
            chrome_ext: self.chrome_ext,
            arrow_width_ratio: self.arrow_width_ratio,
            arrow_len_min: self.arrow_len_min,
            arrow_len_max: self.arrow_len_max,
            eraser_tol: self.eraser_tol,
            watermark_angle: self.watermark_angle,
            watermark_opacity: self.watermark_opacity,
            watermark_tile: self.watermark_tile,
            watermark_gap: self.watermark_gap,
        }
    }
}

static CACHE: Mutex<Option<Config>> = Mutex::new(None);

/// 配置文件路径：与 PixShot.exe 同目录的 `config.ini`；
/// 取不到 exe 路径时（极少见）回退当前工作目录。
pub fn path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.ini")))
        .unwrap_or_else(|| PathBuf::from("config.ini"))
}

/// 取当前配置（首次调用时加载并缓存）。
pub fn get() -> Config {
    let mut g = lock();
    if g.is_none() {
        *g = Some(load());
    }
    g.clone().unwrap_or_default()
}

/// 更新配置并写盘（写失败只记日志，不影响功能）。
pub fn update<F: FnOnce(&mut Config)>(f: F) {
    let mut g = lock();
    let mut c = g.clone().unwrap_or_else(|| load());
    f(&mut c);
    if let Err(e) = write(&c) {
        crate::diag::log(&format!("[config] 写配置失败: {e}"));
    }
    *g = Some(c);
}

/// 强制从磁盘重新加载（丢弃缓存）。
/// 预留：配置文件被外部改动后热重载（当前在启动时一次性读取）。
#[allow(dead_code)]
pub fn reload() -> Config {
    let c = load();
    *lock() = Some(c.clone());
    c
}

fn lock() -> std::sync::MutexGuard<'static, Option<Config>> {
    // 配置只在 GUI 线程访问；即便发生 panic 中毒也照常使用旧值
    match CACHE.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn load() -> Config {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(text) => {
            // BOM 剥离在 parse_detailed 内统一处理
            let (c, warnings) = parse_detailed(&text);
            for w in &warnings {
                crate::diag::log(&format!("[config] {w}"));
            }
            crate::diag::log(&format!(
                "[config] 已加载 {} -> hotkey={} format={} save_dir={:?} capture_cursor={}",
                p.display(),
                c.hotkey,
                c.format.ext(),
                c.save_dir,
                c.capture_cursor
            ));
            c
        }
        Err(e) => {
            // 只有「文件不存在」才是首次运行——落一份带注释的模板。
            // 读取失败（非 UTF-8/GBK 编辑、磁盘错误、权限拒绝）**不得覆盖原文件**：
            // 备份留底后用默认值运行（用户改过的配置不应因编码问题被清盘）。
            if e.kind() == std::io::ErrorKind::NotFound {
                let c = Config::default();
                if let Err(e) = write(&c) {
                    crate::diag::log(&format!("[config] 创建默认配置失败: {e}"));
                } else {
                    crate::diag::log(&format!("[config] 已创建默认配置 {}", p.display()));
                }
                c
            } else {
                crate::diag::log(&format!(
                    "[config] 读取失败({e})，已备份为 config.ini.bak 并改用默认配置（原文件未覆盖）"
                ));
                if let Err(be) = std::fs::copy(&p, p.with_extension("ini.bak")) {
                    crate::diag::log(&format!("[config] 备份失败: {be}"));
                }
                Config::default()
            }
        }
    }
}

fn write(c: &Config) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, render(c))
}

/// 生成配置文件内容（带分节与说明注释）。
pub fn render(c: &Config) -> String {
    let b = |v: bool| if v { "1" } else { "0" };
    let mut s = String::new();
    s.push_str("# PixShot 配置文件（必须以 UTF-8 保存；用记事本编辑请选 UTF-8 编码）\n");
    s.push_str("# 分节（[section]）仅用于组织；键名在全文件内唯一。\n");
    s.push_str("# 修改后重启 PixShot 生效；删除本文件即恢复默认。\n");
    s.push_str("# 注意：程序内改动设置（如保存目录）会整体重写本文件，手写注释不保留；\n");
    s.push_str("# 请将本程序放在有写权限的目录（勿放 Program Files），否则配置无法保存。\n");

    s.push_str("\n[hotkey]\n");
    s.push_str("# 截图热键：F1..F24 / 字母 / 数字 / PrintScreen / Home / End / Enter / Esc / 方向键等，\n");
    s.push_str("# 可加修饰键 Ctrl、Alt、Shift、Win，用 + 连接。例：Ctrl+Shift+S\n");
    s.push_str("#（热键固定为按住不重复触发，当前不可配置）\n");
    s.push_str(&format!("hotkey = {}\n", c.hotkey));
    s.push_str("# 剪贴板贴图热键：把剪贴板内容（图片/文本/Excel/富文本）以图片置顶显示。\n");
    s.push_str(&format!("pin_hotkey = {}\n", c.pin_hotkey));

    s.push_str("\n[general]\n");
    s.push_str("# 默认保存格式：png / jpg / bmp\n");
    s.push_str(&format!("format = {}\n", c.format.ext()));
    s.push_str("# 上次保存目录（程序自动维护，也可手工指定）\n");
    s.push_str(&format!("save_dir = {}\n", c.save_dir.as_deref().unwrap_or("")));
    s.push_str("# 成功提示停留时长（毫秒，300-10000）\n");
    s.push_str(&format!("toast_ms = {}\n", c.toast_ms));
    s.push_str("# 截图是否包含鼠标指针：true / false\n");
    s.push_str(&format!("capture_cursor = {}\n", b(c.capture_cursor)));

    s.push_str("\n[ocr]\n");
    s.push_str("# OCR 首选语言（留空则按系统语言自动回退），例：zh-Hans-CN\n");
    s.push_str("# 注：内置 ONNX 引擎使用自带的中英模型，此项仅在回退到系统 OCR 时生效。\n");
    s.push_str(&format!("lang = {}\n", c.ocr_lang.as_deref().unwrap_or("")));
    s.push_str("# 版面：auto（自动）/ single（单行）/ multi（多行，段落空行分隔）\n");
    s.push_str(&format!("layout_type = {}\n", c.ocr_layout.label()));
    s.push_str("# 标点：none（不转换）/ full（半角→全角）/ half（全角→半角）\n");
    s.push_str(&format!("punctuation_type = {}\n", c.ocr_punctuation.label()));
    s.push_str("# 是否删除空格（保留换行）：0 / 1\n");
    s.push_str(&format!("remove_spaces = {}\n", b(c.ocr_remove_spaces)));
    s.push_str("# 行置信度下限 0.0-1.0（仅内置 ONNX 引擎生效，低于它的行不输出）\n");
    s.push_str(&format!("confidence = {:.2}\n", c.ocr_confidence));
    s.push_str("# 识别前放大目标长边（像素，提升小字识别率）\n");
    s.push_str(&format!("upsample_target = {}\n", c.ocr_upsample_target));
    s.push_str("# 放大倍数上限\n");
    s.push_str(&format!("max_upscale = {:.2}\n", c.ocr_max_upscale));
    s.push_str("# 深底判定的平均亮度阈值（0-255）\n");
    s.push_str(&format!("dark_luma = {:.1}\n", c.ocr_dark_luma));
    s.push_str("# 检测模型输入长边（越大越准越慢，对齐到 32 的倍数）\n");
    s.push_str(&format!("det_limit = {}\n", c.ocr_det_limit));
    s.push_str("# 文本区域置信度阈值\n");
    s.push_str(&format!("det_thresh = {:.2}\n", c.ocr_det_thresh));
    s.push_str("# 文本框分数阈值\n");
    s.push_str(&format!("box_thresh = {:.2}\n", c.ocr_box_thresh));
    s.push_str("# 文本框扩张系数\n");
    s.push_str(&format!("unclip_ratio = {:.2}\n", c.ocr_unclip_ratio));

    s.push_str("\n[annot]\n");
    s.push_str("# 马赛克模式：pixelate（块平均）/ blur（盒式模糊）\n");
    s.push_str(&format!("mosaic_mode = {}\n", c.mosaic_mode.label()));
    s.push_str("# 高亮模式：marker（荧光笔色块）/ spotlight（聚光灯：整图压暗 + 圈出亮区）\n");
    s.push_str(&format!("highlight_mode = {}\n", c.highlight_mode.label()));
    s.push_str("# 画笔形态：solid（硬笔）/ soft（软笔，按笔速渐细）/ fade（渐隐笔）\n");
    s.push_str(&format!("pen_mode = {}\n", c.pen_kind.label()));
    s.push_str("# 文字是否加粗 / 斜体 / 描边：0 / 1\n");
    s.push_str(&format!(
        "text_bold = {}\ntext_italic = {}\ntext_stroke = {}\n",
        b(c.text_bold),
        b(c.text_italic),
        b(c.text_stroke)
    ));
    s.push_str("# 文字外框：none（无）/ bubble（对话气泡）/ burst（爆炸云）\n");
    s.push_str(&format!("text_frame = {}\n", c.text_frame.label()));
    s.push_str("# 文字落点靠近线段/箭头时自动挂线（随线段移动）：0 / 1\n");
    s.push_str(&format!("text_attach = {}\n", b(c.text_attach)));
    s.push_str("# 颜色写法：RRGGBB 或 RRGGBBAA（十六进制，可带 #）。\n");
    s.push_str("# 默认描边色 / 线宽 / 文字色 / 字号\n");
    s.push_str(&format!("stroke_color = {}\n", fmt_color(c.stroke_color)));
    s.push_str(&format!("stroke_width = {}\n", c.stroke_width));
    s.push_str(&format!("text_color = {}\n", fmt_color(c.text_color)));
    s.push_str(&format!("text_size = {}\n", c.text_size));
    s.push_str("# 步骤序号徽章：底色 / 文字色 / 外圈色（空 = 无外圈）\n");
    s.push_str(&format!("badge_fill = {}\n", fmt_color(c.badge_fill)));
    s.push_str(&format!("badge_text = {}\n", fmt_color(c.badge_text)));
    s.push_str(&format!(
        "badge_outline = {}\n",
        c.badge_outline.map(fmt_color).unwrap_or_default()
    ));
    s.push_str("# 高亮色及混合系数（0-1）/ 聚光灯保留系数 / 渐隐笔起点透明度\n");
    s.push_str(&format!("highlight_color = {}\n", fmt_color(c.highlight_color)));
    s.push_str(&format!("highlight_keep = {:.2}\n", c.highlight_keep));
    s.push_str(&format!("spotlight_dim = {:.2}\n", c.spotlight_dim));
    s.push_str(&format!("fade_min_alpha = {:.2}\n", c.fade_min_alpha));
    s.push_str("# 交互尺寸（逻辑像素）：序号半径 / 最小尺寸 / 画笔步长 / 挂线容差\n");
    s.push_str(&format!("step_radius = {}\n", c.step_radius));
    s.push_str(&format!("min_size = {}\n", c.min_size));
    s.push_str(&format!("pen_min_step = {}\n", c.pen_min_step));
    s.push_str(&format!("attach_tol = {}\n", c.attach_tol));
    s.push_str("# 命中容差 / 控制柄命中半径 / 控制柄边长 / 选中外扩\n");
    s.push_str(&format!("hit_tol = {}\n", c.hit_tol));
    s.push_str(&format!("handle_hit = {}\n", c.handle_hit));
    s.push_str(&format!("handle_size = {}\n", c.handle_size));
    s.push_str(&format!("chrome_ext = {}\n", c.chrome_ext));
    s.push_str("# 箭头头部：长度/线宽比 / 长度下限 / 长度上限\n");
    s.push_str(&format!("arrow_width_ratio = {:.2}\n", c.arrow_width_ratio));
    s.push_str(&format!("arrow_len_min = {}\n", c.arrow_len_min));
    s.push_str(&format!("arrow_len_max = {}\n", c.arrow_len_max));
    s.push_str("# 文字字体族 / 爆炸云波峰数 / 波幅比\n");
    s.push_str(&format!("text_font = {}\n", c.text_font));
    s.push_str(&format!("burst_bumps = {:.1}\n", c.burst_bumps));
    s.push_str(&format!("burst_amp = {:.3}\n", c.burst_amp));
    s.push_str("# 控制柄填充/描边色、选区虚线框色\n");
    s.push_str(&format!("handle_fill = {}\n", fmt_color(c.handle_fill)));
    s.push_str(&format!("handle_stroke = {}\n", fmt_color(c.handle_stroke)));
    s.push_str(&format!("chrome_color = {}\n", fmt_color(c.chrome_color)));
    s.push_str("# 橡皮命中容差（逻辑像素）/ 水印角度 / 透明度(0-255) / 平铺 / 平铺间距\n");
    s.push_str(&format!("eraser_tol = {}\n", c.eraser_tol));
    s.push_str(&format!("watermark_angle = {}\n", c.watermark_angle));
    s.push_str(&format!("watermark_opacity = {}\n", c.watermark_opacity));
    s.push_str(&format!("watermark_tile = {}\n", b(c.watermark_tile)));
    s.push_str(&format!("watermark_gap = {}\n", c.watermark_gap));
    s.push_str("# 颜色面板开关 / 预设色（逗号分隔，最多 8 个 RRGGBB）\n");
    s.push_str(&format!("style_panel = {}\n", b(c.style_panel)));
    s.push_str(&format!(
        "style_panel_palette = {}\n",
        c.style_palette
            .iter()
            .map(|x| fmt_color(*x))
            .collect::<Vec<_>>()
            .join(",")
    ));

    s.push_str("\n[pick]\n");
    s.push_str("# 智能选区：none / region（单击自动扩充）/ window / element\n");
    s.push_str(&format!("pick_level = {}\n", c.pick_level.label()));
    s.push_str("# 候选框外扩（逻辑像素）/ 客户区自动扩充为窗口矩形 / 显示标签\n");
    s.push_str(&format!("pick_padding = {}\n", c.pick_padding));
    s.push_str(&format!("pick_auto_expand = {}\n", b(c.pick_auto_expand)));
    s.push_str(&format!("pick_show_label = {}\n", b(c.pick_show_label)));
    s.push_str("# 探测节流（毫秒，>=16）/ 元素级最大深度\n");
    s.push_str(&format!("pick_probe_interval_ms = {}\n", c.pick_probe_interval_ms));
    s.push_str(&format!("pick_max_depth = {}\n", c.pick_max_depth));

    s.push_str("\n[assist]\n");
    s.push_str("# 放大镜开关 / 边长（逻辑像素）/ 放大倍数\n");
    s.push_str(&format!("assist_magnifier = {}\n", b(c.assist_magnifier)));
    s.push_str(&format!("assist_magnifier_size = {}\n", c.assist_magnifier_size));
    s.push_str(&format!("assist_magnifier_zoom = {}\n", c.assist_magnifier_zoom));
    s.push_str("# 辅助线开关 / 选区内九宫格 / 信息条\n");
    s.push_str(&format!("assist_aux_lines = {}\n", b(c.assist_aux_lines)));
    s.push_str(&format!("assist_aux_grid = {}\n", b(c.assist_aux_grid)));
    s.push_str(&format!("assist_show_info = {}\n", b(c.assist_show_info)));
    s.push_str("# 取色格式：hex_rgb / hex_rgba / rgb / hsl / hsv\n");
    s.push_str(&format!("assist_color_format = {}\n", c.assist_color_format.label()));
    s.push_str("# 标注阶段是否保留辅助层\n");
    s.push_str(&format!("assist_in_annot = {}\n", b(c.assist_in_annot)));

    s.push_str("\n[ui]\n");
    s.push_str("# 工具栏（逻辑像素）：按钮边长 / 间距 / 上下内边距 / 左右内边距 / 圆角\n");
    s.push_str(&format!("toolbar_btn_size = {}\n", c.ui_toolbar_btn_size));
    s.push_str(&format!("toolbar_gap = {}\n", c.ui_toolbar_gap));
    s.push_str(&format!("toolbar_pad_v = {}\n", c.ui_toolbar_pad_v));
    s.push_str(&format!("toolbar_pad_h = {}\n", c.ui_toolbar_pad_h));
    s.push_str(&format!("toolbar_radius = {}\n", c.ui_toolbar_radius));
    s.push_str("# 悬停提示：延时(ms) / 水平内边距 / 垂直内边距 / 与按钮间距 / 最大宽度\n");
    s.push_str(&format!("tooltip_delay_ms = {}\n", c.ui_tooltip_delay_ms));
    s.push_str(&format!("tooltip_pad_x = {}\n", c.ui_tooltip_pad_x));
    s.push_str(&format!("tooltip_pad_y = {}\n", c.ui_tooltip_pad_y));
    s.push_str(&format!("tooltip_gap = {}\n", c.ui_tooltip_gap));
    s.push_str(&format!("tooltip_max_width = {}\n", c.ui_tooltip_max_width));
    s.push_str("# 选区遮罩：暗化系数(0-1) / 框线色 / 框线粗细 / 尺寸标签字号 / 最小拖动边长\n");
    s.push_str(&format!("overlay_dim = {:.2}\n", c.ui_overlay_dim));
    s.push_str(&format!("overlay_frame_color = {}\n", fmt_color(c.ui_overlay_frame_color)));
    s.push_str(&format!("overlay_frame_width = {}\n", c.ui_overlay_frame_width));
    s.push_str(&format!("overlay_label_px = {}\n", c.ui_overlay_label_px));
    s.push_str(&format!("overlay_label_color = {}\n", fmt_color(c.ui_overlay_label_color)));
    s.push_str(&format!("overlay_min_drag = {}\n", c.ui_overlay_min_drag));
    s.push_str("# 轻提示（Toast）：内边距 / 光标间距 / 圆角 / 字号 / 底色 / 描边 / 文字色\n");
    s.push_str(&format!("toast_pad_x = {}\n", c.ui_toast_pad_x));
    s.push_str(&format!("toast_pad_y = {}\n", c.ui_toast_pad_y));
    s.push_str(&format!("toast_gap = {}\n", c.ui_toast_gap));
    s.push_str(&format!("toast_radius = {}\n", c.ui_toast_radius));
    s.push_str(&format!("toast_font_pt = {}\n", c.ui_toast_font_pt));
    s.push_str(&format!("toast_bg = {}\n", fmt_color(c.ui_toast_bg)));
    s.push_str(&format!("toast_border = {}\n", fmt_color(c.ui_toast_border)));
    s.push_str(&format!("toast_text = {}\n", fmt_color(c.ui_toast_text)));
    s.push_str("# 右键菜单（逻辑像素）：项高 / 分隔线高 / 内边距 / 最小宽 / 右侧留白 / 圆角\n");
    s.push_str(&format!("menu_item_h = {}\n", c.ui_menu_item_h));
    s.push_str(&format!("menu_sep_h = {}\n", c.ui_menu_sep_h));
    s.push_str(&format!("menu_pad = {}\n", c.ui_menu_pad));
    s.push_str(&format!("menu_min_w = {}\n", c.ui_menu_min_w));
    s.push_str(&format!("menu_right_pad = {}\n", c.ui_menu_right_pad));
    s.push_str(&format!("menu_radius = {}\n", c.ui_menu_radius));
    s.push_str(&format!("menu_hover_radius = {}\n", c.ui_menu_hover_radius));
    s.push_str(&format!("menu_text_left = {}\n", c.ui_menu_text_left));
    s.push_str(&format!("menu_text_right = {}\n", c.ui_menu_text_right));
    s.push_str(&format!("menu_sep_inset = {}\n", c.ui_menu_sep_inset));
    s.push_str(&format!("menu_font_pt = {}\n", c.ui_menu_font_pt));
    s.push_str(&format!("menu_accel_gap = {}\n", c.ui_menu_accel_gap));
    s.push_str(&format!("menu_disabled_color = {}\n", fmt_color(c.ui_menu_disabled_color)));
    s.push_str("# 是否启用 Mica 背景：0 / 1\n");
    s.push_str(&format!("mica = {}\n", b(c.ui_mica)));

    s.push_str("\n[pin]\n");
    s.push_str("# 贴图：最小/最大缩放（0.05-8）/ 阴影留白 / 阴影峰值透明度(0-1)\n");
    s.push_str("# 滚轮缩放步进 / 首次贴合屏幕比例(0-1)\n");
    s.push_str(&format!("min_scale = {:.2}\n", c.pin_min_scale));
    s.push_str(&format!("max_scale = {:.2}\n", c.pin_max_scale));
    s.push_str(&format!("shadow_margin = {}\n", c.pin_shadow_margin));
    s.push_str(&format!("shadow_peak = {:.2}\n", c.pin_shadow_peak));
    s.push_str(&format!("zoom_step = {:.2}\n", c.pin_zoom_step));
    s.push_str(&format!("fit_ratio = {:.2}\n", c.pin_fit_ratio));
    s.push_str("# 新贴图默认：不透明度(26-255，即 10%-100%) / 锁定 / 阴影 / 置顶\n");
    s.push_str(&format!("opacity = {}\n", c.pin_opacity));
    s.push_str(&format!("locked = {}\n", b(c.pin_locked)));
    s.push_str(&format!("shadow = {}\n", b(c.pin_shadow)));
    s.push_str(&format!("topmost = {}\n", b(c.pin_topmost)));
    s.push_str("# 边缘缩放热区（逻辑像素）/ 缩放后至少保留的可见像素\n");
    s.push_str(&format!("edge_hit = {}\n", c.pin_edge_hit));
    s.push_str(&format!("min_visible_px = {}\n", c.pin_min_visible_px));

    s.push_str("\n[textimg]\n");
    s.push_str("# 文本贴图渲染：字体族 / 字号(px) / 页边距 / 最大宽度 / 行距倍数 / 前景色 / 背景色\n");
    s.push_str(&format!("font_family = {}\n", c.textimg_font_family));
    s.push_str(&format!("font_px = {}\n", c.textimg_font_px));
    s.push_str(&format!("margin = {}\n", c.textimg_margin));
    s.push_str(&format!("max_width = {}\n", c.textimg_max_width));
    s.push_str(&format!("line_spacing = {:.2}\n", c.textimg_line_spacing));
    s.push_str(&format!("fg = {}\n", fmt_color(c.textimg_fg)));
    s.push_str(&format!("bg = {}\n", fmt_color(c.textimg_bg)));

    s.push_str("\n[rtfimg]\n");
    s.push_str("# 富文本贴图渲染宽度（像素）/ 渲染后裁边留白\n");
    s.push_str(&format!("render_width = {}\n", c.rtfimg_render_width));
    s.push_str(&format!("render_margin = {}\n", c.rtfimg_render_margin));

    s.push_str("\n[imageio]\n");
    s.push_str("# 自动保存目录（空 = %USERPROFILE%\\Pictures\\PixShot）/ 文件名前缀 / 序号上限\n");
    s.push_str(&format!(
        "autosave_dir = {}\n",
        c.imageio_autosave_dir.as_deref().unwrap_or("")
    ));
    s.push_str(&format!("prefix = {}\n", c.imageio_prefix));
    s.push_str(&format!("max_seq = {}\n", c.imageio_max_seq));

    s.push_str("\n[tray]\n");
    s.push_str("# 热键预捕获快照有效期（毫秒）\n");
    s.push_str(&format!("precapture_ttl_ms = {}\n", c.tray_precapture_ttl_ms));
    s
}

/// 解析配置文本（纯函数，可单测）。任何非法项都静默回退默认值。
/// 只取解析结果、丢弃告警的简化入口（供单测使用）。
#[allow(dead_code)]
pub fn parse(text: &str) -> Config {
    parse_detailed(text).0
}

/// 解析并返回被忽略的非法项说明（供日志输出；解析本身无副作用，便于单测）。
///
/// 支持 `[section]` 标题（仅组织用，键按叶子名匹配）、`#`/`;` 注释、
/// 以及旧版扁平键名（别名）。
pub fn parse_detailed(text: &str) -> (Config, Vec<String>) {
    // 剥 UTF-8 BOM（部分编辑器保存时附加，否则首行键名被污染而静默忽略）
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut c = Config::default();
    let mut warnings: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        // 分节标题 [section]：仅用于组织，解析按叶子键名匹配（键名全局唯一）
        if line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        // 兼容 `section.key = value` 写法：取最后一段
        let key = key.rsplit('.').next().unwrap_or("").to_string();
        let val = v.trim();
        match key.as_str() {
            // [hotkey]
            "hotkey" => {
                if parse_hotkey(val).is_some() {
                    c.hotkey = val.to_string();
                } else {
                    warnings.push(format!("无法识别的热键 {val:?}，沿用默认 F1"));
                }
            }
            "pin_hotkey" => {
                if parse_hotkey(val).is_some() {
                    c.pin_hotkey = val.to_string();
                } else {
                    warnings.push(format!("无法识别的剪贴板贴图热键 {val:?}，沿用默认 F3"));
                }
            }
            // [general]
            "format" => match SaveFormat::parse(val) {
                Some(f) => c.format = f,
                None => warnings.push(format!("无法识别的保存格式 {val:?}，沿用默认 png")),
            },
            "save_dir" => {
                if !val.is_empty() {
                    c.save_dir = Some(val.to_string());
                }
            }
            "toast_ms" => match parse_u32(val, 300, 10_000) {
                Some(n) => c.toast_ms = n,
                None => warnings.push(format!("无法识别的 toast_ms {val:?}，沿用默认 1600")),
            },
            "capture_cursor" => c.capture_cursor = parse_bool(val).unwrap_or(false),
            // [ocr]
            "lang" | "ocr_lang" => {
                if !val.is_empty() {
                    c.ocr_lang = Some(val.to_string());
                }
            }
            "layout_type" | "ocr_layout_type" => match OcrLayout::parse(val) {
                Some(v) => c.ocr_layout = v,
                None => warnings.push(format!("无法识别的版面类型 {val:?}，沿用默认 auto")),
            },
            "punctuation_type" | "ocr_punctuation_type" => match OcrPunctuation::parse(val) {
                Some(v) => c.ocr_punctuation = v,
                None => warnings.push(format!("无法识别的标点类型 {val:?}，沿用默认 none")),
            },
            "remove_spaces" | "ocr_remove_spaces" => match parse_bool(val) {
                Some(v) => c.ocr_remove_spaces = v,
                None => warnings.push(format!("无法识别的 remove_spaces {val:?}，沿用默认 0")),
            },
            "confidence" | "ocr_confidence" => match parse_f32(val, 0.0, 1.0) {
                Some(v) => c.ocr_confidence = v,
                None => warnings.push(format!("无法识别的置信度阈值 {val:?}，沿用默认 0")),
            },
            "upsample_target" => match parse_i32(val, 0, 20_000) {
                Some(v) => c.ocr_upsample_target = v,
                None => warnings.push(format!("无法识别的 upsample_target {val:?}，沿用默认 2400")),
            },
            "max_upscale" => match parse_f32(val, 1.0, 10.0) {
                Some(v) => c.ocr_max_upscale = v,
                None => warnings.push(format!("无法识别的 max_upscale {val:?}，沿用默认 3.0")),
            },
            "dark_luma" => match parse_f32(val, 0.0, 255.0) {
                Some(v) => c.ocr_dark_luma = v,
                None => warnings.push(format!("无法识别的 dark_luma {val:?}，沿用默认 110")),
            },
            "det_limit" => match parse_u32(val, 32, 20_000) {
                Some(v) => c.ocr_det_limit = v as usize,
                None => warnings.push(format!("无法识别的 det_limit {val:?}，沿用默认 960")),
            },
            "det_thresh" => match parse_f32(val, 0.0, 1.0) {
                Some(v) => c.ocr_det_thresh = v,
                None => warnings.push(format!("无法识别的 det_thresh {val:?}，沿用默认 0.3")),
            },
            "box_thresh" => match parse_f32(val, 0.0, 1.0) {
                Some(v) => c.ocr_box_thresh = v,
                None => warnings.push(format!("无法识别的 box_thresh {val:?}，沿用默认 0.6")),
            },
            "unclip_ratio" => match parse_f32(val, 0.1, 10.0) {
                Some(v) => c.ocr_unclip_ratio = v,
                None => warnings.push(format!("无法识别的 unclip_ratio {val:?}，沿用默认 1.5")),
            },
            // [annot]
            "mosaic_mode" => match MosaicMode::parse(val) {
                Some(v) => c.mosaic_mode = v,
                None => warnings.push(format!("无法识别的 mosaic_mode {val:?}，沿用默认 pixelate")),
            },
            "highlight_mode" => match HighlightMode::parse(val) {
                Some(v) => c.highlight_mode = v,
                None => warnings.push(format!("无法识别的 highlight_mode {val:?}，沿用默认 marker")),
            },
            "pen_mode" => match PenKind::parse(val) {
                Some(v) => c.pen_kind = v,
                None => warnings.push(format!("无法识别的 pen_mode {val:?}，沿用默认 solid")),
            },
            "text_frame" => match TextFrame::parse(val) {
                Some(v) => c.text_frame = v,
                None => warnings.push(format!("无法识别的 text_frame {val:?}，沿用默认 none")),
            },
            "text_bold" => c.text_bold = parse_bool(val).unwrap_or(false),
            "text_italic" => c.text_italic = parse_bool(val).unwrap_or(false),
            "text_stroke" => c.text_stroke = parse_bool(val).unwrap_or(false),
            "text_attach" => c.text_attach = parse_bool(val).unwrap_or(true),
            "stroke_color" => c.stroke_color = parse_color_or(val, c.stroke_color, "stroke_color", &mut warnings),
            "text_color" => c.text_color = parse_color_or(val, c.text_color, "text_color", &mut warnings),
            "badge_fill" => c.badge_fill = parse_color_or(val, c.badge_fill, "badge_fill", &mut warnings),
            "badge_text" => c.badge_text = parse_color_or(val, c.badge_text, "badge_text", &mut warnings),
            "highlight_color" => {
                c.highlight_color = parse_color_or(val, c.highlight_color, "highlight_color", &mut warnings)
            }
            "handle_fill" => c.handle_fill = parse_color_or(val, c.handle_fill, "handle_fill", &mut warnings),
            "handle_stroke" => {
                c.handle_stroke = parse_color_or(val, c.handle_stroke, "handle_stroke", &mut warnings)
            }
            "chrome_color" => c.chrome_color = parse_color_or(val, c.chrome_color, "chrome_color", &mut warnings),
            "eraser_tol" => c.eraser_tol = parse_i32_or(val, 0, 200, c.eraser_tol, "eraser_tol", &mut warnings),
            "watermark_angle" => c.watermark_angle = parse_i32_or(val, -90, 90, c.watermark_angle, "watermark_angle", &mut warnings),
            "watermark_opacity" => c.watermark_opacity = parse_i32_or(val, 0, 255, c.watermark_opacity as i32, "watermark_opacity", &mut warnings) as u8,
            "watermark_tile" => c.watermark_tile = parse_bool(val).unwrap_or(true),
            "watermark_gap" => c.watermark_gap = parse_i32_or(val, 80, 2000, c.watermark_gap, "watermark_gap", &mut warnings),
            "style_panel" => c.style_panel = parse_bool(val).unwrap_or(true),
            "style_panel_palette" => {
                for (i, item) in val.split(',').take(8).enumerate() {
                    if let Some(col) = parse_color(item) {
                        c.style_palette[i] = col;
                    }
                }
            }
            // [pick]
            "pick_level" => match crate::pick::PickLevel::parse(val) {
                Some(v) => c.pick_level = v,
                None => warnings.push(format!("无法识别的 pick_level {val:?}，沿用默认 region")),
            },
            "pick_padding" => c.pick_padding = parse_i32_or(val, 0, 200, c.pick_padding, "pick_padding", &mut warnings),
            "pick_auto_expand" => c.pick_auto_expand = parse_bool(val).unwrap_or(true),
            "pick_probe_interval_ms" => c.pick_probe_interval_ms = parse_u32_or(val, 16, 2000, c.pick_probe_interval_ms, "pick_probe_interval_ms", &mut warnings),
            "pick_show_label" => c.pick_show_label = parse_bool(val).unwrap_or(true),
            "pick_max_depth" => c.pick_max_depth = parse_u32_or(val, 1, 64, c.pick_max_depth, "pick_max_depth", &mut warnings),
            // [assist]
            "assist_magnifier" => c.assist_magnifier = parse_bool(val).unwrap_or(true),
            "assist_magnifier_size" => c.assist_magnifier_size = parse_i32_or(val, 24, 600, c.assist_magnifier_size, "assist_magnifier_size", &mut warnings),
            "assist_magnifier_zoom" => c.assist_magnifier_zoom = parse_i32_or(val, 1, 16, c.assist_magnifier_zoom, "assist_magnifier_zoom", &mut warnings),
            "assist_aux_lines" => c.assist_aux_lines = parse_bool(val).unwrap_or(true),
            "assist_aux_grid" => c.assist_aux_grid = parse_bool(val).unwrap_or(false),
            "assist_show_info" => c.assist_show_info = parse_bool(val).unwrap_or(true),
            "assist_color_format" => match crate::assist::ColorFormat::parse(val) {
                Some(v) => c.assist_color_format = v,
                None => warnings.push(format!("无法识别的 assist_color_format {val:?}，沿用默认 hex_rgb")),
            },
            "assist_in_annot" => c.assist_in_annot = parse_bool(val).unwrap_or(false),
            // badge_outline：空 = 无外圈
            "badge_outline" => {
                if val.is_empty() {
                    c.badge_outline = None;
                } else {
                    match parse_color(val) {
                        Some(v) => c.badge_outline = Some(v),
                        None => warnings.push(format!("无法识别的 badge_outline {val:?}，沿用默认白色")),
                    }
                }
            }
            "stroke_width" => c.stroke_width = parse_i32_or(val, 1, 200, c.stroke_width, "stroke_width", &mut warnings),
            "text_size" => c.text_size = parse_i32_or(val, 4, 400, c.text_size, "text_size", &mut warnings),
            "step_radius" => c.step_radius = parse_i32_or(val, 2, 200, c.step_radius, "step_radius", &mut warnings),
            "min_size" => c.min_size = parse_i32_or(val, 1, 200, c.min_size, "min_size", &mut warnings),
            "pen_min_step" => c.pen_min_step = parse_i32_or(val, 1, 100, c.pen_min_step, "pen_min_step", &mut warnings),
            "attach_tol" => c.attach_tol = parse_i32_or(val, 0, 200, c.attach_tol, "attach_tol", &mut warnings),
            "hit_tol" => c.hit_tol = parse_i32_or(val, 0, 200, c.hit_tol, "hit_tol", &mut warnings),
            "handle_hit" => c.handle_hit = parse_i32_or(val, 1, 200, c.handle_hit, "handle_hit", &mut warnings),
            "handle_size" => c.handle_size = parse_i32_or(val, 1, 200, c.handle_size, "handle_size", &mut warnings),
            "chrome_ext" => c.chrome_ext = parse_i32_or(val, 0, 200, c.chrome_ext, "chrome_ext", &mut warnings),
            "arrow_len_min" => c.arrow_len_min = parse_i32_or(val, 1, 500, c.arrow_len_min, "arrow_len_min", &mut warnings),
            "arrow_len_max" => c.arrow_len_max = parse_i32_or(val, 1, 1000, c.arrow_len_max, "arrow_len_max", &mut warnings),
            "highlight_keep" => c.highlight_keep = parse_f32_or(val, 0.0, 1.0, c.highlight_keep, "highlight_keep", &mut warnings),
            "spotlight_dim" => c.spotlight_dim = parse_f32_or(val, 0.0, 1.0, c.spotlight_dim, "spotlight_dim", &mut warnings),
            "fade_min_alpha" => c.fade_min_alpha = parse_f32_or(val, 0.0, 1.0, c.fade_min_alpha, "fade_min_alpha", &mut warnings),
            "arrow_width_ratio" => c.arrow_width_ratio = parse_f32_or(val, 1.0, 20.0, c.arrow_width_ratio, "arrow_width_ratio", &mut warnings),
            "burst_bumps" => c.burst_bumps = parse_f32_or(val, 1.0, 60.0, c.burst_bumps, "burst_bumps", &mut warnings),
            "burst_amp" => c.burst_amp = parse_f32_or(val, 0.0, 0.5, c.burst_amp, "burst_amp", &mut warnings),
            "text_font" => {
                if !val.is_empty() {
                    c.text_font = val.to_string();
                }
            }
            // [ui]
            "toolbar_btn_size" => c.ui_toolbar_btn_size = parse_i32_or(val, 8, 200, c.ui_toolbar_btn_size, "toolbar_btn_size", &mut warnings),
            "toolbar_gap" => c.ui_toolbar_gap = parse_i32_or(val, 0, 100, c.ui_toolbar_gap, "toolbar_gap", &mut warnings),
            "toolbar_pad_v" => c.ui_toolbar_pad_v = parse_i32_or(val, 0, 100, c.ui_toolbar_pad_v, "toolbar_pad_v", &mut warnings),
            "toolbar_pad_h" => c.ui_toolbar_pad_h = parse_i32_or(val, 0, 100, c.ui_toolbar_pad_h, "toolbar_pad_h", &mut warnings),
            "toolbar_radius" => c.ui_toolbar_radius = parse_i32_or(val, 0, 100, c.ui_toolbar_radius, "toolbar_radius", &mut warnings),
            "tooltip_delay_ms" => c.ui_tooltip_delay_ms = parse_u32_or(val, 0, 10_000, c.ui_tooltip_delay_ms, "tooltip_delay_ms", &mut warnings),
            "tooltip_pad_x" => c.ui_tooltip_pad_x = parse_i32_or(val, 0, 200, c.ui_tooltip_pad_x, "tooltip_pad_x", &mut warnings),
            "tooltip_pad_y" => c.ui_tooltip_pad_y = parse_i32_or(val, 0, 200, c.ui_tooltip_pad_y, "tooltip_pad_y", &mut warnings),
            "tooltip_gap" => c.ui_tooltip_gap = parse_i32_or(val, 0, 200, c.ui_tooltip_gap, "tooltip_gap", &mut warnings),
            "tooltip_max_width" => c.ui_tooltip_max_width = parse_i32_or(val, 40, 4000, c.ui_tooltip_max_width, "tooltip_max_width", &mut warnings),
            "overlay_dim" => c.ui_overlay_dim = parse_f32_or(val, 0.0, 1.0, c.ui_overlay_dim, "overlay_dim", &mut warnings),
            "overlay_frame_width" => c.ui_overlay_frame_width = parse_i32_or(val, 1, 50, c.ui_overlay_frame_width, "overlay_frame_width", &mut warnings),
            "overlay_label_px" => c.ui_overlay_label_px = parse_i32_or(val, 6, 100, c.ui_overlay_label_px, "overlay_label_px", &mut warnings),
            "overlay_label_color" => c.ui_overlay_label_color = parse_color_or(val, c.ui_overlay_label_color, "overlay_label_color", &mut warnings),
            "overlay_min_drag" => c.ui_overlay_min_drag = parse_i32_or(val, 0, 200, c.ui_overlay_min_drag, "overlay_min_drag", &mut warnings),
            "overlay_frame_color" => c.ui_overlay_frame_color = parse_color_or(val, c.ui_overlay_frame_color, "overlay_frame_color", &mut warnings),
            "toast_pad_x" => c.ui_toast_pad_x = parse_i32_or(val, 0, 200, c.ui_toast_pad_x, "toast_pad_x", &mut warnings),
            "toast_pad_y" => c.ui_toast_pad_y = parse_i32_or(val, 0, 200, c.ui_toast_pad_y, "toast_pad_y", &mut warnings),
            "toast_gap" => c.ui_toast_gap = parse_i32_or(val, 0, 200, c.ui_toast_gap, "toast_gap", &mut warnings),
            "toast_radius" => c.ui_toast_radius = parse_i32_or(val, 0, 100, c.ui_toast_radius, "toast_radius", &mut warnings),
            "toast_font_pt" => c.ui_toast_font_pt = parse_i32_or(val, 6, 72, c.ui_toast_font_pt, "toast_font_pt", &mut warnings),
            "toast_bg" => c.ui_toast_bg = parse_color_or(val, c.ui_toast_bg, "toast_bg", &mut warnings),
            "toast_border" => c.ui_toast_border = parse_color_or(val, c.ui_toast_border, "toast_border", &mut warnings),
            "toast_text" => c.ui_toast_text = parse_color_or(val, c.ui_toast_text, "toast_text", &mut warnings),
            "menu_item_h" => c.ui_menu_item_h = parse_i32_or(val, 8, 200, c.ui_menu_item_h, "menu_item_h", &mut warnings),
            "menu_sep_h" => c.ui_menu_sep_h = parse_i32_or(val, 0, 100, c.ui_menu_sep_h, "menu_sep_h", &mut warnings),
            "menu_pad" => c.ui_menu_pad = parse_i32_or(val, 0, 100, c.ui_menu_pad, "menu_pad", &mut warnings),
            "menu_min_w" => c.ui_menu_min_w = parse_i32_or(val, 20, 2000, c.ui_menu_min_w, "menu_min_w", &mut warnings),
            "menu_right_pad" => c.ui_menu_right_pad = parse_i32_or(val, 0, 500, c.ui_menu_right_pad, "menu_right_pad", &mut warnings),
            "menu_radius" => c.ui_menu_radius = parse_i32_or(val, 0, 100, c.ui_menu_radius, "menu_radius", &mut warnings),
            "menu_hover_radius" => c.ui_menu_hover_radius = parse_i32_or(val, 0, 100, c.ui_menu_hover_radius, "menu_hover_radius", &mut warnings),
            "menu_text_left" => c.ui_menu_text_left = parse_i32_or(val, 0, 200, c.ui_menu_text_left, "menu_text_left", &mut warnings),
            "menu_text_right" => c.ui_menu_text_right = parse_i32_or(val, 0, 200, c.ui_menu_text_right, "menu_text_right", &mut warnings),
            "menu_sep_inset" => c.ui_menu_sep_inset = parse_i32_or(val, 0, 200, c.ui_menu_sep_inset, "menu_sep_inset", &mut warnings),
            "menu_font_pt" => c.ui_menu_font_pt = parse_i32_or(val, 6, 72, c.ui_menu_font_pt, "menu_font_pt", &mut warnings),
            "menu_accel_gap" => c.ui_menu_accel_gap = parse_i32_or(val, 0, 200, c.ui_menu_accel_gap, "menu_accel_gap", &mut warnings),
            "menu_disabled_color" => c.ui_menu_disabled_color = parse_color_or(val, c.ui_menu_disabled_color, "menu_disabled_color", &mut warnings),
            "mica" => c.ui_mica = parse_bool(val).unwrap_or(true),
            // [pin]
            "min_scale" => c.pin_min_scale = parse_f32_or(val, 0.01, 1.0, c.pin_min_scale, "min_scale", &mut warnings),
            "max_scale" => c.pin_max_scale = parse_f32_or(val, 1.0, 100.0, c.pin_max_scale, "max_scale", &mut warnings),
            "shadow_margin" => c.pin_shadow_margin = parse_i32_or(val, 0, 200, c.pin_shadow_margin, "shadow_margin", &mut warnings),
            "shadow_peak" => c.pin_shadow_peak = parse_f32_or(val, 0.0, 1.0, c.pin_shadow_peak, "shadow_peak", &mut warnings),
            "zoom_step" => c.pin_zoom_step = parse_f32_or(val, 1.001, 5.0, c.pin_zoom_step, "zoom_step", &mut warnings),
            "fit_ratio" => c.pin_fit_ratio = parse_f32_or(val, 0.05, 1.0, c.pin_fit_ratio, "fit_ratio", &mut warnings),
            "opacity" => c.pin_opacity = parse_i32_or(val, 26, 255, c.pin_opacity as i32, "opacity", &mut warnings) as u8,
            "locked" => c.pin_locked = parse_bool(val).unwrap_or(false),
            "shadow" => c.pin_shadow = parse_bool(val).unwrap_or(true),
            "topmost" => c.pin_topmost = parse_bool(val).unwrap_or(true),
            "edge_hit" => c.pin_edge_hit = parse_i32_or(val, 2, 40, c.pin_edge_hit, "edge_hit", &mut warnings),
            "min_visible_px" => c.pin_min_visible_px = parse_i32_or(val, 8, 200, c.pin_min_visible_px, "min_visible_px", &mut warnings),
            // [textimg]
            "font_family" => {
                if !val.is_empty() {
                    c.textimg_font_family = val.to_string();
                }
            }
            "font_px" => c.textimg_font_px = parse_i32_or(val, 4, 400, c.textimg_font_px, "font_px", &mut warnings),
            "margin" => c.textimg_margin = parse_i32_or(val, 0, 400, c.textimg_margin, "margin", &mut warnings),
            "max_width" => c.textimg_max_width = parse_i32_or(val, 40, 20_000, c.textimg_max_width, "max_width", &mut warnings),
            "line_spacing" => c.textimg_line_spacing = parse_f32_or(val, 0.8, 4.0, c.textimg_line_spacing, "line_spacing", &mut warnings),
            "fg" => c.textimg_fg = parse_color_or(val, c.textimg_fg, "fg", &mut warnings),
            "bg" => c.textimg_bg = parse_color_or(val, c.textimg_bg, "bg", &mut warnings),
            // [rtfimg]
            "render_width" => c.rtfimg_render_width = parse_i32_or(val, 100, 20_000, c.rtfimg_render_width, "render_width", &mut warnings),
            "render_margin" => c.rtfimg_render_margin = parse_i32_or(val, 0, 200, c.rtfimg_render_margin, "render_margin", &mut warnings),
            // [imageio]
            "autosave_dir" => {
                if !val.is_empty() {
                    c.imageio_autosave_dir = Some(val.to_string());
                }
            }
            "prefix" => {
                if !val.is_empty() {
                    c.imageio_prefix = val.to_string();
                }
            }
            "max_seq" => c.imageio_max_seq = parse_u32_or(val, 0, 9999, c.imageio_max_seq, "max_seq", &mut warnings),
            // [tray]
            "precapture_ttl_ms" => c.tray_precapture_ttl_ms = parse_u32_or(val, 0, 60_000, c.tray_precapture_ttl_ms as u32, "precapture_ttl_ms", &mut warnings) as u64,
            _ => {}
        }
    }
    (c, warnings)
}

/// 解析布尔开关：接受 1/0、true/false、yes/no、on/off（大小写无关）。
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_i32(s: &str, lo: i32, hi: i32) -> Option<i32> {
    s.trim().parse::<i32>().ok().map(|n| n.clamp(lo, hi))
}

fn parse_u32(s: &str, lo: u32, hi: u32) -> Option<u32> {
    s.trim().parse::<u32>().ok().map(|n| n.clamp(lo, hi))
}

fn parse_f32(s: &str, lo: f32, hi: f32) -> Option<f32> {
    // 显式排除 NaN/inf：Rust 的 f32::from_str 接受它们，而 NaN.clamp() 仍返回
    // NaN——会穿透钳制进入配置（下游 `< 阈值` 比较恒为假，静默改变行为）
    s.trim()
        .parse::<f32>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(lo, hi))
}

/// 整数项解析：非法时记告警并沿用 `cur`。
fn parse_i32_or(v: &str, lo: i32, hi: i32, cur: i32, name: &str, w: &mut Vec<String>) -> i32 {
    match parse_i32(v, lo, hi) {
        Some(n) => n,
        None => {
            w.push(format!("无法识别的 {name} {v:?}，沿用默认 {cur}"));
            cur
        }
    }
}

/// 无符号整数项解析：非法时记告警并沿用 `cur`。
fn parse_u32_or(v: &str, lo: u32, hi: u32, cur: u32, name: &str, w: &mut Vec<String>) -> u32 {
    match parse_u32(v, lo, hi) {
        Some(n) => n,
        None => {
            w.push(format!("无法识别的 {name} {v:?}，沿用默认 {cur}"));
            cur
        }
    }
}

/// 浮点项解析：非法时记告警并沿用 `cur`。
fn parse_f32_or(v: &str, lo: f32, hi: f32, cur: f32, name: &str, w: &mut Vec<String>) -> f32 {
    match parse_f32(v, lo, hi) {
        Some(n) => n,
        None => {
            w.push(format!("无法识别的 {name} {v:?}，沿用默认 {cur}"));
            cur
        }
    }
}

/// 颜色项解析：非法时记告警并沿用 `cur`。
fn parse_color_or(v: &str, cur: Rgba, name: &str, w: &mut Vec<String>) -> Rgba {
    match parse_color(v) {
        Some(c) => c,
        None => {
            w.push(format!("无法识别的 {name} {v:?}，沿用默认"));
            cur
        }
    }
}

/// 解析颜色：`RRGGBB` 或 `RRGGBBAA`（十六进制，可带 `#`）。非法返回 None。
fn parse_color(s: &str) -> Option<Rgba> {
    let t = s.trim().trim_start_matches('#');
    if !t.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(t.get(i..i + 2)?, 16).ok();
    match t.len() {
        6 => Some(Rgba::new(byte(0)?, byte(2)?, byte(4)?, 0xFF)),
        8 => Some(Rgba::new(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

/// 格式化颜色：alpha=255 输出 6 位，否则输出 8 位（小写十六进制）。
fn fmt_color(c: Rgba) -> String {
    if c.a == 0xFF {
        format!("{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    } else {
        format!("{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
    }
}

/// `Rgba` → Win32 `COLORREF` 原始值（`0x00BBGGRR`）。
/// 放在 config 内保持其"不依赖 Windows API"的纯函数特性。
pub fn colorref_raw(c: Rgba) -> u32 {
    (c.r as u32) | ((c.g as u32) << 8) | ((c.b as u32) << 16)
}

/// 解析热键字符串 → (修饰键位, 虚拟键码)。
/// 非法输入返回 None（调用方应回退默认热键并记录日志）。
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk: Option<u32> = None;
    for part in s.split('+') {
        let k = part.trim().to_ascii_lowercase();
        if k.is_empty() {
            return None;
        }
        match k.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "meta" => mods |= MOD_WIN,
            other => {
                // 只允许一个主键；出现两个即视为非法
                if vk.is_some() {
                    return None;
                }
                vk = vk_from_name(other);
                vk?;
            }
        }
    }
    vk.map(|v| (mods, v))
}

fn vk_from_name(k: &str) -> Option<u32> {
    // F1..F24 → 0x70..0x87
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then(|| 0x70 + n - 1);
    }
    // 单个字母/数字 → 其 ASCII 大写（VK_A..VK_Z == 'A'..'Z'，VK_0..VK_9 == '0'..'9'）
    let b = k.as_bytes();
    if b.len() == 1 {
        let ch = b[0].to_ascii_uppercase();
        if ch.is_ascii_alphanumeric() {
            return Some(ch as u32);
        }
    }
    Some(match k {
        "printscreen" | "prtsc" | "snapshot" => 0x2C,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "space" => 0x20,
        "tab" => 0x09,
        "backspace" | "back" => 0x08,
        "pause" => 0x13,
        "scrolllock" | "scroll" => 0x91,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_single_key() {
        assert_eq!(parse_hotkey("F1"), Some((MOD_NOREPEAT, 0x70)));
        assert_eq!(parse_hotkey("f12"), Some((MOD_NOREPEAT, 0x7B)));
        assert_eq!(parse_hotkey("PrintScreen"), Some((MOD_NOREPEAT, 0x2C)));
        assert_eq!(parse_hotkey("A"), Some((MOD_NOREPEAT, 0x41)));
        assert_eq!(parse_hotkey("7"), Some((MOD_NOREPEAT, 0x37)));
    }

    #[test]
    fn hotkey_with_modifiers() {
        assert_eq!(
            parse_hotkey("Ctrl+Shift+S"),
            Some((MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT, 0x53))
        );
        // 顺序无关、大小写无关
        assert_eq!(parse_hotkey("shift+ctrl+s"), parse_hotkey("Ctrl+Shift+S"));
        assert_eq!(
            parse_hotkey("Alt+PrintScreen"),
            Some((MOD_NOREPEAT | MOD_ALT, 0x2C))
        );
    }

    #[test]
    fn hotkey_rejects_invalid() {
        assert_eq!(parse_hotkey(""), None);
        assert_eq!(parse_hotkey("F25"), None);
        assert_eq!(parse_hotkey("Ctrl+"), None);
        assert_eq!(parse_hotkey("Ctrl+A+B"), None); // 两个主键
        assert_eq!(parse_hotkey("NoSuchKey"), None);
    }

    #[test]
    fn hotkey_supports_enter_esc_arrows() {
        assert_eq!(parse_hotkey("Enter"), Some((MOD_NOREPEAT, 0x0D)));
        assert_eq!(parse_hotkey("Esc"), Some((MOD_NOREPEAT, 0x1B)));
        assert_eq!(parse_hotkey("Ctrl+Up"), Some((MOD_NOREPEAT | MOD_CONTROL, 0x26)));
        assert_eq!(parse_hotkey("Right"), Some((MOD_NOREPEAT, 0x27)));
    }

    #[test]
    fn parse_strips_utf8_bom() {
        // 带 BOM 的文件：首行键名不得被污染
        let c = parse("\u{FEFF}hotkey = F5\ntoast_ms = 3000\n");
        assert_eq!(c.hotkey, "F5");
        assert_eq!(c.toast_ms, 3000);
    }

    #[test]
    fn parse_f32_rejects_nan_and_inf() {
        // NaN.clamp 仍返回 NaN——必须显式拒绝，否则穿透钳制
        let c = parse("confidence = NaN\n");
        assert!(c.ocr_confidence.is_finite(), "NaN 不得进入配置");
        let c = parse("confidence = inf\n");
        assert!(c.ocr_confidence.is_finite(), "inf 不得进入配置");
        let c = parse("confidence = 0.7\n");
        assert!((c.ocr_confidence - 0.7).abs() < 1e-6);
    }

    /// 越界与非法项必须回退默认值，绝不能让程序起不来。
    #[test]
    fn parse_falls_back_on_bad_values() {
        let c = parse("hotkey = NotAKey\nformat = tiff\ntoast_ms = 999999\n");
        assert_eq!(c.hotkey, "F1");
        assert_eq!(c.format, SaveFormat::Png);
        assert_eq!(c.toast_ms, 10_000); // 被钳制到上限
    }

    #[test]
    fn pin_hotkey_default_and_fallbacks() {
        assert_eq!(Config::default().pin_hotkey, "F3");
        assert_eq!(parse("pin_hotkey = Ctrl+F9\n").pin_hotkey, "Ctrl+F9");
        // 非法值回退默认 F3
        assert_eq!(parse("pin_hotkey = NoSuchKey\n").pin_hotkey, "F3");
        assert_eq!(parse("pin_hotkey = \n").pin_hotkey, "F3");
    }

    #[test]
    fn parse_reads_valid_values() {
        let c = parse(
            "# 注释\n\
             hotkey = Ctrl+Alt+F5\n\
             format = jpg\n\
             save_dir = D:\\shots\n\
             ocr_lang = zh-Hans-CN\n\
             toast_ms = 900\n",
        );
        assert_eq!(c.hotkey, "Ctrl+Alt+F5");
        assert_eq!(c.format, SaveFormat::Jpeg);
        assert_eq!(c.save_dir.as_deref(), Some(r"D:\shots"));
        assert_eq!(c.ocr_lang.as_deref(), Some("zh-Hans-CN"));
        assert_eq!(c.toast_ms, 900);
    }

    #[test]
    fn parse_capture_cursor_flag() {
        assert!(!Config::default().capture_cursor, "默认不含指针");
        assert!(parse("capture_cursor = true\n").capture_cursor);
        assert!(parse("capture_cursor = YES\n").capture_cursor);
        assert!(!parse("capture_cursor = false\n").capture_cursor);
        assert!(!parse("capture_cursor = 随便\n").capture_cursor); // 非法值回退 false
    }

    #[test]
    fn parse_pick_assist_and_pin_extensions() {
        let c = parse(
            "[pick]\n\
             pick_level = element\n\
             pick_padding = 12\n\
             pick_auto_expand = false\n\
             pick_probe_interval_ms = 100\n\
             pick_show_label = false\n\
             pick_max_depth = 20\n\
             [assist]\n\
             assist_magnifier = false\n\
             assist_magnifier_size = 200\n\
             assist_magnifier_zoom = 8\n\
             assist_aux_lines = false\n\
             assist_aux_grid = true\n\
             assist_show_info = false\n\
             assist_color_format = hsl\n\
             assist_in_annot = true\n\
             [pin]\n\
             opacity = 128\n\
             locked = true\n\
             shadow = false\n\
             topmost = false\n\
             edge_hit = 10\n\
             min_visible_px = 40\n",
        );
        assert_eq!(c.pick_level, crate::pick::PickLevel::Element);
        assert_eq!(c.pick_padding, 12);
        assert!(!c.pick_auto_expand);
        assert_eq!(c.pick_probe_interval_ms, 100);
        assert!(!c.pick_show_label);
        assert_eq!(c.pick_max_depth, 20);
        assert!(!c.assist_magnifier);
        assert_eq!(c.assist_magnifier_size, 200);
        assert_eq!(c.assist_magnifier_zoom, 8);
        assert!(!c.assist_aux_lines);
        assert!(c.assist_aux_grid);
        assert!(!c.assist_show_info);
        assert_eq!(c.assist_color_format, crate::assist::ColorFormat::Hsl);
        assert!(c.assist_in_annot);
        assert_eq!(c.pin_opacity, 128);
        assert!(c.pin_locked && !c.pin_shadow && !c.pin_topmost);
        assert_eq!(c.pin_edge_hit, 10);
        assert_eq!(c.pin_min_visible_px, 40);

        // 非法拾取级别回退默认 region
        assert_eq!(
            parse("pick_level = bogus\n").pick_level,
            crate::pick::PickLevel::Region
        );
        // 非法色标项不污染其它色块
        let p = parse("style_panel_palette = zzzz,00ff00\n").style_palette;
        assert_eq!(p[0], DEF_STYLE_PALETTE[0]);
        assert_eq!(p[1], Rgba::new(0, 0xFF, 0, 0xFF));
    }

    /// 颜色面板固定 8 色，顺序为 红/绿/蓝/青/品红/黄/黑/白。
    #[test]
    fn style_palette_is_eight_colors_in_fixed_order() {
        let p = Config::default().style_palette;
        assert_eq!(p.len(), 8);
        assert_eq!(p[0], Rgba::new(0xFF, 0x00, 0x00, 0xFF), "红");
        assert_eq!(p[1], Rgba::new(0x00, 0xFF, 0x00, 0xFF), "绿");
        assert_eq!(p[2], Rgba::new(0x00, 0x00, 0xFF, 0xFF), "蓝");
        assert_eq!(p[3], Rgba::new(0x00, 0xFF, 0xFF, 0xFF), "青");
        assert_eq!(p[4], Rgba::new(0xFF, 0x00, 0xFF, 0xFF), "品红");
        assert_eq!(p[5], Rgba::new(0xFF, 0xFF, 0x00, 0xFF), "黄");
        assert_eq!(p[6], Rgba::new(0x00, 0x00, 0x00, 0xFF), "黑");
        assert_eq!(p[7], Rgba::new(0xFF, 0xFF, 0xFF, 0xFF), "白");
    }

    /// 新增段/键必须参与 render → parse 幂等。
    #[test]
    fn new_sections_roundtrip() {
        let mut c = Config::default();
        c.pick_level = crate::pick::PickLevel::Window;
        c.pick_padding = 7;
        c.assist_magnifier = false;
        c.assist_color_format = crate::assist::ColorFormat::Rgb;
        c.pin_opacity = 128;
        c.eraser_tol = 9;
        c.watermark_opacity = 90;
        let back = parse(&render(&c));
        assert_eq!(back.pick_level, c.pick_level);
        assert_eq!(back.pick_padding, c.pick_padding);
        assert_eq!(back.assist_magnifier, c.assist_magnifier);
        assert_eq!(back.assist_color_format, c.assist_color_format);
        assert_eq!(back.pin_opacity, c.pin_opacity);
        assert_eq!(back.eraser_tol, c.eraser_tol);
        assert_eq!(back.watermark_opacity, c.watermark_opacity);
    }

    /// 分节写法（`[section]` + 叶子键）与旧扁平键名（别名）都应被识别。
    #[test]
    fn sections_and_legacy_aliases() {
        let c = parse(
            "[ocr]\n\
             lang = en-US\n\
             confidence = 0.4\n\
             det_thresh = 0.5\n\
             [ui]\n\
             toolbar_btn_size = 40\n\
             [annot]\n\
             stroke_color = #00ff00\n",
        );
        assert_eq!(c.ocr_lang.as_deref(), Some("en-US"));
        assert!((c.ocr_confidence - 0.4).abs() < 1e-6);
        assert!((c.ocr_det_thresh - 0.5).abs() < 1e-6);
        assert_eq!(c.ui_toolbar_btn_size, 40);
        assert_eq!(c.stroke_color, Rgba::new(0x00, 0xFF, 0x00, 0xFF));
    }

    #[test]
    fn color_parse_and_format() {
        assert_eq!(parse_color("ff0000"), Some(Rgba::new(0xFF, 0, 0, 0xFF)));
        assert_eq!(parse_color("#FFCC00"), Some(Rgba::new(0xFF, 0xCC, 0, 0xFF)));
        assert_eq!(parse_color("11223344"), Some(Rgba::new(0x11, 0x22, 0x33, 0x44)));
        assert_eq!(parse_color("xyz"), None);
        assert_eq!(parse_color("12345"), None);
        assert_eq!(fmt_color(Rgba::new(0xE5, 0x39, 0x35, 0xFF)), "e53935");
        assert_eq!(fmt_color(Rgba::new(0x11, 0x22, 0x33, 0x44)), "11223344");
        assert_eq!(colorref_raw(Rgba::new(0x11, 0x22, 0x33, 0xFF)), 0x0033_2211);
    }

    #[test]
    fn badge_outline_can_be_empty() {
        assert_eq!(parse("badge_outline = \n").badge_outline, None);
        assert_eq!(
            parse("badge_outline = ffffff\n").badge_outline,
            Some(Rgba::new(0xFF, 0xFF, 0xFF, 0xFF))
        );
    }

    /// 渲染 → 解析 必须幂等（否则每次退出都会改写用户配置）。
    #[test]
    fn render_parse_roundtrip() {
        let mut c = Config::default();
        c.hotkey = "Ctrl+Shift+A".into();
        c.pin_hotkey = "Ctrl+Alt+F4".into();
        c.format = SaveFormat::Bmp;
        c.save_dir = Some(r"C:\temp".into());
        c.ocr_lang = Some("en-US".into());
        c.ocr_layout = OcrLayout::Multi;
        c.ocr_punctuation = OcrPunctuation::Half;
        c.ocr_remove_spaces = true;
        c.ocr_confidence = 0.35;
        c.toast_ms = 2000;
        c.capture_cursor = true;
        c.mosaic_mode = MosaicMode::Blur;
        c.highlight_mode = HighlightMode::Spotlight;
        c.pen_kind = PenKind::Fade;
        c.text_bold = true;
        c.text_italic = true;
        c.text_frame = TextFrame::Burst;
        c.text_attach = false;
        c.stroke_color = Rgba::new(0x12, 0x34, 0x56, 0xFF);
        c.text_color = Rgba::new(0x65, 0x43, 0x21, 0x80);
        c.badge_outline = None;
        c.stroke_width = 7;
        c.text_size = 30;
        c.highlight_keep = 0.4;
        c.spotlight_dim = 0.3;
        c.fade_min_alpha = 0.2;
        c.step_radius = 20;
        c.min_size = 5;
        c.pen_min_step = 3;
        c.attach_tol = 15;
        c.hit_tol = 6;
        c.handle_hit = 7;
        c.handle_size = 9;
        c.chrome_ext = 8;
        c.arrow_width_ratio = 4.0;
        c.arrow_len_min = 10;
        c.arrow_len_max = 60;
        c.text_font = "SimSun".into();
        c.burst_bumps = 9.0;
        c.burst_amp = 0.12;
        c.ui_toolbar_btn_size = 40;
        c.ui_tooltip_delay_ms = 300;
        c.ui_overlay_dim = 0.6;
        c.ui_overlay_frame_color = Rgba::new(0x00, 0xFF, 0x00, 0xFF);
        c.ui_toast_bg = Rgba::new(0x10, 0x20, 0x30, 0xFF);
        c.ui_menu_item_h = 30;
        c.ui_mica = false;
        c.pin_min_scale = 0.1;
        c.pin_max_scale = 4.0;
        c.pin_zoom_step = 1.25;
        c.pin_fit_ratio = 0.9;
        c.textimg_font_px = 24;
        c.textimg_margin = 10;
        c.textimg_fg = Rgba::new(0x00, 0x00, 0x00, 0xFF);
        c.rtfimg_render_width = 1200;
        c.rtfimg_render_margin = 8;
        c.imageio_autosave_dir = Some(r"D:\shots".into());
        c.imageio_prefix = "shot".into();
        c.imageio_max_seq = 50;
        c.tray_precapture_ttl_ms = 2000;
        c.ocr_upsample_target = 3000;
        c.ocr_max_upscale = 2.5;
        c.ocr_dark_luma = 100.0;
        c.ocr_det_limit = 1024;
        c.ocr_det_thresh = 0.25;
        c.ocr_box_thresh = 0.55;
        c.ocr_unclip_ratio = 1.8;

        let back = parse(&render(&c));
        assert_eq!(back.hotkey, c.hotkey);
        assert_eq!(back.pin_hotkey, c.pin_hotkey);
        assert_eq!(back.format, c.format);
        assert_eq!(back.save_dir, c.save_dir);
        assert_eq!(back.ocr_lang, c.ocr_lang);
        assert_eq!(back.ocr_layout, c.ocr_layout);
        assert_eq!(back.ocr_punctuation, c.ocr_punctuation);
        assert_eq!(back.ocr_remove_spaces, c.ocr_remove_spaces);
        assert!((back.ocr_confidence - c.ocr_confidence).abs() < 1e-6);
        assert_eq!(back.toast_ms, c.toast_ms);
        assert_eq!(back.capture_cursor, c.capture_cursor);
        assert_eq!(back.mosaic_mode, c.mosaic_mode);
        assert_eq!(back.highlight_mode, c.highlight_mode);
        assert_eq!(back.pen_kind, c.pen_kind);
        assert_eq!(back.text_bold, c.text_bold);
        assert_eq!(back.text_italic, c.text_italic);
        assert_eq!(back.text_frame, c.text_frame);
        assert_eq!(back.text_attach, c.text_attach);
        assert_eq!(back.stroke_color, c.stroke_color);
        assert_eq!(back.text_color, c.text_color);
        assert_eq!(back.badge_outline, c.badge_outline);
        assert_eq!(back.stroke_width, c.stroke_width);
        assert_eq!(back.text_size, c.text_size);
        assert!((back.highlight_keep - c.highlight_keep).abs() < 1e-6);
        assert!((back.spotlight_dim - c.spotlight_dim).abs() < 1e-6);
        assert!((back.fade_min_alpha - c.fade_min_alpha).abs() < 1e-6);
        assert_eq!(back.step_radius, c.step_radius);
        assert_eq!(back.min_size, c.min_size);
        assert_eq!(back.pen_min_step, c.pen_min_step);
        assert_eq!(back.attach_tol, c.attach_tol);
        assert_eq!(back.hit_tol, c.hit_tol);
        assert_eq!(back.handle_hit, c.handle_hit);
        assert_eq!(back.handle_size, c.handle_size);
        assert_eq!(back.chrome_ext, c.chrome_ext);
        assert!((back.arrow_width_ratio - c.arrow_width_ratio).abs() < 1e-6);
        assert_eq!(back.arrow_len_min, c.arrow_len_min);
        assert_eq!(back.arrow_len_max, c.arrow_len_max);
        assert_eq!(back.text_font, c.text_font);
        assert!((back.burst_bumps - c.burst_bumps).abs() < 1e-6);
        assert!((back.burst_amp - c.burst_amp).abs() < 1e-6);
        assert_eq!(back.ui_toolbar_btn_size, c.ui_toolbar_btn_size);
        assert_eq!(back.ui_tooltip_delay_ms, c.ui_tooltip_delay_ms);
        assert!((back.ui_overlay_dim - c.ui_overlay_dim).abs() < 1e-6);
        assert_eq!(back.ui_overlay_frame_color, c.ui_overlay_frame_color);
        assert_eq!(back.ui_toast_bg, c.ui_toast_bg);
        assert_eq!(back.ui_menu_item_h, c.ui_menu_item_h);
        assert_eq!(back.ui_mica, c.ui_mica);
        assert!((back.pin_min_scale - c.pin_min_scale).abs() < 1e-6);
        assert!((back.pin_max_scale - c.pin_max_scale).abs() < 1e-6);
        assert!((back.pin_zoom_step - c.pin_zoom_step).abs() < 1e-6);
        assert!((back.pin_fit_ratio - c.pin_fit_ratio).abs() < 1e-6);
        assert_eq!(back.textimg_font_px, c.textimg_font_px);
        assert_eq!(back.textimg_margin, c.textimg_margin);
        assert_eq!(back.textimg_fg, c.textimg_fg);
        assert_eq!(back.rtfimg_render_width, c.rtfimg_render_width);
        assert_eq!(back.rtfimg_render_margin, c.rtfimg_render_margin);
        assert_eq!(back.imageio_autosave_dir, c.imageio_autosave_dir);
        assert_eq!(back.imageio_prefix, c.imageio_prefix);
        assert_eq!(back.imageio_max_seq, c.imageio_max_seq);
        assert_eq!(back.tray_precapture_ttl_ms, c.tray_precapture_ttl_ms);
        assert_eq!(back.ocr_upsample_target, c.ocr_upsample_target);
        assert!((back.ocr_max_upscale - c.ocr_max_upscale).abs() < 1e-6);
        assert!((back.ocr_dark_luma - c.ocr_dark_luma).abs() < 1e-6);
        assert_eq!(back.ocr_det_limit, c.ocr_det_limit);
        assert!((back.ocr_det_thresh - c.ocr_det_thresh).abs() < 1e-6);
        assert!((back.ocr_box_thresh - c.ocr_box_thresh).abs() < 1e-6);
        assert!((back.ocr_unclip_ratio - c.ocr_unclip_ratio).abs() < 1e-6);
    }

    #[test]
    fn parse_annot_options_and_fallbacks() {
        let c = parse(
            "mosaic_mode = blur\n\
             highlight_mode = spotlight\n\
             pen_mode = soft\n\
             text_frame = bubble\n\
             text_bold = 1\ntext_italic = yes\ntext_attach = 0\n",
        );
        assert_eq!(c.mosaic_mode, MosaicMode::Blur);
        assert_eq!(c.highlight_mode, HighlightMode::Spotlight);
        assert_eq!(c.pen_kind, PenKind::Soft);
        assert_eq!(c.text_frame, TextFrame::Bubble);
        assert!(c.text_bold && c.text_italic);
        assert!(!c.text_attach);

        // 非法值 → 默认
        let c = parse("mosaic_mode = x\nhighlight_mode = y\npen_mode = z\ntext_frame = w\n");
        assert_eq!(c.mosaic_mode, MosaicMode::Pixelate);
        assert_eq!(c.highlight_mode, HighlightMode::Marker);
        assert_eq!(c.pen_kind, PenKind::Solid);
        assert_eq!(c.text_frame, TextFrame::None);
        assert!(c.text_attach, "缺省时挂线默认开启");
        // 组合偏好
        let p = c.annot_prefs();
        assert_eq!(p.mosaic_mode, MosaicMode::Pixelate);
    }

    #[test]
    fn parse_ocr_options_and_fallbacks() {
        let c = parse(
            "ocr_layout_type = single\n\
             ocr_punctuation_type = full\n\
             ocr_remove_spaces = yes\n\
             ocr_confidence = 0.5\n",
        );
        assert_eq!(c.ocr_layout, OcrLayout::Single);
        assert_eq!(c.ocr_punctuation, OcrPunctuation::Full);
        assert!(c.ocr_remove_spaces);
        assert!((c.ocr_confidence - 0.5).abs() < 1e-6);

        // 非法值 → 默认
        let c = parse("ocr_layout_type = bad\nocr_remove_spaces = maybe\nocr_confidence = abc\n");
        assert_eq!(c.ocr_layout, OcrLayout::Auto);
        assert!(!c.ocr_remove_spaces);
        assert_eq!(c.ocr_confidence, 0.0);
        // 越界置信度被钳制
        assert!((parse("ocr_confidence = 5\n").ocr_confidence - 1.0).abs() < 1e-6);
    }

    /// 标注样式默认值必须与历史硬编码常量一致（保证行为不变）。
    #[test]
    fn annot_defaults_match_legacy_constants() {
        let c = Config::default();
        assert_eq!(c.stroke_color, DEF_STROKE);
        assert_eq!(c.stroke_width, DEF_STROKE_W);
        assert_eq!(c.text_size, DEF_TEXT_PX);
        assert_eq!(c.highlight_color, HIGHLIGHT);
        assert_eq!(c.badge_fill, DEF_BADGE.fill);
        assert!((c.highlight_keep - 0.55).abs() < 1e-6);
        assert!((c.spotlight_dim - 0.55).abs() < 1e-6);
        assert!((c.fade_min_alpha - 0.12).abs() < 1e-6);
    }

    #[test]
    fn format_parse_aliases() {
        assert_eq!(SaveFormat::parse("PNG"), Some(SaveFormat::Png));
        assert_eq!(SaveFormat::parse("jpeg"), Some(SaveFormat::Jpeg));
        assert_eq!(SaveFormat::parse(" jpg "), Some(SaveFormat::Jpeg));
        assert_eq!(SaveFormat::parse("bmp"), Some(SaveFormat::Bmp));
        assert_eq!(SaveFormat::parse("gif"), None);
    }
}
