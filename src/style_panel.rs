//! 标注颜色面板：独立浮层（不抢焦点），提供 8 个预设色块：
//! 红 / 绿 / 蓝 / 青 / 品红 / 黄 / 黑 / 白（顺序固定）。
//!
//! 面板只发 `WM_APP_SET_STYLE(Color)` 给冻结层；冻结层应用后回传
//! `WM_APP_STYLE_SYNC(Color)`，面板据此刷新选中态。控件 ID 101–108 保持稳定。
//! 选中一个色块即视为完成一次取色，面板随即自动收起；销毁时通过
//! `WM_APP_STYLE_PANEL_CLOSED` 通知工具条清空其缓存的窗口句柄。

use crate::config;
use crate::overlay::{
    STYLE_PANEL_CLASS, StyleField, WM_APP_SET_STYLE, WM_APP_STYLE_PANEL_CLOSED,
    WM_APP_STYLE_SYNC,
};
use crate::util::{self, raw, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 预设色块数量：红 / 绿 / 蓝 / 青 / 品红 / 黄 / 黑 / 白。
const COLOR_COUNT: usize = 8;
/// 8 个 30px 色块 + 7 个 4px 间距 + 左右各 10px 内边距。
const PANEL_W: i32 = COLOR_COUNT as i32 * 30 + (COLOR_COUNT as i32 - 1) * 4 + 20;
const PANEL_H: i32 = 44;
const ID_COLOR0: usize = 101;

struct PanelData {
    toolbar: HWND,
    overlay: HWND,
    palette: [crate::annot::style::Rgba; COLOR_COUNT],
    color: crate::annot::style::Rgba,
    syncing: bool,
    color_btns: [HWND; COLOR_COUNT],
}

unsafe fn data_of(hwnd: HWND) -> Option<&'static mut PanelData> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut PanelData))
    }
}

unsafe fn mk_color_button(parent: HWND, id: usize, x: i32, y: i32, w: i32, h: i32) -> HWND {
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR::from_raw(to_wide("BUTTON").as_ptr()),
        PCWSTR::null(),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_OWNERDRAW as u32),
        x,
        y,
        w,
        h,
        parent,
        HMENU(id as isize as *mut core::ffi::c_void),
        GetModuleHandleW(None).unwrap_or_default(),
        None,
    )
    .unwrap_or_default()
}

/// 打开颜色面板（由工具条按钮 17 调用）。已存在时由工具条负责先关闭。
pub unsafe fn toggle(toolbar: HWND, overlay: HWND) -> HWND {
    register_class_w(STYLE_PANEL_CLASS, style_panel_proc, false, false, false);
    let cfg = config::get();
    let dpi = util::dpi_of(toolbar);
    let (w, h) = (util::scaled(PANEL_W, dpi), util::scaled(PANEL_H, dpi));
    let mut tr = RECT::default();
    if GetWindowRect(toolbar, &mut tr).is_err() {
        return HWND::default();
    }
    let (vx, vy, vw, vh) = crate::capture::virtual_screen_rect();
    let gap = util::scaled(8, dpi);
    // 显示在工具条下方，并与工具条右边缘对齐。
    let x = (tr.right - w).clamp(vx, (vx + vw - w).max(vx));
    let y_below = tr.bottom + gap;
    // 下方空间不足时翻到工具条上方，避免面板被屏幕边缘裁掉。
    let y = if y_below + h > vy + vh {
        (tr.top - h - gap).max(vy)
    } else {
        y_below
    };

    let data = Box::into_raw(Box::new(PanelData {
        toolbar,
        overlay,
        palette: cfg.style_palette,
        color: cfg.stroke_color,
        syncing: false,
        color_btns: [HWND::default(); COLOR_COUNT],
    }));

    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        PCWSTR::from_raw(to_wide(STYLE_PANEL_CLASS).as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        x,
        y,
        w,
        h,
        None,
        None,
        hinst,
        Some(data as *const _),
    )
    .unwrap_or_default();
    if hwnd.is_invalid() {
        drop(Box::from_raw(data));
        return HWND::default();
    }
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    hwnd
}

unsafe fn send_color(d: &PanelData, c: crate::annot::style::Rgba) {
    let v = ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32;
    let _ = PostMessageW(
        d.overlay,
        WM_APP_SET_STYLE,
        WPARAM(StyleField::Color as usize),
        LPARAM(v as isize),
    );
}

/// 颜色色块子控件：把滚轮转发给面板（面板再转给冻结层）。
///
/// Windows 的「悬停滚动」会把滚轮消息发给光标下的子控件；色块是普通
/// `BUTTON`，自身不处理滚轮，不转发就会导致在色块上 Ctrl+滚轮失效。
unsafe extern "system" fn color_btn_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if msg == WM_MOUSEWHEEL {
        if let Ok(parent) = GetParent(hwnd) {
            let _ = PostMessageW(parent, WM_MOUSEWHEEL, wp, lp);
            return LRESULT(0);
        }
    }
    util::call_orig(hwnd, msg, wp, lp)
}

unsafe extern "system" fn style_panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
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
            let dpi = util::dpi_of(hwnd);
            let pad = util::scaled(10, dpi);
            let cw = util::scaled(30, dpi);
            let cg = util::scaled(4, dpi);
            let bh = util::scaled(24, dpi);
            for i in 0..COLOR_COUNT {
                d.color_btns[i] = mk_color_button(
                    hwnd,
                    ID_COLOR0 + i,
                    pad + i as i32 * (cw + cg),
                    pad,
                    cw,
                    bh,
                );
                util::subclass(d.color_btns[i], color_btn_proc);
            }
            LRESULT(0)
        }

        WM_ERASEBKGND => LRESULT(1),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let bg = CreateSolidBrush(COLORREF(0x002B_2B2B));
            let _ = FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_DRAWITEM => {
            let dis = lp.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                let id = (wp.0 & 0xFFFF) as usize;
                if (ID_COLOR0..ID_COLOR0 + COLOR_COUNT).contains(&id) {
                    if let Some(d) = data_of(hwnd) {
                        let c = d.palette[id - ID_COLOR0];
                        let brush = CreateSolidBrush(COLORREF(
                            (c.r as u32) | ((c.g as u32) << 8) | ((c.b as u32) << 16),
                        ));
                        let _ = FillRect((*dis).hDC, &(*dis).rcItem, brush);
                        let _ = DeleteObject(brush);
                        let border = CreateSolidBrush(COLORREF(if d.color == c {
                            0x00FF_FFFF
                        } else {
                            0x0060_6060
                        }));
                        let _ = FrameRect((*dis).hDC, &(*dis).rcItem, border);
                        let _ = DeleteObject(border);
                    }
                }
            }
            LRESULT(1)
        }

        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as usize;
            if let Some(d) = data_of(hwnd) {
                if !d.syncing && (ID_COLOR0..ID_COLOR0 + COLOR_COUNT).contains(&id) {
                    let c = d.palette[id - ID_COLOR0];
                    send_color(d, c);
                    // 选色即完成一次操作：自动收起面板。用异步 WM_CLOSE 而不是
                    // 就地 DestroyWindow —— 此刻仍在色块按钮的 WM_COMMAND 同步栈里，
                    // 直接销毁父窗口会让按钮过程访问已释放的窗口数据。
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            LRESULT(0)
        }

        WM_APP_STYLE_SYNC => {
            if let Some(d) = data_of(hwnd) {
                if let Some(StyleField::Color)
                | Some(StyleField::StrokeColor)
                | Some(StyleField::TextColor)
                | Some(StyleField::BadgeColor) =
                    StyleField::from_u32(wp.0 as u32)
                {
                    let v = lp.0 as u32;
                    d.syncing = true;
                    d.color = crate::annot::style::Rgba::new(
                        ((v >> 16) & 0xFF) as u8,
                        ((v >> 8) & 0xFF) as u8,
                        (v & 0xFF) as u8,
                        0xFF,
                    );
                    for b in d.color_btns {
                        let _ = raw::InvalidateRect(b, std::ptr::null(), 1);
                    }
                    d.syncing = false;
                }
            }
            LRESULT(0)
        }

        // 面板位于光标下时 Windows 会把滚轮发给本窗口：转发给冻结层，
        // 保证选中标注工具后在面板上方也能 Ctrl+滚轮调整尺寸。
        WM_MOUSEWHEEL => {
            if let Some(d) = data_of(hwnd) {
                let _ = PostMessageW(d.overlay, WM_MOUSEWHEEL, wp, lp);
            }
            LRESULT(0)
        }

        WM_KEYDOWN => {
            if wp.0 == 0x1B {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if p != 0 {
                let d = Box::from_raw(p as *mut PanelData);
                // 无论面板是被自身（选色 / Esc）还是被工具条销毁，都通知工具条
                // 清空缓存句柄：HWND 可能被系统复用，留着会误销毁别的窗口。
                let _ = PostMessageW(
                    d.toolbar,
                    WM_APP_STYLE_PANEL_CLOSED,
                    WPARAM(0),
                    LPARAM(hwnd.0 as isize),
                );
                for b in d.color_btns {
                    util::unsubclass(b);
                }
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
