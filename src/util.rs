//! 共享的 Win32 辅助函数（沿用本仓库 screen_recorder 的写法：
//! 少量签名不便的函数以 extern "system" 直接声明）。
//!
//! Fluent 现代化部分：主题字体工厂（线程局部缓存）、BS_OWNERDRAW 按钮工厂、
//! 悬停动画引擎（HoverFx，仅鼠标位于窗口内时启用定时器，空闲零成本）、
//! Win11 风格圆角按钮绘制（draw_od_button）。
//!
//! DPI：`scaled()` 把逻辑像素换算为物理像素。本项目全部坐标为物理像素，
//! 因此高 DPI 下的布局必须显式换算，不能沿用 96 DPI 的固定常量。

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, GetStockObject, RoundRect, SelectObject, NULL_BRUSH,
    PS_SOLID,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, GetCursorPos, GetWindowTextW,
    GWLP_WNDPROC, KillTimer, LoadCursorW, MessageBoxW, RegisterClassW, SetTimer, SetWindowLongPtrW,
    WindowFromPoint, HMENU, IDC_ARROW, IDC_CROSS, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETFONT, WNDPROC, WS_CHILD, WS_VISIBLE,
};

/// 全局「流程进行中」标志：防止遮罩/工具栏打开时被托盘/热键重复触发。
static APP_BUSY: AtomicBool = AtomicBool::new(false);
/// 全局「已请求退出」标志：托盘退出在嵌套消息循环中发出后向外层传播。
pub static APP_QUIT: AtomicBool = AtomicBool::new(false);

/// 重入守卫：`acquire()` 成功即持有，`Drop` 时自动复位。
///
/// 用 RAII 替代裸 `swap(true)`：截图流程中任何一处 panic 都不会让
/// APP_BUSY 永久留在 true（否则表现为「F1 从此失灵」）。
/// 流程进行中再次 `acquire()` 返回 None。
pub struct BusyGuard(());

impl BusyGuard {
    pub fn acquire() -> Option<Self> {
        (!APP_BUSY.swap(true, Ordering::SeqCst)).then(|| Self(()))
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        APP_BUSY.store(false, Ordering::SeqCst);
    }
}

/// 请求退出（托盘「退出」项）。
pub fn request_quit() {
    APP_QUIT.store(true, Ordering::SeqCst);
}

/// 是否已请求退出（截图流程结束时检查，补偿嵌套循环吞掉的消息）。
pub fn quit_requested() -> bool {
    APP_QUIT.load(Ordering::SeqCst)
}

/// UTF-8 → UTF-16（NUL 结尾）。
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 窗口当前 DPI（下限 96）。
pub fn dpi_of(hwnd: HWND) -> i32 {
    unsafe { GetDpiForWindow(hwnd).max(96) as i32 }
}

/// 逻辑像素 → 物理像素（四舍五入）。
pub fn scaled(v: i32, dpi: i32) -> i32 {
    let d = dpi.max(96);
    ((v as f32 * d as f32) / 96.0).round() as i32
}

pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// 注册窗口类（重复注册返回 0，忽略）。
/// `drop_shadow` 追加经典 CS_DROPSHADOW（无边框窗口的轻量阴影替代 THICKFRAME 方案）；
/// `dbl_clicks` 追加 CS_DBLCLKS —— 窗口过程要收到 WM_LBUTTONDBLCLK 必须带此样式。
pub fn register_class_w(
    name: &str,
    wndproc: WndProc,
    cross_cursor: bool,
    drop_shadow: bool,
    dbl_clicks: bool,
) {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            CS_DBLCLKS, CS_DROPSHADOW, CS_HREDRAW, CS_VREDRAW, WNDCLASSW,
        };
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let cn = to_wide(name);
        let cursor = if cross_cursor {
            LoadCursorW(None, IDC_CROSS)
        } else {
            LoadCursorW(None, IDC_ARROW)
        };
        let mut style = CS_HREDRAW | CS_VREDRAW;
        if drop_shadow {
            style |= CS_DROPSHADOW;
        }
        if dbl_clicks {
            style |= CS_DBLCLKS;
        }
        let wc = WNDCLASSW {
            style,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR::from_raw(cn.as_ptr()),
            hCursor: cursor.unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// 创建顶层窗口；`lp` 作为 CREATESTRUCT 传给 WM_CREATE。
#[allow(clippy::too_many_arguments)]
pub fn create_window(
    class: &str,
    title: &str,
    ex: WINDOW_EX_STYLE,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    lp: *mut core::ffi::c_void,
) -> windows::core::Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(
            ex,
            PCWSTR::from_raw(to_wide(class).as_ptr()),
            PCWSTR::from_raw(to_wide(title).as_ptr()),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            hinst,
            Some(lp as *const _),
        )
    }
}

// ─────────────────────────────────────────────────────────────────
// 字体：主题字体 + 线程局部缓存（CJK 优先族名 / DPI 感知）
// ─────────────────────────────────────────────────────────────────

// 字体只在 GUI 线程创建与使用；用 thread_local 替代 static mut，
// 既保证单线程无锁，也不再需要 `#![allow(static_mut_refs)]`。
thread_local! {
    static FONT_CACHE: RefCell<Vec<((i32, i32, i32), windows::Win32::Graphics::Gdi::HFONT)>> =
        const { RefCell::new(Vec::new()) };
}

/// 取主题 UI 字体（带缓存；控件 WM_SETFONT 用，勿手动 DeleteObject）。
pub fn gui_font(hwnd: HWND, pt: i32, weight: i32) -> windows::Win32::Graphics::Gdi::HFONT {
    let dpi = dpi_of(hwnd);
    let key = (pt, weight, dpi);
    FONT_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((_, f)) = c.iter().find(|(k, _)| *k == key) {
            return *f;
        }
        let f = crate::theme::ui_font(hwnd, pt, weight);
        c.push((key, f));
        f
    })
}

fn set_gui_font(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    unsafe {
        let font = gui_font(hwnd, 9, 400);
        let _ = SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

// ─────────────────────────────────────────────────────────────────
// 控件工厂（按钮为 BS_OWNERDRAW，由宿主 WM_DRAWITEM 调 draw_od_button 绘制；
// 控件 ID 与 BM_CLICK 协议保持不变，自动化测试不受影响）
// ─────────────────────────────────────────────────────────────────

/// 创建 owner-draw 按钮（Fluent 圆角/态/动画）。
#[allow(clippy::too_many_arguments)]
pub fn add_button(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32, id: usize) -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(to_wide("BUTTON").as_ptr()),
            PCWSTR::from_raw(to_wide(text).as_ptr()),
            WINDOW_STYLE(WS_VISIBLE.0 | WS_CHILD.0 | 0x0000_000B), // BS_OWNERDRAW
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        set_gui_font(hwnd);
        hwnd
    }
}

/// 创建静态文本控件。
///
/// 当前无调用方：工具条已按要求不再显示任何标签文字（只保留按钮）。
/// 保留该工厂以便后续需要文本控件时与 [`add_button`] 配套使用。
#[allow(dead_code)]
pub fn add_label(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32) -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(to_wide("STATIC").as_ptr()),
            PCWSTR::from_raw(to_wide(text).as_ptr()),
            WS_VISIBLE | WS_CHILD,
            x,
            y,
            w,
            h,
            parent,
            None,
            hinst,
            None,
        )
        .unwrap_or_default();
        set_gui_font(hwnd);
        hwnd
    }
}

/// WM_CTLCOLORSTATIC / WM_CTLCOLOREDIT：指定文本色 + 透明底（露出卡片）。
///
/// 当前无调用方（OCR 结果窗口移除后不再有 STATIC/EDIT 子控件）；
/// 保留该处理与 [`add_label`] 配套，便于后续窗口复用。
#[allow(dead_code)]
pub unsafe fn ctlcolor_static(wp: WPARAM, text: COLORREF) -> LRESULT {
    raw::SetTextColor(wp.0 as isize, text);
    raw::SetBkMode(wp.0 as isize, 1); // TRANSPARENT
    LRESULT(GetStockObject(NULL_BRUSH).0 as isize)
}

/// 弹出错误提示框（错误必须可见，保留系统模态框是刻意选择）。
pub fn error_msgbox(text: &str) {
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR::from_raw(to_wide(text).as_ptr()),
            PCWSTR::from_raw(to_wide("PixShot").as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 弹出普通提示框（非错误场景，如图标/信息提示，不带错误图标）。
pub fn info_msgbox(text: &str) {
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR::from_raw(to_wide(text).as_ptr()),
            PCWSTR::from_raw(to_wide("PixShot").as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// Fluent owner-draw 按钮：悬停动画引擎 + 绘制
// ─────────────────────────────────────────────────────────────────

pub const TIMER_HOVER_TRACK: usize = 777; // 光标命中轮询（仅鼠标在窗口内时存在）
pub const TIMER_HOVER_ANIM: usize = 778; // 0.15s 过渡插值（仅在过渡期间存在）

/// WM_MOUSELEAVE（0x02A3）—— windows 0.58 元数据未提供，本地定义。
pub const WM_MOUSELEAVE: u32 = 0x02A3;

/// 每个宿主窗口持有一份；注册其全部 owner-draw 按钮即获得悬停动画。
/// 空闲时无任何定时器（CPU≈0）；动画期间 16ms 节拍，结束即停。
pub struct HoverFx {
    /// (按钮 HWND, 当前插值值 0..1, 目标值 0/1)
    pub entries: Vec<(HWND, f32, f32)>,
    tracking: bool,
}

impl Default for HoverFx {
    fn default() -> Self {
        Self::new()
    }
}

impl HoverFx {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            tracking: false,
        }
    }

    pub fn register(&mut self, b: HWND) {
        self.entries.push((b, 0.0, 0.0));
    }

    pub fn val(&self, b: HWND) -> f32 {
        self.entries
            .iter()
            .find(|(h, _, _)| *h == b)
            .map(|(_, v, _)| *v)
            .unwrap_or(0.0)
    }

    /// 宿主 WM_MOUSEMOVE：开启离开跟踪并（仅首次）启动光标轮询。
    ///
    /// 注意：不能每条 WM_MOUSEMOVE 都 `SetTimer` —— 对同一 ID 的调用会**重置**
    /// 计时，高频鼠标（125Hz+）会让 16ms 定时器几乎永不触发，反而使悬停高亮变迟钝。
    pub fn on_mouse_move(&mut self, parent: HWND) {
        unsafe {
            if !self.tracking {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: parent,
                    dwHoverTime: 0,
                };
                // 即便 TrackMouseEvent 失败也继续轮询：仍能靠光标位置收尾
                let _ = TrackMouseEvent(&mut tme);
                self.tracking = true;
                let _ = SetTimer(parent, TIMER_HOVER_TRACK, 16, None);
            }
            self.track_tick(parent);
        }
    }

    /// 宿主 WM_MOUSELEAVE：停止轮询，全部目标归零（淡出动画收尾）。
    pub fn on_mouse_leave(&mut self, parent: HWND) {
        self.tracking = false;
        unsafe {
            let _ = KillTimer(parent, TIMER_HOVER_TRACK);
        }
        self.finish_fade(parent);
    }

    /// 宿主被销毁/隐藏时复位（避免定时器残留与野指针重绘）。
    pub fn stop(&mut self, parent: HWND) {
        self.tracking = false;
        unsafe {
            let _ = KillTimer(parent, TIMER_HOVER_TRACK);
            let _ = KillTimer(parent, TIMER_HOVER_ANIM);
        }
        for e in self.entries.iter_mut() {
            e.1 = 0.0;
            e.2 = 0.0;
        }
    }

    fn finish_fade(&mut self, parent: HWND) {
        let mut changed = false;
        for e in self.entries.iter_mut() {
            if e.2 != 0.0 {
                e.2 = 0.0;
                changed = true;
            }
        }
        if changed {
            unsafe {
                let _ = SetTimer(parent, TIMER_HOVER_ANIM, 16, None);
            }
        }
    }

    /// 宿主 WM_TIMER：返回 true 表示已消费该定时器。
    pub fn on_timer(&mut self, parent: HWND, id: usize) -> bool {
        match id {
            TIMER_HOVER_TRACK => {
                self.track_tick(parent);
                true
            }
            TIMER_HOVER_ANIM => {
                unsafe {
                    let mut all_done = true;
                    let step = 16.0f32 / 150.0; // 16ms 步长 / 150ms 过渡
                    for e in self.entries.iter_mut() {
                        let d = e.2 - e.1;
                        if d.abs() <= step {
                            e.1 = e.2;
                        } else {
                            e.1 += d.signum() * step;
                            all_done = false;
                        }
                        let _ = raw::InvalidateRect(e.0, std::ptr::null(), 0);
                    }
                    if all_done {
                        let _ = KillTimer(parent, TIMER_HOVER_ANIM);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// 一次 `WindowFromPoint` 命中判定，替代「每个按钮一次 GetWindowRect」
    /// （5 个按钮时把 5 次内核调用降到 1 次）。
    fn track_tick(&mut self, parent: HWND) {
        unsafe {
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt).is_err() {
                return;
            }
            let hit = WindowFromPoint(pt);
            let mut changed = false;
            for e in self.entries.iter_mut() {
                let target = if e.0 == hit { 1.0 } else { 0.0 };
                if (e.2 - target).abs() > f32::EPSILON {
                    e.2 = target;
                    changed = true;
                }
            }
            if changed {
                let _ = SetTimer(parent, TIMER_HOVER_ANIM, 16, None);
            }
        }
    }
}

/// 按钮视觉样式。
///
/// 当前只有工具条的图标按钮在用（`Icon` / `IconClose`）；`Accent` / `Subtle` /
/// `CloseGlyph` 为通用 owner-draw 按钮样式（OCR 结果窗口移除后暂无调用方），
/// 作为窗口控件的通用能力保留。
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum BtnStyle {
    /// 强调色实心（主操作）
    Accent,
    /// 卡片表面 + 描边（次级操作）
    Subtle,
    /// 标题栏关闭 ✕（悬停变红）
    CloseGlyph,
    /// 图标按钮：悬停时卡片淡入，**不绘制文本**（图标由调用方自绘）
    Icon,
    /// 图标按钮 + 关闭语义：悬停变红，不绘制文本
    IconClose,
}

/// 按样式与悬停插值算出按钮（填充色, 前景色）。绘制与图标取色共用，避免两处发散。
pub fn btn_colors(
    style: BtnStyle,
    t: crate::theme::Theme,
    v: f32,
    pressed: bool,
) -> (COLORREF, COLORREF) {
    match style {
        BtnStyle::Accent => {
            let f = if pressed {
                crate::theme::lerp(t.accent, t.text2, 0.25)
            } else {
                t.accent
            };
            (f, t.accent_text)
        }
        BtnStyle::Subtle => {
            let mut f = crate::theme::lerp(t.card, t.hover, v);
            if pressed {
                f = crate::theme::lerp(f, t.press, 0.9);
            }
            (f, t.text)
        }
        BtnStyle::CloseGlyph => {
            let f = if pressed {
                t.close_hover
            } else {
                crate::theme::lerp(t.card, t.close_hover, v)
            };
            (f, crate::theme::lerp(t.text, COLORREF(0x00FF_FFFF), v))
        }
        BtnStyle::Icon => {
            let mut f = crate::theme::lerp(t.card, t.hover, v);
            if pressed {
                f = crate::theme::lerp(f, t.press, 0.9);
            }
            (f, t.text)
        }
        BtnStyle::IconClose => {
            let f = if pressed {
                t.close_hover
            } else {
                crate::theme::lerp(t.card, t.close_hover, v)
            };
            (f, crate::theme::lerp(t.text, COLORREF(0x00FF_FFFF), v))
        }
    }
}

/// WM_DRAWITEM 绘制：Win11 风格圆角按钮（含悬停过渡/按下态）。
/// `t` 为该窗口的皮肤（支持每个窗口独立主题，如工具栏固定深色）。
/// `Icon`/`IconClose` 只画底色、不画文本，供图标按钮在宿主里叠加矢量图标。
pub unsafe fn draw_od_button(
    parent: HWND,
    dis: &DRAWITEMSTRUCT,
    fx: &HoverFx,
    style: BtnStyle,
    t: crate::theme::Theme,
) {
    let v = fx.val(dis.hwndItem);
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    let (fill, txt) = btn_colors(style, t, v, pressed);
    let weight = if matches!(style, BtnStyle::Accent) {
        600
    } else {
        400
    };

    let hdc = dis.hDC;
    let r = dis.rcItem;
    // 圆角半径随 DPI 缩放（96DPI 下轴长 8 ≈ 4px 圆角）
    let radius = scaled(8, dpi_of(parent)).max(4);

    let brush = CreateSolidBrush(fill);
    let stroke_col = match style {
        BtnStyle::Subtle => t.stroke,
        _ => fill,
    };
    let pen = CreatePen(PS_SOLID, 1, stroke_col);
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    // 图标按钮无文本（图标由调用方另绘）
    if matches!(style, BtnStyle::Icon | BtnStyle::IconClose) {
        return;
    }

    // 文本（✕ 字形同样走窗口文本）
    let mut buf = [0u16; 128];
    let n = GetWindowTextW(dis.hwndItem, &mut buf);
    if n > 0 {
        let font = gui_font(parent, 9, weight);
        let old_font = SelectObject(hdc, font);
        raw::SetBkMode(hdc.0 as isize, 1);
        raw::SetTextColor(hdc.0 as isize, txt);
        let mut tr = RECT {
            left: r.left + 4,
            top: r.top,
            right: r.right - 4,
            bottom: r.bottom,
        };
        // 必须传显式长度 n：buf[..n] 不含结尾 NUL，
        // 传 -1（表示"以 NUL 结尾"）会让 GDI 越界读取堆内存，显示为乱码。
        let tw: Vec<u16> = buf[..(n as usize).min(127)].to_vec();
        let _ = raw::DrawTextW(
            hdc.0 as isize,
            PCWSTR::from_raw(tw.as_ptr()),
            tw.len() as i32,
            &mut tr,
            0x25, // DT_CENTER | DT_VCENTER | DT_SINGLELINE
        );
        SelectObject(hdc, old_font);
    }
}

// ─────────────────────────────────────────────────────────────────
// 控件子类化：替换子控件窗口过程（工具条图标按钮的悬停提示用）
// ─────────────────────────────────────────────────────────────────

thread_local! {
    /// (控件 HWND, 原窗口过程)：登记后供 [`call_orig`] 转发，控件销毁时用 [`unsubclass`] 注销。
    static OLD_PROCS: RefCell<Vec<(isize, isize)>> = const { RefCell::new(Vec::new()) };
}

/// 用 `proc` 替换控件窗口过程，返回原过程；原过程内部登记，供 [`call_orig`] 转发。
pub unsafe fn subclass(hwnd: HWND, proc: WndProc) -> isize {
    let old = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, proc as usize as isize);
    OLD_PROCS.with(|m| m.borrow_mut().push((hwnd.0 as isize, old)));
    old
}

/// 注销 [`subclass`] 的登记（控件销毁前调用，避免同一句柄复用时命中原过程）。
pub fn unsubclass(hwnd: HWND) {
    OLD_PROCS.with(|m| m.borrow_mut().retain(|(h, _)| *h != hwnd.0 as isize));
}

/// 把消息转发到 [`subclass`] 替换前的原窗口过程（取不到时退回 `DefWindowProcW`）。
pub unsafe fn call_orig(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let old = OLD_PROCS.with(|m| {
        m.borrow()
            .iter()
            .rev()
            .find(|(h, _)| *h == hwnd.0 as isize)
            .map(|(_, p)| *p)
            .unwrap_or(0)
    });
    if old == 0 {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let f: WNDPROC = std::mem::transmute(old);
    CallWindowProcW(f, hwnd, msg, wp, lp)
}

/// 少量签名不便的 GDI/User32 函数直接声明
/// （与本仓库 screen_recorder/overlay.rs 的做法一致）。
pub mod raw {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{COLORREF, HWND, RECT};

    extern "system" {
        pub fn InvalidateRect(hwnd: HWND, rect: *const RECT, erase: i32) -> i32;
        pub fn UpdateWindow(hwnd: HWND) -> i32;
        pub fn SetBkMode(hdc: isize, mode: i32) -> i32;
        pub fn SetTextColor(hdc: isize, color: COLORREF) -> COLORREF;
        pub fn DrawTextW(hdc: isize, text: PCWSTR, len: i32, rect: *mut RECT, format: u32) -> i32;
    }
}
