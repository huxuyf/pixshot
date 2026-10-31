//! 区域框选：按下热键后**整屏冻结 + 暗化**（选区遮罩，参照 SnowShot 的遮罩：
//! 背景每通道 >>1 变暗、拖动时选区内还原为原始亮度、实时显示「宽 × 高」），
//! 在冻结画面上拖动框选 / Esc 取消。
//!
//! **实现方式是逐像素 alpha 的分层窗口**（`UpdateLayeredWindow`）：表面用整屏快照
//! 铺满后原位暗化（BGRA、alpha=255），拖动中每帧把选区矩形还原为原始像素（提亮）、
//! 画红框、画尺寸标签（GDI+ + alpha 防御）。原位暗化不多占一张整屏位图——擦帧时
//! 从快照取像素按暗化值写回即可。
//!
//! **冻结持续到工具条关闭**：松开鼠标（选区确认）后表面**保持冻结画面不变**，
//! 屏幕继续呈冻结状态，直到用户点了工具条任一图标 / 按 Esc / 点关闭 —— 也就是
//! 本次截图会话结束时，窗口才随 [`Selection`] 析构被拆掉，屏幕恢复实时状态。
//!
//! **确认时回收用于裁剪的整屏资源**：选区确认那一刻成品已从整屏快照裁出，随即释放
//! 快照，因此"用于裁剪的整屏缓冲"不再存活；而**显示中的**冻结画面留在窗口自己的
//! 绘制表面上（这块表面本来就要占一整屏），保持显示不额外增加内存，随窗口销毁一并释放。
//!
//! 表面先用透明底（alpha=1）铺一遍、再用快照覆盖：透明底是兜底 —— 万一快照尺寸小于
//! 虚拟屏幕，也不会留下 alpha=0 的像素。alpha=0 在分层窗口上会让点击**穿透**到桌面
//! 上的其它窗口，拖拽根本发不起来（alpha=1 的视觉贡献是 1/255，不可察觉）。
//!
//! 重要设计（截屏管线）：快照优先取用热键链路上低级键盘钩子在按键投递到前台窗口之前
//! 抓好的那一帧（`tray::take_precapture`，仅走钩子兜底的热键链路才有）；拿不到时
//! （`RegisterHotKey` 可用、或由托盘点击触发）就在这里当场抓帧。成品从该快照裁剪，
//! 因此**所见即所得**；红框只画在窗口表面、不写进快照，**不会出现在成品里**。
//!
//! 状态存放：`GWLP_USERDATA` 持有 `Box<State>` 指针（与 pin/results 同一模式），
//! 不再使用 `static mut`。
//!
//! **窗口保留到会话结束**：拖动确认后窗口**不销毁**，冻结画面与红框继续显示，直到用户
//! 点击工具条任一图标或关闭截图。此时 `run()` 结束模态循环但保留窗口与 `State`，
//! 交由 [`Selection`] 持有；`Selection` 析构（或显式 [`Selection::dismiss`]）
//! 时才真正拆窗、释放 GDI 资源。
//!
//! **确认后进入标注模式**（见设计文档《标注引擎 MVP 设计文档》1.2 A）：窗口**不再**
//! 转为鼠标穿透（`WS_EX_TRANSPARENT`），而是继续接收鼠标消息用于矩形/箭头/步骤序号/
//! 椭圆/线段/画笔/马赛克/高亮/文字标注；键盘消息由工具条经自定义消息（`WM_APP_KEY` /
//! `WM_APP_SET_TOOL` / `WM_APP_CHAR`）转发。
//! 工具条/贴图/菜单都是 `WS_EX_TOPMOST` 且在其后创建，会压在冻结层之上；
//! 标注期间冻结层对 `WM_MOUSEACTIVATE` 返回 `MA_NOACTIVATE`，不抢工具条的焦点与 z 序。
//!
//! **文字输入与 IME**（中文输入）：主进程已 `ImmDisableIME`（见 `main.rs`），因此
//! 本进程内**没有输入法**。进入文字输入时改为拉起**文字输入子进程**
//! （[`crate::textinput`]）：子进程在其输入窗内用系统原生 IME 完成组字/候选，
//! 正文经 `WM_COPYDATA` 回传（见本文件 `WM_COPYDATA` 分支）。离开输入收掉子进程、
//! 焦点还给工具条。子进程失败时兜底回退到进程内输入（仅 ASCII 可用）。

use std::sync::Arc;

use crate::annot::{self, AnnotDoc, AnnotSession, CursorKind, DibSurface, Pt, Rect, SizeKind, Tool};
use crate::annot::shape::Shape;
use crate::annot::style::{HighlightMode, MosaicMode, PenKind, Rgba, TextFrame};
use crate::assist::{self, ColorFormat};
use crate::capture::{self, CapturedImage};
use crate::pick::{PickEngine, PickLevel, PickTarget};
use crate::util::{self, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint,
    GdiFlush, GetDC, MonitorFromPoint, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 自定义消息：工具条 → 冻结层（设置当前工具）。`wp` = [`Tool`] 判别值。
pub const WM_APP_SET_TOOL: u32 = WM_APP + 1;
/// 自定义消息：工具条 → 冻结层（转发按键）。`wp` = VK，`lp` = Ctrl 是否按下。
pub const WM_APP_KEY: u32 = WM_APP + 2;
/// 自定义消息：请求重绘标注层（预留；当前未使用，收到即整选区重绘）。
pub const WM_APP_ANNOT_DIRTY: u32 = WM_APP + 3;
/// 自定义消息：冻结层 → 工具条（工具已变化）。`wp` = [`Tool`] 判别值。
pub const WM_APP_TOOL_CHANGED: u32 = WM_APP + 4;
/// 自定义消息：冻结层 → 工具条（Esc 交还，关闭会话）。
pub const WM_APP_ESC_CLOSE: u32 = WM_APP + 5;
/// 自定义消息：工具条 → 冻结层（转发 `WM_CHAR` 字符，文字输入用）。
/// `wp` = 字符码（UTF-32）。
pub const WM_APP_CHAR: u32 = WM_APP + 6;
/// 自定义消息：工具条 → 冻结层（同步查询是否处于文字输入中）。
/// 返回 1 = 输入中（工具条的字母键应放行为正文输入，不当工具快捷键）。
pub const WM_APP_QUERY_TYPING: u32 = WM_APP + 7;
/// 颜色面板 → 标注会话（投递给 overlay 窗口）。`wp` = [`StyleField`]，`lp` 打包值。
pub const WM_APP_SET_STYLE: u32 = WM_APP + 8;
/// 标注会话 → 颜色面板（选中项/偏好变化时同步）。`wp` = [`StyleField`]，`lp` 打包值。
pub const WM_APP_STYLE_SYNC: u32 = WM_APP + 9;
/// 颜色面板 → 标注会话：清空全部标注。
pub const WM_APP_CLEAR_ALL: u32 = WM_APP + 10;
/// 查询当前选中（E2E 用）：返回 1 + 选中 id 的低 15 位（无选中返回 0）。
pub const WM_APP_QUERY_STYLE: u32 = WM_APP + 11;
/// 查询当前智能选区候选（E2E 用）：返回 1 表示有候选。
pub const WM_APP_QUERY_PICK: u32 = WM_APP + 12;
/// 工具条 → 冻结层：Ctrl+滚轮调整标注尺寸。`wp` = [`SizeKind`] 编码，`lp` = ±1。
pub const WM_APP_ADJUST_SIZE: u32 = WM_APP + 13;
/// 颜色面板 → 工具条：面板已销毁（选色 / Esc / 工具条主动关闭）。
/// `lp` = 面板窗口句柄，工具条据此清空缓存句柄，避免留下失效 HWND。
pub const WM_APP_STYLE_PANEL_CLOSED: u32 = WM_APP + 14;

/// 样式字段编号（避免字符串协议）。
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleField {
    /// 同时设置描边色与文字色（面板色块）。
    Color = 0,
    StrokeColor = 1,
    StrokeWidth,
    TextColor,
    TextSize,
    TextBold,
    TextItalic,
    PenKind,
    MosaicMode,
    HighlightMode,
    TextFrame,
    BadgeColor,
    Clear,
}

impl StyleField {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::Color,
            1 => Self::StrokeColor,
            2 => Self::StrokeWidth,
            3 => Self::TextColor,
            4 => Self::TextSize,
            5 => Self::TextBold,
            6 => Self::TextItalic,
            7 => Self::PenKind,
            8 => Self::MosaicMode,
            9 => Self::HighlightMode,
            10 => Self::TextFrame,
            11 => Self::BadgeColor,
            12 => Self::Clear,
            _ => return None,
        })
    }
}

/// 冻结层窗口类名（供冻结层反向定位工具条窗口）。
const TOOLBAR_CLASS: &str = "PixShotToolbar";
/// 标注颜色面板窗口类名。
pub const STYLE_PANEL_CLASS: &str = "PixShotStylePanel";

/// 选区（虚拟屏幕坐标，物理像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// 已确认的选区：**已裁好的成品** + 仍在屏幕上显示的「冻结画面 + 红框」窗口。
///
/// 成品在确认时就已从整屏快照裁出，整屏快照随即释放（不随本结构存活）——
/// 因此这里只持有选区大小的图。窗口表面仍在显示"按下热键那一刻"的冻结画面，
/// 用来让屏幕保持冻结直到本结构析构（工具条关闭 / Esc / 点关闭按钮）。
///
/// 拖动确认后窗口不销毁，而是继续显示（鼠标穿透，工具条可正常点选）；
/// 窗口在 [`Selection`] 被析构或显式调用 [`Selection::dismiss`] 时拆掉 ——
/// 那一刻屏幕才恢复为实时状态。
pub struct Selection {
    /// 成品图（已从整屏快照按选区裁好）。
    pub image: Arc<CapturedImage>,
    /// 红框窗口；`dismiss()` 后置空。
    hwnd: HWND,
    /// 红框窗口的 `State`（`GWLP_USERDATA` 指向同一块内存）。
    state: *mut State,
}

impl Selection {
    /// 冻结层窗口句柄（工具条向其转发工具切换/按键，见设计文档 5.5）。
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 只读借用标注文档（供合成导出，见设计文档 6）。
    ///
    /// **调用方必须立即使用、不得跨消息持有**：标注编辑发生在冻结层窗口过程里，
    /// 长借会与 `overlay_proc` 的写入别名。
    pub fn doc(&self) -> &AnnotDoc {
        unsafe { &(*self.state).annot.doc }
    }

    /// 完成输入中的文字（若有）：把瞬态正文落入标注文档并重绘。
    ///
    /// 工具栏在导出（贴图/复制/保存/OCR）前调用——文字输入中的正文是瞬态
    /// （见 `AnnotSession::commit_text`），不先提交导出就会丢文字。
    pub fn commit_typing(&self) {
        unsafe {
            let s = &mut *self.state;
            if s.mode != OverlayMode::Annotating {
                return;
            }
            let a = s.annot.commit_text();
            if let Some(d) = a.redraw {
                repaint_annot(self.hwnd, s, d);
            }
            // 导出前提交后，输入已结束：同步收掉文字输入子进程（若在运行）
            sync_text_helper(self.hwnd, s);
        }
    }

    /// 收掉红框窗口并释放其 GDI 资源。幂等（`Drop` 会再调一次也无害）。
    pub fn dismiss(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            // 抑制 WM_DESTROY 里的 PostQuitMessage：此时已不在框选的消息循环内，
            // 漏出去会让主消息循环误以为要退出程序。
            (*self.state).quiet_destroy = true;
            let _ = DestroyWindow(self.hwnd);
            let s = Box::from_raw(self.state);
            cleanup(&s);
        }
        self.hwnd = HWND::default();
        self.state = std::ptr::null_mut();
    }
}

impl Drop for Selection {
    fn drop(&mut self) {
        self.dismiss();
    }
}

const VK_ESCAPE: usize = 0x1B;
/// 透明底的像素值（预乘 BGRA）。alpha 取 1 而非 0：分层窗口按 alpha 命中鼠标，
/// alpha=0 会让点击穿透到桌面（拖拽无法发起）；1/255 视觉上不可察觉。
const BG_PX: u32 = 1 << 24;

/// 把 [`crate::annot::style::Rgba`] 打成预乘 BGRA u32（小端 → 字节序 B,G,R,A）。
/// 分层窗口要求预乘；不透明色（a=255）预乘后即原始通道值。
fn packed(c: crate::annot::style::Rgba) -> u32 {
    let a = c.a as u32;
    let pm = |v: u8| (v as u32) * a / 255;
    (a << 24) | (pm(c.r) << 16) | (pm(c.g) << 8) | pm(c.b)
}

/// 冻结层所处阶段（替代原 `locked: bool`，见设计文档 1.2 F）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayMode {
    /// 框选中（含确认前的拖动）。
    Selecting,
    /// 选区已确认：进入标注模式（窗口不再鼠标穿透）。
    Annotating,
}

struct AssistState {
    enabled_magnifier: bool,
    enabled_aux: bool,
    enabled_info: bool,
    grid: bool,
    format: ColorFormat,
    /// 放大镜边长（物理像素，创建时按 DPI 换算）。
    lens_size: i32,
    /// 放大倍数。
    lens_zoom: i32,
    /// 上一帧辅助层脏区（精确擦除）。
    last_dirty: Vec<Rect4>,
    /// 光标最后一次位置（屏幕坐标）。
    cursor: Option<POINT>,
    /// 信息条文本缓冲。
    info: String,
}

struct PickUi {
    /// 当前候选（悬停预览）。
    hover: Option<PickTarget>,
    /// 上次探测的光标位置与时间（节流用）。
    last_probe: Option<(i32, i32, u32)>,
    /// 上次绘制的候选框（用于精确擦除）。
    last_drew: Option<Rect4>,
}

struct State {
    /// 确认时从整屏快照裁出的成品（小图）；`Some` 即表示"选区已确认"，
    /// `run()` 结尾取出交给 [`Selection`]。
    result_image: Option<Arc<CapturedImage>>,
    /// 整屏快照：既是"冻结画面"的底图，也是成品裁剪的来源。
    /// **选区确认后立即置 `None` 释放**（不占整屏内存）。
    snapshot: Option<Arc<CapturedImage>>,
    dragging: bool,
    /// 当前阶段（框选中 / 标注中）。
    mode: OverlayMode,
    /// 已确认的选区（虚拟屏幕坐标，物理像素）。
    region: SelectedRegion,
    /// 标注会话（逻辑状态机）；确认后以真实选区尺寸重建。
    annot: AnnotSession,
    /// 标注底图：确认时从冻结表面复制选区（底图快照 + 红框），此后不可变。
    annot_base: Option<DibSurface>,
    /// 由 `Selection` 主动拆窗：「是」则在 `WM_DESTROY` 里跳过 `PostQuitMessage`。
    quiet_destroy: bool,
    start: POINT, // 屏幕坐标
    cur: POINT,   // 屏幕坐标
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    /// 屏幕 DC（`UpdateLayeredWindowIndirect` 的 hdcDst），run() 期间持有。
    screen_dc: HDC,
    /// 离屏表面：整屏 32bpp 预乘 BGRA（`UpdateLayeredWindow` 的内容来源）。
    surface_dc: HDC,
    surface_bmp: HBITMAP,
    /// `SelectObject(surface_dc, surface_bmp)` 返回的原对象（清理时恢复，否则删不掉位图）。
    surface_old: HGDIOBJ,
    /// 表面像素基址（预乘 BGRA、自上而下、行距 = vw*4）。
    surface_bits: *mut u32,
    /// 上一帧框线矩形（客户区坐标），用于只重绘变动区域。
    last: Option<Rect4>,
    /// 上一帧尺寸标签矩形（客户区坐标）：擦标签时还原为暗化像素。
    last_label: Option<Rect4>,
    /// 缓存的 GDI+ 上下文（建在 surface_dc 上）：拖动框选期间每个 mousemove
    /// 都画尺寸标签，逐帧创建/销毁 GpGraphics 是不必要的固定开销。
    /// 生命周期 ≤ surface_dc（cleanup 中先释放它）。
    label_g: *mut windows::Win32::Graphics::GdiPlus::GpGraphics,
    /// 尺寸标签文本的复用缓冲（避免每帧 `format!` 堆分配）。
    label_text: String,
    /// 当前文字输入子进程所服务的**标注锚点**（选区局部坐标）。`Some` 表示子进程
    /// 正在运行；锚点变化（点别处重新落点）时重挂子进程。`None` 表示未在输入。
    text_helper_anchor: Option<Pt>,
    /// 截图辅助（放大镜 / 辅助线 / 信息条 / 取色）。
    assist: AssistState,
    /// 智能选区候选状态。
    pick: PickUi,
    /// 智能选区引擎（UIA 不可用时内部自动退化为窗口级）。
    pick_engine: Option<PickEngine>,
    /// 当前拾取级别。
    pick_level: PickLevel,
    /// 候选框额外外扩（逻辑像素）。
    pick_padding: i32,
    /// 单击是否自动扩充为窗口矩形。
    pick_auto_expand: bool,
    /// 探测节流（毫秒）。
    pick_probe_interval_ms: u32,
    /// 是否显示候选标签。
    pick_show_label: bool,
}


/// 进入框选模式（模态，阻塞直到用户确认/取消）。
/// 确认时返回「已裁好的成品 + 仍在显示的红框」；取消返回 None。
pub fn select_region() -> Option<Selection> {
    unsafe { run() }
}

unsafe fn run() -> Option<Selection> {
    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    if vw <= 0 || vh <= 0 {
        return None;
    }

    // 优先使用「按下热键那一刻」抓到的整屏预捕获快照（由低级键盘钩子在按键进入
    // 前台窗口之前抓取）：这样前台弹出的菜单/下拉框不会因热键按键先行关闭而从
    // 成品里消失。没有（例如托盘点击触发）或已过期时，退回当前即刻抓帧。
    let shot = match crate::tray::take_precapture() {
        Some(img) if (img.x, img.y, img.width, img.height) == (vx, vy, vw, vh) => {
            crate::diag::log(&format!(
                "[overlay] 使用热键预捕获快照 {}×{}（按下热键那一刻的整屏）",
                img.width, img.height
            ));
            img
        }
        _ => {
            crate::diag::log("[overlay] 无可用预捕获快照 -> 当场抓帧");
            Arc::new(
                capture::capture_region_ex(vx, vy, vw, vh, crate::config::get().capture_cursor)
                    .ok()?,
            )
        }
    };

    // 屏幕 DC 与整屏绘制表面（32bpp 预乘 BGRA）
    let screen_dc = GetDC(HWND::default());
    if screen_dc.is_invalid() {
        crate::diag::log("[overlay] GetDC(屏幕) 失败 -> 框选取消");
        return None;
    }
    let surface_dc = CreateCompatibleDC(screen_dc);
    if surface_dc.is_invalid() {
        let _ = ReleaseDC(HWND::default(), screen_dc);
        crate::diag::log("[overlay] CreateCompatibleDC 失败 -> 框选取消");
        return None;
    }
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let surface_bmp = match CreateDIBSection(
        surface_dc,
        &top_down_32bpp(vw, vh),
        DIB_RGB_COLORS,
        &mut bits,
        HANDLE::default(),
        0,
    ) {
        Ok(b) if !bits.is_null() => b,
        _ => {
            let _ = DeleteDC(surface_dc);
            let _ = ReleaseDC(HWND::default(), screen_dc);
            crate::diag::log("[overlay] CreateDIBSection 失败 -> 框选取消");
            return None;
        }
    };
    let surface_old = SelectObject(surface_dc, surface_bmp);

    register_class_w("PixShotOverlay", overlay_proc, true, false, false);
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let cfg = crate::config::get();
    let dpi = overlay_dpi();
    let assist_state = AssistState {
        enabled_magnifier: cfg.assist_magnifier,
        enabled_aux: cfg.assist_aux_lines,
        enabled_info: cfg.assist_show_info,
        grid: cfg.assist_aux_grid,
        format: cfg.assist_color_format,
        lens_size: util::scaled(cfg.assist_magnifier_size, dpi).max(48),
        lens_zoom: cfg.assist_magnifier_zoom.clamp(1, 16),
        last_dirty: Vec::new(),
        cursor: None,
        info: String::new(),
    };

    let state = Box::into_raw(Box::new(State {
        result_image: None,
        snapshot: Some(shot),
        dragging: false,
        mode: OverlayMode::Selecting,
        region: SelectedRegion {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        annot: AnnotSession::new(Rect::new(0, 0, 0, 0), overlay_dpi()),
        annot_base: None,
        quiet_destroy: false,
        start: POINT::default(),
        cur: POINT::default(),
        vx,
        vy,
        vw,
        vh,
        screen_dc,
        surface_dc,
        surface_bmp,
        surface_old,
        surface_bits: bits as *mut u32,
        last: None,
        last_label: None,
        label_g: crate::annot::render_gdiplus::graphics_from(surface_dc)
            .unwrap_or(std::ptr::null_mut()),
        label_text: String::new(),
        text_helper_anchor: None,
        assist: assist_state,
        pick: PickUi {
            hover: None,
            last_probe: None,
            last_drew: None,
        },
        pick_engine: {
            let mut e = PickEngine::new();
            e.set_max_depth(cfg.pick_max_depth);
            Some(e)
        },
        pick_level: cfg.pick_level,
        pick_padding: cfg.pick_padding,
        pick_auto_expand: cfg.pick_auto_expand,
        pick_probe_interval_ms: cfg.pick_probe_interval_ms.max(16),
        pick_show_label: cfg.pick_show_label,
    }));

    // 不带 WS_VISIBLE：先提交一次"全透明"内容，避免显示瞬间出现不透明黑屏
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
        PCWSTR::from_raw(to_wide("PixShotOverlay").as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        vx,
        vy,
        vw,
        vh,
        None,
        None,
        hinst,
        Some(state as *const _),
    )
    .unwrap_or_default();

    if hwnd.is_invalid() {
        let s = Box::from_raw(state);
        cleanup(&s);
        crate::diag::log("[overlay] 创建冻结层窗口失败 -> 框选取消");
        return None;
    }

    // 遮罩（冻结画面 + 红框）永不出现在任何截图里。测试用 escape hatch：
    // `PIXSHOT_OVERLAY_CAPTURABLE=1` 时跳过，便于用普通屏幕截图回归"冻结观感"
    // （正常使用不应设置此变量）。
    if std::env::var_os("PIXSHOT_OVERLAY_CAPTURABLE").is_none() {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
    // 表面先用透明底铺一遍（兜底：万一快照比虚拟屏幕小，也不会留下 alpha=0 的像素，
    // 那种像素会让点击穿透、拖拽发不起来），再用整屏快照覆盖并**原位暗化**（选区
    // 遮罩，每通道 >>1）：拖动中再把选区矩形还原为原始亮度。窗口一显示就是
    // "暗化的冻结"整屏画面。
    {
        let s = &*state;
        clear_rect(s.surface_bits, vw, vh, (0, 0, vw, vh));
        if let Some(shot) = &s.snapshot {
            copy_snapshot_into_surface(s.surface_bits, vw, vh, shot);
        }
        dim_surface(s.surface_bits, vw, vh, crate::config::get().ui_overlay_dim);
        commit(hwnd, s, (0, 0, vw, vh));
    }
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    let _ = SetForegroundWindow(hwnd);
    // 显示后**再次**置顶：显示前的那次 SetWindowPos 可能已被其他置顶窗口
    // （如锁屏兜底窗口、其他截图工具）压回，重申一次确保处于最顶层。
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);

    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 {
            break; // WM_QUIT
        }
        if r == -1 {
            crate::diag::log("[overlay] GetMessageW 返回 -1（消息错误），结束框选循环");
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // 消息循环结束：
    // - 取消（Esc）→ 窗口已销毁、无选区，释放资源后返回 None（屏幕立即恢复实时）；
    // - 确认选区 → 窗口**保留**（冻结画面 + 红框继续显示），成品已在 `finish_selection`
    //   里裁好、整屏快照也已在那一刻释放；把 `State` 交还调用方，直到调用方 dismiss
    //   （点工具条图标 / Esc / 关闭按钮）才拆窗 —— 那一刻屏幕才恢复实时状态。
    let mut s = Box::from_raw(state);
    if let Some(image) = s.result_image.take() {
        let state = Box::into_raw(s);
        return Some(Selection {
            image,
            hwnd,
            state,
        });
    }
    // 无成品 = 用户取消（Esc / 关闭 / 外部 WM_QUIT）。外部 WM_QUIT（如托盘「退出」）
    // 打断模态循环时窗口仍在、`GWLP_USERDATA` 仍指向 State —— 必须先拆窗再释放，
    // 否则窗口残留且消息处理会解引用悬垂指针（Esc/关闭路径窗口已销毁，重复调用无害）。
    // 置 quiet_destroy 抑制 WM_DESTROY 里的 PostQuitMessage（退出发令由外层统一处理）。
    s.quiet_destroy = true;
    let _ = DestroyWindow(hwnd);
    cleanup(&s);
    None
}

/// 整屏 32bpp 自上而下的 BITMAPINFO（供 `CreateDIBSection` 建绘制表面）。
fn top_down_32bpp(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // 负值 = 自上而下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 把表面内容提交给分层窗口（`prcDirty` 限定重合成区域，避免整屏重合成）。
unsafe fn commit(hwnd: HWND, s: &State, dirty: Rect4) {
    let dst = POINT { x: s.vx, y: s.vy };
    let src = POINT { x: 0, y: 0 };
    let size = SIZE {
        cx: s.vw,
        cy: s.vh,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let dirty_rc = RECT {
        left: dirty.0.max(0),
        top: dirty.1.max(0),
        right: (dirty.0 + dirty.2).min(s.vw),
        bottom: (dirty.1 + dirty.3).min(s.vh),
    };
    let info = UPDATELAYEREDWINDOWINFO {
        cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
        hdcDst: s.screen_dc,
        pptDst: &dst,
        psize: &size,
        hdcSrc: s.surface_dc,
        pptSrc: &src,
        crKey: COLORREF(0),
        pblend: &blend,
        dwFlags: ULW_ALPHA,
        prcDirty: &dirty_rc,
    };
    let _ = UpdateLayeredWindowIndirect(hwnd, &info);
}

/// 释放框选占用的全部 GDI 资源。
unsafe fn cleanup(s: &State) {
    // 先释放缓存的 GDI+ 上下文（建在 surface_dc 上，必须先于 DC 删除）
    crate::annot::render_gdiplus::free_graphics(s.label_g);
    // 先恢复原对象再删除位图（位图被选入 DC 时 DeleteObject 会失败并泄漏）
    let _ = SelectObject(s.surface_dc, s.surface_old);
    let _ = DeleteObject(s.surface_bmp);
    let _ = DeleteDC(s.surface_dc);
    let _ = ReleaseDC(HWND::default(), s.screen_dc);
}

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut State> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut State))
    }
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = lp.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let p = (*cs).lpCreateParams as isize;
                if p != 0 {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, p);
                }
            }
            LRESULT(0)
        }

        WM_ERASEBKGND => LRESULT(1),

        // 分层窗口的内容由 UpdateLayeredWindow 提供，系统不会要求我们绘制；
        // 这里仅把可能残留的重绘请求"消费"掉。
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let p = annot_local_from_lparam(lp, s);
                    let shift = shift_down();
                    crate::diag::log(&format!(
                        "[overlay] annot down p=({},{}) tool={:?}",
                        p.x,
                        p.y,
                        s.annot.tool()
                    ));
                    let a = s.annot.on_l_down(p, shift);
                    // 进入绘制/拖动/缩放即捕获鼠标（见 5.1）
                    if s.annot.capturing() {
                        SetCapture(hwnd);
                    }
                    annot_apply(hwnd, s, a);
                    return LRESULT(0);
                }
                SetCapture(hwnd);
                // 开始拖拽前先擦掉悬停辅助层/候选框，避免与新框线互相覆盖
                clear_hover_layer(hwnd, s);
                s.dragging = true;
                s.start = point_from_lparam(lp, s.vx, s.vy);
                s.cur = s.start;
                repaint_frame(hwnd, s);
            }
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let p = annot_local_from_lparam(lp, s);
                    let shift = shift_down();
                    let a = s.annot.on_mouse_move(p, shift);
                    annot_apply(hwnd, s, a);
                    return LRESULT(0);
                }
                if s.dragging {
                    s.cur = point_from_lparam(lp, s.vx, s.vy);
                    repaint_frame(hwnd, s);
                } else {
                    let pt = point_from_lparam(lp, s.vx, s.vy);
                    s.assist.cursor = Some(pt);
                    update_pick_hover(hwnd, s, pt);
                    repaint_hover(hwnd, s);
                }
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let p = annot_local_from_lparam(lp, s);
                    let shift = shift_down();
                    let a = s.annot.on_l_up(p, shift);
                    crate::diag::log(&format!(
                        "[overlay] annot up p=({},{}) tool={:?} annotations={}",
                        p.x,
                        p.y,
                        s.annot.tool(),
                        s.annot.doc.len()
                    ));
                    let _ = ReleaseCapture();
                    annot_apply(hwnd, s, a);
                    sync_style_to_panel(hwnd, s);
                    return LRESULT(0);
                }
            }
            if !state_of(hwnd)
                .map(|s| s.mode == OverlayMode::Annotating)
                .unwrap_or(true)
            {
                finish_selection(hwnd, lp);
            }
            LRESULT(0)
        }

        WM_SETCURSOR => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating && (lp.0 & 0xFFFF) as u32 == HTCLIENT {
                    let mut cpt = POINT::default();
                    if GetCursorPos(&mut cpt).is_ok() {
                        let p = Pt::new(cpt.x - s.region.x, cpt.y - s.region.y);
                        set_annot_cursor(s.annot.cursor_at(p));
                    }
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        // 标注期间不抢焦点：保持工具条为前台窗口（键盘消息继续由工具条转发），
        // 同时冻结层仍能收到鼠标消息用于标注。
        WM_MOUSEACTIVATE => {
            if state_of(hwnd)
                .map(|s| s.mode == OverlayMode::Annotating)
                .unwrap_or(false)
            {
                return LRESULT(MA_NOACTIVATE as isize);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        WM_MOUSEWHEEL => {
            if let Some(s) = state_of(hwnd) {
                let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                if delta != 0 && ctrl_down() {
                    match s.mode {
                        // 选区阶段：Ctrl+滚轮调放大镜倍率
                        OverlayMode::Selecting => {
                            let step = if delta > 0 { 1 } else { -1 };
                            s.assist.lens_zoom = (s.assist.lens_zoom + step).clamp(1, 16);
                            repaint_hover(hwnd, s);
                            crate::diag::log(&format!(
                                "[assist] lens_zoom={}x cursor={:?}",
                                s.assist.lens_zoom, s.assist.cursor
                            ));
                            return LRESULT(0);
                        }
                        // 标注阶段：已选中工具时，光标在画布上滚轮也调整该工具尺寸
                        OverlayMode::Annotating => {
                            if let Some(kind) = SizeKind::for_tool(s.annot.tool()) {
                                adjust_annot_size(hwnd, s, kind, if delta > 0 { 1 } else { -1 });
                            }
                            return LRESULT(0);
                        }
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        WM_KEYDOWN => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let ctrl = ctrl_down();
                    let shift = shift_down();
                    annot_key(hwnd, s, wp.0, ctrl, shift);
                    return LRESULT(0);
                }
                if wp.0 == VK_ESCAPE {
                    // 选区确认前：Esc 取消框选拆窗（确认后由工具条/标注侧处理）。
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                handle_selecting_key(hwnd, s, wp.0, shift_down());
            }
            LRESULT(0)
        }

        // 工具条 → 冻结层：切换工具
        WM_APP_SET_TOOL => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let tool = tool_from_usize(wp.0);
                    crate::diag::log(&format!(
                        "[overlay] set_tool wp={} -> {:?}",
                        wp.0, tool
                    ));
                    let (a, cur_tool) = {
                        let sess = &mut s.annot;
                        let a = sess.set_tool(tool);
                        (a, sess.tool())
                    };
                    annot_apply(hwnd, s, a);
                    post_to_toolbar(WM_APP_TOOL_CHANGED, cur_tool as usize, 0);
                }
            }
            LRESULT(0)
        }

        // 工具条 → 冻结层：转发按键（wp = VK，lp bit0=Ctrl / bit1=Shift）
        WM_APP_KEY => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let ctrl = lp.0 & 1 != 0;
                    let shift = lp.0 & 2 != 0;
                    annot_key(hwnd, s, wp.0, ctrl, shift);
                }
            }
            LRESULT(0)
        }

        // 文字输入：字符直达（焦点在冻结层，含 IME 提交的中文 —— WM_IME_CHAR
        // 经 DefWindowProc 翻译为 WM_CHAR 到达，见模块头「文字输入与 IME」），
        // 或经工具条转发（WM_APP_CHAR，焦点在工具条时的兜底路径）。仅输入中消费。
        WM_CHAR => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    if let Some(c) = char::from_u32(wp.0 as u32) {
                        let a = s.annot.on_char(c);
                        annot_apply(hwnd, s, a);
                    }
                }
            }
            LRESULT(0)
        }

        // 工具条 → 冻结层：转发 WM_CHAR 字符（文字输入）。仅输入中消费。
        WM_APP_CHAR => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    if let Some(c) = char::from_u32(wp.0 as u32) {
                        let a = s.annot.on_char(c);
                        annot_apply(hwnd, s, a);
                    }
                }
            }
            LRESULT(0)
        }

        // 工具条 → 冻结层：同步查询是否处于文字输入中。
        // 输入中时工具条的字母键应放行为正文输入，不当工具快捷键。
        WM_APP_QUERY_TYPING => {
            let typing = state_of(hwnd)
                .map(|s| s.mode == OverlayMode::Annotating && s.annot.typing())
                .unwrap_or(false);
            LRESULT(typing as isize)
        }

        // 预留：请求整选区重绘
        WM_APP_ANNOT_DIRTY => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let sel = Rect::new(0, 0, s.region.width, s.region.height);
                    repaint_annot(hwnd, s, sel);
                }
            }
            LRESULT(0)
        }

        // 颜色面板 → 标注会话：应用样式补丁
        WM_APP_SET_STYLE => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let field = StyleField::from_u32(wp.0 as u32);
                    if let Some(f) = field {
                        let a = apply_style_message(&mut s.annot, f, lp.0);
                        annot_apply(hwnd, s, a);
                        sync_style_to_panel(hwnd, s);
                    }
                }
            }
            LRESULT(0)
        }

        // 颜色面板 → 标注会话：清空全部（一条撤销记录）
        WM_APP_CLEAR_ALL => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let a = s.annot.clear_all();
                    annot_apply(hwnd, s, a);
                    sync_style_to_panel(hwnd, s);
                }
            }
            LRESULT(0)
        }

        // 工具条 → 冻结层：Ctrl+滚轮调整标注默认尺寸
        WM_APP_ADJUST_SIZE => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    if let Some(kind) = SizeKind::from_u32(wp.0 as u32) {
                        adjust_annot_size(hwnd, s, kind, lp.0 as i32);
                    }
                }
            }
            LRESULT(0)
        }

        // E2E 只读查询：当前选中 id
        WM_APP_QUERY_STYLE => {
            let id = state_of(hwnd)
                .and_then(|s| s.annot.selected_id())
                .unwrap_or(0);
            LRESULT((id > 0) as isize + ((id as isize) & 0x7FFF))
        }

        // E2E 只读查询：当前智能选区候选是否存在
        WM_APP_QUERY_PICK => {
            let has = state_of(hwnd)
                .map(|s| s.pick.hover.is_some())
                .unwrap_or(false);
            LRESULT(has as isize)
        }

        // 文字输入子进程 → 冻结层：正文变化 / 结束输入（`dwData` 0=变化 1=结束）。
        // 变化只更新"输入中的正文"（不重绘，正文显示在子进程的输入窗里）；结束时提交落图。
        // 发送者必须是当前输入子进程的窗口（防同会话其他进程注入标注文字）。
        WM_COPYDATA => {
            if let Some((commit, text)) = crate::textinput::parse_payload(lp) {
                if !crate::textinput::is_expected_sender(wp) {
                    crate::diag::log("[overlay] 忽略非输入子进程发来的 WM_COPYDATA");
                    return LRESULT(0);
                }
                if let Some(s) = state_of(hwnd) {
                    if s.mode == OverlayMode::Annotating && s.annot.typing() {
                        let a = if commit {
                            s.annot.commit_typing_with(text)
                        } else {
                            s.annot.set_typing_text(text)
                        };
                        annot_apply(hwnd, s, a);
                    }
                }
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        // 确认后保留的窗口不接受关闭请求（否则红框提前消失、run() 误判为已确认）；
        // 它的生命周期由调用方持有的 Selection 决定。
        WM_CLOSE => {
            if !state_of(hwnd)
                .map(|s| s.mode == OverlayMode::Annotating)
                .unwrap_or(true)
            {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            // 会话结束：确保文字输入子进程随之退出（不再等待 owner 轮询）
            crate::textinput::stop();
            // State 由 run()/Selection 释放（需要读回选区结果），这里只断开引用。
            // 注意：由 Selection 主动拆窗时不能再 PostQuitMessage —— 那时已不在
            // 框选消息循环内，多出来的 WM_QUIT 会被主循环当成"退出程序"。
            let quiet = state_of(hwnd).map(|s| s.quiet_destroy).unwrap_or(false);
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !quiet {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 鼠标松开：拖动够大则取选区并**保留红框**；单击在智能选区开启时自动扩充；
/// 过小且无候选则擦掉框线，留在框选状态。
unsafe fn finish_selection(hwnd: HWND, lp: LPARAM) {
    let Some(s) = state_of(hwnd) else { return };
    if !s.dragging {
        return;
    }
    s.dragging = false;
    let _ = ReleaseCapture();

    let r = norm_rect(s.start, s.cur);
    let cfg = crate::config::get();
    let min_side = util::scaled(cfg.ui_overlay_min_drag, overlay_dpi());
    if r.2 >= min_side && r.3 >= min_side {
        let region = clamp_region(r, s.vx, s.vy, s.vw, s.vh);
        if begin_annotating(hwnd, region) {
            return;
        }
    }

    // 单击（位移 ≤ 4px）且智能选区开启：点击自动扩充为光标处窗口/元素。
    let click = (s.cur.x - s.start.x).abs() <= 4 && (s.cur.y - s.start.y).abs() <= 4;
    if click && s.pick_level != PickLevel::None && s.pick_auto_expand {
        let sp = point_from_lparam(lp, s.vx, s.vy);
        let mut target = s.pick.hover.clone();
        if target.is_none() {
            target = probe_at(s, hwnd, sp.x, sp.y, effective_level(s.pick_level), true);
        }
        if let Some(t) = target {
            let region = clamp_region(
                (t.rect.x, t.rect.y, t.rect.w, t.rect.h),
                s.vx,
                s.vy,
                s.vw,
                s.vh,
            );
            if region.width >= min_side && region.height >= min_side
                && begin_annotating(hwnd, region)
            {
                crate::diag::log(&format!(
                    "[pick] click auto-expand -> {}×{} @ {},{}",
                    region.width, region.height, region.x, region.y
                ));
                return;
            }
        }
    }

    // 单击 / 过小的拖动：不产生选区，擦掉已画的框线与标签（还原为暗化像素），
    // 继续等待框选。脏区必须覆盖框线与标签的**并集**——漏掉标签会让它残留在屏幕上。
    if let Some(bits) = s.last.take() {
        let label = s.last_label.take();
        let dirty = label.map(|ll| union_rect(bits, ll)).unwrap_or(bits);
        if let Some(shot) = &s.snapshot {
            restore_dim_rect(s.surface_bits, s.vw, s.vh, shot, bits, cfg.ui_overlay_dim);
            if let Some(ll) = label {
                restore_dim_rect(s.surface_bits, s.vw, s.vh, shot, ll, cfg.ui_overlay_dim);
            }
        }
        commit(hwnd, s, dirty);
    }
}

/// 以 `region`（屏幕物理像素）作为最终选区，进入标注模式。
/// 负责：从 snapshot 裁图、构造 AnnotSession、创建 annot_base、切换 mode、重绘。
/// 返回 `false` 表示区域非法（过小/越界），调用方回退到 `Selecting`。
unsafe fn begin_annotating(hwnd: HWND, region: SelectedRegion) -> bool {
    let Some(s) = state_of(hwnd) else { return false };
    if region.width < 1 || region.height < 1 {
        return false;
    }
    clear_hover_layer(hwnd, s);
    let cfg = crate::config::get();
    let dpi = overlay_dpi();
    let (dim, frame_px) = (cfg.ui_overlay_dim, packed(cfg.ui_overlay_frame_color));
    let sel_c = (
        region.x - s.vx,
        region.y - s.vy,
        region.width,
        region.height,
    );
    let width = util::scaled(cfg.ui_overlay_frame_width, dpi);
    // 擦上一帧/标签（还原为暗化像素）、提亮最终选区、画框线。
    if let Some(shot) = &s.snapshot {
        if let Some(last) = s.last {
            if last != sel_c {
                restore_dim_rect(s.surface_bits, s.vw, s.vh, shot, last, dim);
            }
        }
        if let Some(ll) = s.last_label.take() {
            restore_dim_rect(s.surface_bits, s.vw, s.vh, shot, ll, dim);
        }
        restore_rect(s.surface_bits, s.vw, s.vh, shot, sel_c);
        draw_frame(s.surface_bits, s.vw, s.vh, sel_c, width, frame_px);
    }
    s.last_label = None;
    // 成品从整屏快照裁出后**立即释放快照**（用于裁剪的整屏资源及时回收）。
    let Some(shot) = s.snapshot.take() else {
        return false;
    };
    let cropped = Arc::new(shot.crop(region.x, region.y, region.width, region.height));
    commit(hwnd, s, (0, 0, s.vw, s.vh));
    s.last = Some(sel_c);
    s.result_image = Some(cropped);
    // 进入标注模式：不转鼠标穿透，冻结层继续接收鼠标用于标注。
    s.region = region;
    s.annot = AnnotSession::new_with_prefs(
        Rect::new(0, 0, region.width, region.height),
        dpi,
        crate::config::get().annot_prefs(),
    );
    s.annot_base = DibSurface::new(region.width, region.height);
    if s.annot_base.is_none() {
        crate::diag::log(&format!(
            "[overlay] 标注底图创建失败（{}×{}）：本次会话标注将不可见",
            region.width, region.height
        ));
    }
    if let Some(base) = &s.annot_base {
        base.blit_from(s.surface_dc, sel_c.0, sel_c.1, sel_c.2, sel_c.3);
    }
    s.mode = OverlayMode::Annotating;
    crate::diag::log(&format!(
        "[overlay] 选区确认 {}×{} @ {},{}：已裁出成品、建立标注底图（进入标注模式）",
        region.width, region.height, region.x, region.y
    ));
    PostQuitMessage(0);
    true
}

// ─── 标注集成（见设计文档 1.2 / 4.1 / 5.5 / 5.6） ───────────────────

/// 鼠标位置 → 选区局部坐标。
unsafe fn annot_local_from_lparam(lp: LPARAM, s: &State) -> Pt {
    let sp = point_from_lparam(lp, s.vx, s.vy);
    Pt::new(sp.x - s.region.x, sp.y - s.region.y)
}

unsafe fn shift_down() -> bool {
    (GetKeyState(VK_SHIFT.0 as i32) as u16) & 0x8000 != 0
}

unsafe fn ctrl_down() -> bool {
    (GetKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0
}

/// `WM_APP_SET_TOOL` 的 WPARAM → [`Tool`]。
fn tool_from_usize(v: usize) -> Tool {
    match v {
        1 => Tool::Rect,
        2 => Tool::Arrow,
        3 => Tool::StepNo,
        4 => Tool::Ellipse,
        5 => Tool::Line,
        6 => Tool::Pen,
        7 => Tool::Mosaic,
        8 => Tool::Highlight,
        9 => Tool::Text,
        10 => Tool::Eraser,
        11 => Tool::Watermark,
        _ => Tool::None,
    }
}

unsafe fn set_annot_cursor(kind: CursorKind) {
    let id = match kind {
        CursorKind::Arrow => IDC_ARROW,
        CursorKind::Cross => IDC_CROSS,
        CursorKind::SizeAll => IDC_SIZEALL,
        CursorKind::SizeNS => IDC_SIZENS,
        CursorKind::SizeWE => IDC_SIZEWE,
        CursorKind::SizeNESW => IDC_SIZENESW,
        CursorKind::SizeNWSE => IDC_SIZENWSE,
        CursorKind::IBeam => IDC_IBEAM,
    };
    if let Ok(c) = LoadCursorW(None, id) {
        SetCursor(c);
    }
}

/// 应用一次 Ctrl+滚轮尺寸调整：更新默认偏好、重绘，并用 Toast 实时显示当前值。
///
/// 工具条转发（[`WM_APP_ADJUST_SIZE`]）与冻结层自身收到滚轮时共用。
unsafe fn adjust_annot_size(hwnd: HWND, s: &mut State, kind: SizeKind, delta: i32) {
    let a = s.annot.adjust_size(kind, delta);
    annot_apply(hwnd, s, a);
    crate::toast::show(&format!(
        "{} {}",
        kind.label(),
        s.annot.size_value(kind)
    ));
    crate::diag::log(&format!(
        "[annot] adjust {} -> {}",
        kind.label(),
        s.annot.size_value(kind)
    ));
}

/// 消费一次标注事件结果：光标、重绘、Esc 交还工具条，以及文字输入的焦点/IME 管理。
unsafe fn annot_apply(hwnd: HWND, s: &mut State, a: annot::Action) {
    if let Some(c) = a.cursor {
        set_annot_cursor(c);
    }
    if let Some(d) = a.redraw {
        repaint_annot(hwnd, s, d);
    }
    // 文字输入的子进程管理（见模块头「文字输入与 IME」）。
    sync_text_helper(hwnd, s);
    if a.esc_passthrough {
        post_to_toolbar(WM_APP_ESC_CLOSE, 0, 0);
    }
}

/// 同步文字输入子进程：
/// - 进入输入 / 切换落点 → 拉起（或重挂）子进程，输入窗贴在锚点上；
/// - 离开输入 → 收掉子进程并把焦点还给工具条（工具条快捷键恢复可用）。
///
/// 幂等：供 [`annot_apply`] 与 [`Selection::commit_typing`] 共用。
unsafe fn sync_text_helper(hwnd: HWND, s: &mut State) {
    if s.annot.typing() {
        if let Some((anchor, _text, style)) = s.annot.typing_state() {
            if s.text_helper_anchor != Some(anchor) {
                crate::textinput::stop();
                // 锚点是选区局部坐标 → 加选区原点得到虚拟屏幕（物理像素）坐标
                let sx = s.region.x + anchor.x;
                let sy = s.region.y + anchor.y;
                if !crate::textinput::start(hwnd, sx, sy, style.font_px) {
                    // 子进程起不来时的兜底：进程内输入（主进程已禁用 IME，仅 ASCII 可用）
                    crate::diag::log("[overlay] 文字输入子进程启动失败 -> 进程内兜底输入");
                    let _ = SetFocus(hwnd);
                }
                s.text_helper_anchor = Some(anchor);
            }
        }
    } else if s.text_helper_anchor.is_some() {
        crate::textinput::stop();
        s.text_helper_anchor = None;
        let name = to_wide(TOOLBAR_CLASS);
        if let Ok(tb) = FindWindowW(PCWSTR::from_raw(name.as_ptr()), PCWSTR::null()) {
            if !tb.is_invalid() {
                let _ = SetFocus(tb);
            }
        }
    }
}

/// 处理一次标注按键并把（可能变化的）工具同步给工具条。
unsafe fn annot_key(hwnd: HWND, s: &mut State, vk: usize, ctrl: bool, shift: bool) {
    let a = s.annot.on_key(vk, ctrl, shift);
    let tool = s.annot.tool();
    annot_apply(hwnd, s, a);
    post_to_toolbar(WM_APP_TOOL_CHANGED, tool as usize, 0);
    sync_style_to_panel(hwnd, s);
}

/// 向工具条窗口投递消息（用类名定位；找不到则忽略）。
unsafe fn post_to_toolbar(msg: u32, wp: usize, lp: isize) {
    let name = to_wide(TOOLBAR_CLASS);
    if let Ok(h) = FindWindowW(PCWSTR::from_raw(name.as_ptr()), PCWSTR::null()) {
        if !h.is_invalid() {
            let _ = PostMessageW(h, msg, WPARAM(wp), LPARAM(lp));
        }
    }
}

// ─── 智能选区 / 截图辅助 ────────────────────────────────────────────

const VK_TAB: usize = 0x09;
const VK_RETURN: usize = 0x0D;
const VK_C_: usize = 0x43;
const VK_R_: usize = 0x52;
const VK_W_: usize = 0x57;
const VK_E_: usize = 0x45;

/// Region 级别在"悬停"阶段不显示候选框；单击时才按窗口扩充。
fn effective_level(level: PickLevel) -> PickLevel {
    match level {
        PickLevel::Region => PickLevel::Window,
        other => other,
    }
}

/// 临时取出引擎做探测（避免同时借用 `State` 的其它字段）。
unsafe fn probe_at(
    s: &mut State,
    hwnd: HWND,
    x: i32,
    y: i32,
    level: PickLevel,
    expand: bool,
) -> Option<PickTarget> {
    if level == PickLevel::None {
        return None;
    }
    let mut engine = s.pick_engine.take().unwrap_or_else(PickEngine::new);
    let t = engine.probe(hwnd, x, y, level, s.pick_padding, expand);
    s.pick_engine = Some(engine);
    t
}

/// 鼠标悬停探测（节流）。结果写入 `s.pick.hover`（Region 级别也保存，供单击使用）。
unsafe fn update_pick_hover(hwnd: HWND, s: &mut State, pt: POINT) {
    if s.pick_level == PickLevel::None || s.pick_level == PickLevel::Region {
        // Region 级别不在悬停时探测（避免默认配置下每帧临时改 overlay 样式造成闪烁）；
        // 单击时由 finish_selection 探测并自动扩充。
        s.pick.hover = None;
        return;
    }
    let now = windows::Win32::System::SystemInformation::GetTickCount();
    if let Some((lx, ly, lt)) = s.pick.last_probe {
        if lx == pt.x && ly == pt.y && now.wrapping_sub(lt) < s.pick_probe_interval_ms {
            return;
        }
    }
    s.pick.last_probe = Some((pt.x, pt.y, now));
    let level = effective_level(s.pick_level);
    s.pick.hover = probe_at(s, hwnd, pt.x, pt.y, level, s.pick_auto_expand);
}

/// 擦除悬停辅助层（放大镜/辅助线/信息条/候选框），并清空光标与候选。
unsafe fn clear_hover_layer(hwnd: HWND, s: &mut State) {
    let mut erase: Vec<Rect4> = s.assist.last_dirty.drain(..).collect();
    if let Some(r) = s.pick.last_drew.take() {
        erase.push(r);
    }
    s.pick.hover = None;
    s.assist.cursor = None;
    s.pick.last_probe = None;
    if erase.is_empty() {
        return;
    }
    let dim = crate::config::get().ui_overlay_dim;
    if let Some(shot) = s.snapshot.clone() {
        for r in &erase {
            restore_dim_rect(s.surface_bits, s.vw, s.vh, &shot, *r, dim);
        }
        if let Some(u) = union_all(&erase) {
            commit(hwnd, s, u);
        }
    }
}

fn union_all(rects: &[Rect4]) -> Option<Rect4> {
    let mut it = rects.iter().copied().filter(|r| r.2 > 0 && r.3 > 0);
    let first = it.next()?;
    Some(it.fold(first, |a, b| union_rect(a, b)))
}

/// 采样快照原始 BGRA 像素（越界返回 None）。
fn sample_bgra(shot: &CapturedImage, x: i32, y: i32) -> Option<u32> {
    let lx = x - shot.x;
    let ly = y - shot.y;
    if lx < 0 || ly < 0 || lx >= shot.width || ly >= shot.height {
        return None;
    }
    let off = (ly as usize * shot.width.max(1) as usize + lx as usize) * 4;
    if off + 4 > shot.data.len() {
        return None;
    }
    Some(
        (shot.data[off + 3] as u32) << 24
            | (shot.data[off + 2] as u32) << 16
            | (shot.data[off + 1] as u32) << 8
            | shot.data[off] as u32,
    )
}

/// 与背景对比度足够的 1px 辅助线颜色（亮背景黑线、暗背景白线）。
fn contrast_px(shot: &CapturedImage, x: i32, y: i32) -> u32 {
    let Some(px) = sample_bgra(shot, x, y) else {
        return 0xFFFF_FFFF;
    };
    let r = (px >> 16) & 0xFF;
    let g = (px >> 8) & 0xFF;
    let b = px & 0xFF;
    let luma = (r * 299 + g * 587 + b * 114) / 1000;
    if luma > 140 {
        0xFF00_0000 // 不透明黑
    } else {
        0xFFFF_FFFF // 不透明白
    }
}

unsafe fn write_px(bits: *mut u32, vw: i32, vh: i32, x: i32, y: i32, px: u32) {
    if x < 0 || y < 0 || x >= vw || y >= vh {
        return;
    }
    *bits.add(y as usize * vw as usize + x as usize) = px;
}

/// 重绘悬停辅助层：先精确擦除上一帧，再画候选框/辅助线/放大镜/信息条。
unsafe fn repaint_hover(hwnd: HWND, s: &mut State) {
    let Some(shot) = s.snapshot.clone() else { return };
    let cfg = crate::config::get();
    let dpi = overlay_dpi();
    let dim = cfg.ui_overlay_dim;

    let mut erase: Vec<Rect4> = s.assist.last_dirty.drain(..).collect();
    if let Some(r) = s.pick.last_drew.take() {
        erase.push(r);
    }
    for r in &erase {
        restore_dim_rect(s.surface_bits, s.vw, s.vh, &shot, *r, dim);
    }
    let mut dirty = erase.clone();

    let Some(cursor) = s.assist.cursor else {
        if let Some(u) = union_all(&dirty) {
            commit(hwnd, s, u);
        }
        return;
    };
    let cursor_local = Pt::new(cursor.x - s.vx, cursor.y - s.vy);

    // 1) 候选框（Region 级别只在单击时生效，不画悬停框）
    if matches!(s.pick_level, PickLevel::Window | PickLevel::Element) {
        if let Some(t) = s.pick.hover.clone() {
            let frame = (
                t.rect.x - s.vx,
                t.rect.y - s.vy,
                t.rect.w,
                t.rect.h,
            );
            let label_rect = if s.pick_show_label {
                draw_pick_label(s, frame, &t.label, dpi)
            } else {
                None
            };
            draw_pick_frame(s, frame);
            let drew = label_rect.map(|l| union_rect(frame, l)).unwrap_or(frame);
            s.pick.last_drew = Some(drew);
            dirty.push(drew);
        }
    }

    // 2) 辅助线（从光标向四边延伸）
    if s.assist.enabled_aux {
        let line = contrast_px(&shot, cursor.x, cursor.y);
        let (lx, ly) = (cursor_local.x, cursor_local.y);
        let mut push = |r: Rect4| {
            if r.2 > 0 && r.3 > 0 {
                dirty.push(r);
            }
        };
        if ly > 0 {
            fill(s.surface_bits, s.vw, s.vh, (lx, 0, 1, ly), line);
            push((lx, 0, 1, ly));
        }
        if ly < s.vh {
            let h = s.vh - ly;
            fill(s.surface_bits, s.vw, s.vh, (lx, ly, 1, h), line);
            push((lx, ly, 1, h));
        }
        if lx > 0 {
            fill(s.surface_bits, s.vw, s.vh, (0, ly, lx, 1), line);
            push((0, ly, lx, 1));
        }
        if lx < s.vw {
            let w = s.vw - lx;
            fill(s.surface_bits, s.vw, s.vh, (lx, ly, w, 1), line);
            push((lx, ly, w, 1));
        }
        if s.assist.grid {
            if let Some(sel) = grid_selection(s) {
                for r in draw_grid(s, sel) {
                    dirty.push(r);
                }
            }
        }
    }

    // 3) 放大镜
    if s.assist.enabled_magnifier {
        if let Some(r) = draw_lens(s, &shot, cursor, dpi) {
            dirty.push(r);
        }
    }

    // 4) 信息条
    if s.assist.enabled_info {
        if let Some(r) = draw_info(s, &shot, cursor, dpi) {
            dirty.push(r);
        }
    }

    // 辅助层脏区（不含候选框：候选框单独存在 pick.last_drew）
    s.assist.last_dirty = dirty
        .iter()
        .copied()
        .filter(|r| Some(*r) != s.pick.last_drew)
        .collect();
    if let Some(u) = union_all(&dirty) {
        commit(hwnd, s, u);
    }
}

/// 选区九宫格（拖拽中才存在选区；悬停阶段无选区，返回 None）。
fn grid_selection(s: &State) -> Option<Rect4> {
    let cur = norm_rect(s.start, s.cur);
    if cur.2 > 0 && cur.3 > 0 {
        Some((cur.0 - s.vx, cur.1 - s.vy, cur.2, cur.3))
    } else {
        None
    }
}

unsafe fn draw_grid(s: &State, sel: Rect4) -> Vec<Rect4> {
    let x1 = sel.0 + sel.2 / 3;
    let x2 = sel.0 + sel.2 * 2 / 3;
    let y1 = sel.1 + sel.3 / 3;
    let y2 = sel.1 + sel.3 * 2 / 3;
    let px = 0x9999_9999; // 半透明白（预乘：alpha=0x99，通道=0x99）
    let mut out = Vec::with_capacity(4);
    fill(s.surface_bits, s.vw, s.vh, (x1, sel.1, 1, sel.3), px);
    fill(s.surface_bits, s.vw, s.vh, (x2, sel.1, 1, sel.3), px);
    fill(s.surface_bits, s.vw, s.vh, (sel.0, y1, sel.2, 1), px);
    fill(s.surface_bits, s.vw, s.vh, (sel.0, y2, sel.2, 1), px);
    out.push((x1, sel.1, 1, sel.3));
    out.push((x2, sel.1, 1, sel.3));
    out.push((sel.0, y1, sel.2, 1));
    out.push((sel.0, y2, sel.2, 1));
    out
}

/// 候选框虚线（2px 实、3px 空）+ 高对比色。
unsafe fn draw_pick_frame(s: &State, frame: Rect4) {
    let px = packed(Rgba::new(0x1E, 0x6F, 0xFF, 0xFF));
    let mut x = frame.0;
    while x < frame.0 + frame.2 {
        let w = (frame.0 + frame.2 - x).min(2);
        fill(s.surface_bits, s.vw, s.vh, (x, frame.1, w, 1), px);
        fill(
            s.surface_bits,
            s.vw,
            s.vh,
            (x, frame.1 + frame.3 - 1, w, 1),
            px,
        );
        x += 5;
    }
    let mut y = frame.1;
    while y < frame.1 + frame.3 {
        let h = (frame.1 + frame.3 - y).min(2);
        fill(s.surface_bits, s.vw, s.vh, (frame.0, y, 1, h), px);
        fill(
            s.surface_bits,
            s.vw,
            s.vh,
            (frame.0 + frame.2 - 1, y, 1, h),
            px,
        );
        y += 5;
    }
}

/// 候选标签（填深底 + 白字），返回标签矩形。
unsafe fn draw_pick_label(s: &State, frame: Rect4, label: &str, dpi: i32) -> Option<Rect4> {
    let px = util::scaled(crate::config::get().ui_overlay_label_px, dpi);
    let w = label_w(label, px);
    let h = (px as f32 * 1.4).ceil() as i32;
    let mut x = frame.0;
    let mut y = frame.1 + frame.3 + 4;
    if y + h > s.vh {
        y = (frame.1 - h - 4).max(0);
    }
    if x + w > s.vw {
        x = s.vw - w;
    }
    x = x.max(0);
    y = y.max(0);
    let r = (x, y, w, h);
    fill(s.surface_bits, s.vw, s.vh, r, 0xFF22_2222);
    draw_text_on_surface(s, r, label, px, Rgba::new(0xFF, 0xFF, 0xFF, 0xFF));
    Some(r)
}

/// 在表面矩形内绘制一行白字（GDI+ + alpha 防御）。
unsafe fn draw_text_on_surface(s: &State, r: Rect4, text: &str, px: i32, color: Rgba) {
    let g = s.label_g;
    if g.is_null() {
        return;
    }
    crate::annot::render_gdiplus::draw_label_string(
        g,
        (r.0 + 4) as f32,
        (r.1 + 2) as f32,
        text,
        px as f32,
        color,
    );
    let _ = GdiFlush();
    if let Some(cr) = clip_rect((r.0 - 2, r.1 - 2, r.2 + 4, r.3 + 4), s.vw, s.vh) {
        crate::annot::session::alpha_fix(s.surface_bits, s.vw, s.vh, Rect::new(cr.0, cr.1, cr.2, cr.3));
    }
}

/// 放大镜（最近邻放大 + 边框 + 十字准星）；返回脏区。
unsafe fn draw_lens(s: &State, shot: &CapturedImage, cursor: POINT, _dpi: i32) -> Option<Rect4> {
    let screen = Rect::new(s.vx, s.vy, s.vw, s.vh);
    let lens = assist::lens_rect(Pt::new(cursor.x, cursor.y), s.assist.lens_size, screen);
    let local = (lens.x - s.vx, lens.y - s.vy, lens.w, lens.h);
    if local.2 <= 0 || local.3 <= 0 {
        return None;
    }
    let zoom = s.assist.lens_zoom.max(1);
    let src_w = (lens.w / zoom).max(1);
    let src_h = (lens.h / zoom).max(1);
    let sx0 = cursor.x - src_w / 2;
    let sy0 = cursor.y - src_h / 2;
    for dy in 0..lens.h {
        let sy = sy0 + dy / zoom;
        for dx in 0..lens.w {
            let sx = sx0 + dx / zoom;
            let px = sample_bgra(shot, sx, sy).unwrap_or(0xFF20_2020);
            write_px(s.surface_bits, s.vw, s.vh, local.0 + dx, local.1 + dy, px);
        }
    }
    // 边框（外深内亮，任意背景都可辨认）
    let dark = 0xFF00_0000;
    let light = 0xFFFF_FFFF;
    fill(s.surface_bits, s.vw, s.vh, (local.0, local.1, local.2, 1), dark);
    fill(s.surface_bits, s.vw, s.vh, (local.0, local.1 + local.3 - 1, local.2, 1), dark);
    fill(s.surface_bits, s.vw, s.vh, (local.0, local.1, 1, local.3), dark);
    fill(s.surface_bits, s.vw, s.vh, (local.0 + local.2 - 1, local.1, 1, local.3), dark);
    fill(s.surface_bits, s.vw, s.vh, (local.0 + 1, local.1 + 1, local.2 - 2, 1), light);
    fill(s.surface_bits, s.vw, s.vh, (local.0 + 1, local.1 + local.3 - 2, local.2 - 2, 1), light);
    // 十字准星（光标像素在镜头中的局部位置）
    let c = Pt::new(cursor.x - lens.x, cursor.y - lens.y);
    let cross = contrast_px(shot, cursor.x, cursor.y);
    fill(s.surface_bits, s.vw, s.vh, (local.0 + 1, local.1 + c.y, local.2 - 2, 1), cross);
    fill(s.surface_bits, s.vw, s.vh, (local.0 + c.x, local.1 + 1, 1, local.3 - 2), cross);
    // 中心像素外框（5×5）
    let bx = local.0 + c.x - 2;
    let by = local.1 + c.y - 2;
    fill(s.surface_bits, s.vw, s.vh, (bx, by, 5, 1), cross);
    fill(s.surface_bits, s.vw, s.vh, (bx, by + 4, 5, 1), cross);
    fill(s.surface_bits, s.vw, s.vh, (bx, by, 1, 5), cross);
    fill(s.surface_bits, s.vw, s.vh, (bx + 4, by, 1, 5), cross);
    clip_rect((local.0 - 4, local.1 - 4, local.2 + 8, local.3 + 8), s.vw, s.vh)
}

/// 信息条（尺寸/坐标/颜色），返回脏区。
unsafe fn draw_info(s: &mut State, shot: &CapturedImage, cursor: POINT, dpi: i32) -> Option<Rect4> {
    let color = assist::color_at(shot, cursor.x, cursor.y);
    let sel = None;
    s.assist.info = assist::info_text(sel, Pt::new(cursor.x, cursor.y), color, s.assist.format);
    let px = util::scaled(crate::config::get().ui_overlay_label_px, dpi);
    let w = label_w(&s.assist.info, px);
    let h = (px as f32 * 1.4).ceil() as i32;
    let lx = cursor.x - s.vx + 14;
    let ly = cursor.y - s.vy + 14;
    let x = lx.clamp(0, (s.vw - w).max(0));
    let y = ly.clamp(0, (s.vh - h).max(0));
    let r = (x, y, w, h);
    fill(s.surface_bits, s.vw, s.vh, r, 0xFF22_2222);
    let text = s.assist.info.clone();
    draw_text_on_surface(s, r, &text, px, Rgba::new(0xFF, 0xFF, 0xFF, 0xFF));
    Some(r)
}

/// 选区阶段的键盘操作：拾取级别切换 / 父子切换 / Enter 提交 / C 取色。
unsafe fn handle_selecting_key(hwnd: HWND, s: &mut State, vk: usize, shift: bool) {
    match vk {
        VK_TAB => {
            let Some(hover) = s.pick.hover.clone() else { return };
            let mut engine = s.pick_engine.take().unwrap_or_else(PickEngine::new);
            let t = if shift {
                engine.parent(&hover)
            } else if let Some(c) = s.assist.cursor {
                engine.child(&hover, c.x, c.y)
            } else {
                None
            };
            s.pick_engine = Some(engine);
            if let Some(t) = t {
                crate::diag::log(&format!("[pick] tab -> {} rect={:?}", t.label, t.rect));
                s.pick.hover = Some(t);
                repaint_hover(hwnd, s);
            }
        }
        VK_RETURN => {
            let Some(t) = s.pick.hover.clone() else { return };
            let region = clamp_region(
                (t.rect.x, t.rect.y, t.rect.w, t.rect.h),
                s.vx,
                s.vy,
                s.vw,
                s.vh,
            );
            if begin_annotating(hwnd, region) {
                crate::diag::log(&format!(
                    "[pick] enter commit {}×{} @ {},{}",
                    region.width, region.height, region.x, region.y
                ));
            }
        }
        VK_R_ => set_pick_level(hwnd, s, PickLevel::Region),
        VK_W_ => set_pick_level(hwnd, s, PickLevel::Window),
        VK_E_ => set_pick_level(hwnd, s, PickLevel::Element),
        VK_C_ => {
            let Some(cursor) = s.assist.cursor else { return };
            let Some(shot) = s.snapshot.clone() else { return };
            if shift {
                s.assist.format = s.assist.format.next();
                repaint_hover(hwnd, s);
                crate::diag::log(&format!("[assist] color format -> {}", s.assist.format.label()));
                return;
            }
            if let Some(c) = assist::color_at(&shot, cursor.x, cursor.y) {
                let text = assist::format_color(c, s.assist.format);
                match crate::clipboard::copy_text(&text) {
                    Ok(()) => {
                        crate::diag::log(&format!("[assist] copy color {text}"));
                        crate::toast::show(&format!("已复制颜色 {text}"));
                    }
                    Err(e) => crate::diag::log(&format!("[assist] 复制颜色失败: {e}")),
                }
            }
        }
        _ => {}
    }
}

unsafe fn set_pick_level(hwnd: HWND, s: &mut State, level: PickLevel) {
    s.pick_level = level;
    s.pick.hover = None;
    s.pick.last_probe = None;
    repaint_hover(hwnd, s);
    crate::diag::log(&format!("[pick] level={}", level.label()));
}

// ─── 颜色面板消息 ───────────────────────────────────────────────────

fn rgba_from_lp(v: isize) -> Rgba {
    let v = v as u32;
    Rgba::new(
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
        0xFF,
    )
}

fn color_lp(c: Rgba) -> isize {
    (((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32) as isize
}

unsafe fn apply_style_message(
    sess: &mut AnnotSession,
    field: StyleField,
    lp: isize,
) -> annot::Action {
    use crate::annot::controller::StylePatch;
    match field {
        StyleField::Clear => sess.clear_all(),
        StyleField::Color => {
            let c = rgba_from_lp(lp);
            sess.apply_style(StylePatch {
                stroke_color: Some(c),
                text_color: Some(c),
                badge: Some(crate::annot::style::BadgeStyle {
                    fill: c,
                    text: Rgba::new(0xFF, 0xFF, 0xFF, 0xFF),
                    outline: Some(Rgba::new(0xFF, 0xFF, 0xFF, 0xFF)),
                }),
                ..Default::default()
            })
        }
        StyleField::StrokeColor => sess.apply_style(StylePatch {
            stroke_color: Some(rgba_from_lp(lp)),
            ..Default::default()
        }),
        StyleField::StrokeWidth => sess.apply_style(StylePatch {
            stroke_width: Some((lp as i32).max(1)),
            ..Default::default()
        }),
        StyleField::TextColor => sess.apply_style(StylePatch {
            text_color: Some(rgba_from_lp(lp)),
            ..Default::default()
        }),
        StyleField::TextSize => sess.apply_style(StylePatch {
            text_size: Some((lp as i32).max(4)),
            ..Default::default()
        }),
        StyleField::TextBold => sess.apply_style(StylePatch {
            text_bold: Some(lp != 0),
            ..Default::default()
        }),
        StyleField::TextItalic => sess.apply_style(StylePatch {
            text_italic: Some(lp != 0),
            ..Default::default()
        }),
        StyleField::PenKind => sess.apply_style(StylePatch {
            pen_kind: Some(match lp {
                1 => PenKind::Soft,
                2 => PenKind::Fade,
                _ => PenKind::Solid,
            }),
            ..Default::default()
        }),
        StyleField::MosaicMode => sess.apply_style(StylePatch {
            mosaic_mode: Some(match lp {
                1 => MosaicMode::Blur,
                _ => MosaicMode::Pixelate,
            }),
            ..Default::default()
        }),
        StyleField::HighlightMode => sess.apply_style(StylePatch {
            highlight_mode: Some(match lp {
                1 => HighlightMode::Spotlight,
                _ => HighlightMode::Marker,
            }),
            ..Default::default()
        }),
        StyleField::TextFrame => sess.apply_style(StylePatch {
            text_frame: Some(match lp {
                1 => TextFrame::Bubble,
                2 => TextFrame::Burst,
                _ => TextFrame::None,
            }),
            ..Default::default()
        }),
        StyleField::BadgeColor => sess.apply_style(StylePatch {
            badge: Some(crate::annot::style::BadgeStyle {
                fill: rgba_from_lp(lp),
                text: Rgba::new(0xFF, 0xFF, 0xFF, 0xFF),
                outline: Some(Rgba::new(0xFF, 0xFF, 0xFF, 0xFF)),
            }),
            ..Default::default()
        }),
    }
}

/// 把当前选中项/默认偏好的样式同步给颜色面板（找不到面板则忽略）。
unsafe fn sync_style_to_panel(_overlay: HWND, s: &State) {
    let name = to_wide(STYLE_PANEL_CLASS);
    let Ok(panel) = FindWindowW(PCWSTR::from_raw(name.as_ptr()), PCWSTR::null()) else {
        return;
    };
    if panel.is_invalid() {
        return;
    }
    let prefs = s.annot.prefs();
    let color = match s.annot.selected_shape() {
        Some(Shape::Rect { style, .. })
        | Some(Shape::Ellipse { style, .. })
        | Some(Shape::Line { style, .. })
        | Some(Shape::Arrow { style, .. }) => style.color,
        Some(Shape::Pen { style, .. }) => style.color,
        Some(Shape::Text { style, .. }) | Some(Shape::Watermark { style, .. }) => style.color,
        Some(Shape::Highlight { color, .. }) => *color,
        Some(Shape::StepNo { style, .. }) => style.fill,
        Some(Shape::Mosaic { .. }) | None => prefs.stroke_color,
    };
    let _ = PostMessageW(
        panel,
        WM_APP_STYLE_SYNC,
        WPARAM(StyleField::Color as usize),
        LPARAM(color_lp(color)),
    );
}


/// 局部脏区裁剪到选区；完全在外返回 None。
fn clip_local(r: Rect, sel: Rect) -> Option<Rect> {
    let x0 = r.x.max(0);
    let y0 = r.y.max(0);
    let x1 = r.right().min(sel.w);
    let y1 = r.bottom().min(sel.h);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// 标注层重绘（局部坐标脏区）：恢复底图 → 重画标注 → alpha 防御 → 提交脏区（见 4.1）。
unsafe fn repaint_annot(hwnd: HWND, s: &State, dirty: Rect) {
    let Some(base) = &s.annot_base else {
        return;
    };
    let ox = s.region.x - s.vx;
    let oy = s.region.y - s.vy;
    let sel = Rect::new(0, 0, s.region.width, s.region.height);
    // 外扩余量覆盖描边半宽与 GDI+ 抗锯齿溢出，避免残留旧像素
    let pad = util::scaled(10, overlay_dpi()) + 2;
    let Some(d) = clip_local(dirty.inflate(pad), sel) else {
        return;
    };

    // 1. 恢复底图
    let _ = BitBlt(s.surface_dc, ox + d.x, oy + d.y, d.w, d.h, base.dc, d.x, d.y, SRCCOPY);
    // 2. 重画标注 + 预览 + 选中态（render_into 按 `d` 裁剪/剔除/限制 CPU 写入，
    //    部分重绘与全量重绘逐像素一致——见 session::render_into）。
    //    base 供马赛克/高亮取样 pristine 底图（annot_base = 冻结选区 + 框线）。
    let base_src = s.annot_base.as_ref().map(annot::BaseSource::Surface);
    annot::render_into(
        s.surface_dc,
        s.surface_bits,
        s.vw,
        s.vh,
        Pt::new(ox, oy),
        &s.annot,
        overlay_dpi(),
        base_src.as_ref(),
        d,
    );
    // 3. alpha 防御（表面坐标）
    let sr = Rect::new(ox + d.x, oy + d.y, d.w, d.h);
    crate::annot::session::alpha_fix(s.surface_bits, s.vw, s.vh, sr);
    // 4. 提交脏区
    commit(hwnd, s, (sr.x, sr.y, sr.w, sr.h));
}

/// 框选层横跨整个虚拟屏幕：以**光标所在显示器**的 DPI 作为框线粗细/拖动阈值/
/// 标签字号的换算基准（多屏异缩放下比系统 DPI 更贴近用户正在看的屏幕），
/// 失败时回退系统 DPI。
fn overlay_dpi() -> i32 {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let (mut dx, mut dy) = (0u32, 0u32);
            if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 {
                return (dx as i32).max(96);
            }
        }
        windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32
    }
}

/// 重画选区遮罩帧：擦上一帧（还原为**暗化**像素）→ 提亮当前选区（快照原像素）
/// → 画框线 → 尺寸标签（GDI+ + alpha 防御）→ 提交脏区并集。
/// 只提交变动区域而非整屏，避免每帧整屏重合成。
unsafe fn repaint_frame(hwnd: HWND, s: &mut State) {
    let cfg = crate::config::get();
    let (dim, frame_px) = (cfg.ui_overlay_dim, packed(cfg.ui_overlay_frame_color));
    let cur = norm_rect(s.start, s.cur);
    let cur_c = (cur.0 - s.vx, cur.1 - s.vy, cur.2, cur.3);
    let width = util::scaled(cfg.ui_overlay_frame_width, overlay_dpi());
    let has = cur_c.2 > 0 && cur_c.3 > 0;
    let prev_rect = s.last;
    let prev_label = s.last_label;

    // Arc 克隆避免持有借用（后续还要写 s.last / s.last_label）
    let shot = s.snapshot.clone();
    if let Some(shot) = &shot {
        // 1) 擦上一帧整矩形与上一帧标签区：还原为暗化像素（选区遮罩语义）
        if let Some(last) = prev_rect {
            restore_dim_rect(s.surface_bits, s.vw, s.vh, shot, last, dim);
        }
        if let Some(ll) = prev_label {
            restore_dim_rect(s.surface_bits, s.vw, s.vh, shot, ll, dim);
        }
        // 2) 当前选区提亮（写快照原像素）+ 框线
        if has {
            restore_rect(s.surface_bits, s.vw, s.vh, shot, cur_c);
            draw_frame(s.surface_bits, s.vw, s.vh, cur_c, width, frame_px);
        }
    }
    s.last = has.then_some(cur_c);

    // 3) 尺寸标签（画后记录标签区，供下一帧擦除）；文本用复用缓冲，避免每帧堆分配
    let label = if has {
        use std::fmt::Write as _;
        s.label_text.clear();
        let _ = write!(s.label_text, "{} × {}", cur_c.2, cur_c.3);
        let r = label_rect(
            s.vw,
            s.vh,
            cur_c,
            &s.label_text,
            util::scaled(cfg.ui_overlay_label_px, overlay_dpi()),
        );
        draw_size_label(s, &r, cfg.ui_overlay_label_px);
        Some(r)
    } else {
        None
    };
    s.last_label = label;

    // 4) 脏区 = 上一帧 + 当前帧 + 两帧标签的并集
    let mut dirty: Option<Rect4> = None;
    for r in [prev_rect, has.then_some(cur_c), prev_label, label]
        .into_iter()
        .flatten()
    {
        dirty = Some(match dirty {
            Some(d) => union_rect(d, r),
            None => r,
        });
    }
    if let Some(d) = dirty {
        commit(hwnd, s, d);
    }
}

/// 选区尺寸标签："宽 × 高" GDI+ 白字，画在标签矩形处（文本取 `s.label_text`）。
/// GDI+ 文字不写 alpha：画后 `GdiFlush` + `alpha_fix` 强制不透明
/// （分层窗口上 alpha<255 的像素会显示为半透明并穿透点击）。
unsafe fn draw_size_label(s: &State, r: &Rect4, label_px: i32) {
    // 复用缓存的 GDI+ 上下文（拖动期间每个 mousemove 都画标签，
    // 逐帧 GdipCreateFromHDC 是不必要的固定开销）
    let g = s.label_g;
    if g.is_null() {
        return;
    }
    let cfg = crate::config::get();
    let px = util::scaled(label_px, overlay_dpi()) as f32;
    crate::annot::render_gdiplus::draw_label_string(
        g,
        r.0 as f32,
        r.1 as f32,
        &s.label_text,
        px,
        cfg.ui_overlay_label_color,
    );
    let _ = GdiFlush();
    // alpha 防御：标签矩形（外扩 2px 覆盖抗锯齿边缘）强制不透明
    if let Some((x, y, w, h)) = clip_rect((r.0 - 2, r.1 - 2, r.2 + 4, r.3 + 4), s.vw, s.vh) {
        crate::annot::session::alpha_fix(s.surface_bits, s.vw, s.vh, Rect::new(x, y, w, h));
    }
}

/// 标签宽度估算（数字/×/空格，按字符数近似；仅供脏区计算与定位）。
fn label_w(text: &str, px: i32) -> i32 {
    (text.chars().count() as i32) * px * 62 / 100 + 8
}

/// 标签矩形：选区上方（贴顶则移到下方），左对齐选区左缘（钳回屏幕内）。
/// `sel_c` 为表面（客户区）坐标，返回同坐标系。上下方向都钳回屏幕内。
fn label_rect(vw: i32, vh: i32, sel_c: Rect4, text: &str, px: i32) -> Rect4 {
    let w = label_w(text, px);
    let h = (px as f32 * 1.4).ceil() as i32;
    let mut x = sel_c.0;
    let mut y = sel_c.1 - h - 4;
    if y < 0 {
        y = sel_c.1 + sel_c.3 + 4;
    }
    if y + h > vh {
        y = (vh - h).max(0);
    }
    if x + w > vw {
        x = vw - w;
    }
    if x < 0 {
        x = 0;
    }
    (x, y, w, h)
}

/// 两矩形的最小并集。
fn union_rect(a: Rect4, b: Rect4) -> Rect4 {
    let x0 = a.0.min(b.0);
    let y0 = a.1.min(b.1);
    let x1 = (a.0 + a.2).max(b.0 + b.2);
    let y1 = (a.1 + a.3).max(b.1 + b.3);
    (x0, y0, x1 - x0, y1 - y0)
}

/// 把矩形裁剪到表面范围内；完全在范围外返回 None。
fn clip_rect(r: Rect4, vw: i32, vh: i32) -> Option<Rect4> {
    let x0 = r.0.max(0);
    let y0 = r.1.max(0);
    let x1 = (r.0 + r.2).min(vw);
    let y1 = (r.1 + r.3).min(vh);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// 把表面上某矩形区域复位为透明底。
unsafe fn clear_rect(bits: *mut u32, vw: i32, vh: i32, r: Rect4) {
    fill(bits, vw, vh, r, BG_PX);
}

/// 用整屏快照铺满绘制表面（框选期间的"冻结画面"底图）。
///
/// 常规路径快照与表面同为 `vw×vh`、行距一致；防御路径（快照比虚拟屏幕小）下
/// 源/目标行距不同，必须**逐行**拷贝。快照 alpha 恒为 255，与分层窗口要求的
/// 预乘 BGRA 在此取值下等价。
unsafe fn copy_snapshot_into_surface(bits: *mut u32, vw: i32, vh: i32, shot: &CapturedImage) {
    let rows = vh.min(shot.height.max(0));
    let cols = vw.min(shot.width.max(0));
    if rows <= 0 || cols <= 0 {
        return;
    }
    let src_stride = shot.width.max(1) as usize;
    let src = shot.data.as_ptr() as *const u32;
    for r in 0..rows as usize {
        let off = r * src_stride;
        if (off + cols as usize) * 4 > shot.data.len() {
            break; // 越界保护（快照尺寸异常时宁可留原样也不越界读）
        }
        std::ptr::copy_nonoverlapping(src.add(off), bits.add(r * vw as usize), cols as usize);
    }
}

/// 把表面上某矩形区域恢复为快照原像素（拖拽期间擦除上一帧框线）。
///
/// 与 [`clear_rect`] 的区别：框选期间表面底图是"冻结的整屏快照"而非透明底，
/// 擦帧必须还原快照像素，否则会擦出一块透明空洞。
///
/// 遮罩语义下本函数同时承担**选区提亮**：快照原像素 = 原始亮度。
unsafe fn restore_rect(bits: *mut u32, vw: i32, vh: i32, shot: &CapturedImage, r: Rect4) {
    let Some((x, y, w, h)) = clip_rect(r, vw, vh) else {
        return;
    };
    let src_stride = shot.width.max(1) as usize;
    let src = shot.data.as_ptr() as *const u32;
    for row in 0..h as usize {
        let sy = y as usize + row;
        let off = sy * src_stride + x as usize;
        if (off + w as usize) * 4 > shot.data.len() {
            break; // 越界保护（快照尺寸异常时宁可留原样也不越界读）
        }
        let dst = sy * vw as usize + x as usize;
        std::ptr::copy_nonoverlapping(src.add(off), bits.add(dst), w as usize);
    }
}

/// 暗化像素：每通道 `× factor`（alpha 不变；参照 SnowShot 的遮罩暗化）。
/// 默认 factor=0.5 时等价于整数 `>>1`。
fn dim_px(px: u32, factor: f32) -> u32 {
    let f = factor.clamp(0.0, 1.0);
    let b = ((px & 0xFF) as f32 * f) as u32;
    let g = (((px >> 8) & 0xFF) as f32 * f) as u32;
    let r = (((px >> 16) & 0xFF) as f32 * f) as u32;
    (px & 0xFF00_0000) | (r << 16) | (g << 8) | b
}

/// 表面**原位**暗化（每通道 `× factor`，alpha 不变）——选区遮罩的初始底图。
/// 原位进行，不多占一张整屏位图；擦帧时由 [`restore_dim_rect`] 从快照重算暗化像素。
unsafe fn dim_surface(bits: *mut u32, vw: i32, vh: i32, factor: f32) {
    let n = (vw.max(0) as usize) * (vh.max(0) as usize);
    for i in 0..n {
        *bits.add(i) = dim_px(*bits.add(i), factor);
    }
}

/// 把表面上某矩形区域还原为**暗化**的快照像素（遮罩语义的擦帧/擦标签）。
///
/// 与 [`restore_rect`]（提亮，写快照原像素）的区别：像素按每通道 `× factor` 暗化，
/// 与 [`dim_surface`] 的初始底图一致——否则擦过的区域会比遮罩亮一截。
unsafe fn restore_dim_rect(bits: *mut u32, vw: i32, vh: i32, shot: &CapturedImage, r: Rect4, factor: f32) {
    let Some((x, y, w, h)) = clip_rect(r, vw, vh) else {
        return;
    };
    let src_stride = shot.width.max(1) as usize;
    let src = shot.data.as_ptr() as *const u32;
    for row in 0..h as usize {
        let sy = y as usize + row;
        let off = sy * src_stride + x as usize;
        if (off + w as usize) * 4 > shot.data.len() {
            break; // 越界保护（快照尺寸异常时宁可留原样也不越界读）
        }
        let dst = sy * vw as usize + x as usize;
        for col in 0..w as usize {
            *bits.add(dst + col) = dim_px(*src.add(off + col), factor);
        }
    }
}

/// 在矩形四条边内侧画 `width` 像素宽的框线（`frame_px` 为预乘 BGRA 像素值）。
///
/// 直接写预乘 BGRA，不经 GDI：GDI 往 32bpp DIB 绘制时**不写 alpha 字节**，
/// 画面会因 alpha=0 而全透明 —— 这正是"无遮罩分层窗口"最常见的坑。
unsafe fn draw_frame(bits: *mut u32, vw: i32, vh: i32, r: Rect4, width: i32, frame_px: u32) {
    fill(bits, vw, vh, (r.0, r.1, r.2, width), frame_px); // 上
    fill(bits, vw, vh, (r.0, r.1 + r.3 - width, r.2, width), frame_px); // 下
    fill(bits, vw, vh, (r.0, r.1, width, r.3), frame_px); // 左
    fill(bits, vw, vh, (r.0 + r.2 - width, r.1, width, r.3), frame_px); // 右
}

/// 把表面某矩形区域填成指定像素值（自动裁剪越界部分）。
unsafe fn fill(bits: *mut u32, vw: i32, vh: i32, r: Rect4, px: u32) {
    if let Some((x, y, w, h)) = clip_rect(r, vw, vh) {
        for row in 0..h {
            let off = (y + row) as usize * vw as usize + x as usize;
            std::slice::from_raw_parts_mut(bits.add(off), w as usize).fill(px);
        }
    }
}

fn point_from_lparam(lp: LPARAM, vx: i32, vy: i32) -> POINT {
    let x = (lp.0 & 0xFFFF) as u16 as i16 as i32 + vx;
    let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 + vy;
    POINT { x, y }
}

type Rect4 = (i32, i32, i32, i32); // (x, y, w, h)

/// 由两个对角点得到规范化矩形（支持反向拖动）。min/max 后相减，不走
/// `(a-b).abs()` 的 i32 取负溢出路径。
fn norm_rect(a: POINT, b: POINT) -> Rect4 {
    let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
    let (y0, y1) = (a.y.min(b.y), a.y.max(b.y));
    (x0, y0, x1 - x0, y1 - y0)
}

/// 把矩形钳制到虚拟屏幕范围内（宽高至少 1）。
fn clamp_region(r: Rect4, vx: i32, vy: i32, vw: i32, vh: i32) -> SelectedRegion {
    let x = r.0.max(vx).min(vx + vw - 1);
    let y = r.1.max(vy).min(vy + vh - 1);
    let w = r.2.min(vx + vw - x).max(1);
    let h = r.3.min(vy + vh - y).max(1);
    SelectedRegion {
        x,
        y,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用固定框线像素（纯红不透明，预乘 BGRA 小端）。
    const FRAME_PX: u32 = 0xFFFF_0000;

    /// 测试用固定暗化系数（与默认配置 ui_overlay_dim=0.5 一致）。
    const DIM: f32 = 0.5;

    /// 颜色 → 预乘 BGRA 打包（不透明时等于原始通道；半透明时 RGB 按 alpha 预乘）。
    #[test]
    fn packed_is_premultiplied_bgra() {
        use crate::annot::style::Rgba;
        assert_eq!(packed(Rgba::new(0xFF, 0x00, 0x00, 0xFF)), FRAME_PX);
        assert_eq!(packed(Rgba::new(0xFF, 0xFF, 0xFF, 0x00)), 0);
        assert_eq!(packed(Rgba::new(0xFF, 0x00, 0x00, 0x80)), 0x8080_0000);
    }

    fn p(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    #[test]
    fn norm_rect_forward_and_reverse() {
        // 左上 → 右下
        assert_eq!(norm_rect(p(10, 20), p(110, 120)), (10, 20, 100, 100));
        // 右下 → 左上（反向拖动）
        assert_eq!(norm_rect(p(110, 120), p(10, 20)), (10, 20, 100, 100));
        // 右上 → 左下
        assert_eq!(norm_rect(p(110, 20), p(10, 120)), (10, 20, 100, 100));
    }

    #[test]
    fn norm_rect_zero_area() {
        assert_eq!(norm_rect(p(50, 50), p(50, 50)), (50, 50, 0, 0));
    }

    #[test]
    fn clamp_region_into_virtual_screen() {
        // 完全在内：原样
        let r = clamp_region((100, 100, 200, 150), 0, 0, 1920, 1080);
        assert_eq!((r.x, r.y, r.width, r.height), (100, 100, 200, 150));
        // 右侧越界：宽度被裁到剩余
        let r = clamp_region((1800, 100, 400, 100), 0, 0, 1920, 1080);
        assert_eq!(r.width, 120);
        // 起点越界：被拉回边界内，且仍为合法矩形
        let r = clamp_region((5000, 5000, 10, 10), 0, 0, 1920, 1080);
        assert!(r.x < 1920 && r.y < 1080 && r.width >= 1 && r.height >= 1);
        // 负坐标副屏（虚拟屏幕原点为负）
        let r = clamp_region((-1900, -100, 100, 100), -1920, -200, 3840, 1280);
        assert_eq!((r.x, r.y), (-1900, -100));
    }

    #[test]
    fn clamp_region_never_returns_empty() {
        for (x, y, w, h) in [(0, 0, 0, 0), (0, 0, -5, -5), (9999, 9999, 0, 0)] {
            let r = clamp_region((x, y, w, h), 0, 0, 1920, 1080);
            assert!(r.width >= 1 && r.height >= 1, "宽高必须 ≥1: {r:?}");
        }
    }

    /// 无遮罩表面的像素级约束：透明底与框线都必须 alpha > 0。
    /// 分层窗口按 alpha 命中鼠标，alpha=0 的像素会让点击穿透到桌面 ——
    /// 那样"拖动框选"根本无法发起。
    #[test]
    fn surface_alpha_is_never_zero() {
        assert_eq!(BG_PX >> 24, 1, "透明底 alpha 必须 ≥1（否则点击穿透）");
        assert_eq!(FRAME_PX >> 24, 0xFF, "框线必须不透明");
        // 预乘 BGRA：低字节是蓝通道 → 纯红 = B=0, G=0, R=0xFF
        assert_eq!(FRAME_PX & 0x00FF_FFFF, 0x00FF_0000, "框线必须是纯红（不是蓝）");
    }

    /// 与快照同尺寸、内容可辨认的假快照（每像素 = (x, y, 0x40, 0xFF)）。
    fn fake_shot(w: i32, h: i32) -> CapturedImage {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = x as u8;
                data[i + 1] = y as u8;
                data[i + 2] = 0x40;
                data[i + 3] = 0xFF;
            }
        }
        CapturedImage {
            x: 0,
            y: 0,
            width: w,
            height: h,
            data: Arc::from(data),
        }
    }

    /// 冻结底图：整屏快照铺满表面后，每个像素都等于快照像素且 alpha=255。
    /// alpha>0 是拖拽能发起的前提；冻结期间不允许出现透明空洞。
    #[test]
    fn surface_is_filled_with_snapshot() {
        let (vw, vh) = (8i32, 6i32);
        let shot = fake_shot(vw, vh);
        let mut buf = vec![0u32; (vw * vh) as usize];
        unsafe { copy_snapshot_into_surface(buf.as_mut_ptr(), vw, vh, &shot) };
        assert!(buf.iter().all(|&px| px >> 24 == 0xFF), "冻结底图必须整块不透明");
        let want: Vec<u32> = shot
            .data
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(buf, want, "冻结底图必须与快照逐像素一致");
    }

    /// 擦帧必须把框线还原成**快照像素**（而非透明底）。
    /// 这是"冻结画面 + 拖动"与旧"透明底 + 拖动"的关键差异：若仍用透明底擦帧，
    /// 拖动过的区域会留下透明空洞，冻结效果就破了。
    #[test]
    fn restore_rect_returns_to_snapshot_pixels() {
        let (vw, vh) = (10i32, 10i32);
        let shot = fake_shot(vw, vh);
        let mut buf = vec![0u32; (vw * vh) as usize];
        unsafe { copy_snapshot_into_surface(buf.as_mut_ptr(), vw, vh, &shot) };

        let rect = (2, 2, 6, 6);
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, rect, 2, FRAME_PX) };
        assert_eq!(buf[(2 * vw + 2) as usize], FRAME_PX, "应先画出框线");

        unsafe { restore_rect(buf.as_mut_ptr(), vw, vh, &shot, rect) };
        let want: Vec<u32> = shot
            .data
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(buf, want, "擦帧后必须完全还原为快照像素");
    }

    /// 越界矩形在铺底/擦帧里都不得 panic，也不得越界写；快照比表面小时
    /// 必须按行距逐行铺到正确位置（不允许把二维拷贝拍平成一维）。
    #[test]
    fn snapshot_blit_clips_out_of_bounds() {
        let (vw, vh) = (6i32, 5i32);
        let shot = fake_shot(4, 3); // 故意比表面小：只应铺满交错区域，不越界
        let mut buf = vec![0u32; (vw * vh) as usize];
        unsafe {
            copy_snapshot_into_surface(buf.as_mut_ptr(), vw, vh, &shot);
        }
        // 位置断言：快照 (x,y) 像素必须落在表面同一坐标，而不是连续铺开错位
        for y in 0..3 {
            for x in 0..4 {
                let got = buf[(y * vw + x) as usize];
                let want = u32::from_le_bytes([
                    shot.data[((y * 4 + x) * 4) as usize],
                    shot.data[((y * 4 + x) * 4 + 1) as usize],
                    shot.data[((y * 4 + x) * 4 + 2) as usize],
                    shot.data[((y * 4 + x) * 4 + 3) as usize],
                ]);
                assert_eq!(got, want, "({x},{y}) 位置错位");
            }
        }
        // 快照之外的表面像素必须保持原样（未被写）
        assert_eq!(buf[(0 * vw + 4) as usize], 0);
        assert_eq!(buf[(3 * vw + 0) as usize], 0);
        unsafe {
            restore_rect(buf.as_mut_ptr(), vw, vh, &shot, (4, 3, 100, 100));
            restore_rect(buf.as_mut_ptr(), vw, vh, &shot, (-50, -50, 8, 8));
        }
    }

    /// 框线只出现在四条边上，内部与外部保持透明底。
    #[test]
    fn frame_pixels_are_red_and_interior_stays_transparent() {
        let (vw, vh) = (20i32, 12i32);
        let mut buf = vec![BG_PX; (vw * vh) as usize];
        let rect = (3, 2, 10, 6); // x, y, w, h
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, rect, 2, FRAME_PX) };
        let at = |x: i32, y: i32| buf[(y * vw + x) as usize];

        // 四条边都是不透明红
        assert_eq!(at(3, 2), FRAME_PX, "左上角");
        assert_eq!(at(7, 2), FRAME_PX, "上边中");
        assert_eq!(at(12, 7), FRAME_PX, "右下角");
        assert_eq!(at(3, 7), FRAME_PX, "左下角");
        assert_eq!(at(4, 6), FRAME_PX, "右边");
        // 内部保持透明底（红的是"框线"而不是填充）
        assert_eq!(at(8, 5), BG_PX);
        // 矩形之外保持透明底
        assert_eq!(at(0, 0), BG_PX);
        assert_eq!(at(19, 11), BG_PX);
    }

    /// 清帧必须把框线擦回透明底，否则拖动时会残留上一条线。
    #[test]
    fn clear_rect_erases_previous_frame() {
        let (vw, vh) = (16i32, 16i32);
        let mut buf = vec![BG_PX; (vw * vh) as usize];
        let rect = (2, 2, 8, 8);
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, rect, 2, FRAME_PX) };
        assert_eq!(buf[(2 * vw + 2) as usize], FRAME_PX);
        unsafe { clear_rect(buf.as_mut_ptr(), vw, vh, rect) };
        assert!(buf.iter().all(|&px| px == BG_PX), "清帧后应恢复全透明底");
    }

    /// 越界矩形不得 panic，且只影响表面范围内的像素。
    #[test]
    fn raster_clips_at_surface_bounds() {
        let (vw, vh) = (10i32, 10i32);
        let mut buf = vec![BG_PX; (vw * vh) as usize];
        // 部分越界：右/下超出
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, (8, 8, 6, 6), 2, FRAME_PX) };
        assert_eq!(buf[(8 * vw + 8) as usize], FRAME_PX, "界内像素应被绘制");
        assert_eq!(buf[(9 * vw + 9) as usize], FRAME_PX);
        // 完全在外：什么都不画
        let mut buf2 = vec![BG_PX; (vw * vh) as usize];
        unsafe { draw_frame(buf2.as_mut_ptr(), vw, vh, (-50, -50, 4, 4), 2, FRAME_PX) };
        assert!(buf2.iter().all(|&px| px == BG_PX));
        // 负坐标 / 零尺寸矩形不 panic
        unsafe {
            draw_frame(buf2.as_mut_ptr(), vw, vh, (-3, -3, 8, 8), 2, FRAME_PX);
            draw_frame(buf2.as_mut_ptr(), vw, vh, (4, 4, 0, 0), 2, FRAME_PX);
            clear_rect(buf2.as_mut_ptr(), vw, vh, (99, 99, 5, 5));
        }
    }

    /// 脏区并集要覆盖两帧的全部变动像素，否则旧线会残留。
    #[test]
    fn union_rect_covers_both_frames() {
        assert_eq!(union_rect((10, 10, 5, 5), (20, 30, 4, 4)), (10, 10, 14, 24));
        // 一帧为空时退化为另一帧
        assert_eq!(union_rect((3, 4, 0, 0), (3, 4, 7, 7)), (3, 4, 7, 7));
    }

    /// 暗化像素：每通道 >>1，alpha 不变（选区遮罩的核心语义）。
    #[test]
    fn dim_px_halves_channels_keeps_alpha() {
        // 预乘 BGRA u32：低字节 B，然后 G、R，最高字节 A
        let px = 0xFF80_4020u32; // B=0x20, G=0x40, R=0x80, A=0xFF
        let d = dim_px(px, DIM);
        assert_eq!(d >> 24, 0xFF, "alpha 不变");
        assert_eq!(d & 0xFF, 0x10, "蓝 >>1");
        assert_eq!((d >> 8) & 0xFF, 0x20, "绿 >>1");
        assert_eq!((d >> 16) & 0xFF, 0x40, "红 >>1");
        // 纯红框线暗化后仍不透明
        assert_eq!(dim_px(FRAME_PX, DIM) >> 24, 0xFF);
    }

    /// 遮罩语义：擦帧必须还原为**暗化**像素；提亮区与遮罩区必须可区分。
    #[test]
    fn restore_dim_matches_initial_mask() {
        let (vw, vh) = (8i32, 6i32);
        let shot = fake_shot(vw, vh);
        // 1. 铺快照 + 原位暗化（与 run() 的初始化一致）
        let mut buf = vec![BG_PX; (vw * vh) as usize];
        unsafe {
            copy_snapshot_into_surface(buf.as_mut_ptr(), vw, vh, &shot);
            dim_surface(buf.as_mut_ptr(), vw, vh, DIM);
        }
        let mask: Vec<u32> = buf.clone();
        // 2. 模拟拖动：提亮选区 + 画框线
        let rect = (2, 1, 4, 3);
        unsafe {
            restore_rect(buf.as_mut_ptr(), vw, vh, &shot, rect);
            draw_frame(buf.as_mut_ptr(), vw, vh, rect, 2, FRAME_PX);
        }
        assert_eq!(buf[(1 * vw + 2) as usize], FRAME_PX, "框线已画");
        // 3. 擦帧（还原为暗化像素）后必须与初始遮罩逐像素一致
        unsafe { restore_dim_rect(buf.as_mut_ptr(), vw, vh, &shot, rect, DIM) };
        assert_eq!(buf, mask, "擦帧后必须还原为暗化遮罩");
        // 4. 提亮区必须比遮罩亮（快照原像素 ≠ 暗化像素）
        unsafe { restore_rect(buf.as_mut_ptr(), vw, vh, &shot, rect) };
        let px_at = |x: i32, y: i32| buf[(y * vw + x) as usize];
        assert_eq!(
            px_at(4, 2),
            {
                let c = u32::from_le_bytes([
                    shot.data[((2 * shot.width + 4) * 4) as usize],
                    shot.data[((2 * shot.width + 4) * 4 + 1) as usize],
                    shot.data[((2 * shot.width + 4) * 4 + 2) as usize],
                    shot.data[((2 * shot.width + 4) * 4 + 3) as usize],
                ]);
                c
            },
            "提亮区 = 快照原像素"
        );
        assert_ne!(
            px_at(4, 2),
            mask[(2 * vw + 4) as usize],
            "提亮区必须不同于暗化遮罩"
        );
    }

    /// 标签定位：默认在选区上方；贴顶移到下方；水平钳回屏幕内；下方越界钳回屏内。
    #[test]
    fn label_rect_positioning() {
        // 常规：上方
        let r = label_rect(1000, 800, (100, 200, 300, 100), "1920 × 1080", 13);
        assert_eq!(r.0, 100, "左对齐选区左缘");
        assert!(r.1 + r.3 <= 200 - 4, "标签在选区上方");
        // 贴顶：移到下方
        let r = label_rect(1000, 800, (100, 2, 300, 100), "1920 × 1080", 13);
        assert!(r.1 >= 2 + 100 + 4, "贴顶时标签在选区下方");
        // 左缘贴屏右：钳回屏幕内
        let r = label_rect(1000, 800, (990, 200, 8, 8), "1920 × 1080", 13);
        assert!(r.0 + r.2 <= 1000, "标签不得超出屏幕");
        assert!(r.0 >= 0);
        // 贴顶且选区延伸到底边：下方放不下时钳回屏内（不得画出下缘）
        let r = label_rect(1000, 120, (100, 2, 300, 118), "1920 × 1080", 13);
        assert!(r.1 >= 0 && r.1 + r.3 <= 120, "标签不得超出屏幕下缘: {r:?}");
    }

    #[test]
    fn label_w_is_positive_and_proportional() {
        assert!(label_w("", 13) > 0, "空文本也有兜底宽度");
        let w1 = label_w("10 × 20", 13);
        let w2 = label_w("1920 × 1080", 13);
        assert!(w2 > w1, "更长的文本更宽");
    }
}
