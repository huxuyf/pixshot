//! 自绘 Fluent 弹出菜单 —— 托盘右键与贴图右键共用同一套实现。
//!
//! 为什么不用系统 `TrackPopupMenuEx`：
//! - 它要求宿主窗口能成为**前台窗口**。托盘宿主是隐藏窗口（无法置前台），
//!   实测 `TrackPopupMenuEx` 会**立即返回 0 且完全不显示菜单**；
//! - 贴图窗口虽然可以置前台，但系统菜单与整体 Fluent 视觉割裂（同一程序两种右键观感）。
//!
//! 自绘方案用 `SetCapture` 实现「点击菜单外关闭」，不依赖前台权限，
//! 且支持禁用项与分隔线。

use crate::theme;
use crate::util::{self, raw, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetDC,
    GetStockObject, ReleaseDC, RoundRect, SelectObject, NULL_BRUSH, PS_SOLID,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "PixShotMenu";
const VK_ESCAPE: usize = 0x1B;
/// DT_VCENTER | DT_SINGLELINE
const DT_MID_LEFT: u32 = 0x24;

/// 一个菜单项。
pub struct Item {
    pub label: String,
    pub enabled: bool,
    /// 该项上方是否画一条分隔线（第一项忽略）。
    pub separator_before: bool,
}

impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            separator_before: false,
        }
    }

    pub fn sep(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            separator_before: true,
        }
    }

    /// 预留：构建禁用态菜单项（如无选中内容时的「复制」）。
    #[allow(dead_code)]
    pub fn disabled(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: false,
            separator_before: false,
        }
    }
}

struct Data {
    items: Vec<Item>,
    hover: Option<usize>,
    chosen: Option<usize>,
    /// 计算出的几何（客户区坐标）
    item_h: i32,
    sep_h: i32,
    pad: i32,
    width: i32,
}

/// 弹出菜单；返回被选中项的下标（Esc / 点击外部返回 None）。
pub fn show(x: i32, y: i32, items: &[Item]) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    unsafe { show_impl(x, y, items) }
}

/// 托盘「退出」菜单的兼容封装。
pub fn show_quit(x: i32, y: i32) -> bool {
    show(x, y, &[Item::new("退出")]) == Some(0)
}

unsafe fn show_impl(x: i32, y: i32, items: &[Item]) -> Option<usize> {
    register_class_w(CLASS, menu_proc, false, true, false);

    let owned: Vec<Item> = items
        .iter()
        .map(|i| Item {
            label: i.label.clone(),
            enabled: i.enabled,
            separator_before: i.separator_before,
        })
        .collect();
    let data = Box::into_raw(Box::new(Data {
        items: owned,
        hover: None,
        chosen: None,
        item_h: 0,
        sep_h: 0,
        pad: 0,
        width: 0,
    }));

    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        PCWSTR::from_raw(to_wide(CLASS).as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        10,
        10,
        None,
        None,
        hinst,
        Some(data as *const _),
    )
    .unwrap_or_default();
    if hwnd.is_invalid() {
        drop(Box::from_raw(data));
        return None;
    }
    // WM_CREATE 已把 data 指针写入 USERDATA；此处 SAFETY：窗口存活期间指针有效
    let d = &mut *data;
    measure(hwnd, d);

    let dpi = util::dpi_of(hwnd).max(GetDpiForSystem() as i32);
    let (vx, vy, vw, vh) = crate::capture::virtual_screen_rect();
    let mh = menu_height(d, dpi);
    // 贴边时向内收 / 向上翻转，避免菜单跑出屏幕
    let mut px = x;
    let mut py = if y + mh > vy + vh { y - mh } else { y };
    px = px.clamp(vx, (vx + vw - d.width).max(vx));
    py = py.clamp(vy, (vy + vh - mh).max(vy));

    let _ = theme::apply_frame(hwnd, theme::current(), false);
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        px,
        py,
        d.width,
        mh,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    // 尽力置前台（失败也无妨：点击外部靠 SetCapture 捕获）
    let _ = SetForegroundWindow(hwnd);
    // 关键：捕获鼠标，使「点击菜单外任意位置」也能被我们收到并关闭菜单
    SetCapture(hwnd);
    let _ = raw::UpdateWindow(hwnd);

    // 模态消息循环（WM_DESTROY 时 PostQuitMessage 会被本循环消费，不影响主循环）
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    let chosen = d.chosen;
    drop(Box::from_raw(data));
    chosen
}

fn menu_height(d: &Data, _dpi: i32) -> i32 {
    let mut h = d.pad * 2;
    for (i, it) in d.items.iter().enumerate() {
        if i > 0 && it.separator_before {
            h += d.sep_h;
        }
        h += d.item_h;
    }
    h
}

/// 用真实字体测量最宽项，得出菜单宽度与各项高度。
unsafe fn measure(hwnd: HWND, d: &mut Data) {
    let dpi = util::dpi_of(hwnd).max(GetDpiForSystem() as i32);
    d.item_h = util::scaled(34, dpi);
    d.sep_h = util::scaled(9, dpi);
    d.pad = util::scaled(6, dpi);

    let hdc = GetDC(hwnd);
    let font = util::gui_font(hwnd, 10, 400);
    let old = SelectObject(hdc, font);
    let mut max_w = util::scaled(72, dpi);
    for it in &d.items {
        let w = util::to_wide(&it.label);
        let mut r = RECT::default();
        let _ = raw::DrawTextW(
            hdc.0 as isize,
            PCWSTR::from_raw(w.as_ptr()),
            -1,
            &mut r,
            0x0400 | 0x20, // DT_CALCRECT | DT_SINGLELINE
        );
        max_w = max_w.max(r.right - r.left);
    }
    SelectObject(hdc, old);
    let _ = ReleaseDC(hwnd, hdc);

    d.width = max_w + util::scaled(44, dpi);
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
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
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = client_xy(lp);
            let idx = hit_test(hwnd, x, y);
            if let Some(d) = data_of(hwnd) {
                if d.hover != idx {
                    d.hover = idx;
                    let _ = raw::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = client_xy(lp);
            match hit_test(hwnd, x, y) {
                Some(i) => {
                    if let Some(d) = data_of(hwnd) {
                        d.chosen = Some(i);
                    }
                    let _ = DestroyWindow(hwnd);
                }
                // 点击菜单内空白/分隔线：什么都不做（保持菜单）
                None => {}
            }
            LRESULT(0)
        }
        // 点击菜单外：SetCapture 把消息送到这里，坐标落在客户区之外 → 关闭
        WM_RBUTTONDOWN => {
            let (x, y) = client_xy(lp);
            let mut cr = RECT::default();
            let _ = GetClientRect(hwnd, &mut cr);
            if x < 0 || y < 0 || x >= cr.right || y >= cr.bottom {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 == VK_ESCAPE {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            // 失去焦点（例如用户切窗口）时自动关闭，避免残留
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 数据由 show_impl 在循环结束后释放（需要读 chosen），这里只复位
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = ReleaseCapture();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn data_of(hwnd: HWND) -> Option<&'static mut Data> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut Data))
    }
}

/// 客户区坐标 → 菜单项下标（落在分隔线/内边距上返回 None）。
unsafe fn hit_test(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    let d = data_of(hwnd)?;
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    if x < 0 || y < 0 || x >= cr.right || y >= cr.bottom {
        return None;
    }
    let mut top = d.pad;
    for (i, it) in d.items.iter().enumerate() {
        if i > 0 && it.separator_before {
            top += d.sep_h;
        }
        if y >= top && y < top + d.item_h {
            return it.enabled.then_some(i);
        }
        top += d.item_h;
    }
    None
}

fn client_xy(lp: LPARAM) -> (i32, i32) {
    (
        (lp.0 & 0xFFFF) as u16 as i16 as i32,
        ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

unsafe fn paint(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let t = theme::current();
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (w, h) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);

    // 背景卡片
    let bg = CreateSolidBrush(t.card);
    let rc = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    FillRect(hdc, &rc, bg);
    let _ = DeleteObject(bg);
    let pen = CreatePen(PS_SOLID, 1, theme::edge_line(t));
    let op = SelectObject(hdc, pen);
    let _ = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let radius = util::scaled(12, dpi);
    let _ = RoundRect(hdc, 0, 0, w, h, radius, radius);
    SelectObject(hdc, op);
    let _ = DeleteObject(pen);

    let font = util::gui_font(hwnd, 10, 400);
    let old_font = SelectObject(hdc, font);
    raw::SetBkMode(hdc.0 as isize, 1);

    let mut top = d.pad;
    for (i, it) in d.items.iter().enumerate() {
        if i > 0 && it.separator_before {
            // 分隔线
            let y = top + d.sep_h / 2;
            let sep = CreatePen(PS_SOLID, 1, t.stroke);
            let old = SelectObject(hdc, sep);
            let inset = util::scaled(12, dpi);
            let _ = windows::Win32::Graphics::Gdi::MoveToEx(hdc, inset, y, None);
            let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, w - inset, y);
            SelectObject(hdc, old);
            let _ = DeleteObject(sep);
            top += d.sep_h;
        }
        // 悬停高亮（仅启用项）
        if d.hover == Some(i) && it.enabled {
            let hb = CreateSolidBrush(t.hover);
            let old = SelectObject(hdc, hb);
            let _ = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            let r2 = util::scaled(10, dpi);
            let _ = RoundRect(
                hdc,
                d.pad,
                top,
                w - d.pad,
                top + d.item_h,
                r2,
                r2,
            );
            SelectObject(hdc, old);
            let _ = DeleteObject(hb);
        }
        // 文本
        let color = if it.enabled { t.text } else { COLORREF(0x0080_8080) };
        raw::SetTextColor(hdc.0 as isize, color);
        let tw = util::to_wide(&it.label);
        let mut tr = RECT {
            left: util::scaled(18, dpi),
            top,
            right: w - util::scaled(12, dpi),
            bottom: top + d.item_h,
        };
        let _ = raw::DrawTextW(hdc.0 as isize, PCWSTR::from_raw(tw.as_ptr()), -1, &mut tr, DT_MID_LEFT);
        top += d.item_h;
    }

    SelectObject(hdc, old_font);
    let _ = EndPaint(hwnd, &ps);
}
