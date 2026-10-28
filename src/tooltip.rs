//! 工具条图标提示气泡（自绘，非 comctl32 标准 tooltip）。
//!
//! 为什么不用 `tooltips_class32`：本程序未嵌入 comctl32 v6 清单，标准 tooltip
//! 会以「经典浅色方框」呈现，与工具条固定深色的 Fluent 视觉冲突。这里用一个
//! 轻量顶层 popup 窗口自绘：圆角卡片 + 1px 描边 + 主题字体，风格与工具条一致。
//!
//! 生命周期：由工具条创建（[`Tip::new`]）、随工具条销毁（[`Tip::destroy`]）。
//! 显示时机由调用方（工具条的按钮子类化过程）控制 —— 悬停延时后 [`Tip::show`]，
//! 离开/点击时 [`Tip::hide`]。

use crate::theme;
use crate::util::{self, raw, register_class_w};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, GetDC, GetMonitorInfoW, HDC,
    MonitorFromRect, ReleaseDC, RoundRect, SelectObject, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    PAINTSTRUCT, PS_SOLID,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 气泡窗口类名。
const CLASS: &str = "PixShotTip";
/// 内边距 / 与按钮间距 / 最大宽度（逻辑像素）由配置提供，见 config.ini [ui] 的
/// tooltip_pad_x / tooltip_pad_y / tooltip_gap / tooltip_max_width。
fn metrics() -> (i32, i32, i32, i32) {
    let c = crate::config::get();
    (
        c.ui_tooltip_pad_x,
        c.ui_tooltip_pad_y,
        c.ui_tooltip_gap,
        c.ui_tooltip_max_width,
    )
}

/// 气泡窗口私有数据（经 GWLP_USERDATA 持有，WM_NCDESTROY 释放）。
struct TipData {
    /// 说明文字（如「贴图」）
    label: Vec<u16>,
    /// 快捷键后缀（如「（Ctrl+T）」），以次级文本色**紧接**说明文字绘制
    suffix: Vec<u16>,
    skin: theme::Theme,
}

/// 提示气泡句柄包装。
pub struct Tip {
    hwnd: HWND,
}

unsafe fn data_of(hwnd: HWND) -> Option<&'static mut TipData> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    (p != 0).then(|| &mut *(p as *mut TipData))
}

impl Tip {
    /// 占位（尚未创建）：所有方法安全空操作。用于在窗口创建前初始化宿主数据。
    pub fn none() -> Tip {
        Tip {
            hwnd: HWND::default(),
        }
    }

    /// 创建气泡窗口（初始隐藏）。`owner` 为工具条窗口，仅作所有者，不影响 Z 序。
    pub unsafe fn new(owner: HWND) -> Tip {
        register_class_w(CLASS, tip_proc, false, false, false);
        let data = Box::into_raw(Box::new(TipData {
            label: Vec::new(),
            suffix: Vec::new(),
            skin: theme::toolbar_skin(),
        }));
        let hinst =
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(util::to_wide(CLASS).as_ptr()),
            PCWSTR::from_raw(util::to_wide("").as_ptr()),
            WS_POPUP,
            0,
            0,
            10,
            10,
            owner,
            None,
            hinst,
            Some(data as *const core::ffi::c_void),
        )
        .unwrap_or_default();
        if hwnd.is_invalid() {
            drop(Box::from_raw(data));
            return Tip {
                hwnd: HWND::default(),
            };
        }
        // 与工具条一致：排除出截屏捕获
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        Tip { hwnd }
    }

    /// 在 `anchor`（按钮屏幕矩形）附近显示提示：`label` 为说明文字，
    /// `shortcut` 为快捷键，显示为 `贴图（Ctrl+T）` —— 括号部分用次级文本色、紧随标签。
    pub unsafe fn show(&mut self, label: &str, shortcut: &str, anchor: &RECT) {
        let Some(d) = data_of(self.hwnd) else { return };
        let dpi = util::dpi_of(self.hwnd);
        let s = |v: i32| util::scaled(v, dpi);
        let (pad_x, pad_y, gap, max_w) = metrics();
        d.label = util::to_wide(label);
        d.suffix = util::to_wide(&format!("（{shortcut}）"));
        // 窗口标题 = 完整文案：可访问性/自动化可直接读到气泡内容（E2E 依赖）
        let title = util::to_wide(&format!("{label}（{shortcut}）"));
        let _ = SetWindowTextW(self.hwnd, PCWSTR::from_raw(title.as_ptr()));

        // 量测两段文本（DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX）
        let hdc = GetDC(self.hwnd);
        let font = util::gui_font(self.hwnd, 9, 400);
        let old_font = SelectObject(hdc, font);
        let measure = |text: &[u16]| {
            let mut tr = RECT {
                left: 0,
                top: 0,
                right: s(max_w),
                bottom: 0,
            };
            let _ = raw::DrawTextW(
                hdc.0 as isize,
                PCWSTR::from_raw(text.as_ptr()),
                -1,
                &mut tr,
                0x400 | 0x20 | 0x800, // DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX
            );
            ((tr.right - tr.left).max(1), (tr.bottom - tr.top).max(1))
        };
        let (lw, lh) = measure(&d.label);
        let (sw, sh) = measure(&d.suffix);
        SelectObject(hdc, old_font);
        let _ = ReleaseDC(self.hwnd, hdc);

        // 括号后缀紧接标签（全角括号自带间隔，无需额外间距）
        let tw = lw + sw;
        let th = lh.max(sh);
        let (w, h) = (
            (tw + s(pad_x) * 2).min(s(max_w)),
            th + s(pad_y) * 2,
        );

        // 优先显示在按钮上方居中；越界则改到下方；再钳制回显示器工作区
        let (mut x, mut y) = (
            anchor.left + ((anchor.right - anchor.left) - w) / 2,
            anchor.top - s(gap) - h,
        );
        let mi = monitor_work(anchor);
        if y < mi.top {
            y = anchor.bottom + s(gap);
        }
        x = x.clamp(mi.left, (mi.right - w).max(mi.left));
        y = y.clamp(mi.top, (mi.bottom - h).max(mi.top));

        let _ = SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = raw::InvalidateRect(self.hwnd, core::ptr::null(), 1);
    }

    /// 隐藏提示气泡。
    pub unsafe fn hide(&mut self) {
        if !self.hwnd.is_invalid() {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// 销毁气泡窗口（工具条关闭时调用）。
    pub unsafe fn destroy(&mut self) {
        if !self.hwnd.is_invalid() {
            let _ = DestroyWindow(self.hwnd);
            self.hwnd = HWND::default();
        }
    }

}

/// 取包含 `rc` 的显示器工作区（失败时回退为 `rc` 本身）。
unsafe fn monitor_work(rc: &RECT) -> RECT {
    let mon = MonitorFromRect(rc, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: core::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(mon, &mut mi).as_bool() {
        mi.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 4096,
            bottom: 2160,
        }
    }
}

unsafe extern "system" fn tip_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = lp.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let p = (*cs).lpCreateParams as isize;
                if p != 0 {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, p);
                    if let Some(d) = data_of(hwnd) {
                        d.skin = theme::toolbar_skin();
                    }
                }
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            if let Some(d) = data_of(hwnd) {
                paint(hwnd, d);
            }
            LRESULT(0)
        }
        // 不接收输入/焦点，鼠标穿透
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_NCDESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if p != 0 {
                drop(Box::from_raw(p as *mut TipData));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn paint(hwnd: HWND, d: &TipData) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    paint_card(hwnd, hdc, cr.right, cr.bottom, d);
    let _ = EndPaint(hwnd, &ps);
}

/// 气泡绘制主体：把「圆角卡片 + 文本」画到任意 DC（WM_PAINT 与预览测试共用）。
/// `hwnd` 仅用于取 DPI 与主题字体。
unsafe fn paint_card(hwnd: HWND, hdc: HDC, cw: i32, ch: i32, d: &TipData) {
    let t = d.skin;
    let dpi = util::dpi_of(hwnd);

    // 卡片底 + 描边
    let brush = CreateSolidBrush(t.card);
    let pen = CreatePen(PS_SOLID, 1, t.stroke);
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, pen);
    let r = util::scaled(8, dpi);
    let _ = RoundRect(hdc, 0, 0, cw, ch, r, r);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    // 文本：说明文字「贴图」（主文本色）+ 后缀「（Ctrl+T）」（次级文本色，紧接其后）
    if !d.label.is_empty() {
        let font = util::gui_font(hwnd, 9, 400);
        let old_font = SelectObject(hdc, font);
        raw::SetBkMode(hdc.0 as isize, 1);
        let (mpad_x, mpad_y, _, mmax_w) = metrics();
        let pad_x = util::scaled(mpad_x, dpi);
        let pad_y = util::scaled(mpad_y, dpi);
        let single = |text: &[u16], x: i32, color: windows::Win32::Foundation::COLORREF| {
            raw::SetTextColor(hdc.0 as isize, color);
            let mut tr = RECT {
                left: x,
                top: pad_y,
                right: cw - pad_x,
                bottom: ch - pad_y,
            };
            let _ = raw::DrawTextW(
                hdc.0 as isize,
                PCWSTR::from_raw(text.as_ptr()),
                -1,
                &mut tr,
                0x20 | 0x800, // DT_SINGLELINE | DT_NOPREFIX
            );
        };
        // 先量出说明文字宽度（画在同一 DC 上，字体已选入）
        let mut calc = RECT {
            left: 0,
            top: 0,
            right: util::scaled(mmax_w, dpi),
            bottom: 0,
        };
        let _ = raw::DrawTextW(
            hdc.0 as isize,
            PCWSTR::from_raw(d.label.as_ptr()),
            -1,
            &mut calc,
            0x400 | 0x20 | 0x800, // DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX
        );
        let lw = (calc.right - calc.left).max(1);
        let _ = single(&d.label, pad_x, t.text);
        if !d.suffix.is_empty() {
            // 紧接标签绘制「（Ctrl+T）」，次级文本色
            let _ = single(&d.suffix, pad_x + lw, t.text2);
        }
        SelectObject(hdc, old_font);
    }
}

#[cfg(test)]
mod preview {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };

    /// 渲染一张气泡截图供人工核对（需要真实桌面）。运行：
    /// `cargo test --bin pixshot render_tip_preview -- --ignored`
    #[test]
    #[ignore = "仅在需要人工核对提示气泡外观时运行"]
    fn render_tip_preview() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let mut tip = Tip::new(HWND::default());
            let anchor = RECT {
                left: 300,
                top: 500,
                right: 400,
                bottom: 540,
            };
            tip.show("复制识别到的文字", "Ctrl+O", &anchor);

            let mut wr = RECT::default();
            let _ = GetWindowRect(tip.hwnd, &mut wr);
            let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
            assert!(w > 60 && h > 20, "气泡尺寸异常：{w}x{h}（DT_CALCRECT 失效？）");

            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let hbmp = CreateDIBSection(
                windows::Win32::Graphics::Gdi::HDC::default(),
                &bi,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            )
            .unwrap();
            let mem = CreateCompatibleDC(None);
            let old = SelectObject(mem, hbmp);
            if let Some(d) = data_of(tip.hwnd) {
                paint_card(tip.hwnd, mem, w, h, d);
            }

            let n = (w * h) as usize;
            let mut px = std::slice::from_raw_parts(bits as *const u8, n * 4).to_vec();
            for p in px.chunks_exact_mut(4) {
                p[3] = 255;
            }
            SelectObject(mem, old);
            let _ = DeleteObject(hbmp);
            let _ = DeleteDC(mem);
            tip.destroy();

            let _ = crate::imageio::save_png("target/tip_preview.png", &px, w, h);
        }
    }
}
