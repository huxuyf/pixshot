//! 截图操作工具栏（Fluent：无边框自绘标题栏 / 圆角卡片 / 悬停动画）。
//!
//! 从 `main.rs` 拆出（原本与托盘/热键/流程编排挤在同一文件，接近千行）。
//! 皮肤固定深色（见 `theme::toolbar_skin`），布局按窗口 DPI 换算并在 `WM_DPICHANGED` 重排。
//!
//! 按钮为**纯图标**（矢量自绘，见 [`crate::icons`]），依次为
//! 贴图 / 保存图像 / 复制并关闭 / 复制识别到的文字 ｜ 矩形 / 箭头 / 步骤序号 /
//! 文字 ｜ 颜色面板 / 更多（…）/ 关闭；前 4 个功能性按钮与后续标注按钮之间绘制一条竖杠分组；
//! 悬停延时后在图标上方弹出提示气泡（[`crate::tooltip`]）。
//!
//! 矩形、箭头、文字、更多、颜色面板按钮在悬停超过 1 秒（或矩形/箭头/文字被点击）时
//! 展开对应的子工具条：矩形/椭圆、箭头/线段、文字样式（粗体/斜体/描边/字体）、
//! 其余标注工具、8 色调色板。子工具条是共享的顶层圆角浮层（见 [`show_sub`]），
//! 点击浮层按钮之外的任意位置或光标离开后自动收起。
//!
//! 控件 ID 保持稳定（E2E 用 `Find-ChildById` 定位，见 `scripts/gui_e2e_*.ps1`）：
//! 1=贴图 / 2=复制 / 3=保存 / 4=OCR / 5=关闭；标注工具 6=矩形 / 7=箭头 / 8=步骤序号 /
//! 9=马赛克 / 10=高亮 / 11=文字；新增图形 12=椭圆 / 13=线段 / 14=画笔；
//! 15=橡皮 / 16=水印 / 17=颜色面板 / 18=更多（子工具条）；
//! 19=粗体 / 20=斜体 / 21=描边 / 22=字体下拉框。
//!
//! `Ctrl+滚轮` 调整当前选中标注工具的默认尺寸（线宽 / 字号 / 序号半径 /
//! 橡皮容差），光标无需悬停在按钮上；未选中工具时可按光标下的标注按钮调整。
//! 尺寸类别经 [`WM_APP_ADJUST_SIZE`] 投递给冻结层的 [`annot::SizeKind`]，
//! 每步调整都用 Toast 实时显示当前值。
//!
//! 标注（见设计文档 1.2 D/E / 5.5）：工具按钮把当前工具经 `WM_APP_SET_TOOL`
//! 投递给冻结层，并按冻结层回传的 `WM_APP_TOOL_CHANGED` 高亮；文字输入经
//! `WM_APP_CHAR` 转发 `WM_CHAR`（输入中字母键放行为正文，不当工具快捷键，
//! 见 `WM_APP_QUERY_TYPING`）；贴图/复制/保存/OCR 动作在导出前先经
//! [`crate::annot::composite`] 合成标注，且先 `commit_typing` 完成输入中的文字。

use std::sync::Arc;

use crate::annot;
use crate::capture::{self, CapturedImage};
use crate::config;
use crate::error::Result;
use crate::overlay::{
    Selection, StyleField, WM_APP_ADJUST_SIZE, WM_APP_ANNOT_DIRTY, WM_APP_CHAR, WM_APP_ESC_CLOSE,
    WM_APP_KEY, WM_APP_QUERY_TYPING, WM_APP_SET_STYLE, WM_APP_SET_TOOL,
    WM_APP_STYLE_PANEL_CLOSED, WM_APP_TOOL_CHANGED,
};
use crate::theme;
use crate::util::{self, add_button, raw, register_class_w, BtnStyle};
use crate::{clipboard, icons, imageio, ocr, pin, tooltip};

use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, EndPaint, EnumFontFamiliesExW, FillRect, GetDC, LineTo, MoveToEx,
    Polygon, RedrawWindow, ReleaseDC, RoundRect, SelectObject, DEFAULT_CHARSET, LOGFONTW,
    PS_SOLID, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW, SRCCOPY, TEXTMETRICW,
};
use windows::core::PCWSTR;
use windows::Win32::UI::Controls::{CB_SETMINVISIBLE, DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

// 工具栏按钮 ID（与 E2E 的控件查找协议一致）
const BTN_PIN: usize = 1;
const BTN_COPY: usize = 2;
const BTN_SAVE: usize = 3;
const BTN_OCR: usize = 4;
const BTN_CLOSE: usize = 5;
/// 标注工具按钮 ID（见 5.5）。
const BTN_RECT: usize = 6;
const BTN_ARROW: usize = 7;
const BTN_STEP: usize = 8;
/// 标注工具按钮 ID 扩展：马赛克 / 高亮 / 文字。
const BTN_MOSAIC: usize = 9;
const BTN_HIGHLIGHT: usize = 10;
const BTN_TEXT: usize = 11;
/// 新增图形工具按钮 ID：椭圆 / 线段 / 画笔（追加在末尾，保持既有 ID 稳定）。
const BTN_ELLIPSE: usize = 12;
const BTN_LINE: usize = 13;
const BTN_PEN: usize = 14;
/// 样式/橡皮/水印扩展按钮（保持 1..14 语义不变）。
const BTN_ERASER: usize = 15;
const BTN_WATERMARK: usize = 16;
const BTN_STYLE: usize = 17;
/// 「更多」按钮：悬停展开标注子工具条（位于颜色面板右侧、关闭按钮左侧）。
const BTN_MORE: usize = 18;
/// 文字子工具条按钮：粗体 / 斜体 / 描边（追加在末尾，保持既有 ID 稳定）。
const BTN_TEXT_BOLD: usize = 19;
const BTN_TEXT_ITALIC: usize = 20;
const BTN_TEXT_STROKE: usize = 21;
/// 文字子工具条中的字体下拉框（非 owner-draw 按钮）。
const BTN_TEXT_FONT: usize = 22;

/// 主工具条按钮从左到右的排列顺序（图标顺序 = 需求给定顺序）。
/// 文字按钮移到「序号」之后；椭圆/线段/马赛克/画笔/高亮/橡皮/水印 收进各自子工具条。
const BTN_ORDER: [usize; 11] = [
    BTN_PIN,
    BTN_SAVE,
    BTN_COPY,
    BTN_OCR,
    BTN_RECT,
    BTN_ARROW,
    BTN_STEP,
    BTN_TEXT,
    BTN_STYLE,
    BTN_MORE,
    BTN_CLOSE,
];
/// 「更多」子工具条：马赛克 / 画笔 / 高亮 / 橡皮 / 水印。
const MORE_ORDER: [usize; 5] = [
    BTN_MOSAIC,
    BTN_PEN,
    BTN_HIGHLIGHT,
    BTN_ERASER,
    BTN_WATERMARK,
];
/// 矩形按钮的子工具条：矩形 / 椭圆。
const SHAPE_ORDER: [usize; 2] = [BTN_RECT, BTN_ELLIPSE];
/// 箭头按钮的子工具条：箭头 / 线段。
const ARROW_ORDER: [usize; 2] = [BTN_ARROW, BTN_LINE];
/// 文字按钮的子工具条：粗体 / 斜体 / 描边（字体下拉框单独创建）。
const TEXT_STYLE_ORDER: [usize; 3] = [BTN_TEXT_BOLD, BTN_TEXT_ITALIC, BTN_TEXT_STROKE];
/// 所有可能出现在子工具条里的 owner-draw 按钮（窗口创建时统一建好，按需显示）。
const SUB_IDS: [usize; 12] = [
    BTN_ELLIPSE,
    BTN_LINE,
    BTN_MOSAIC,
    BTN_PEN,
    BTN_HIGHLIGHT,
    BTN_ERASER,
    BTN_WATERMARK,
    BTN_RECT,
    BTN_ARROW,
    BTN_TEXT_BOLD,
    BTN_TEXT_ITALIC,
    BTN_TEXT_STROKE,
];
const MAIN_BTN_COUNT: usize = BTN_ORDER.len();
const SUB_BTN_COUNT: usize = SUB_IDS.len();
/// 文字子工具条里字体下拉框的宽度（逻辑像素）。
const FONT_COMBO_W: i32 = 132;
/// 字体下拉列表展开后的宽度（逻辑像素）：略宽于控件本身，避免长字体名被截断。
const FONT_COMBO_DROP_W: i32 = 190;
/// 文字子工具条的字体候选（显示名 + 系统中可能的字体族名，按需求顺序）。
/// 只有至少一个别名已安装时才显示该项；配置里保存解析到的实际字体族名。
const TEXT_FONT_FAMILIES: [(&str, &[&str]); 8] = [
    (
        "微软雅黑",
        &["微软雅黑", "Microsoft YaHei UI", "Microsoft YaHei"],
    ),
    ("宋体", &["宋体", "SimSun"]),
    ("楷体", &["楷体", "KaiTi"]),
    ("黑体", &["黑体", "SimHei"]),
    ("新宋体", &["新宋体", "NSimSun"]),
    (
        "汉仪中黑",
        &["汉仪中黑", "汉仪中黑 197", "HYZhongHeiTi-197", "HYZhongHeiJ"],
    ),
    (
        "方正小标宋简体",
        &["方正小标宋简体", "FZXiaoBiaoSong-B05S", "FZXBSJW"],
    ),
    (
        "方正隶书简体",
        &["方正隶书简体", "FZLiShu-S01S", "FZLiShu II-S06S", "FZLSJW"],
    ),
];

/// 共享子工具条的四类内容；颜色面板是独立窗口，不在此列。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SubKind {
    #[default]
    None,
    More,
    Shape,
    Arrow,
    Text,
}

impl SubKind {
    fn buttons(self) -> &'static [usize] {
        match self {
            Self::More => &MORE_ORDER,
            Self::Shape => &SHAPE_ORDER,
            Self::Arrow => &ARROW_ORDER,
            Self::Text => &TEXT_STYLE_ORDER,
            Self::None => &[],
        }
    }

}

/// 主工具条按钮 → 悬停/点击后展开的子工具条种类。
fn sub_kind_for_anchor(id: usize) -> Option<SubKind> {
    match id {
        BTN_MORE => Some(SubKind::More),
        BTN_RECT => Some(SubKind::Shape),
        BTN_ARROW => Some(SubKind::Arrow),
        BTN_TEXT => Some(SubKind::Text),
        _ => None,
    }
}

/// 主工具条按钮的悬停提示（子按钮仍用 [`tip_text`] 的单项名称）。
fn main_tip_text(id: usize) -> &'static str {
    match id {
        BTN_RECT => "矩形/椭圆",
        BTN_ARROW => "箭头/线段",
        BTN_MORE => "更多",
        _ => tip_text(id),
    }
}

/// 各按钮的悬停提示文字（子工具条按钮与主工具条共用）。
fn tip_text(id: usize) -> &'static str {
    match id {
        BTN_PIN => "贴图",
        BTN_SAVE => "保存图像",
        BTN_COPY => "复制并关闭",
        BTN_OCR => "复制识别到的文字",
        BTN_RECT => "矩形",
        BTN_ARROW => "箭头",
        BTN_STEP => "步骤序号",
        BTN_ELLIPSE => "椭圆",
        BTN_LINE => "线段",
        BTN_PEN => "画笔",
        BTN_MOSAIC => "马赛克",
        BTN_HIGHLIGHT => "高亮",
        BTN_TEXT => "文字",
        BTN_ERASER => "橡皮",
        BTN_WATERMARK => "水印",
        BTN_STYLE => "颜色面板",
        BTN_MORE => "更多",
        BTN_TEXT_BOLD => "粗体",
        BTN_TEXT_ITALIC => "斜体",
        BTN_TEXT_STROKE => "描边",
        BTN_TEXT_FONT => "字体",
        BTN_CLOSE => "关闭",
        _ => "",
    }
}
/// 各按钮的快捷键（显示在提示文字之后；无快捷键返回空串）。
fn shortcut_of(id: usize) -> &'static str {
    match id {
        BTN_PIN => "Ctrl+T",
        BTN_SAVE => "Ctrl+S",
        BTN_COPY => "Ctrl+C",
        BTN_OCR => "Ctrl+O",
        BTN_RECT => "R",
        BTN_ARROW => "A",
        BTN_STEP => "N",
        BTN_ELLIPSE => "E",
        BTN_LINE => "L",
        BTN_PEN => "P",
        BTN_MOSAIC => "B",
        BTN_HIGHLIGHT => "H",
        BTN_TEXT => "T",
        BTN_ERASER => "X",
        BTN_WATERMARK => "M",
        BTN_CLOSE => "Esc",
        _ => "",
    }
}

/// 提示气泡里显示的快捷键：R/A 只属于子工具条里的「矩形」「箭头」按钮，
/// 主工具条的「矩形/椭圆」「箭头/线段」入口不显示快捷键说明。
fn tip_shortcut_text(id: usize, is_main: bool) -> &'static str {
    if is_main && matches!(id, BTN_RECT | BTN_ARROW) {
        ""
    } else {
        shortcut_of(id)
    }
}
/// 悬停后弹出提示气泡的延时（毫秒）
/// 悬停提示延时（毫秒）由配置提供，见 config.ini [ui] tooltip_delay_ms。
/// 提示气泡定时器 ID
const TIMER_TIP: usize = 779;
/// 子工具条：悬停延时展开 / 可见期间侦测光标离开与点击外部的定时器 ID。
const TIMER_SUB_SHOW: usize = 780;
const TIMER_SUB_POLL: usize = 781;
/// 悬停主工具条按钮后弹出子工具条的延时（毫秒）——需求要求超过 1 秒。
const SUB_SHOW_DELAY_MS: u32 = 1100;
/// 子工具条可见时轮询光标/鼠标按键的间隔（毫秒）。
const SUB_POLL_MS: u32 = 40;
/// 连续多少次轮询都发现光标在外才收起（容忍从按钮跨过工具条内边距进入浮层的短暂空隙）。
const SUB_HIDE_MISSES: u8 = 8;
/// 鼠标左键物理状态（GetAsyncKeyState 用）。
const VK_LBUTTON_CODE: i32 = 0x01;
/// 自投递：焦点/激活切换后延迟重绘窗口非客户区（清除 DWM 残留的灰色外框）。
const WM_APP_REFRESH_FRAME: u32 = WM_APP + 100;
/// 子工具条窗口类名（顶层 popup：子控件可画在主工具条窗口之外）。
const MORE_CLASS: &str = "PixShotToolbarMore";
/// 子工具条指向「…」按钮的小尖角：高度 / 半底宽（逻辑像素）。
const MORE_TAIL_H: i32 = 8;
const MORE_TAIL_HALF_W: i32 = 7;
/// 分层窗口透明色（LWA_COLORKEY）：尖角/圆角之外的像素透出桌面，避免白色方块。
const MORE_KEY: COLORREF = COLORREF(0x00FF_00FF);
/// VK_ESCAPE（关闭）/ VK_T / VK_S / VK_C / VK_O（配合 Ctrl 触发对应动作）
const VK_ESCAPE: usize = 0x1B;
const VK_T: usize = 0x54;
const VK_S: usize = 0x53;
const VK_C: usize = 0x43;
const VK_O: usize = 0x4F;
/// 标注工具键：R=矩形 / A=箭头 / N=步骤序号 / E=椭圆 / L=线段 / P=画笔 /
/// B=马赛克 / H=高亮（无 Ctrl，见 5.5）；T 在无 Ctrl 时 = 文字工具（有 Ctrl 时 = 贴图）。
const VK_R: usize = 0x52;
const VK_A: usize = 0x41;
const VK_N: usize = 0x4E;
const VK_E: usize = 0x45;
const VK_L: usize = 0x4C;
const VK_P: usize = 0x50;
const VK_B: usize = 0x42;
const VK_H: usize = 0x48;
/// 橡皮 `X` / 水印 `M`。
const VK_X: usize = 0x58;
const VK_M: usize = 0x4D;
/// 撤销/重做/删除：Ctrl+Z / Ctrl+Y / Delete（转发给冻结层，见 5.5）
const VK_Z: usize = 0x5A;
const VK_Y: usize = 0x59;
const VK_DELETE: usize = 0x2E;
/// VK_CONTROL（GetKeyState 读物理按键状态）
const VK_CONTROL: i32 = 0x11;
/// VK_SHIFT
const VK_SHIFT: i32 = 0x10;

/// 标注组起始下标（前面的贴图/保存/复制/OCR 为功能性按钮）。
const ANNOT_GROUP_START: usize = 4;
/// 功能组与标注组之间的竖杠占位宽度（逻辑像素）。
const GROUP_SEP_W: i32 = 11;
/// 分组竖杠颜色：白底上可见但不抢眼的浅灰（比卡片描边深一档）。
const GROUP_SEP_COLOR: COLORREF = COLORREF(0x00C8_C8C8);

/// 工具栏布局尺寸来自配置（逻辑像素，96 DPI 基准），见 config.ini [ui] 段。
fn btn_sz() -> i32 {
    crate::config::get().ui_toolbar_btn_size
}
fn gap_l() -> i32 {
    crate::config::get().ui_toolbar_gap
}
fn radius_l() -> i32 {
    crate::config::get().ui_toolbar_radius
}
/// 逻辑客户区尺寸：仅容纳一行方形图标按钮，力求紧凑。
fn logical_size() -> (i32, i32) {
    let c = crate::config::get();
    let n = BTN_ORDER.len() as i32;
    let w = c.ui_toolbar_btn_size * n
        + c.ui_toolbar_gap * (n - 1)
        + GROUP_SEP_W
        + c.ui_toolbar_pad_h * 2;
    let h = c.ui_toolbar_pad_v * 2 + c.ui_toolbar_btn_size;
    (w, h)
}
/// 子工具条逻辑尺寸：一行按钮（文字子工具条额外含字体下拉框）+ 顶部/底部小尖角占位。
fn sub_logical_size(kind: SubKind) -> (i32, i32) {
    let c = crate::config::get();
    let n = kind.buttons().len() as i32;
    let extra = if kind == SubKind::Text {
        FONT_COMBO_W + c.ui_toolbar_gap
    } else {
        0
    };
    let w = c.ui_toolbar_btn_size * n
        + c.ui_toolbar_gap * (n - 1).max(0)
        + extra
        + c.ui_toolbar_pad_h * 2;
    let h = MORE_TAIL_H + c.ui_toolbar_pad_v * 2 + c.ui_toolbar_btn_size;
    (w, h)
}
/// 子工具条圆角卡片在客户区内的上下边界（`above` = 浮层位于工具条上方，尖角朝下）。
fn sub_card_bounds(ch: i32, dpi: i32, above: bool) -> (i32, i32) {
    let tail = util::scaled(MORE_TAIL_H, dpi).min((ch - 1).max(1));
    if above {
        (0, (ch - tail).max(1))
    } else {
        (tail, ch)
    }
}

struct ToolbarData {
    image: Arc<CapturedImage>,
    /// 主工具条图标按钮，顺序见 [`BTN_ORDER`]
    buttons: [HWND; MAIN_BTN_COUNT],
    /// 共享子工具条窗口（0 = 创建失败：退回仅快捷键可用）
    sub: HWND,
    /// 当前子工具条展示的按钮集合
    sub_kind: SubKind,
    /// 子工具条上的全部按钮（顺序见 [`SUB_IDS`]），按当前种类显示/隐藏
    sub_buttons: [HWND; SUB_BTN_COUNT],
    /// 子工具条独立的悬停动画引擎（与主工具条分开，避免两个窗口的跟踪定时器互相干扰）
    sub_fx: util::HoverFx,
    /// 子工具条是否位于工具条上方（尖角朝下），由 [`show_sub`] 定位时更新
    sub_above: bool,
    /// 尖角尖端相对子工具条客户区的 X 坐标（与展开按钮中心对齐）
    sub_tail_x: i32,
    /// 是否已武装悬停展开定时器（避免高频 WM_MOUSEMOVE 反复重置）
    sub_show_armed: bool,
    /// 当前武装悬停展开的按钮 ID
    sub_armed_id: usize,
    /// 子工具条可见期间「光标不在其上」的连续轮询次数（达到阈值才收起）
    sub_miss: u8,
    /// 上一次轮询时左键是否按下（用于识别「点击了别处」）
    sub_mouse_down: bool,
    /// 当前子工具条锚定的主工具条按钮 ID
    sub_anchor: usize,
    /// 文字子工具条的字体下拉框
    font_combo: HWND,
    /// 字体下拉框每一项对应的实际字体族名（显示名见 [`TEXT_FONT_FAMILIES`]）
    font_faces: Vec<String>,
    /// 文字子工具条三个开关的当前状态（用于按钮高亮）
    text_bold: bool,
    text_italic: bool,
    text_stroke: bool,
    /// 主工具条窗口句柄（子工具条过程据此回投命令/重绘）
    bar: HWND,
    /// 工具栏皮肤：固定深色
    skin: theme::Theme,
    /// Mica 背景是否启用（工具栏固定深色为纯色面，恒为 false）
    backdrop: bool,
    /// 按钮悬停动画
    fx: util::HoverFx,
    /// 悬停提示气泡窗口
    tip: tooltip::Tip,
    /// 已武装提示延时的按钮 ID（0 = 无）
    tip_for: usize,
    /// 提示气泡锚定的按钮句柄（子工具条里同 ID 的按钮与主工具条不同）
    tip_hwnd: HWND,
    /// 提示气泡当前是否可见
    tip_shown: bool,
    /// 冻结层窗口（转发工具切换/按键；见 5.5）
    overlay: HWND,
    /// 冻结层 [`Selection`]（只读借用其标注文档做导出合成；见 6）
    sel: *const Selection,
    /// 当前激活的标注工具按钮 ID（0 = 无），用于高亮
    active_tool: usize,
    /// 颜色面板窗口（0 = 未打开）
    style_panel: HWND,
}

unsafe fn data_of(hwnd: HWND) -> Option<&'static mut ToolbarData> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut ToolbarData))
    }
}

/// 进入工具栏（模态，直到用户选择操作或关闭）。须在 GUI 线程调用。
///
/// `overlay`：确认后的冻结层（提供窗口句柄用于消息转发、标注文档用于导出合成）。
pub fn run(image: Arc<CapturedImage>, overlay: &Selection) -> Result<()> {
    unsafe { run_impl(image, overlay) }
}

unsafe fn run_impl(image: Arc<CapturedImage>, overlay: &Selection) -> Result<()> {
    register_class_w("PixShotToolbar", toolbar_proc, false, false, false);
    register_class_w(MORE_CLASS, more_proc, false, false, false);

    // 初始 DPI 按选区所在显示器（多屏异缩放下比系统 DPI 更贴近实际显示位置；
    // 创建后 WM_DPICHANGED 仍会纠正一次）
    let dpi = util::dpi_for_rect(image.x, image.y, image.width, image.height);
    let (lw, lh) = logical_size();
    let (tw, th) = (util::scaled(lw, dpi), util::scaled(lh, dpi));

    // 定位在选区下方 8px，水平对齐选区中线；下方放不下则翻到选区上方，
    // 仍不足再钳制回虚拟屏幕内（贴底时直接压在选区上会遮挡标注操作区）
    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    let x = (image.x + (image.width - tw) / 2).clamp(vx, (vx + vw - tw).max(vx));
    let gap = util::scaled(8, dpi);
    let y_below = image.y + image.height + gap;
    let y = if y_below + th > vy + vh {
        (image.y - th - gap).max(vy) // 下方空间不足 → 选区上方
    } else {
        y_below
    }
    .clamp(vy, (vy + vh - th).max(vy));

    let data = Box::into_raw(Box::new(ToolbarData {
        image,
        buttons: [HWND::default(); MAIN_BTN_COUNT],
        sub: HWND::default(),
        sub_kind: SubKind::None,
        sub_buttons: [HWND::default(); SUB_BTN_COUNT],
        sub_fx: util::HoverFx::new(),
        sub_above: false,
        sub_tail_x: 0,
        sub_show_armed: false,
        sub_armed_id: 0,
        sub_miss: 0,
        sub_mouse_down: false,
        sub_anchor: 0,
        font_combo: HWND::default(),
        font_faces: Vec::new(),
        text_bold: config::get().text_bold,
        text_italic: config::get().text_italic,
        text_stroke: config::get().text_stroke,
        bar: HWND::default(),
        skin: theme::toolbar_skin(),
        backdrop: false,
        fx: util::HoverFx::new(),
        tip: tooltip::Tip::none(),
        tip_for: 0,
        tip_hwnd: HWND::default(),
        tip_shown: false,
        overlay: overlay.hwnd(),
        sel: overlay as *const Selection,
        active_tool: 0,
        style_panel: HWND::default(),
    }));

    // WS_THICKFRAME 供 DWM 绘制阴影；WM_NCCALCSIZE(0) 去除边框（全客户区）
    let hwnd = util::create_window(
        "PixShotToolbar",
        "PixShot 截图",
        WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0),
        WS_POPUP | WS_THICKFRAME,
        x,
        y,
        tw,
        th,
        data as *mut core::ffi::c_void,
    )?;
    // 注意：不要在失败路径回收 `data` —— WM_CREATE 已把指针交给窗口
    // （WM_DESTROY 里 Box::from_raw 释放），创建中途失败时 WM_DESTROY 会照常派发；
    // 而 create_window 以 Err 提前返回时不会再执行到这里。两处只留一处释放点。
    debug_assert!(!hwnd.is_invalid());
    // 排除出截屏捕获：防止工具栏出现在后续截图/录屏画面中（截图工具惯例）
    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    let _ = SetForegroundWindow(hwnd);
    // 显式提到置顶带最上层：框选遮罩此时是**不透明的置顶窗口**（显示冻结画面），
    // 工具条必须稳稳压在它上面才看得见。工具条本身已是 WS_EX_TOPMOST 且在其后创建，
    // 这里再显式 SetWindowPos 一次做保险（SWP_NOACTIVATE：只调 z 序，不改前台）。
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );

    // 模态消息循环
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 {
            break; // WM_QUIT
        }
        if r == -1 {
            crate::diag::log("[toolbar] GetMessageW 返回 -1（消息错误），结束工具条循环");
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = DestroyWindow(hwnd);
    Ok(())
}

/// 按 DPI 重排控件：主工具条一行方形图标按钮横向等距排布（紧凑，不随窗口拉伸）。
unsafe fn layout(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);
    let s = |v: i32| util::scaled(v, dpi);
    // 窗口尚未成尺寸时先不动（避免控件堆在 (0,0)）
    if cw < s(btn_sz()) || ch < s(btn_sz()) {
        return;
    }

    let btn = s(btn_sz());
    let gap = s(gap_l());
    let sep = s(GROUP_SEP_W);
    let n = d.buttons.len() as i32;
    // 水平居中：两侧留白均分剩余空间（窗口即为按钮行量身定制，差值为取整误差）
    let used = btn * n + gap * (n - 1) + sep;
    let x0 = ((cw - used) / 2).max(0);

    // 垂直居中：按钮上缘到顶部、下缘到底部的距离相等（均由客户区高算出）
    let by = ((ch - btn) / 2).max(0);
    let mut bx = x0;
    for (i, b) in d.buttons.iter().enumerate() {
        if i == ANNOT_GROUP_START {
            bx += sep; // 功能组与标注组之间的竖杠占位
        }
        let _ = MoveWindow(*b, bx, by, btn, btn, BOOL(1));
        bx += btn + gap;
    }
}

/// 子工具条内部布局（窗口过程入口，自行取数据）。
unsafe fn layout_sub(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    layout_sub_with(d, hwnd);
}

/// 子工具条内部布局：当前种类的按钮在圆角卡片内水平等距排布（尖角区域留空）。
unsafe fn layout_sub_with(d: &ToolbarData, hwnd: HWND) {
    let ids = d.sub_kind.buttons();
    // 所有子按钮先隐藏；只显示当前种类用到的
    for b in d.sub_buttons {
        let _ = ShowWindow(b, SW_HIDE);
    }
    if !d.font_combo.is_invalid() {
        let _ = ShowWindow(d.font_combo, SW_HIDE);
    }
    if ids.is_empty() {
        return;
    }
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(d.bar);
    let s = |v: i32| util::scaled(v, dpi);
    if cw < s(btn_sz()) || ch < s(btn_sz()) {
        return;
    }
    let btn = s(btn_sz());
    let gap = s(gap_l());
    let combo_w = s(FONT_COMBO_W);
    let extra = if d.sub_kind == SubKind::Text {
        combo_w + gap
    } else {
        0
    };
    let n = ids.len() as i32;
    let used = btn * n + gap * (n - 1) + extra;
    let x0 = ((cw - used) / 2).max(0);
    let (card_top, card_bottom) = sub_card_bounds(ch, dpi, d.sub_above);
    let by = card_top + ((card_bottom - card_top - btn) / 2).max(0);
    let mut bx = x0;
    for id in ids {
        let Some(b) = sub_button(d, *id) else { continue };
        let _ = MoveWindow(b, bx, by, btn, btn, BOOL(1));
        let _ = ShowWindow(b, SW_SHOWNOACTIVATE);
        bx += btn + gap;
    }
    if d.sub_kind == SubKind::Text && !d.font_combo.is_invalid() {
        // 下拉列表的展开高度由 combo 窗口高度决定：按候选数量把窗口撑高。
        // 闭合时 Windows 仍按字体把可见选择框收缩到自然高度，但会保留展开高度。
        let item_h = SendMessageW(d.font_combo, CB_GETITEMHEIGHT, WPARAM(0), LPARAM(0))
            .0
            .max(1) as i32;
        let rows = d.font_faces.len().max(1) as i32;
        let drop_h = item_h * rows + s(2);
        let _ = MoveWindow(
            d.font_combo,
            bx,
            by,
            combo_w,
            btn + drop_h,
            BOOL(1),
        );
        // 取系统实际采用的闭合态高度，把字体框与同行按钮的垂直中心对齐
        let mut rr = RECT::default();
        let _ = GetWindowRect(d.font_combo, &mut rr);
        let field_h = (rr.bottom - rr.top).max(1);
        let combo_y = by + ((btn - field_h) / 2).max(0);
        let _ = MoveWindow(
            d.font_combo,
            bx,
            combo_y,
            combo_w,
            btn + drop_h,
            BOOL(1),
        );
        let _ = SendMessageW(
            d.font_combo,
            CB_SETMINVISIBLE,
            WPARAM(rows.min(10) as usize),
            LPARAM(0),
        );
        let _ = ShowWindow(d.font_combo, SW_SHOWNOACTIVATE);
    }
}

/// 工具栏自绘：整窗即白底圆角卡片（1px 浅灰描边），按钮图标绘制其上。
/// 除图标外不绘制任何文字。
unsafe fn paint(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let t = d.skin;
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);

    // 1. 白底铺满整个窗口
    let base = if d.backdrop { COLORREF(0) } else { t.card };
    let base_brush = CreateSolidBrush(base);
    let rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    FillRect(hdc, &rc, base_brush);
    let _ = DeleteObject(base_brush);

    // 2. 圆角 + 1px 描边（内缩 1px，避免被窗口边缘裁掉半像素）
    let brush = CreateSolidBrush(t.card);
    let pen = CreatePen(PS_SOLID, 1, t.stroke);
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, pen);
    let r = util::scaled(radius_l(), dpi);
    let _ = RoundRect(hdc, 0, 0, cw, ch, r, r);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    // 3. 功能组（贴图/保存/复制/OCR）与标注组之间画一条竖杠
    let btn = util::scaled(btn_sz(), dpi);
    let gap = util::scaled(gap_l(), dpi);
    let sep = util::scaled(GROUP_SEP_W, dpi);
    let n = d.buttons.len() as i32;
    let used = btn * n + gap * (n - 1) + sep;
    let x0 = ((cw - used) / 2).max(0);
    let sx = separator_x(x0, btn, gap, sep);
    let by = ((ch - btn) / 2).max(0);
    let sep_pen = CreatePen(PS_SOLID, 1, GROUP_SEP_COLOR);
    let old_pen = SelectObject(hdc, sep_pen);
    let _ = MoveToEx(hdc, sx, by + util::scaled(6, dpi), None);
    let _ = LineTo(hdc, sx, ch - by - util::scaled(6, dpi));
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(sep_pen);

    let _ = EndPaint(hwnd, &ps);
}

/// 「…」子工具条自绘：色键透明底 + 圆角卡片 + 指向「…」按钮的小尖角。
/// 透明底让尖角两侧与圆角之外的像素透出桌面（否则是一整块白色方块）。
unsafe fn paint_sub(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let t = d.skin;
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(d.bar);
    let s = |v: i32| util::scaled(v, dpi);

    // 1. 透明底：色键区域不参与绘制，也不会拦截鼠标
    let key = CreateSolidBrush(MORE_KEY);
    let rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    FillRect(hdc, &rc, key);
    let _ = DeleteObject(key);

    let (card_top, card_bottom) = sub_card_bounds(ch, dpi, d.sub_above);

    // 2. 圆角卡片 + 1px 描边（半径按 DPI 缩放；RoundRect 轴长 = 直径）
    let brush = CreateSolidBrush(t.card);
    let edge = CreatePen(PS_SOLID, 1, t.stroke);
    let edge_fill = CreatePen(PS_SOLID, 1, t.card);
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, edge);
    let r = s(radius_l()).max(1);
    let _ = RoundRect(hdc, 0, card_top, cw, card_bottom, r, r);

    // 3. 尖角：先以卡片色填充三角形（顺带遮住卡片底边描边），再描两侧斜边
    let half = s(MORE_TAIL_HALF_W);
    let tx = d.sub_tail_x.clamp(r + half, (cw - r - half).max(r + half));
    let pts = if d.sub_above {
        [
            POINT {
                x: tx,
                y: ch - 1,
            },
            POINT {
                x: tx - half,
                y: card_bottom,
            },
            POINT {
                x: tx + half,
                y: card_bottom,
            },
        ]
    } else {
        [
            POINT { x: tx, y: 0 },
            POINT {
                x: tx - half,
                y: card_top,
            },
            POINT {
                x: tx + half,
                y: card_top,
            },
        ]
    };
    let _ = SelectObject(hdc, edge_fill);
    let _ = Polygon(hdc, &pts);
    let _ = SelectObject(hdc, edge);
    let _ = MoveToEx(hdc, pts[1].x, pts[1].y, None);
    let _ = LineTo(hdc, pts[0].x, pts[0].y);
    let _ = LineTo(hdc, pts[2].x, pts[2].y);

    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(edge);
    let _ = DeleteObject(edge_fill);

    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn toolbar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE => LRESULT(0), // 去边框：客户区 == 外框

        WM_NCHITTEST => {
            // 无边框：按钮行以外的区域（含上下内边距、按钮之间的间隙两侧）整块可拖动；
            // 按钮行内返回 HTCLIENT，让子控件（按钮）正常接收鼠标。
            let mut r = RECT::default();
            let mut cr = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() || GetClientRect(hwnd, &mut cr).is_err() {
                return LRESULT(HTCLIENT as isize);
            }
            let dpi = util::dpi_of(hwnd);
            let btn = util::scaled(btn_sz(), dpi);
            let gap = util::scaled(gap_l(), dpi);
            let n = BTN_ORDER.len() as i32;
            let used = btn * n + gap * (n - 1) + util::scaled(GROUP_SEP_W, dpi);
            let x0 = ((cr.right - used) / 2).max(0);
            let x = (((lp.0 & 0xFFFF) as u16 as i16 as i32) - r.left).max(0);
            let y = (((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 - r.top).max(0);
            let by = ((cr.bottom - btn) / 2).max(0);
            let in_button_band = y >= by && y < by + btn && x >= x0 && x < x0 + used;
            LRESULT(if in_button_band {
                HTCLIENT as isize
            } else {
                HTCAPTION as isize
            })
        }

        WM_CREATE => {
            let cs = lp.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let p = (*cs).lpCreateParams as isize;
                if p != 0 {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, p);
                }
            }
            let Some(d) = data_of(hwnd) else {
                return LRESULT(-1);
            };
            d.bar = hwnd;
            d.skin = theme::toolbar_skin();
            d.backdrop = theme::apply_frame(hwnd, d.skin, false);

            // 图标按钮：按钮不带可见文本（图标自绘），窗口文本保留为提示文案
            // （可访问性/自动化可读）。先以占位尺寸创建，再由 layout 统一定位。
            for (i, &id) in BTN_ORDER.iter().enumerate() {
                d.buttons[i] = add_button(hwnd, main_tip_text(id), 0, 0, 10, 10, id);
            }
            for b in d.buttons {
                d.fx.register(b);
                // 子类化以获取按钮自身的悬停/离开消息（父窗口收不到子控件上的鼠标消息）
                util::subclass(b, btn_proc);
            }
            d.tip = tooltip::Tip::new(hwnd);
            layout(hwnd);

            // 共享子工具条：顶层分层 popup（子控件才能画到主窗口之外），初始隐藏。
            // 与主工具条共享同一份 ToolbarData（USERDATA 指向同一结构），
            // 生命周期由主工具条 WM_DESTROY 显式销毁。
            let (lw, lh) = sub_logical_size(SubKind::More);
            let dpi = util::dpi_of(hwnd);
            let userdata = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut core::ffi::c_void;
            let sub = util::create_window(
                MORE_CLASS,
                "",
                WINDOW_EX_STYLE(
                    WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_LAYERED.0,
                ),
                WS_POPUP,
                0,
                0,
                util::scaled(lw, dpi),
                util::scaled(lh, dpi),
                userdata,
            )
            .unwrap_or_default();
            if !sub.is_invalid() {
                // 色键透明：尖角/圆角之外透出桌面
                let _ = SetLayeredWindowAttributes(sub, MORE_KEY, 0, LWA_COLORKEY);
                // 与工具条一致：排除出截屏捕获
                let _ = SetWindowDisplayAffinity(sub, WDA_EXCLUDEFROMCAPTURE);
                d.sub = sub;
                layout_sub(sub);
            }
            LRESULT(0)
        }

        WM_ERASEBKGND => LRESULT(1),

        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }

        WM_DRAWITEM => {
            let dis = lp.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                if let Some(d) = data_of(hwnd) {
                    let id = (wp.0 & 0xFFFF) as usize;
                    let style = if id == BTN_CLOSE {
                        BtnStyle::IconClose
                    } else {
                        BtnStyle::Icon
                    };
                    let selected = button_selected(d, id);
                    draw_icon_frame(hwnd, &*dis, style, d.skin, &d.fx, icon_of(id), selected);
                }
            }
            LRESULT(1)
        }

        WM_MOUSEMOVE => {
            if let Some(d) = data_of(hwnd) {
                d.fx.on_mouse_move(hwnd);
            }
            LRESULT(0)
        }

        // 焦点/激活切换：点击颜色面板色块或「…」子工具条按钮时，焦点会短暂
        // 离开主工具条再交还；DWM 会在这类切换后于无客户区的 WS_THICKFRAME
        // 窗口外缘留下一圈灰色边框。这里延迟到当前消息处理完再请求一次
        // 「非客户区重绘」（RDW_FRAME），把残留的灰边擦掉。
        WM_SETFOCUS | WM_NCACTIVATE => {
            let r = DefWindowProcW(hwnd, msg, wp, lp);
            // 同步重绘太早：DWM 的帧更新随后才发生，会再次画上灰边，故走队列
            let _ = PostMessageW(hwnd, WM_APP_REFRESH_FRAME, WPARAM(0), LPARAM(0));
            r
        }

        WM_APP_REFRESH_FRAME => {
            let _ = RedrawWindow(
                hwnd,
                None,
                None,
                RDW_FRAME | RDW_INVALIDATE | RDW_UPDATENOW,
            );
            LRESULT(0)
        }

        util::WM_MOUSELEAVE => {
            if let Some(d) = data_of(hwnd) {
                d.fx.on_mouse_leave(hwnd);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            let mut handled = false;
            if let Some(d) = data_of(hwnd) {
                if wp.0 == TIMER_TIP {
                    show_tip_for(d);
                    let _ = KillTimer(hwnd, TIMER_TIP);
                    handled = true;
                } else if wp.0 == TIMER_SUB_SHOW {
                    // 悬停延时到点：光标仍在原按钮上才展开（已移开则直接取消）
                    let id = d.sub_armed_id;
                    d.sub_show_armed = false;
                    d.sub_armed_id = 0;
                    if cursor_on_main_button(d, id) {
                        show_popup_for_anchor(d, id);
                    }
                    let _ = KillTimer(hwnd, TIMER_SUB_SHOW);
                    handled = true;
                } else if wp.0 == TIMER_SUB_POLL {
                    poll_popups(d);
                    handled = true;
                } else {
                    handled = d.fx.on_timer(hwnd, wp.0);
                }
            }
            if handled {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }

        WM_SETTINGCHANGE => {
            // 系统主题变化：刷新主题缓存（菜单等用 theme::current() 的窗口依赖它；
            // 工具栏皮肤固定深色，自身只需重绘）
            theme::reload();
            let _ = raw::InvalidateRect(hwnd, std::ptr::null(), 1);
            LRESULT(0)
        }

        WM_DPICHANGED => {
            let sug = lp.0 as *const RECT;
            if !sug.is_null() {
                let r = *sug;
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            layout(hwnd);
            if let Some(d) = data_of(hwnd) {
                hide_all_popups(d); // 尺寸/位置按 DPI 重算，旧浮层位置已失效
            }
            let _ = raw::InvalidateRect(hwnd, std::ptr::null(), 1);
            LRESULT(0)
        }

        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as usize;
            handle_command(hwnd, id);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if handle_annotation_wheel(hwnd, wp) {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        // 快捷键：Esc 关闭；Ctrl+T/S/C/O 分别对应贴图/保存/复制/OCR
        WM_KEYDOWN => {
            handle_shortcut(hwnd, wp.0);
            LRESULT(0)
        }
        // 字符输入（TranslateMessage 生成）：全部转发给冻结层（文字标注）。
        // 冻结层仅在文字输入中消费（追加/换行/退格）；非输入时为无操作。
        WM_CHAR => {
            if let Some(d) = data_of(hwnd) {
                let _ = PostMessageW(d.overlay, WM_APP_CHAR, wp, LPARAM(0));
            }
            LRESULT(0)
        }
        // 点击工具条空白处（按钮之间的间隙）同样收起所有子工具条。
        WM_LBUTTONDOWN => {
            if let Some(d) = data_of(hwnd) {
                hide_all_popups(d);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        // 拖动工具条（点在按钮行以外的 HTCAPTION 区）时立即收起气泡与子工具条
        WM_NCLBUTTONDOWN => {
            if let Some(d) = data_of(hwnd) {
                hide_all_popups(d);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        // 冻结层 → 工具条：工具已变化（点击工具按钮/Esc/工具键后回传真实工具）
        WM_APP_TOOL_CHANGED => {
            if let Some(d) = data_of(hwnd) {
                d.active_tool = tool_button_id_from_wp(wp.0);
            }
            refresh_buttons(hwnd);
            LRESULT(0)
        }

        // 冻结层 → 工具条：Esc 交还，关闭会话
        WM_APP_ESC_CLOSE => {
            close_bar(hwnd);
            LRESULT(0)
        }

        // 颜色面板 → 工具条：面板已销毁（选色自动收起 / Esc / 主动关闭），
        // 清空缓存句柄；只处理当前面板，避免误清刚重新打开的新面板。
        WM_APP_STYLE_PANEL_CLOSED => {
            if let Some(d) = data_of(hwnd) {
                if d.style_panel.0 as isize == lp.0 {
                    d.style_panel = HWND::default();
                    stop_poll_if_idle(d);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if p != 0 {
                let mut data = Box::from_raw(p as *mut ToolbarData);
                if !data.style_panel.is_invalid() {
                    let _ = DestroyWindow(data.style_panel);
                }
                // 子工具条是独立顶层窗口，需显式销毁（其 WM_NCDESTROY 只清 USERDATA，
                // 不重复释放这份共享数据）；先注销子类化登记，再停掉它的悬停定时器。
                if !data.font_combo.is_invalid() {
                    util::unsubclass(data.font_combo);
                }
                if !data.sub.is_invalid() {
                    for b in data.sub_buttons {
                        util::unsubclass(b);
                    }
                    data.sub_fx.stop(data.sub);
                    let _ = DestroyWindow(data.sub);
                    data.sub = HWND::default();
                }
                data.fx.stop(hwnd);
                data.tip.destroy();
                // 按钮随父窗口销毁；先注销子类化登记（工具条每次截图都会重建）
                for b in data.buttons {
                    util::unsubclass(b);
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 关闭工具条：隐藏后结束其模态消息循环，回到主循环。
/// 贴图 / 保存 / 复制 / OCR / 关闭五个动作都走这里 —— 工具条是「一次性操作条」。
unsafe fn close_bar(hwnd: HWND) {
    if let Some(d) = data_of(hwnd) {
        hide_all_popups(d);
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
    PostQuitMessage(0);
}

/// 切换颜色面板（按钮 17）：隐藏 → 打开；已打开 → 关闭。
unsafe fn toggle_style_panel(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    if popup_style_visible(d) {
        hide_style_panel(d);
        return;
    }
    if !config::get().style_panel {
        crate::toast::show("颜色面板已在配置中关闭");
        return;
    }
    show_style_panel(d);
}

/// 打开颜色面板（悬停延时或点击触发）。
unsafe fn show_style_panel(d: &mut ToolbarData) {
    if !config::get().style_panel || popup_style_visible(d) {
        return;
    }
    // 颜色面板与共享子工具条不会同时弹出
    hide_sub(d);
    let p = crate::style_panel::toggle(d.bar, d.overlay);
    if !p.is_invalid() {
        d.style_panel = p;
        d.tip.hide();
        d.tip_shown = false;
        d.tip_for = 0;
        d.tip_hwnd = HWND::default();
        let _ = KillTimer(d.bar, TIMER_TIP);
        start_poll(d);
    }
}

/// 关闭颜色面板。
unsafe fn hide_style_panel(d: &mut ToolbarData) {
    if !d.style_panel.is_invalid() {
        let _ = DestroyWindow(d.style_panel);
        d.style_panel = HWND::default();
    }
    stop_poll_if_idle(d);
}

/// Ctrl 是否按下（读物理按键状态，不依赖焦点/键序）。
unsafe fn ctrl_down() -> bool {
    (GetKeyState(VK_CONTROL) as u16) & 0x8000 != 0
}

unsafe fn shift_down() -> bool {
    (GetKeyState(VK_SHIFT) as u16) & 0x8000 != 0
}

/// 绘制一个图标按钮的**整帧**（`WM_DRAWITEM` 与预览测试共用）。
///
/// 双缓冲：整帧（圆角底 + 抗锯齿图标）先合成到内存位图，再一次性 `BitBlt` 上屏。
/// 直接画到按钮 DC 会分两批落屏 —— GDI+ 开始绘制前会先冲刷 GDI 队列，于是"底色"先到屏、
/// "图标"后到屏；悬停动画 60fps 下看起来就是图标一闪一闪。
unsafe fn draw_icon_frame(
    parent: HWND,
    dis: &DRAWITEMSTRUCT,
    style: BtnStyle,
    skin: theme::Theme,
    fx: &util::HoverFx,
    icon: icons::Icon,
    selected: bool,
) {
    let r = dis.rcItem;
    let (bw, bh) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
    // 选中态只用强调色图标表示，背景仍按真实的悬停/按下插值绘制：
    // 旧实现给选中按钮强制 `v = 1.0`，于是首次点击工具按钮后浅灰「悬停底」
    // 会一直留在按钮上（看起来就是图标周围多了一圈灰框）。这里不再强制，
    // 灰色圆角底只在光标悬停或按下时出现，选中信息的表达不受影响。
    let v = fx.val(dis.hwndItem);
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    let (_, ink) = util::btn_colors(style, skin, v, pressed);
    let ink = if selected { skin.accent } else { ink };

    let mem = CreateCompatibleDC(dis.hDC);
    let bmp = CreateCompatibleBitmap(dis.hDC, bw, bh);
    if !mem.is_invalid() && !bmp.is_invalid() {
        let old_bmp = SelectObject(mem, bmp);
        let full = RECT {
            left: 0,
            top: 0,
            right: bw,
            bottom: bh,
        };
        // 先铺父窗口卡片底色：圆角以外的四角与工具条底色一致（否则是未初始化像素）
        let card = CreateSolidBrush(skin.card);
        FillRect(mem, &full, card);
        let _ = DeleteObject(card);

        let mut dis2 = *dis;
        dis2.hDC = mem;
        dis2.rcItem = full;
        util::draw_od_button(parent, &dis2, v, style, skin);
        icons::draw(mem, full, icon, ink);

        let _ = BitBlt(dis.hDC, r.left, r.top, bw, bh, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
    } else {
        // 退让：拿不到离屏资源时直接绘制（宁可闪一下，也不能留下空白按钮）
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp);
        }
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
        util::draw_od_button(parent, dis, v, style, skin);
        icons::draw(dis.hDC, r, icon, ink);
    }
}

/// 处理工具条快捷键。返回 true 表示已消费该按键。
///
/// 映射（见 5.5）：Esc / Ctrl+Z / Ctrl+Y / Delete → 转发冻结层 `WM_APP_KEY`；
/// R/A/N/B/H/T（无 Ctrl）→ 工具切换；Ctrl+T/S/C/O → 贴图/保存/复制/OCR。
/// **文字输入中**字母键放行（返回 false），让 WM_CHAR 流向冻结层作正文输入。
unsafe fn handle_shortcut(hwnd: HWND, vk: usize) -> bool {
    let Some(d) = data_of(hwnd) else {
        return false;
    };
    let overlay = d.overlay;
    match vk {
        // Esc 不再本地直接关闭：交由冻结层按标注状态决定（取消绘制/取消选中/交还关闭）
        VK_ESCAPE => {
            if PostMessageW(overlay, WM_APP_KEY, WPARAM(VK_ESCAPE), LPARAM(0)).is_err() {
                close_bar(hwnd); // 冻结层不可达时兜底关闭
            }
            true
        }
        VK_T if ctrl_down() => {
            handle_command(hwnd, BTN_PIN);
            true
        }
        VK_S if ctrl_down() => {
            handle_command(hwnd, BTN_SAVE);
            true
        }
        VK_C if ctrl_down() => {
            handle_command(hwnd, BTN_COPY);
            true
        }
        VK_O if ctrl_down() => {
            handle_command(hwnd, BTN_OCR);
            true
        }
        // 标注工具键（无 Ctrl）。文字输入中放行：字母是正文，不是工具键
        // （同步查询冻结层；输入中返回 false 不消费，WM_CHAR 随后流向冻结层）
        VK_R | VK_A | VK_N | VK_E | VK_L | VK_P | VK_B | VK_H | VK_T | VK_X | VK_M
            if !ctrl_down() =>
        {
            if overlay_typing(overlay) {
                return false;
            }
            handle_command(hwnd, button_id_for_key(vk));
            true
        }
        // 撤销/重做：Ctrl+Z / Ctrl+Y（转发冻结层）
        VK_Z | VK_Y => {
            if !ctrl_down() {
                return false;
            }
            let _ = PostMessageW(overlay, WM_APP_KEY, WPARAM(vk), LPARAM(1));
            true
        }
        // 清空全部：Ctrl+Shift+Delete（转发冻结层，一条撤销记录）
        VK_DELETE if ctrl_down() && shift_down() => {
            let _ = PostMessageW(overlay, crate::overlay::WM_APP_CLEAR_ALL, WPARAM(0), LPARAM(0));
            true
        }
        // 删除选中标注：Delete（转发冻结层，无 Ctrl）
        VK_DELETE => {
            let _ = PostMessageW(overlay, WM_APP_KEY, WPARAM(VK_DELETE), LPARAM(0));
            true
        }
        _ => false,
    }
}

/// 分组竖杠的 X 坐标（客户区）：第 [`ANNOT_GROUP_START`] 个按钮之前的
/// [`GROUP_SEP_W`] 占位正中央。
fn separator_x(x0: i32, btn: i32, gap: i32, sep: i32) -> i32 {
    x0 + ANNOT_GROUP_START as i32 * (btn + gap) + sep / 2
}

/// 按钮 → Ctrl+滚轮调整类别编码：1 线宽 / 2 字号 / 3 序号半径 / 4 橡皮容差。
///
/// 与 [`annot::SizeKind::for_tool`] 共用同一份工具映射，避免两处漂移。
fn size_kind_for_button(id: usize) -> Option<usize> {
    annot::SizeKind::for_tool(button_id_tool(id)).map(|k| k.code() as usize)
}

/// Ctrl+滚轮调整当前选中标注工具的尺寸；返回 true 表示已消费滚轮。
///
/// 只要工具条上有选中的标注工具，光标位置不再受限（在画布、面板或任意
/// 按钮上滚动均可）；未选中工具时，退回按光标下的标注按钮调整。
unsafe fn handle_annotation_wheel(hwnd: HWND, wp: WPARAM) -> bool {
    if !ctrl_down() {
        return false;
    }
    let raw_delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    if raw_delta == 0 {
        return false;
    }
    let Some(d) = data_of(hwnd) else { return false };

    // 1) 已选中的标注工具优先（不要求光标悬停在按钮上）。
    // 2) 未选中工具时，再看光标下的标注按钮（便于直接悬停微调）。
    let mut kind = size_kind_for_button(d.active_tool);
    if kind.is_none() {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            let h = WindowFromPoint(pt);
            let parent = GetParent(h).unwrap_or_default();
            // 主工具条按钮与「…」子工具条按钮都算悬停在标注按钮上
            if !h.is_invalid() && (parent == hwnd || parent == d.sub) {
                kind = size_kind_for_button(GetDlgCtrlID(h) as usize);
            }
        }
    }
    let Some(kind) = kind else {
        return false;
    };
    let delta = if raw_delta > 0 { 1 } else { -1 };
    let _ = PostMessageW(
        d.overlay,
        WM_APP_ADJUST_SIZE,
        WPARAM(kind),
        LPARAM(delta),
    );
    true
}

/// 工具键 VK → 按钮控件 ID。
fn button_id_for_key(vk: usize) -> usize {
    match vk {
        VK_R => BTN_RECT,
        VK_A => BTN_ARROW,
        VK_N => BTN_STEP,
        VK_E => BTN_ELLIPSE,
        VK_L => BTN_LINE,
        VK_P => BTN_PEN,
        VK_B => BTN_MOSAIC,
        VK_H => BTN_HIGHLIGHT,
        VK_T => BTN_TEXT,
        VK_X => BTN_ERASER,
        VK_M => BTN_WATERMARK,
        _ => 0,
    }
}

/// 冻结层是否处于文字输入中（同步查询；冻结层不可达视为否）。
unsafe fn overlay_typing(overlay: HWND) -> bool {
    overlay.0 as usize != 0
        && SendMessageW(overlay, WM_APP_QUERY_TYPING, WPARAM(0), LPARAM(0)).0 != 0
}

/// 控件 ID → 图标。
fn icon_of(id: usize) -> icons::Icon {
    match id {
        BTN_PIN => icons::Icon::Pin,
        BTN_SAVE => icons::Icon::Save,
        BTN_COPY => icons::Icon::Copy,
        BTN_OCR => icons::Icon::Ocr,
        BTN_RECT => icons::Icon::Rect,
        BTN_ARROW => icons::Icon::Arrow,
        BTN_STEP => icons::Icon::StepNo,
        BTN_ELLIPSE => icons::Icon::Ellipse,
        BTN_LINE => icons::Icon::Line,
        BTN_PEN => icons::Icon::Pen,
        BTN_MOSAIC => icons::Icon::Mosaic,
        BTN_HIGHLIGHT => icons::Icon::Highlight,
        BTN_TEXT => icons::Icon::Text,
        BTN_ERASER => icons::Icon::Eraser,
        BTN_WATERMARK => icons::Icon::Watermark,
        BTN_STYLE => icons::Icon::Palette,
        BTN_MORE => icons::Icon::More,
        BTN_TEXT_BOLD => icons::Icon::Bold,
        BTN_TEXT_ITALIC => icons::Icon::Italic,
        BTN_TEXT_STROKE => icons::Icon::TextStroke,
        _ => icons::Icon::Close,
    }
}

/// 工具按钮 ID → [`annot::Tool`]。
fn button_id_tool(id: usize) -> annot::Tool {
    match id {
        BTN_RECT => annot::Tool::Rect,
        BTN_ARROW => annot::Tool::Arrow,
        BTN_STEP => annot::Tool::StepNo,
        BTN_ELLIPSE => annot::Tool::Ellipse,
        BTN_LINE => annot::Tool::Line,
        BTN_PEN => annot::Tool::Pen,
        BTN_MOSAIC => annot::Tool::Mosaic,
        BTN_HIGHLIGHT => annot::Tool::Highlight,
        BTN_TEXT => annot::Tool::Text,
        BTN_ERASER => annot::Tool::Eraser,
        BTN_WATERMARK => annot::Tool::Watermark,
        _ => annot::Tool::None,
    }
}

/// [`annot::Tool`] 判别值（`WM_APP_TOOL_CHANGED` 的 WPARAM）→ 工具按钮 ID。
fn tool_button_id_from_wp(v: usize) -> usize {
    match v {
        1 => BTN_RECT,
        2 => BTN_ARROW,
        3 => BTN_STEP,
        4 => BTN_ELLIPSE,
        5 => BTN_LINE,
        6 => BTN_PEN,
        7 => BTN_MOSAIC,
        8 => BTN_HIGHLIGHT,
        9 => BTN_TEXT,
        10 => BTN_ERASER,
        11 => BTN_WATERMARK,
        _ => 0,
    }
}

/// 主工具条按钮 ID → 按钮句柄。
fn main_button(d: &ToolbarData, id: usize) -> Option<HWND> {
    BTN_ORDER
        .iter()
        .position(|&x| x == id)
        .map(|i| d.buttons[i])
}

/// 子工具条按钮 ID → 按钮句柄。
fn sub_button(d: &ToolbarData, id: usize) -> Option<HWND> {
    SUB_IDS
        .iter()
        .position(|&x| x == id)
        .map(|i| d.sub_buttons[i])
}

/// 按钮是否处于「激活」高亮：标注工具取冻结层回传值，文字开关取本地状态。
fn button_selected(d: &ToolbarData, id: usize) -> bool {
    (d.active_tool != 0 && id == d.active_tool)
        || (id == BTN_TEXT_BOLD && d.text_bold)
        || (id == BTN_TEXT_ITALIC && d.text_italic)
        || (id == BTN_TEXT_STROKE && d.text_stroke)
}

/// 重绘全部工具按钮（激活工具/文字开关变化时同步高亮；主工具条与子工具条都要刷新）。
unsafe fn refresh_buttons(hwnd: HWND) {
    if let Some(d) = data_of(hwnd) {
        for b in d.buttons {
            let _ = raw::InvalidateRect(b, std::ptr::null(), 1);
        }
        for b in d.sub_buttons {
            let _ = raw::InvalidateRect(b, std::ptr::null(), 1);
        }
    }
}

/// 光标是否落在按钮矩形内。
unsafe fn cursor_on_button(btn: HWND) -> bool {
    let mut pt = POINT::default();
    let mut br = RECT::default();
    if GetCursorPos(&mut pt).is_err() || GetWindowRect(btn, &mut br).is_err() {
        return false;
    }
    pt.x >= br.left && pt.x < br.right && pt.y >= br.top && pt.y < br.bottom
}

/// 光标是否悬停在主工具条指定按钮上。
unsafe fn cursor_on_main_button(d: &ToolbarData, id: usize) -> bool {
    main_button(d, id).is_some_and(|b| cursor_on_button(b))
}

fn popup_sub_visible(d: &ToolbarData) -> bool {
    !d.sub.is_invalid() && d.sub_kind != SubKind::None && unsafe { IsWindowVisible(d.sub).as_bool() }
}

fn popup_style_visible(d: &ToolbarData) -> bool {
    !d.style_panel.is_invalid() && unsafe { IsWindowVisible(d.style_panel).as_bool() }
}

fn popup_visible(d: &ToolbarData) -> bool {
    popup_sub_visible(d) || popup_style_visible(d)
}

/// 字体下拉框是否已展开（展开时列表是独立弹窗，不能按子工具条矩形判断）。
unsafe fn font_dropdown_open(d: &ToolbarData) -> bool {
    d.sub_kind == SubKind::Text
        && !d.font_combo.is_invalid()
        && SendMessageW(d.font_combo, CB_GETDROPPEDSTATE, WPARAM(0), LPARAM(0)).0 != 0
}

/// 光标是否仍停留在共享子工具条（按钮 + 浮层 + 按钮到浮层的走廊）。
unsafe fn cursor_over_sub_area(d: &ToolbarData) -> bool {
    if !popup_sub_visible(d) {
        return false;
    }
    if font_dropdown_open(d) {
        return true;
    }
    let Some(anchor) = main_button(d, d.sub_anchor) else {
        return false;
    };
    let mut pt = POINT::default();
    let mut ar = RECT::default();
    let mut sr = RECT::default();
    if GetCursorPos(&mut pt).is_err()
        || GetWindowRect(anchor, &mut ar).is_err()
        || GetWindowRect(d.sub, &mut sr).is_err()
    {
        return false;
    }
    if pt.x >= ar.left && pt.x < ar.right && pt.y >= ar.top && pt.y < ar.bottom {
        return true;
    }
    if pt.x >= sr.left && pt.x < sr.right && pt.y >= sr.top && pt.y < sr.bottom {
        return true;
    }
    // 按钮到浮层之间的走廊（跨过工具条内边距时不立刻收起）
    pt.x >= sr.left
        && pt.x < sr.right
        && pt.y >= ar.top.min(sr.top)
        && pt.y < ar.bottom.max(sr.bottom)
}

/// 光标是否仍停留在颜色面板（按钮 + 面板 + 走廊）。
unsafe fn cursor_over_style_area(d: &ToolbarData) -> bool {
    if !popup_style_visible(d) {
        return false;
    }
    let Some(anchor) = main_button(d, BTN_STYLE) else {
        return false;
    };
    let mut pt = POINT::default();
    let mut ar = RECT::default();
    let mut pr = RECT::default();
    if GetCursorPos(&mut pt).is_err()
        || GetWindowRect(anchor, &mut ar).is_err()
        || GetWindowRect(d.style_panel, &mut pr).is_err()
    {
        return false;
    }
    if pt.x >= ar.left && pt.x < ar.right && pt.y >= ar.top && pt.y < ar.bottom {
        return true;
    }
    if pt.x >= pr.left && pt.x < pr.right && pt.y >= pr.top && pt.y < pr.bottom {
        return true;
    }
    pt.x >= pr.left
        && pt.x < pr.right
        && pt.y >= ar.top.min(pr.top)
        && pt.y < ar.bottom.max(pr.bottom)
}

unsafe fn cursor_over_popup_area(d: &ToolbarData) -> bool {
    cursor_over_sub_area(d) || cursor_over_style_area(d)
}

/// 点击是否落在子工具条自己的可交互控件上（用于「点击别处收起」判定）。
///
/// 点击锚定按钮本身也视为工具条控件（展开矩形/箭头/文字后再次点击该按钮不收起）。
unsafe fn cursor_over_popup_control(d: &ToolbarData) -> bool {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return false;
    }
    let h = WindowFromPoint(pt);
    if popup_sub_visible(d) {
        // 字体列表是原生下拉弹窗（独立窗口），点击其条目属于子工具条操作
        if font_dropdown_open(d) && is_combo_list_window(h) {
            return true;
        }
        // 矩形/箭头/文字按钮点击本身要展开（不能先被点击外部判定收起）；
        // 「更多」按钮没有点击展开语义，点击它应视为「点击别处」。
        let anchor_expands = matches!(d.sub_anchor, BTN_RECT | BTN_ARROW | BTN_TEXT);
        if (anchor_expands && cursor_on_main_button(d, d.sub_anchor)) || h == d.font_combo {
            return true;
        }
        if !h.is_invalid() && GetParent(h).is_ok_and(|p| p == d.sub) {
            return true;
        }
    }
    if popup_style_visible(d) {
        if cursor_on_main_button(d, BTN_STYLE) {
            return true;
        }
        if !h.is_invalid() && GetParent(h).is_ok_and(|p| p == d.style_panel) {
            return true;
        }
    }
    false
}

/// 原生 combo 的下拉列表窗口类名固定为 `ComboLBox`。
unsafe fn is_combo_list_window(h: HWND) -> bool {
    if h.is_invalid() {
        return false;
    }
    let mut buf = [0u16; 32];
    let n = GetClassNameW(h, &mut buf);
    let name: String = buf[..n as usize]
        .iter()
        .filter_map(|&c| char::from_u32(c as u32))
        .collect();
    name == "ComboLBox"
}

unsafe fn mouse_left_down() -> bool {
    (GetAsyncKeyState(VK_LBUTTON_CODE) as u16) & 0x8000 != 0
}

/// 子工具条/颜色面板可见期间启动轮询：光标离开或点击别处即收起。
unsafe fn start_poll(d: &mut ToolbarData) {
    if !popup_visible(d) {
        stop_poll_if_idle(d);
        return;
    }
    d.sub_mouse_down = mouse_left_down();
    d.sub_miss = 0;
    let _ = SetTimer(d.bar, TIMER_SUB_POLL, SUB_POLL_MS, None);
}

unsafe fn stop_poll_if_idle(d: &ToolbarData) {
    if !popup_visible(d) {
        let _ = KillTimer(d.bar, TIMER_SUB_POLL);
    }
}

/// 轮询：点击浮层之外的任意位置 → 立即收起；光标离开浮层过久 → 收起。
unsafe fn poll_popups(d: &mut ToolbarData) {
    if !popup_visible(d) {
        let _ = KillTimer(d.bar, TIMER_SUB_POLL);
        return;
    }
    let down = mouse_left_down();
    let pressed = down && !d.sub_mouse_down;
    d.sub_mouse_down = down;
    if pressed && !cursor_over_popup_control(d) {
        hide_all_popups(d);
        return;
    }
    if cursor_over_popup_area(d) {
        d.sub_miss = 0;
    } else {
        d.sub_miss = d.sub_miss.saturating_add(1);
        if d.sub_miss >= SUB_HIDE_MISSES {
            hide_all_popups(d);
            return;
        }
    }
    stop_poll_if_idle(d);
}

/// 收起所有子工具条（含颜色面板）与提示气泡。
unsafe fn hide_all_popups(d: &mut ToolbarData) {
    hide_sub(d);
    hide_style_panel(d);
    d.tip.hide();
    d.tip_shown = false;
    d.tip_for = 0;
    d.tip_hwnd = HWND::default();
    let _ = KillTimer(d.bar, TIMER_TIP);
}

/// 按主工具条按钮展开对应的子工具条（`BTN_STYLE` 对应颜色面板）。
unsafe fn show_popup_for_anchor(d: &mut ToolbarData, id: usize) {
    if id == BTN_STYLE {
        if !popup_style_visible(d) {
            show_style_panel(d);
        }
    } else if let Some(kind) = sub_kind_for_anchor(id) {
        show_sub(d, kind, id);
    }
}

unsafe fn close_font_dropdown(d: &ToolbarData) {
    if !d.font_combo.is_invalid() {
        let _ = SendMessageW(d.font_combo, CB_SHOWDROPDOWN, WPARAM(0), LPARAM(0));
    }
}

/// 展开共享子工具条：定位在主工具条下方并让尖角对准锚定按钮；
/// 下方空间不足时翻到工具条上方（尖角改为朝下）。
unsafe fn show_sub(d: &mut ToolbarData, kind: SubKind, anchor_id: usize) {
    if d.sub.is_invalid() || kind == SubKind::None {
        return;
    }
    if popup_sub_visible(d) && d.sub_kind == kind && d.sub_anchor == anchor_id {
        start_poll(d);
        return;
    }
    hide_style_panel(d);
    close_font_dropdown(d);

    let hwnd = d.bar;
    let dpi = util::dpi_of(hwnd);
    let s = |v: i32| util::scaled(v, dpi);
    let (lw, lh) = sub_logical_size(kind);
    let (w, h) = (s(lw), s(lh));
    let Some(btn) = main_button(d, anchor_id) else {
        return;
    };
    let mut br = RECT::default();
    let mut tr = RECT::default();
    if GetWindowRect(btn, &mut br).is_err() || GetWindowRect(hwnd, &mut tr).is_err() {
        return;
    }

    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    let x = (br.left + ((br.right - br.left) - w) / 2).clamp(vx, (vx + vw - w).max(vx));
    let (y, above) = if tr.bottom + h <= vy + vh {
        (tr.bottom, false)
    } else {
        ((tr.top - h).max(vy), true)
    };

    d.sub_kind = kind;
    d.sub_anchor = anchor_id;
    d.sub_above = above;
    let r = s(radius_l()).max(1);
    let half = s(MORE_TAIL_HALF_W);
    let tip_x = br.left + (br.right - br.left) / 2 - x;
    d.sub_tail_x = tip_x.clamp(r + half, (w - r - half).max(r + half));

    let _ = SetWindowPos(
        d.sub,
        HWND_TOPMOST,
        x,
        y,
        w,
        h,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    layout_sub_with(d, d.sub);
    let _ = raw::InvalidateRect(d.sub, std::ptr::null(), 1);

    // 浮层已展开：收起锚定按钮的悬停提示，并开始侦测光标离开/点击别处
    d.tip.hide();
    d.tip_shown = false;
    d.tip_for = 0;
    d.tip_hwnd = HWND::default();
    let _ = KillTimer(hwnd, TIMER_TIP);
    let _ = KillTimer(hwnd, TIMER_SUB_SHOW);
    d.sub_show_armed = false;
    d.sub_armed_id = 0;
    start_poll(d);
}

/// 收起共享子工具条（光标离开/点击别处后由轮询定时器调用）。
unsafe fn hide_sub(d: &mut ToolbarData) {
    close_font_dropdown(d);
    if !d.sub.is_invalid() {
        let _ = ShowWindow(d.sub, SW_HIDE);
        // 复位悬停淡出状态，避免下次展开时沿用旧的悬停底
        d.sub_fx.stop(d.sub);
    }
    d.sub_kind = SubKind::None;
    d.sub_anchor = 0;
    let _ = KillTimer(d.bar, TIMER_SUB_SHOW);
    d.sub_show_armed = false;
    d.sub_armed_id = 0;
    d.sub_miss = 0;
    screen_tip_reset(d);
    stop_poll_if_idle(d);
}

unsafe fn screen_tip_reset(d: &mut ToolbarData) {
    d.tip.hide();
    d.tip_shown = false;
    d.tip_for = 0;
    d.tip_hwnd = HWND::default();
}

/// 导出前把标注合成进成品图（无标注时零拷贝，见 6）。
unsafe fn compose(d: &ToolbarData) -> Arc<CapturedImage> {
    if d.sel.is_null() {
        return Arc::clone(&d.image);
    }
    let doc = (*d.sel).doc(); // 短借用：立即使用，不跨消息持有
    Arc::new(annot::composite(&d.image, doc))
}

/// 弹出 `d.tip_for` 对应按钮的提示气泡（延时到点后由其屏幕矩形定位）。
unsafe fn show_tip_for(d: &mut ToolbarData) {
    let btn = if !d.tip_hwnd.is_invalid() {
        d.tip_hwnd
    } else if let Some(b) = main_button(d, d.tip_for) {
        b
    } else if let Some(b) = sub_button(d, d.tip_for) {
        b
    } else {
        return;
    };
    let mut r = RECT::default();
    if GetWindowRect(btn, &mut r).is_err() {
        return;
    }
    // 主工具条锚定按钮用组合提示（如「矩形/椭圆」）；子按钮用单项名称。
    let is_main = GetParent(btn).is_ok_and(|p| p == d.bar);
    let label = if is_main {
        main_tip_text(d.tip_for)
    } else {
        tip_text(d.tip_for)
    };
    d.tip
        .show(label, tip_shortcut_text(d.tip_for, is_main), &r);
    d.tip_shown = true;
}

/// 「…」子工具条窗口过程：与主工具条共享同一份 ToolbarData，
/// 命令/快捷键统一回投给主工具条处理（保证动作语义只实现一份）。
unsafe extern "system" fn more_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = lp.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let p = (*cs).lpCreateParams as isize;
                if p != 0 {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, p);
                }
            }
            let Some(d) = data_of(hwnd) else {
                return LRESULT(-1);
            };
            for (i, &id) in SUB_IDS.iter().enumerate() {
                d.sub_buttons[i] = add_button(hwnd, tip_text(id), 0, 0, 10, 10, id);
            }
            for b in d.sub_buttons {
                d.sub_fx.register(b);
                util::subclass(b, btn_proc);
            }
            // 字体下拉框（文字子工具条专用），初始隐藏
            let (combo, faces) = create_font_combo(hwnd);
            d.font_combo = combo;
            d.font_faces = faces;
            if !d.font_combo.is_invalid() {
                util::subclass(d.font_combo, combo_proc);
            }
            layout_sub(hwnd);
            LRESULT(0)
        }

        // 浮层不抢焦点：点击子按钮只切换工具，不打断冻结层的键盘输入/快捷键
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),

        WM_PAINT => {
            paint_sub(hwnd);
            LRESULT(0)
        }

        // 点击浮层背景（而不是按钮）也视为「点击别处」：收起浮层
        WM_LBUTTONDOWN => {
            if let Some(d) = data_of(hwnd) {
                if !cursor_over_sub_button(d) {
                    hide_all_popups(d);
                }
            }
            LRESULT(0)
        }

        WM_DRAWITEM => {
            let dis = lp.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                if let Some(d) = data_of(hwnd) {
                    let id = (wp.0 & 0xFFFF) as usize;
                    let selected = button_selected(d, id);
                    draw_icon_frame(
                        d.bar,
                        &*dis,
                        BtnStyle::Icon,
                        d.skin,
                        &d.sub_fx,
                        icon_of(id),
                        selected,
                    );
                }
            }
            LRESULT(1)
        }

        WM_MOUSEMOVE => {
            if let Some(d) = data_of(hwnd) {
                d.sub_fx.on_mouse_move(hwnd);
            }
            LRESULT(0)
        }

        util::WM_MOUSELEAVE => {
            if let Some(d) = data_of(hwnd) {
                d.sub_fx.on_mouse_leave(hwnd);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            let mut handled = false;
            if let Some(d) = data_of(hwnd) {
                if wp.0 == TIMER_TIP {
                    show_tip_for(d);
                    let _ = KillTimer(hwnd, TIMER_TIP);
                    handled = true;
                } else {
                    handled = d.sub_fx.on_timer(hwnd, wp.0);
                }
            }
            if handled {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }

        WM_COMMAND => {
            if let Some(d) = data_of(hwnd) {
                let id = (wp.0 & 0xFFFF) as usize;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                if id == BTN_TEXT_FONT && code == CBN_SELCHANGE {
                    apply_font_choice(d);
                    // 子工具条也可能是悬停弹出的（此时主工具条「文字」按钮未被点击），
                    // 选中字体后同样进入文字标注，避免用户落点却仍是旧工具。
                    handle_command(d.bar, BTN_TEXT);
                } else if id == BTN_TEXT_BOLD
                    || id == BTN_TEXT_ITALIC
                    || id == BTN_TEXT_STROKE
                {
                    toggle_text_style(d, id);
                    // 悬停弹出时主按钮未点击：点样式按钮也自动进入文字标注
                    handle_command(d.bar, BTN_TEXT);
                    // 与其他子工具条一致：点中任一按钮后收起浮层
                    hide_sub(d);
                } else if SUB_IDS.contains(&id) {
                    handle_command(d.bar, id);
                    // 点中任一子工具条按钮后收起浮层（避免遮住画布操作区）
                    hide_sub(d);
                }
            }
            LRESULT(0)
        }

        WM_MOUSEWHEEL => {
            if let Some(d) = data_of(hwnd) {
                if handle_annotation_wheel(d.bar, wp) {
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        WM_KEYDOWN => {
            if let Some(d) = data_of(hwnd) {
                handle_shortcut(d.bar, wp.0);
            }
            LRESULT(0)
        }

        // 共享数据归主工具条释放：这里只断开指针，避免二次释放/悬垂访问
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 图标按钮的窗口过程：补齐父窗口收不到的按钮级鼠标消息（悬停提示 + 高亮）。
/// 除这几条外全部转发给按钮原过程，保证点击/键盘等行为不变。
unsafe extern "system" fn btn_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let parent = GetParent(hwnd).unwrap_or_default();
    if !parent.is_invalid() {
        let id = GetDlgCtrlID(hwnd) as usize;
        if let Some(d) = data_of(parent) {
            // 主工具条与「…」子工具条共享数据，但各用一份悬停引擎；
            // 子工具条的宿主是浮层窗口，命令统一用 d.bar 回投给主工具条。
            let is_sub = parent != d.bar;
            match msg {
                WM_MOUSEMOVE => {
                    if is_sub {
                        d.sub_fx.on_mouse_move(parent);
                    } else {
                        // 悬停高亮：父窗口在按钮上方收不到 WM_MOUSEMOVE，这里代它驱动
                        d.fx.on_mouse_move(parent);
                    }
                    // 移到另一个按钮：旧气泡若尚未消失，必须先隐藏，再为新按钮重新计时；
                    // 保证同一时刻只显示最新的 tip。
                    if d.tip_for != id || d.tip_hwnd != hwnd {
                        if d.tip_shown {
                            d.tip.hide();
                            d.tip_shown = false;
                        }
                        d.tip_for = id;
                        d.tip_hwnd = hwnd;
                        // 会弹出子工具条的锚定按钮：提示必须在 1 秒展开前出现，
                        // 因此把延时上限压到展开前 200ms（默认 450ms 不受影响）。
                        let opens_sub = !is_sub
                            && (id == BTN_MORE
                                || id == BTN_STYLE
                                || sub_kind_for_anchor(id).is_some());
                        let mut delay = crate::config::get().ui_tooltip_delay_ms;
                        if opens_sub {
                            delay = delay.min(SUB_SHOW_DELAY_MS.saturating_sub(200));
                        }
                        let _ = SetTimer(parent, TIMER_TIP, delay, None);
                    }
                    // 悬停主工具条按钮超过 1 秒：展开对应子工具条。
                    // 只对主工具条按钮武装；子工具条内部不再嵌套弹层。
                    if !is_sub {
                        let should_arm = if id == BTN_STYLE {
                            !popup_style_visible(d)
                        } else {
                            sub_kind_for_anchor(id).is_some_and(|_| {
                                !(popup_sub_visible(d) && d.sub_anchor == id)
                            })
                        };
                        if should_arm && (!d.sub_show_armed || d.sub_armed_id != id) {
                            d.sub_show_armed = true;
                            d.sub_armed_id = id;
                            let _ =
                                SetTimer(d.bar, TIMER_SUB_SHOW, SUB_SHOW_DELAY_MS, None);
                        }
                    }
                }
                util::WM_MOUSELEAVE => {
                    if is_sub {
                        // 收尾高亮轮询（移到父窗口其它区域时会由父窗口的 WM_MOUSEMOVE 重新开启）
                        d.sub_fx.on_mouse_leave(parent);
                    } else {
                        d.fx.on_mouse_leave(parent);
                    }
                    d.tip.hide();
                    d.tip_shown = false;
                    d.tip_for = 0;
                    d.tip_hwnd = HWND::default();
                    let _ = KillTimer(parent, TIMER_TIP);
                    if d.sub_armed_id == id {
                        d.sub_show_armed = false;
                        d.sub_armed_id = 0;
                        let _ = KillTimer(d.bar, TIMER_SUB_SHOW);
                    }
                }
                WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                    d.tip.hide();
                    d.tip_shown = false;
                    // 矩形/箭头/文字按钮：点击即展开对应子工具条（无需等待 1 秒）
                    if msg == WM_LBUTTONDOWN
                        && !is_sub
                        && matches!(id, BTN_RECT | BTN_ARROW | BTN_TEXT)
                    {
                        show_popup_for_anchor(d, id);
                    }
                }
                // 按钮持有焦点/位于光标下时，Ctrl+滚轮调整标注尺寸
                WM_MOUSEWHEEL => {
                    if handle_annotation_wheel(d.bar, wp) {
                        return LRESULT(0);
                    }
                }
                // 按钮持有焦点时快捷键同样生效（转发给工具条的统一处理）；
                // 守卫为真即表示该键已被消费，直接返回，不再转发给按钮原过程。
                WM_KEYDOWN => {
                    if handle_shortcut(d.bar, wp.0) {
                        return LRESULT(0);
                    }
                }
                _ => {}
            }
        }
        // 仅在**真实光标**落在按钮内时才武装离开跟踪。
        // 若光标不在窗口内，TrackMouseEvent 会立刻回一个 WM_MOUSELEAVE，
        // 把刚武装的提示延时一并取消（也让消息驱动的自动化测试无法验证提示）。
        if msg == WM_MOUSEMOVE {
            let mut pt = POINT::default();
            let mut br = RECT::default();
            if GetCursorPos(&mut pt).is_ok()
                && GetWindowRect(hwnd, &mut br).is_ok()
                && pt.x >= br.left
                && pt.x < br.right
                && pt.y >= br.top
                && pt.y < br.bottom
            {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
        }
    }
    util::call_orig(hwnd, msg, wp, lp)
}

/// 点击是否落在子工具条的按钮或字体下拉框上（浮层背景不算）。
unsafe fn cursor_over_sub_button(d: &ToolbarData) -> bool {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return false;
    }
    let h = WindowFromPoint(pt);
    if h.is_invalid() {
        return false;
    }
    if h == d.font_combo {
        return true;
    }
    d.sub_buttons.contains(&h) && GetParent(h).is_ok_and(|p| p == d.sub)
}

/// 切换文字粗体/斜体/描边，并同步给冻结层与配置文件。
unsafe fn toggle_text_style(d: &mut ToolbarData, id: usize) {
    let (field, value) = match id {
        BTN_TEXT_BOLD => {
            d.text_bold = !d.text_bold;
            (StyleField::TextBold, d.text_bold)
        }
        BTN_TEXT_ITALIC => {
            d.text_italic = !d.text_italic;
            (StyleField::TextItalic, d.text_italic)
        }
        BTN_TEXT_STROKE => {
            d.text_stroke = !d.text_stroke;
            (StyleField::TextStroke, d.text_stroke)
        }
        _ => return,
    };
    config::update(|c| match id {
        BTN_TEXT_BOLD => c.text_bold = value,
        BTN_TEXT_ITALIC => c.text_italic = value,
        BTN_TEXT_STROKE => c.text_stroke = value,
        _ => {}
    });
    let _ = PostMessageW(
        d.overlay,
        WM_APP_SET_STYLE,
        WPARAM(field as usize),
        LPARAM(value as isize),
    );
    if let Some(b) = sub_button(d, id) {
        let _ = raw::InvalidateRect(b, std::ptr::null(), 1);
    }
}

/// 字体下拉选择变化：更新全局文字字体并触发冻结层重绘。
unsafe fn apply_font_choice(d: &mut ToolbarData) {
    if d.font_combo.is_invalid() {
        return;
    }
    let idx = SendMessageW(d.font_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    if idx < 0 {
        return;
    }
    let Some(face) = d.font_faces.get(idx as usize).cloned() else {
        return;
    };
    config::update(|c| c.text_font = face);
    let _ = PostMessageW(d.overlay, WM_APP_ANNOT_DIRTY, WPARAM(0), LPARAM(0));
}

/// 枚举系统已安装的字体族名（需求列表的过滤依据）。
unsafe fn installed_font_families() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let hdc = GetDC(HWND::default());
    if !hdc.is_invalid() {
        let mut lf: LOGFONTW = std::mem::zeroed();
        // DEFAULT_CHARSET + 空 FaceName：枚举全部字体族（含中文/英文家族名）
        lf.lfCharSet = DEFAULT_CHARSET;
        let _ = EnumFontFamiliesExW(
            hdc,
            &lf,
            Some(enum_font_proc),
            LPARAM(&mut names as *mut Vec<String> as isize),
            0,
        );
        let _ = ReleaseDC(HWND::default(), hdc);
    }
    names
}

/// 在已安装字体里按别名解析出实际字体族名（精确匹配优先，再容忍带后缀的家族名）。
fn resolve_font_face(installed: &[String], aliases: &[&str]) -> Option<String> {
    for alias in aliases {
        if let Some(name) = installed.iter().find(|n| n.eq_ignore_ascii_case(alias)) {
            return Some(name.clone());
        }
    }
    for alias in aliases {
        let lower = alias.to_lowercase();
        if let Some(name) = installed.iter().find(|n| {
            let n = n.to_lowercase();
            n.starts_with(&lower) && !n.contains("light")
        }) {
            return Some(name.clone());
        }
    }
    None
}

/// 需求字体列表 → 系统中确实可用的（显示名, 实际字体族名），保持需求顺序。
unsafe fn text_font_choices() -> Vec<(String, String)> {
    let installed = installed_font_families();
    TEXT_FONT_FAMILIES
        .iter()
        .filter_map(|(label, aliases)| {
            resolve_font_face(&installed, aliases).map(|face| ((*label).to_string(), face))
        })
        .collect()
}

unsafe extern "system" fn enum_font_proc(
    lf: *const LOGFONTW,
    _tm: *const TEXTMETRICW,
    _font_type: u32,
    lp: LPARAM,
) -> i32 {
    if lf.is_null() || lp.0 == 0 {
        return 1;
    }
    let names = &mut *(lp.0 as *mut Vec<String>);
    let face = (*lf).lfFaceName;
    let name: String = face
        .iter()
        .take_while(|&&c| c != 0)
        .filter_map(|&c| char::from_u32(c as u32))
        .collect();
    if !name.is_empty() && !name.starts_with('@') {
        names.push(name);
    }
    1
}

/// 创建文字子工具条的字体下拉框（初始隐藏，字体列表填充一次）。
unsafe fn create_font_combo(parent: HWND) -> (HWND, Vec<String>) {
    let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
    let combo = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR::from_raw(util::to_wide("COMBOBOX").as_ptr()),
        PCWSTR::null(),
        WS_CHILD | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
        0,
        0,
        10,
        10,
        parent,
        HMENU(BTN_TEXT_FONT as *mut _),
        hinst,
        None,
    )
    .unwrap_or_default();
    if combo.is_invalid() {
        return (combo, Vec::new());
    }
    let font = util::gui_font(parent, 9, 400);
    let _ = SendMessageW(combo, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    let choices = text_font_choices();
    let current = config::get().text_font;
    let mut selected = 0usize;
    let mut faces = Vec::with_capacity(choices.len());
    for (i, (label, face)) in choices.iter().enumerate() {
        let w = util::to_wide(label);
        let idx = SendMessageW(
            combo,
            CB_ADDSTRING,
            WPARAM(0),
            LPARAM(w.as_ptr() as isize),
        )
        .0;
        if idx >= 0 {
            faces.push(face.clone());
        }
        let aliases = TEXT_FONT_FAMILIES
            .iter()
            .find(|(l, _)| *l == label.as_str())
            .map(|(_, a)| *a)
            .unwrap_or(&[]);
        if face.eq_ignore_ascii_case(&current)
            || label.eq_ignore_ascii_case(&current)
            || aliases.iter().any(|a| a.eq_ignore_ascii_case(&current))
        {
            selected = i;
        }
    }
    if !faces.is_empty() {
        let selected = selected.min(faces.len() - 1);
        let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(selected), LPARAM(0));
    }
    let dpi = util::dpi_of(parent);
    let _ = SendMessageW(
        combo,
        CB_SETDROPPEDWIDTH,
        WPARAM(util::scaled(FONT_COMBO_DROP_W, dpi).max(1) as usize),
        LPARAM(0),
    );
    (combo, faces)
}

/// 字体下拉框子类化：转发 Ctrl+滚轮调整尺寸与快捷键，其余行为走原过程。
unsafe extern "system" fn combo_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if let Ok(parent) = GetParent(hwnd) {
        if let Some(d) = data_of(parent) {
            match msg {
                WM_MOUSEWHEEL => {
                    if handle_annotation_wheel(d.bar, wp) {
                        return LRESULT(0);
                    }
                }
                WM_KEYDOWN => {
                    if handle_shortcut(d.bar, wp.0) {
                        return LRESULT(0);
                    }
                }
                _ => {}
            }
        }
    }
    util::call_orig(hwnd, msg, wp, lp)
}

unsafe fn handle_command(hwnd: HWND, id: usize) {
    crate::diag::log(&format!("[toolbar] command id={id}"));
    match id {
        // 关闭：不执行任何操作，直接收起工具条（与按 Esc 等价）
        BTN_CLOSE => close_bar(hwnd),
        // 工具按钮：把工具投递给冻结层；高亮由冻结层回传的 TOOL_CHANGED 同步
        BTN_RECT | BTN_ARROW | BTN_STEP | BTN_ELLIPSE | BTN_LINE | BTN_PEN | BTN_MOSAIC
        | BTN_HIGHLIGHT | BTN_TEXT | BTN_ERASER | BTN_WATERMARK => {
            let Some(d) = data_of(hwnd) else { return };
            let tool = button_id_tool(id);
            let _ = PostMessageW(d.overlay, WM_APP_SET_TOOL, WPARAM(tool as usize), LPARAM(0));
        }
        BTN_STYLE => {
            toggle_style_panel(hwnd);
        }
        // 「…」由悬停展开；点击本身不做事（子按钮的点击会带着各自 ID 回到这里）
        BTN_MORE => {}
        BTN_PIN => {
            let Some(d) = data_of(hwnd) else { return };
            commit_typing_if_any(d);
            let img = compose(d);
            match pin::create(&img) {
                Ok(_) => close_bar(hwnd),
                Err(e) => util::error_msgbox(&format!("贴图失败：{e}")),
            }
        }
        BTN_COPY => {
            let Some(d) = data_of(hwnd) else { return };
            commit_typing_if_any(d);
            let img = compose(d);
            // 双格式（参照 SnowShot）：CF_DIB（主）+ 注册的 "PNG" 格式
            // （现代应用粘贴优先取用 PNG）；编码失败只降级为 CF_DIB。
            let png = imageio::encode_png(&img.data, img.width, img.height).ok();
            // Arc 共享像素，copy_image 直接读切片，不再整幅复制
            match clipboard::copy_image_with_png(&img.data, img.width, img.height, png.as_deref()) {
                Ok(()) => {
                    crate::toast::show("已复制图像到剪贴板");
                    close_bar(hwnd);
                }
                Err(e) => util::error_msgbox(&format!("复制失败：{e}")),
            }
        }
        BTN_SAVE => {
            let Some(d) = data_of(hwnd) else { return };
            commit_typing_if_any(d);
            let cfg = config::get();
            let img = compose(d);
            match imageio::save_with_dialog(
                hwnd,
                &img.data,
                img.width,
                img.height,
                cfg.format,
                cfg.save_dir.as_deref(),
            ) {
                Ok(Some(path)) => {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or(path);
                    crate::toast::show(&format!("已保存：{name}"));
                    close_bar(hwnd);
                }
                Ok(None) => close_bar(hwnd), // 取消保存同样关闭工具条
                Err(e) => util::error_msgbox(&format!("保存失败：{e}")),
            }
        }
        BTN_OCR => {
            let Some(d) = data_of(hwnd) else { return };
            // 后台识别 → 直接写剪贴板 → 由主窗口弹 Toast（不弹结果窗口）。
            // 与贴图右键菜单的「识别文字 (OCR)」共用 ocr::recognize_to_clipboard_async。
            commit_typing_if_any(d);
            let img = compose(d);
            ocr::recognize_to_clipboard_async(
                Arc::clone(&img.data),
                img.width,
                img.height,
            );
            close_bar(hwnd);
        }
        _ => {}
    }
}

/// 导出动作前完成输入中的文字（若有）：把瞬态正文落入标注文档，避免导出丢文字。
/// 冻结层不可达（直接模式的异常路径）时跳过。
unsafe fn commit_typing_if_any(d: &ToolbarData) {
    if d.sel.is_null() {
        return;
    }
    (*d.sel).commit_typing();
}

#[cfg(test)]
mod group_layout {
    use super::*;

    #[test]
    fn separator_lies_between_function_group_and_annotation_group() {
        let (btn, gap, sep, x0) = (32, 4, 11, 10);
        let sx = separator_x(x0, btn, gap, sep);
        let ocr_right = x0 + ANNOT_GROUP_START as i32 * (btn + gap) - gap;
        let rect_left = ocr_right + gap + sep;
        assert!(sx > ocr_right, "竖杠应位于 OCR 按钮右侧");
        assert!(sx < rect_left, "竖杠应位于矩形按钮左侧");
    }

    #[test]
    fn more_button_sits_between_style_and_close() {
        let style = BTN_ORDER.iter().position(|&x| x == BTN_STYLE).unwrap();
        let more = BTN_ORDER.iter().position(|&x| x == BTN_MORE).unwrap();
        let close = BTN_ORDER.iter().position(|&x| x == BTN_CLOSE).unwrap();
        assert!(style < more, "「…」应在颜色面板右侧");
        assert!(more < close, "「…」应在关闭按钮左侧");
    }

    #[test]
    fn text_button_sits_after_step_and_before_style() {
        let step = BTN_ORDER.iter().position(|&x| x == BTN_STEP).unwrap();
        let text = BTN_ORDER.iter().position(|&x| x == BTN_TEXT).unwrap();
        let style = BTN_ORDER.iter().position(|&x| x == BTN_STYLE).unwrap();
        assert!(step < text, "文字按钮应位于序号按钮之后");
        assert!(text < style, "文字按钮应位于颜色面板之前");
    }

    #[test]
    fn secondary_annotation_tools_live_only_in_their_subbars() {
        // 「更多」子工具条只保留马赛克/画笔/高亮/橡皮/水印
        for id in MORE_ORDER {
            assert!(!BTN_ORDER.contains(&id), "id={id} 不应出现在主工具条");
            assert!(!tip_text(id).is_empty(), "id={id} 缺少提示文案");
            assert!(button_id_tool(id) != annot::Tool::None, "id={id} 不是标注工具");
        }
        // 矩形/箭头按钮的子工具条各自含原工具 + 同类新工具
        assert_eq!(SHAPE_ORDER, [BTN_RECT, BTN_ELLIPSE]);
        assert_eq!(ARROW_ORDER, [BTN_ARROW, BTN_LINE]);
        // 主工具条的锚定按钮提示为组合名称
        assert_eq!(main_tip_text(BTN_RECT), "矩形/椭圆");
        assert_eq!(main_tip_text(BTN_ARROW), "箭头/线段");
        assert_eq!(main_tip_text(BTN_MORE), "更多");
        // 文字样式子工具条含粗体/斜体/描边，字体下拉框单独存在
        assert_eq!(TEXT_STYLE_ORDER, [BTN_TEXT_BOLD, BTN_TEXT_ITALIC, BTN_TEXT_STROKE]);
        for id in TEXT_STYLE_ORDER {
            assert!(!tip_text(id).is_empty(), "id={id} 缺少提示文案");
            assert!(!SUB_IDS.contains(&BTN_TEXT_FONT), "字体下拉框不是 owner-draw 按钮");
        }
        // 四类子工具条互不混入主工具条的功能/样式按钮
        for id in [BTN_STEP, BTN_STYLE, BTN_MORE, BTN_CLOSE] {
            assert!(!SUB_IDS.contains(&id), "id={id} 不应出现在子工具条");
        }
    }

    #[test]
    fn hover_delay_is_over_one_second() {
        assert!(SUB_SHOW_DELAY_MS > 1000, "悬浮弹出子工具条需超过 1 秒");
    }

    #[test]
    fn shortcut_tips_only_on_text_subbar_for_rect_and_arrow() {
        // 主工具条入口不显示 R/A，子工具条的「矩形」「箭头」仍显示
        assert_eq!(tip_shortcut_text(BTN_RECT, true), "");
        assert_eq!(tip_shortcut_text(BTN_ARROW, true), "");
        assert_eq!(tip_shortcut_text(BTN_RECT, false), "R");
        assert_eq!(tip_shortcut_text(BTN_ARROW, false), "A");
        // 其余主工具条按钮的快捷键保持一致
        assert_eq!(tip_shortcut_text(BTN_STEP, true), "N");
        assert_eq!(tip_shortcut_text(BTN_TEXT, true), "T");
    }

    #[test]
    fn text_font_list_matches_required_order() {
        assert_eq!(
            TEXT_FONT_FAMILIES.map(|(label, _)| label),
            [
                "微软雅黑",
                "宋体",
                "楷体",
                "黑体",
                "新宋体",
                "汉仪中黑",
                "方正小标宋简体",
                "方正隶书简体",
            ]
        );
    }

    #[test]
    #[ignore = "仅用于人工核对本机实际可用的需求字体"]
    fn print_installed_text_font_choices() {
        unsafe {
            for (label, face) in text_font_choices() {
                println!("{label} => {face}");
            }
        }
    }

    #[test]
    fn font_face_resolution_prefers_exact_alias_then_suffix_variant() {
        let installed = vec![
            "Microsoft YaHei UI".to_string(),
            "宋体".to_string(),
            "汉仪中黑 197".to_string(),
        ];
        assert_eq!(
            resolve_font_face(&installed, &["宋体", "SimSun"]).as_deref(),
            Some("宋体")
        );
        // 「汉仪中黑 197」这类带后缀的家族名也能解析
        assert_eq!(
            resolve_font_face(&installed, &["汉仪中黑", "HYZhongHeiJ"]).as_deref(),
            Some("汉仪中黑 197")
        );
        // 系统未安装时返回 None（列表不显示该项）
        assert_eq!(resolve_font_face(&installed, &["楷体"]), None);
    }

    #[test]
    fn ctrl_wheel_maps_annotation_buttons_to_size_kinds() {
        for id in [BTN_RECT, BTN_ARROW, BTN_ELLIPSE, BTN_LINE, BTN_PEN] {
            assert_eq!(size_kind_for_button(id), Some(1), "线宽");
        }
        for id in [BTN_TEXT, BTN_WATERMARK] {
            assert_eq!(size_kind_for_button(id), Some(2), "字号");
        }
        assert_eq!(size_kind_for_button(BTN_STEP), Some(3));
        assert_eq!(size_kind_for_button(BTN_ERASER), Some(4));
        // 功能性按钮与无尺寸概念的工具不参与 Ctrl+滚轮调整
        for id in [
            BTN_PIN,
            BTN_SAVE,
            BTN_COPY,
            BTN_OCR,
            BTN_CLOSE,
            BTN_MOSAIC,
            BTN_HIGHLIGHT,
            BTN_STYLE,
        ] {
            assert_eq!(size_kind_for_button(id), None);
        }
    }
}

#[cfg(test)]
mod preview {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC,
    };

    /// 渲染工具条按钮（默认态 / 悬停态 / 关闭悬停态）供人工核对。
    /// 走的就是 `WM_DRAWITEM` 用的那条双缓冲路径：`cargo test --bin pixshot render_toolbar_preview -- --ignored`
    #[test]
    #[ignore = "仅在需要人工核对工具条按钮观感时运行"]
    fn render_toolbar_preview() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let dpi = 96;
            let btn = util::scaled(btn_sz(), dpi);
            let (cols, rows) = (BTN_ORDER.len() as i32, 3);
            let (w, h) = (cols * btn, rows * btn);
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let hbmp =
                CreateDIBSection(HDC::default(), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let mem = CreateCompatibleDC(None);
            let old = SelectObject(mem, hbmp);

            let skin = theme::toolbar_skin();
            let card = CreateSolidBrush(skin.card);
            let full = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            FillRect(mem, &full, card);
            let _ = DeleteObject(card);

            // 钉子句柄不可用（Icon 样式不读窗口文本），悬停值靠手工设置 entries 模拟
            let fake = HWND::default();
            for (row, (style, v)) in [
                (BtnStyle::Icon, 0.0f32),        // 默认态
                (BtnStyle::Icon, 1.0),           // 悬停态（淡灰底）
                (BtnStyle::IconClose, 1.0),      // 关闭按钮悬停态（红底白叉）
            ]
            .iter()
            .enumerate()
            {
                for (i, &id) in BTN_ORDER.iter().enumerate() {
                    let rc = RECT {
                        left: i as i32 * btn,
                        top: row as i32 * btn,
                        right: (i as i32 + 1) * btn,
                        bottom: (row as i32 + 1) * btn,
                    };
                    let mut fx = util::HoverFx::new();
                    fx.register(fake);
                    fx.entries[0].1 = *v; // 直接把插值设到目标态
                    fx.entries[0].2 = *v;
                    // itemState 用默认值（未按下）：外观完全由悬停插值 v 决定
                    let dis = DRAWITEMSTRUCT {
                        hDC: mem,
                        rcItem: rc,
                        hwndItem: fake,
                        ..Default::default()
                    };
                    draw_icon_frame(HWND::default(), &dis, *style, skin, &fx, icon_of(id), false);
                }
            }

            let n = (w * h) as usize;
            let mut px = std::slice::from_raw_parts(bits as *const u8, n * 4).to_vec();
            for p in px.chunks_exact_mut(4) {
                p[3] = 255;
            }
            SelectObject(mem, old);
            let _ = DeleteObject(hbmp);
            let _ = DeleteDC(mem);
            let _ = crate::imageio::save_png("target/toolbar_preview.png", &px, w, h);
        }
    }
}
