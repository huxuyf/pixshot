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
    GetStockObject, ReleaseDC, RoundRect, SelectObject, HDC, NULL_BRUSH, PS_SOLID,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "PixShotMenu";
const VK_ESCAPE: usize = 0x1B;
/// VK_CONTROL / VK_SHIFT / VK_MENU(Alt)
const VK_CONTROL: i32 = 0x11;
const VK_SHIFT: i32 = 0x10;
const VK_MENU: i32 = 0x12;
/// DT_VCENTER | DT_SINGLELINE
const DT_MID_LEFT: u32 = 0x24;

/// 把当前按键（连同 Ctrl/Shift/Alt 的物理状态）拼成 `Ctrl+C` 形式的组合键名；
/// 非字母/数字键返回 `None`。用于与 [`Item::shortcut`] 上的文案精确比对。
unsafe fn pressed_combo(vk: usize) -> Option<String> {
    let ch = match vk {
        0x30..=0x39 => (b'0' + (vk - 0x30) as u8) as char,
        0x41..=0x5A => (b'A' + (vk - 0x41) as u8) as char,
        _ => return None,
    };
    let down = |v: i32| (GetKeyState(v) as u16) & 0x8000 != 0;
    let mut s = String::new();
    if down(VK_CONTROL) {
        s.push_str("Ctrl+");
    }
    if down(VK_SHIFT) {
        s.push_str("Shift+");
    }
    if down(VK_MENU) {
        s.push_str("Alt+");
    }
    s.push(ch);
    Some(s)
}

/// 一个菜单项。
pub struct Item {
    pub label: String,
    pub enabled: bool,
    /// 该项上方是否画一条分隔线（第一项忽略）。
    pub separator_before: bool,
    /// 快捷键（如 `Ctrl+C`）：右对齐显示在条目右侧，且菜单打开时按下即触发该项。
    pub shortcut: Option<String>,
}

impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            separator_before: false,
            shortcut: None,
        }
    }

    /// 带快捷键的条目（快捷键右对齐显示，并在菜单展开时生效）。
    /// 带快捷键文案的菜单项。`shortcut` 文案必须与 `pressed_combo` 的拼法一致：
    /// 修饰键固定 `Ctrl+Shift+Alt+` 顺序、主键为单个大写字母/数字（如 `Ctrl+S`）。
    pub fn accel(label: impl Into<String>, shortcut: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            separator_before: false,
            shortcut: Some(shortcut.into()),
        }
    }

    pub fn sep(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            separator_before: true,
            shortcut: None,
        }
    }

    /// 预留：构建禁用态菜单项（如无选中内容时的「复制」）。
    #[allow(dead_code)]
    pub fn disabled(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: false,
            separator_before: false,
            shortcut: None,
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

unsafe fn show_impl(x: i32, y: i32, items: &[Item]) -> Option<usize> {
    register_class_w(CLASS, menu_proc, false, true, false);

    let owned: Vec<Item> = items
        .iter()
        .map(|i| Item {
            label: i.label.clone(),
            enabled: i.enabled,
            separator_before: i.separator_before,
            shortcut: i.shortcut.clone(),
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
    // 按目标点所在显示器的 DPI 量测（窗口尚未移到目标屏，dpi_of 此时是主屏 DPI，
    // 多屏异缩放量测会错）
    let dpi = util::dpi_for_rect(x, y, 1, 1);
    measure(hwnd, d, dpi);

    // 钳制到目标点所在显示器的**工作区**（不含任务栏），贴底时向上翻转
    let (vx, vy, vw, vh) = crate::capture::virtual_screen_rect();
    let wa = util::work_area_at(x, y)
        .unwrap_or(RECT { left: vx, top: vy, right: vx + vw, bottom: vy + vh });
    let mh = menu_height(d, dpi);
    let mut px = x;
    let mut py = if y + mh > wa.bottom { y - mh } else { y };
    px = px.clamp(wa.left, (wa.right - d.width).max(wa.left));
    py = py.clamp(wa.top, (wa.bottom - mh).max(wa.top));

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

    // 模态消息循环（WM_DESTROY 时 PostQuitMessage 会被本循环消费，不影响主循环）。
    // 注意：本循环按全线程收消息，期间主窗口的 WM_HOTKEY / 托盘回调仍会被分发——
    // 重入安全由 TRAY_MENU_BUSY / BusyGuard 双保险兜底（见 tray.rs）。
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
/// `dpi` 取目标显示器的 DPI（窗口可能尚未移到目标屏，不能 dpi_of(hwnd)）。
unsafe fn measure(hwnd: HWND, d: &mut Data, dpi: i32) {
    let cfg = crate::config::get();
    d.item_h = util::scaled(cfg.ui_menu_item_h, dpi);
    d.sep_h = util::scaled(cfg.ui_menu_sep_h, dpi);
    d.pad = util::scaled(cfg.ui_menu_pad, dpi);

    let hdc = GetDC(hwnd);
    let font = util::gui_font(hwnd, cfg.ui_menu_font_pt, 400);
    let old = SelectObject(hdc, font);
    let text_w = |s: &str| {
        let w = util::to_wide(s);
        let mut r = RECT::default();
        let _ = raw::DrawTextW(
            hdc.0 as isize,
            PCWSTR::from_raw(w.as_ptr()),
            -1,
            &mut r,
            0x0400 | 0x20, // DT_CALCRECT | DT_SINGLELINE
        );
        (r.right - r.left).max(1)
    };
    let mut max_w = util::scaled(cfg.ui_menu_min_w, dpi);
    for it in &d.items {
        // 标签 + （有快捷键时）标签与快捷键之间的最小间隔 + 快捷键
        let mut w = text_w(&it.label);
        if let Some(sc) = &it.shortcut {
            w += util::scaled(cfg.ui_menu_accel_gap, dpi) + text_w(sc);
        }
        max_w = max_w.max(w);
    }
    SelectObject(hdc, old);
    let _ = ReleaseDC(hwnd, hdc);

    d.width = max_w + util::scaled(cfg.ui_menu_right_pad, dpi);
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
            if point_outside(hwnd, x, y) {
                // 菜单外的左键单击必须在这里关闭：菜单用 SetCapture 捕获鼠标，
                // 落在贴图（同一进程的窗口）上的点击也会被送到本窗口，且不会
                // 引起失焦，所以不会走 WM_KILLFOCUS —— 不判外部坐标的话菜单
                // 就会一直留在贴图上（实测缺陷）。
                let _ = DestroyWindow(hwnd);
            } else if let Some(i) = hit_test(hwnd, x, y) {
                if let Some(d) = data_of(hwnd) {
                    d.chosen = Some(i);
                }
                let _ = DestroyWindow(hwnd);
            }
            // 菜单内空白/分隔线上的点击：保持菜单展开
            LRESULT(0)
        }
        // 菜单外的右键：同样关闭（右击通常意为取消当前菜单）
        WM_RBUTTONDOWN => {
            let (x, y) = client_xy(lp);
            if point_outside(hwnd, x, y) {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 == VK_ESCAPE {
                // Esc = 取消菜单（与「关闭贴图 (Esc)」不是同一件事：那个是贴图窗口的 Esc）
                let _ = DestroyWindow(hwnd);
            } else if let Some(combo) = pressed_combo(wp.0) {
                // 快捷键：与条目上显示的文案精确匹配（如 Ctrl+C）即触发该项
                if let Some(d) = data_of(hwnd) {
                    if let Some(i) = d
                        .items
                        .iter()
                        .position(|it| it.enabled && it.shortcut.as_deref() == Some(combo.as_str()))
                    {
                        d.chosen = Some(i);
                        let _ = DestroyWindow(hwnd);
                    }
                }
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

/// 客户区坐标是否落在菜单窗口之外（用于"点击菜单外即关闭"判定）。
unsafe fn point_outside(hwnd: HWND, x: i32, y: i32) -> bool {
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    x < 0 || y < 0 || x >= cr.right || y >= cr.bottom
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
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    paint_into(hwnd, hdc, cr.right, cr.bottom, d, theme::current());
    let _ = EndPaint(hwnd, &ps);
}

/// 菜单绘制主体：把整张菜单画到任意 DC（WM_PAINT 与预览测试共用）。
/// `hwnd` 仅用于取 DPI 与主题字体。
unsafe fn paint_into(hwnd: HWND, hdc: HDC, w: i32, h: i32, d: &Data, t: theme::Theme) {
    let dpi = util::dpi_of(hwnd);
    let cfg = crate::config::get();

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
    let radius = util::scaled(cfg.ui_menu_radius, dpi);
    // RoundRect 后两个参数是圆角椭圆的轴长（= 半径 × 2）
    let _ = RoundRect(hdc, 0, 0, w, h, radius * 2, radius * 2);
    SelectObject(hdc, op);
    let _ = DeleteObject(pen);

    let font = util::gui_font(hwnd, cfg.ui_menu_font_pt, 400);
    let old_font = SelectObject(hdc, font);
    raw::SetBkMode(hdc.0 as isize, 1);

    let mut top = d.pad;
    for (i, it) in d.items.iter().enumerate() {
        if i > 0 && it.separator_before {
            // 分隔线
            let y = top + d.sep_h / 2;
            let sep = CreatePen(PS_SOLID, 1, t.stroke);
            let old = SelectObject(hdc, sep);
            let inset = util::scaled(cfg.ui_menu_sep_inset, dpi);
            let _ = windows::Win32::Graphics::Gdi::MoveToEx(hdc, inset, y, None);
            let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, w - inset, y);
            SelectObject(hdc, old);
            let _ = DeleteObject(sep);
            top += d.sep_h;
        }
        // 悬停高亮（仅启用项）：灰底圆角块
        //
        // 注意：这里**不能**像画边框那样改选 `NULL_BRUSH` —— 那会让 RoundRect 只描边、
        // 不填充，结果是拿当前（默认黑色）画笔勾出一个「黑色方框」。填充笔与描边笔
        // 取同一颜色，才能得到干净的实心灰底。
        if d.hover == Some(i) && it.enabled {
            let hb = CreateSolidBrush(t.hover);
            let hp = CreatePen(PS_SOLID, 1, t.hover);
            let old_brush = SelectObject(hdc, hb);
            let old_pen = SelectObject(hdc, hp);
            let r2 = util::scaled(cfg.ui_menu_hover_radius, dpi);
            let _ = RoundRect(
                hdc,
                d.pad,
                top,
                w - d.pad,
                top + d.item_h,
                r2,
                r2,
            );
            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(hb);
            let _ = DeleteObject(hp);
        }
        // 文本
        let color = if it.enabled {
            t.text
        } else {
            COLORREF(crate::config::colorref_raw(cfg.ui_menu_disabled_color))
        };
        let right = w - util::scaled(cfg.ui_menu_text_right, dpi);
        raw::SetTextColor(hdc.0 as isize, color);
        let tw = util::to_wide(&it.label);
        let mut tr = RECT {
            left: util::scaled(cfg.ui_menu_text_left, dpi),
            top,
            right,
            bottom: top + d.item_h,
        };
        let _ = raw::DrawTextW(hdc.0 as isize, PCWSTR::from_raw(tw.as_ptr()), -1, &mut tr, DT_MID_LEFT);
        // 快捷键：右对齐、次级文本色（Win11 菜单惯例）
        if let Some(sc) = &it.shortcut {
            let sw = util::to_wide(sc);
            let mut calc = RECT::default();
            let _ = raw::DrawTextW(
                hdc.0 as isize,
                PCWSTR::from_raw(sw.as_ptr()),
                -1,
                &mut calc,
                0x0400 | 0x20, // DT_CALCRECT | DT_SINGLELINE
            );
            let accel_w = (calc.right - calc.left).max(1);
            raw::SetTextColor(
                hdc.0 as isize,
                if it.enabled {
                    t.text2
                } else {
                    COLORREF(crate::config::colorref_raw(cfg.ui_menu_disabled_color))
                },
            );
            let mut sr = RECT {
                left: right - accel_w,
                top,
                right,
                bottom: top + d.item_h,
            };
            let _ = raw::DrawTextW(
                hdc.0 as isize,
                PCWSTR::from_raw(sw.as_ptr()),
                -1,
                &mut sr,
                0x26, // DT_RIGHT | DT_VCENTER | DT_SINGLELINE
            );
        }
        top += d.item_h;
    }

    SelectObject(hdc, old_font);
}

#[cfg(test)]
mod preview {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject as DelObj, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC,
    };

    /// 渲染菜单（含悬停项）供人工核对：`cargo test --bin PixShot render_menu_preview -- --ignored`
    #[test]
    #[ignore = "仅在需要人工核对菜单观感时运行"]
    fn render_menu_preview() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            // 与贴图右键菜单同构：5 项，第 2 项处于悬停态
            let items = vec![
                Item::accel("复制图像", "Ctrl+C"),
                Item::accel("保存为 PNG…", "Ctrl+S"),
                Item::accel("识别文字 (OCR)", "Ctrl+O"),
                Item::sep("重置缩放 (100%)"),
                Item::sep("关闭贴图 (Esc)"),
            ];
            let dpi = 96;
            let s = |v: i32| util::scaled(v, dpi);
            let (item_h, sep_h, pad) = (s(38), s(11), s(6));
            let n_items = items.len() as i32;
            let d = Data {
                items,
                hover: Some(2),
                chosen: None,
                item_h,
                sep_h,
                pad,
                width: s(220),
            };
            let h = pad * 2 + n_items * item_h + 2 * sep_h;
            let (w, h) = (d.width, h);

            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let hbmp = CreateDIBSection(HDC::default(), &bi, DIB_RGB_COLORS, &mut bits, None, 0)
                .unwrap();
            let mem = CreateCompatibleDC(None);
            let old = SelectObject(mem, hbmp);

            // 两张：浅色主题（默认）与深色主题，确认悬停都是"灰底"而不是黑色方框
            let mut canvas: Vec<u8> = Vec::new();
            let mut sheet_h = 0;
            for t in [theme_for(false), theme_for(true)] {
                let mut one = Vec::new();
                paint_into(HWND::default(), mem, w, h, &d, t);
                let n = (w * h) as usize;
                let mut px = std::slice::from_raw_parts(bits as *const u8, n * 4).to_vec();
                for p in px.chunks_exact_mut(4) {
                    p[3] = 255;
                }
                one.extend_from_slice(&px);
                canvas.extend_from_slice(&one);
                sheet_h += h;
            }
            SelectObject(mem, old);
            let _ = DelObj(hbmp);
            let _ = DeleteDC(mem);
            let _ = crate::imageio::save_png("target/menu_preview.png", &canvas, w, sheet_h);
        }
    }

    /// 取浅色 / 深色真实色板做预览（不依赖系统设置）。
    fn theme_for(dark: bool) -> theme::Theme {
        if dark {
            theme::DARK
        } else {
            theme::LIGHT
        }
    }
}
