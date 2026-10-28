//! 轻量提示条（非模态）—— 复制/保存等操作的成功反馈。
//!
//! 为什么不用 `MessageBoxW`：
//! - 它是**模态阻塞**的：一次"已复制"会打断用户的后续操作；
//! - 视觉与整体 Fluent 自绘风格割裂（系统灰底框）；
//! - 自动化测试还得专门 `PostMessage(WM_CLOSE)` 才能继续。
//!
//! 本模块给出一个 1.6s 自动消失、不抢焦点、点击即消的圆角提示条。
//! 同时只存在一个实例：新的提示会复用并重新定位已有窗口，避免连续操作时闪烁。

use crate::util::{self, raw, register_class_w};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, GetDC, ReleaseDC, RoundRect,
    SelectObject, GetStockObject, CreatePen, NULL_BRUSH, PS_SOLID,
};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "PixShotToast";
const TIMER_DISMISS: usize = 1;
/// DT_CALCRECT | DT_SINGLELINE
const DT_MEASURE: u32 = 0x0400 | 0x20;
/// DT_CENTER | DT_VCENTER | DT_SINGLELINE
const DT_CENTER_MID: u32 = 0x25;

/// 显示一条提示（文本由窗口标题承载，避免额外生命周期管理）。
pub fn show(text: &str) {
    unsafe { show_impl(text) }
}

unsafe fn show_impl(text: &str) {
    let cls = util::to_wide(CLASS);
    let ms = crate::config::get().toast_ms;

    // 复用已有窗口：更新文本 + 重新测量定位 + 重置定时器
    if let Ok(old) = FindWindowW(PCWSTR::from_raw(cls.as_ptr()), PCWSTR::null()) {
        if !old.is_invalid() {
            let w = util::to_wide(text);
            let _ = SetWindowTextW(old, PCWSTR::from_raw(w.as_ptr()));
            place(old);
            let _ = SetTimer(old, TIMER_DISMISS, ms, None);
            return;
        }
    }

    register_class_w(CLASS, toast_proc, false, true, false);
    let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        PCWSTR::from_raw(cls.as_ptr()),
        PCWSTR::from_raw(util::to_wide(text).as_ptr()),
        WS_POPUP,
        0,
        0,
        10,
        10,
        None,
        None,
        hinst,
        None,
    )
    .unwrap_or_default();
    if hwnd.is_invalid() {
        return;
    }
    // 圆角 + 直角贴图不同：提示条用圆角更符合系统观感
    theme_frame(hwnd);
    place(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = SetTimer(hwnd, TIMER_DISMISS, ms, None);
}

/// 依据文本测量尺寸并定位到光标右下方（越界则向内翻转）。
unsafe fn place(hwnd: HWND) {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n <= 0 {
        return;
    }
    let text = &buf[..(n as usize).min(255)];

    let dpi = util::dpi_of(hwnd).max(GetDpiForSystem() as i32);
    let cfg = crate::config::get();
    let pad_x = util::scaled(cfg.ui_toast_pad_x, dpi);
    let pad_y = util::scaled(cfg.ui_toast_pad_y, dpi);

    // 用 GDI 精确测量（中英文混排时按字符数估算会明显偏差）
    let hdc = GetDC(hwnd);
    let font = util::gui_font(hwnd, cfg.ui_toast_font_pt, 400);
    let old = SelectObject(hdc, font);
    let mut r = RECT::default();
    let _ = raw::DrawTextW(
        hdc.0 as isize,
        PCWSTR::from_raw(text.as_ptr()),
        -1,
        &mut r,
        DT_MEASURE,
    );
    SelectObject(hdc, old);
    let _ = ReleaseDC(hwnd, hdc);

    let w = (r.right - r.left) + pad_x * 2;
    let h = (r.bottom - r.top).max(util::scaled(16, dpi)) + pad_y * 2;

    let mut pt = windows::Win32::Foundation::POINT::default();
    let _ = GetCursorPos(&mut pt);
    let (vx, vy, vw, vh) = crate::capture::virtual_screen_rect();
    let gap = util::scaled(cfg.ui_toast_gap, dpi);
    let mut x = pt.x + gap;
    let mut y = pt.y + gap;
    if x + w > vx + vw {
        x = pt.x - gap - w; // 右侧放不下 → 移到光标左侧
    }
    if y + h > vy + vh {
        y = pt.y - gap - h; // 下方放不下 → 移到光标上方
    }
    x = x.clamp(vx, (vx + vw - w).max(vx));
    y = y.clamp(vy, (vy + vh - h).max(vy));

    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        x,
        y,
        w,
        h,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
}

unsafe fn theme_frame(hwnd: HWND) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
    let pref = 2i32; // DWMWCP_ROUND
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &pref as *const i32 as *const core::ffi::c_void,
        4,
    );
}

unsafe extern "system" fn toast_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = KillTimer(hwnd, wp.0);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        // 点击即刻关闭（提示条本身不承载交互）
        WM_LBUTTONDOWN => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (w, h) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);

    // 固定深色卡片：提示条出现在任意壁纸上，深底白字更稳（配色见 config.ini [ui]）
    let cfg = crate::config::get();
    let bg = COLORREF(crate::config::colorref_raw(cfg.ui_toast_bg));
    let brush = CreateSolidBrush(bg);
    let pen = CreatePen(PS_SOLID, 1, COLORREF(crate::config::colorref_raw(cfg.ui_toast_border)));
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, pen);
    let _ = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let radius = util::scaled(cfg.ui_toast_radius, dpi);
    let _ = RoundRect(hdc, 0, 0, w, h, radius * 2, radius * 2);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n > 0 {
        let font = util::gui_font(hwnd, cfg.ui_toast_font_pt, 400);
        let old = SelectObject(hdc, font);
        raw::SetBkMode(hdc.0 as isize, 1);
        raw::SetTextColor(hdc.0 as isize, COLORREF(crate::config::colorref_raw(cfg.ui_toast_text)));
        let mut r = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        let _ = raw::DrawTextW(
            hdc.0 as isize,
            PCWSTR::from_raw(buf.as_ptr()),
            n,
            &mut r,
            DT_CENTER_MID,
        );
        SelectObject(hdc, old);
    }

    let _ = EndPaint(hwnd, &ps);
}
