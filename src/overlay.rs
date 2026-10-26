//! 区域框选：按下热键后**整屏冻结**（显示进入时捕获的整屏快照），在冻结画面上拖动
//! 框选 / Esc 取消（只保留区域截图，不支持选窗口与全屏）。
//!
//! **不做暗化**：冻结画面就是原始像素，不叠暗色，只在其上叠加一圈红色框线。
//! 实现方式是逐像素 alpha 的分层窗口（`UpdateLayeredWindow`）：表面用整屏快照
//! 铺满（BGRA、alpha=255），红框直接写进同一表面。
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
//! 转为鼠标穿透（`WS_EX_TRANSPARENT`），而是继续接收鼠标消息用于矩形/箭头/步骤序号
//! 标注；键盘消息由工具条经自定义消息（`WM_APP_KEY` / `WM_APP_SET_TOOL`）转发。
//! 工具条/贴图/菜单都是 `WS_EX_TOPMOST` 且在其后创建，会压在冻结层之上；
//! 标注期间冻结层对 `WM_MOUSEACTIVATE` 返回 `MA_NOACTIVATE`，不抢工具条的焦点与 z 序。

use std::sync::Arc;

use crate::annot::{self, AnnotDoc, AnnotSession, CursorKind, DibSurface, Pt, Rect, Tool};
use crate::capture::{self, CapturedImage};
use crate::util::{self, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint,
    GetDC, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_SHIFT,
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

/// 冻结层窗口类名（供冻结层反向定位工具条窗口）。
const TOOLBAR_CLASS: &str = "PixShotToolbar";

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
/// 小于该边长的拖动视为无效框选（忽略，继续等待用户框选）。
const MIN_DRAG_LOGICAL: i32 = 3;
/// 框线粗细（逻辑像素）。
const FRAME_W_LOGICAL: i32 = 2;
/// 透明底的像素值（预乘 BGRA）。alpha 取 1 而非 0：分层窗口按 alpha 命中鼠标，
/// alpha=0 会让点击穿透到桌面（拖拽无法发起）；1/255 视觉上不可察觉。
const BG_PX: u32 = 1 << 24;
/// 框线像素值（预乘 BGRA，小端 u32 → 字节序为 B,G,R,A）。
/// 纯红 = R=0xFF、G=0、B=0，alpha=255（预乘后颜色不变）。
const FRAME_PX: u32 = 0xFFFF_0000;

/// 冻结层所处阶段（替代原 `locked: bool`，见设计文档 1.2 F）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayMode {
    /// 框选中（含确认前的拖动）。
    Selecting,
    /// 选区已确认：进入标注模式（窗口不再鼠标穿透）。
    Annotating,
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
        return None;
    }
    let surface_dc = CreateCompatibleDC(screen_dc);
    if surface_dc.is_invalid() {
        let _ = ReleaseDC(HWND::default(), screen_dc);
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
            return None;
        }
    };
    let surface_old = SelectObject(surface_dc, surface_bmp);

    register_class_w("PixShotOverlay", overlay_proc, true, false, false);
    let hinst = GetModuleHandleW(None).unwrap_or_default();

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
        return None;
    }

    // 遮罩（冻结画面 + 红框）永不出现在任何截图里。测试用 escape hatch：
    // `PIXSHOT_OVERLAY_CAPTURABLE=1` 时跳过，便于用普通屏幕截图回归"冻结观感"
    // （正常使用不应设置此变量）。
    if std::env::var_os("PIXSHOT_OVERLAY_CAPTURABLE").is_none() {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
    // 表面先用透明底铺一遍（兜底：万一快照比虚拟屏幕小，也不会留下 alpha=0 的像素，
    // 那种像素会让点击穿透、拖拽发不起来），再用整屏快照覆盖。
    // 窗口一显示就是"冻结"的整屏画面（不叠暗色），所见即所得。
    {
        let s = &*state;
        clear_rect(s.surface_bits, vw, vh, (0, 0, vw, vh));
        if let Some(shot) = &s.snapshot {
            copy_snapshot_into_surface(s.surface_bits, vw, vh, shot);
        }
        commit(hwnd, s, (0, 0, vw, vh));
    }
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    let _ = SetForegroundWindow(hwnd);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
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
    // 无成品 = 用户取消（Esc / 关闭）
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
                    let a = s.annot.on_l_down(p, shift);
                    // 进入绘制/拖动/缩放即捕获鼠标（见 5.1）
                    if s.annot.capturing() {
                        SetCapture(hwnd);
                    }
                    annot_apply(hwnd, s, a);
                    return LRESULT(0);
                }
                SetCapture(hwnd);
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
                    let _ = ReleaseCapture();
                    annot_apply(hwnd, s, a);
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

        WM_KEYDOWN => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let ctrl = ctrl_down();
                    annot_key(hwnd, s, wp.0, ctrl);
                    return LRESULT(0);
                }
                if wp.0 == VK_ESCAPE {
                    // 选区确认前：Esc 取消框选拆窗（确认后由工具条/标注侧处理）。
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }

        // 工具条 → 冻结层：切换工具
        WM_APP_SET_TOOL => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    let tool = tool_from_usize(wp.0);
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

        // 工具条 → 冻结层：转发按键（wp = VK，lp = Ctrl）
        WM_APP_KEY => {
            if let Some(s) = state_of(hwnd) {
                if s.mode == OverlayMode::Annotating {
                    annot_key(hwnd, s, wp.0, lp.0 != 0);
                }
            }
            LRESULT(0)
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

/// 鼠标松开：拖动够大则取选区并**保留红框**；过小（含单击）则擦掉框线，留在框选状态。
unsafe fn finish_selection(hwnd: HWND, _lp: LPARAM) {
    let Some(s) = state_of(hwnd) else { return };
    if !s.dragging {
        return;
    }
    s.dragging = false;
    let _ = ReleaseCapture();

    let r = norm_rect(s.start, s.cur);
    let min_side = util::scaled(MIN_DRAG_LOGICAL, overlay_dpi());
    if r.2 >= min_side && r.3 >= min_side {
        let region = clamp_region(r, s.vx, s.vy, s.vw, s.vh);
        let sel_c = (region.x - s.vx, region.y - s.vy, region.width, region.height);
        let width = util::scaled(FRAME_W_LOGICAL, overlay_dpi());
        // 把框线收敛到最终选区；表面底图仍是"冻结画面"、保持不动 ——
        // 因此松手后屏幕**继续冻结**，直到工具条关闭（见 [`Selection::dismiss`]）。
        if let Some(shot) = &s.snapshot {
            if let Some(last) = s.last {
                if last != sel_c {
                    restore_rect(s.surface_bits, s.vw, s.vh, shot, last);
                }
            }
            draw_frame(s.surface_bits, s.vw, s.vh, sel_c, width);
        }
        // 成品从整屏快照裁出后**立即释放快照**（用于裁剪的整屏资源及时回收）。
        // 显示中的冻结画面留在窗口表面上（不额外占内存），随窗口在会话结束时一并销毁。
        let cropped = s
            .snapshot
            .take()
            .map(|shot| Arc::new(shot.crop(region.x, region.y, region.width, region.height)));
        commit(hwnd, s, (0, 0, s.vw, s.vh));
        s.last = Some(sel_c);
        s.result_image = cropped;
        // 进入标注模式（见设计文档 1.2 A）：不转鼠标穿透，冻结层继续接收鼠标用于标注。
        s.region = region;
        s.annot = AnnotSession::new(
            Rect::new(0, 0, region.width, region.height),
            overlay_dpi(),
        );
        s.annot_base = DibSurface::new(region.width, region.height);
        if let Some(base) = &s.annot_base {
            // 底图 = 冻结画面 + 选区红框（此后不可变）
            base.blit_from(s.surface_dc, sel_c.0, sel_c.1, sel_c.2, sel_c.3);
        }
        s.mode = OverlayMode::Annotating;
        crate::diag::log(&format!(
            "[overlay] 选区确认 {}×{} @ {},{}：已裁出成品、建立标注底图（进入标注模式）",
            region.width, region.height, region.x, region.y
        ));
        // 结束框选模态循环但**不拆窗**，由调用方持有的 Selection 负责后续关闭；
        // 冻结层继续显示（不再是鼠标穿透），标注交互挂在其窗口过程上。
        PostQuitMessage(0);
        return;
    }

    // 单击 / 过小的拖动：不产生选区，擦掉已画的框线（恢复冻结底图），继续等待框选
    if let Some(last) = s.last.take() {
        if let Some(shot) = &s.snapshot {
            restore_rect(s.surface_bits, s.vw, s.vh, shot, last);
        }
        commit(hwnd, s, last);
    }
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
    };
    if let Ok(c) = LoadCursorW(None, id) {
        SetCursor(c);
    }
}

/// 消费一次标注事件结果：光标、重绘、Esc 交还工具条。
unsafe fn annot_apply(hwnd: HWND, s: &State, a: annot::Action) {
    if let Some(c) = a.cursor {
        set_annot_cursor(c);
    }
    if let Some(d) = a.redraw {
        repaint_annot(hwnd, s, d);
    }
    if a.esc_passthrough {
        post_to_toolbar(WM_APP_ESC_CLOSE, 0, 0);
    }
}

/// 处理一次标注按键并把（可能变化的）工具同步给工具条。
unsafe fn annot_key(hwnd: HWND, s: &mut State, vk: usize, ctrl: bool) {
    let a = s.annot.on_key(vk, ctrl);
    let tool = s.annot.tool();
    annot_apply(hwnd, s, a);
    post_to_toolbar(WM_APP_TOOL_CHANGED, tool as usize, 0);
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
    // 2. 全量重画标注 + 预览 + 选中态（render_into 内部按选区裁剪）
    annot::render_into(s.surface_dc, Pt::new(ox, oy), &s.annot, overlay_dpi());
    // 3. alpha 防御（表面坐标）
    let sr = Rect::new(ox + d.x, oy + d.y, d.w, d.h);
    crate::annot::session::alpha_fix(s.surface_bits, s.vw, sr);
    // 4. 提交脏区
    commit(hwnd, s, (sr.x, sr.y, sr.w, sr.h));
}

/// 框选层横跨整个虚拟屏幕，用系统 DPI 作为框线粗细/拖动阈值的换算基准。
fn overlay_dpi() -> i32 {
    unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32 }
}

/// 重画框线：清掉上一帧、画上当前帧，再把脏区提交给分层窗口。
/// 只提交变动区域而非整屏，避免每帧整屏重合成。
unsafe fn repaint_frame(hwnd: HWND, s: &mut State) {
    let cur = norm_rect(s.start, s.cur);
    let cur_c = (cur.0 - s.vx, cur.1 - s.vy, cur.2, cur.3);
    let width = util::scaled(FRAME_W_LOGICAL, overlay_dpi());
    let has = cur_c.2 > 0 && cur_c.3 > 0;

    // 先清上一帧（恢复冻结底图原像素）、再画当前帧：顺序保证两帧重叠时结果正确
    if let Some(last) = s.last {
        if let Some(shot) = &s.snapshot {
            restore_rect(s.surface_bits, s.vw, s.vh, shot, last);
        }
    }
    if has {
        draw_frame(s.surface_bits, s.vw, s.vh, cur_c, width);
    }

    let dirty = match (s.last, has) {
        (Some(a), true) => union_rect(a, cur_c),
        (Some(a), false) => a,
        (None, true) => cur_c,
        (None, false) => return,
    };
    s.last = has.then_some(cur_c);
    commit(hwnd, s, dirty);
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
/// 快照与表面同为 `vw×vh`、自上而下 BGRA、行距 `vw*4`，因此整块一次拷贝即可。
/// 快照 alpha 恒为 255，与分层窗口要求的预乘 BGRA 在此取值下等价。
unsafe fn copy_snapshot_into_surface(bits: *mut u32, vw: i32, vh: i32, shot: &CapturedImage) {
    let rows = vh.min(shot.height.max(0));
    let cols = vw.min(shot.width.max(0));
    if rows <= 0 || cols <= 0 {
        return;
    }
    let n = rows as usize * cols as usize;
    let src = shot.data.as_ptr() as *const u32;
    if shot.data.len() >= n * 4 {
        std::ptr::copy_nonoverlapping(src, bits, n);
    }
}

/// 把表面上某矩形区域恢复为快照原像素（拖拽期间擦除上一帧框线）。
///
/// 与 [`clear_rect`] 的区别：框选期间表面底图是"冻结的整屏快照"而非透明底，
/// 擦帧必须还原快照像素，否则会擦出一块透明空洞。
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

/// 在矩形四条边内侧画 `width` 像素宽的框线。
///
/// 直接写预乘 BGRA，不经 GDI：GDI 往 32bpp DIB 绘制时**不写 alpha 字节**，
/// 画面会因 alpha=0 而全透明 —— 这正是"无遮罩分层窗口"最常见的坑。
unsafe fn draw_frame(bits: *mut u32, vw: i32, vh: i32, r: Rect4, width: i32) {
    fill(bits, vw, vh, (r.0, r.1, r.2, width), FRAME_PX); // 上
    fill(bits, vw, vh, (r.0, r.1 + r.3 - width, r.2, width), FRAME_PX); // 下
    fill(bits, vw, vh, (r.0, r.1, width, r.3), FRAME_PX); // 左
    fill(bits, vw, vh, (r.0 + r.2 - width, r.1, width, r.3), FRAME_PX); // 右
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

/// 由两个对角点得到规范化矩形（支持反向拖动）。
fn norm_rect(a: POINT, b: POINT) -> Rect4 {
    (
        a.x.min(b.x),
        a.y.min(b.y),
        (a.x - b.x).abs(),
        (a.y - b.y).abs(),
    )
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
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, rect, 2) };
        assert_eq!(buf[(2 * vw + 2) as usize], FRAME_PX, "应先画出框线");

        unsafe { restore_rect(buf.as_mut_ptr(), vw, vh, &shot, rect) };
        let want: Vec<u32> = shot
            .data
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(buf, want, "擦帧后必须完全还原为快照像素");
    }

    /// 越界矩形在铺底/擦帧里都不得 panic，也不得越界写。
    #[test]
    fn snapshot_blit_clips_out_of_bounds() {
        let (vw, vh) = (6i32, 5i32);
        let shot = fake_shot(4, 3); // 故意比表面小：只应铺满交错区域，不越界
        let mut buf = vec![0u32; (vw * vh) as usize];
        unsafe {
            copy_snapshot_into_surface(buf.as_mut_ptr(), vw, vh, &shot);
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
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, rect, 2) };
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
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, rect, 2) };
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
        unsafe { draw_frame(buf.as_mut_ptr(), vw, vh, (8, 8, 6, 6), 2) };
        assert_eq!(buf[(8 * vw + 8) as usize], FRAME_PX, "界内像素应被绘制");
        assert_eq!(buf[(9 * vw + 9) as usize], FRAME_PX);
        // 完全在外：什么都不画
        let mut buf2 = vec![BG_PX; (vw * vh) as usize];
        unsafe { draw_frame(buf2.as_mut_ptr(), vw, vh, (-50, -50, 4, 4), 2) };
        assert!(buf2.iter().all(|&px| px == BG_PX));
        // 负坐标 / 零尺寸矩形不 panic
        unsafe {
            draw_frame(buf2.as_mut_ptr(), vw, vh, (-3, -3, 8, 8), 2);
            draw_frame(buf2.as_mut_ptr(), vw, vh, (4, 4, 0, 0), 2);
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
}
