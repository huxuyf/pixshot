//! 框选遮罩：拖动框选 / 单击选择窗口 / Enter 全屏 / Esc 取消。
//!
//! 用预截取的整屏快照把选区内容"还原"显示，实现 Snipaste 式暗化遮罩。
//!
//! 重要设计（截屏管线）：本模块在进入时截取**一次**虚拟屏幕快照，并把
//! `(快照, 选区)` 一起返回给调用方裁剪成品。调用方不再重新截屏 ——
//! 这既省掉一次全屏 BitBlt 与一份全屏缓冲，也保证「用户看到的选区还原画面」
//! 与「最终得到的截图」是同一帧（原先两次捕获之间画面可能已变化）。
//!
//! 选区还原直接用 `StretchDIBits` 从该快照像素读取（不再另建整屏 DIB），
//! 故框选期间只驻留「快照 + 双缓冲位图」两份全屏缓冲；也顺带消除了早先
//! 「整屏 DIB 被选入 DC 却先于 DC 删除 → DeleteObject 失败」造成的每次
//! 截图泄漏一整屏 DIB 的缺陷。
//!
//! 状态存放：`GWLP_USERDATA` 持有 `Box<State>` 指针（与 pin/results 同一模式），
//! 不再使用 `static mut`；`State` 由 `run()` 在消息循环结束后释放，因此
//! 窗口销毁后仍能读回选区结果。

use std::sync::Arc;

use crate::capture::{self, CapturedImage};
use crate::theme;
use crate::util::{self, raw, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, EndPaint, FillRect, GetDC, GetStockObject, ReleaseDC, SelectObject,
    StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, NULL_BRUSH,
    PS_SOLID, Rectangle, RoundRect, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 选区（虚拟屏幕坐标，物理像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

const VK_ESCAPE: usize = 0x1B;
const VK_RETURN: usize = 0x0D;
/// 小于该边长视为「单击」而非「拖动框选」（逻辑像素，按 DPI 换算后比较）。
const MIN_DRAG_LOGICAL: i32 = 3;

struct State {
    result: Option<SelectedRegion>,
    dragging: bool,
    start: POINT, // 屏幕坐标
    cur: POINT,   // 屏幕坐标
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    /// 预截取的虚拟屏幕快照（供选区还原）。
    /// 选区还原在 `paint` 里用 `StretchDIBits` 直接从这份像素读取，不再额外建一份
    /// 整屏 DIB —— 既省一份全屏内存，也消除了「位图仍被选入 DC 时 DeleteObject
    /// 失败」导致的每次截图泄漏一整屏 DIB 的隐患。
    shot: Arc<CapturedImage>,
    /// 双缓冲：整屏离屏 DC（避免逐帧「填黑→贴图」造成的闪动）
    buf_dc: HDC,
    buf_bmp: HBITMAP,
    /// SelectObject(buf_dc, buf_bmp) 返回的原对象（清理时恢复，才能安全删除位图）
    buf_old: HGDIOBJ,
    /// 上一帧选区（客户区坐标，用于只失效变动的矩形区域）
    last: Option<(i32, i32, i32, i32)>,
}

/// 进入框选模式（模态，阻塞直到用户确认/取消）。
/// 确认时返回「整屏快照 + 选区」；取消返回 None。
pub fn select_region() -> Option<(Arc<CapturedImage>, SelectedRegion)> {
    unsafe { run() }
}

unsafe fn run() -> Option<(Arc<CapturedImage>, SelectedRegion)> {
    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    if vw <= 0 || vh <= 0 {
        return None;
    }

    // 预截取整个虚拟屏幕（唯一一次全屏捕获）
    let shot = Arc::new(capture::capture_region(vx, vy, vw, vh).ok()?);

    // 双缓冲位图（与虚拟屏幕同尺寸）
    let screen_dc = GetDC(HWND::default());
    let buf_dc = CreateCompatibleDC(screen_dc);
    let buf_bmp = CreateCompatibleBitmap(screen_dc, vw, vh);
    let _ = ReleaseDC(HWND::default(), screen_dc);
    if buf_dc.is_invalid() || buf_bmp.is_invalid() {
        // 二者均尚未选入任何 DC，可安全释放
        let _ = DeleteObject(buf_bmp);
        let _ = DeleteDC(buf_dc);
        return None;
    }
    let buf_old = SelectObject(buf_dc, buf_bmp);

    register_class_w("PixShotOverlay", overlay_proc, true, false, false);
    let hinst = GetModuleHandleW(None).unwrap_or_default();

    let state = Box::into_raw(Box::new(State {
        result: None,
        dragging: false,
        start: POINT::default(),
        cur: POINT::default(),
        vx,
        vy,
        vw,
        vh,
        shot: Arc::clone(&shot),
        buf_dc,
        buf_bmp,
        buf_old,
        last: None,
    }));

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
        PCWSTR::from_raw(to_wide("PixShotOverlay").as_ptr()),
        PCWSTR::null(),
        WS_POPUP | WS_VISIBLE,
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

    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 160, LWA_ALPHA);
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    let _ = SetForegroundWindow(hwnd);
    raw::UpdateWindow(hwnd);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // 窗口此时已销毁；state 指针仍有效（由本函数持有）
    let s = Box::from_raw(state);
    let result = s.result;
    cleanup(&s);
    result.map(|r| (shot, r))
}

/// 释放遮罩占用的全部 GDI 资源。
unsafe fn cleanup(s: &State) {
    // 先恢复原对象再删除位图（位图被选入 DC 时 DeleteObject 会失败并泄漏）
    let _ = SelectObject(s.buf_dc, s.buf_old);
    let _ = DeleteObject(s.buf_bmp);
    let _ = DeleteDC(s.buf_dc);
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

        WM_PAINT => paint(hwnd),

        WM_LBUTTONDOWN => {
            if let Some(s) = state_of(hwnd) {
                SetCapture(hwnd);
                s.dragging = true;
                s.start = point_from_lparam(lp, s.vx, s.vy);
                s.cur = s.start;
                invalidate_selection(hwnd, s);
            }
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            if let Some(s) = state_of(hwnd) {
                if s.dragging {
                    s.cur = point_from_lparam(lp, s.vx, s.vy);
                    invalidate_selection(hwnd, s);
                }
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            finish_selection(hwnd, lp);
            LRESULT(0)
        }

        WM_KEYDOWN => {
            match wp.0 {
                VK_ESCAPE => {
                    let _ = DestroyWindow(hwnd);
                }
                VK_RETURN => {
                    if let Some(s) = state_of(hwnd) {
                        s.result = Some(SelectedRegion {
                            x: s.vx,
                            y: s.vy,
                            width: s.vw,
                            height: s.vh,
                        });
                    }
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            // State 由 run() 释放（需要读回选区结果），这里只断开引用
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 鼠标松开：拖动够大则取选区，否则按「单击选窗口」处理。
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
        s.result = Some(clamp_region(r, s.vx, s.vy, s.vw, s.vh));
        let _ = DestroyWindow(hwnd);
        return;
    }

    // 单击（非拖动）：选中光标下的窗口。
    // 必须先把遮罩自身藏起来 —— 遮罩是覆盖全屏的置顶窗口，
    // 否则 WindowFromPoint 永远返回遮罩自己，该功能形同虚设。
    // 命中点用本次点击的坐标（s.cur，屏幕坐标）而非 GetCursorPos，
    // 以免点击与处理之间光标移动导致选错窗口。
    let pt = POINT {
        x: s.cur.x,
        y: s.cur.y,
    };
    let _ = ShowWindow(hwnd, SW_HIDE);
    let hit = WindowFromPoint(pt);
    let root = GetAncestor(hit, GA_ROOT);
    let target = if root.is_invalid() { hit } else { root };

    let mut picked = None;
    // 诊断：记录命中窗口的类名与矩形，便于定位选错窗口的问题
    {
        let mut cb = [0u16; 64];
        let n = GetClassNameW(target, &mut cb);
        let cls = String::from_utf16_lossy(&cb[..(n.max(0) as usize).min(63)]);
        let mut rr = RECT::default();
        let _ = GetWindowRect(target, &mut rr);
        crate::diag::log(&format!(
            "[overlay] 单击命中: hwnd=0x{:X} class={cls} rect=({},{})-({},{}) visible={}",
            target.0 as usize,
            rr.left,
            rr.top,
            rr.right,
            rr.bottom,
            IsWindowVisible(target).as_bool()
        ));
    }
    if !target.is_invalid() && !is_own_window(target) && IsWindowVisible(target).as_bool() {
        let mut wr = RECT::default();
        if GetWindowRect(target, &mut wr).is_ok() {
            let ww = wr.right - wr.left;
            let wh = wr.bottom - wr.top;
            if ww >= min_side && wh >= min_side {
                picked = Some(clamp_region(
                    (wr.left, wr.top, ww, wh),
                    s.vx,
                    s.vy,
                    s.vw,
                    s.vh,
                ));
            }
        }
    }
    match picked {
        Some(reg) => {
            s.result = Some(reg);
            let _ = DestroyWindow(hwnd);
        }
        None => {
            // 未命中可截窗口：恢复遮罩继续框选
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            invalidate_selection(hwnd, s);
        }
    }
}

/// 遮罩横跨整个虚拟屏幕，用系统 DPI 作为徽章/提示的换算基准。
fn overlay_dpi() -> i32 {
    unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32 }
}

/// 只失效「上一帧选区」与「当前选区」两块区域（各外扩一圈以覆盖边框与徽章）。
/// 逐帧整屏失效会让分层窗口反复整屏重合成，是拖动时闪动的直接原因。
unsafe fn invalidate_selection(hwnd: HWND, s: &mut State) {
    let cur = norm_rect(s.start, s.cur);
    let cur_c = (cur.0 - s.vx, cur.1 - s.vy, cur.2, cur.3);
    // 外扩量随 DPI 放大：覆盖 2px 边框与尺寸徽章
    let inflate = util::scaled(70, overlay_dpi());
    for (x, y, w, h) in [s.last, Some(cur_c)].into_iter().flatten() {
        let r = RECT {
            left: x - inflate,
            top: y - inflate,
            right: x + w + inflate,
            bottom: y + h + inflate,
        };
        let _ = raw::InvalidateRect(hwnd, &r, 0);
    }
    s.last = Some(cur_c);
}

/// 是否是本程序自己的窗口（单击选窗时需排除）。
unsafe fn is_own_window(h: HWND) -> bool {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(h, &mut buf);
    if n <= 0 {
        return false;
    }
    let cls = String::from_utf16_lossy(&buf[..(n as usize).min(63)]);
    matches!(
        cls.as_str(),
        "PixShotMain"
            | "PixShotOverlay"
            | "PixShotToolbar"
            | "PixShotPin"
            | "PixShotOcrResult"
            | "PixShotMenu"
            | "PixShotToast"
    )
}

unsafe fn paint(hwnd: HWND) -> LRESULT {
    let Some(s) = state_of(hwnd) else {
        return LRESULT(0);
    };
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    // 双缓冲：先在离屏 DC 合成整帧，再一次性 BitBlt 到窗口，避免逐帧闪动
    let dc = s.buf_dc;

    // 1. 整屏黑色（叠加窗口半透明 → 暗化遮罩）
    let black = CreateSolidBrush(COLORREF(0));
    let all = RECT {
        left: 0,
        top: 0,
        right: s.vw,
        bottom: s.vh,
    };
    FillRect(dc, &all, black);
    let _ = DeleteObject(black);

    // 2. 拖动中：把预截取的屏幕内容画回选区（视觉"还原"）+ 边框 + 尺寸
    if s.dragging {
        let r = norm_rect(s.start, s.cur);
        if r.2 > 0 && r.3 > 0 {
            let dx = r.0 - s.vx;
            let dy = r.1 - s.vy;
            // 直接从整屏快照还原：源坐标相对快照原点，与选区在遮罩内的偏移一致
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: s.shot.width,
                    biHeight: -s.shot.height, // 自上而下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    ..Default::default()
                },
                ..Default::default()
            };
            let bits = s.shot.data.as_ptr() as *const core::ffi::c_void;
            let _ = StretchDIBits(
                dc, dx, dy, r.2, r.3, dx, dy, r.2, r.3, Some(bits), &bmi, DIB_RGB_COLORS, SRCCOPY,
            );

            let pen = CreatePen(PS_SOLID, 2, theme::current().accent); // 主题强调色边框
            let old: HGDIOBJ = SelectObject(dc, pen);
            let _ = SelectObject(dc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(dc, dx, dy, dx + r.2, dy + r.3);
            SelectObject(dc, old);
            let _ = DeleteObject(pen);

            let badge = format!("{} × {}", r.2, r.3);
            let (bx, by) = (
                dx + util::scaled(8, overlay_dpi()),
                dy + util::scaled(8, overlay_dpi()),
            );
            draw_badge(hwnd, dc, &badge, bx, by);
        }
    }

    // 3. 顶部操作提示
    let hint = "拖动框选 | 单击选择窗口 | Enter 全屏 | Esc 取消";
    draw_text(hwnd, dc, hint, util::scaled(12, overlay_dpi()), util::scaled(10, overlay_dpi()));

    // 4. 一次提交（只提交本次失效区域，减少分层窗口重合成开销）
    let pr = ps.rcPaint;
    let _ = BitBlt(
        hdc,
        pr.left,
        pr.top,
        pr.right - pr.left,
        pr.bottom - pr.top,
        dc,
        pr.left,
        pr.top,
        SRCCOPY,
    );

    let _ = EndPaint(hwnd, &ps);
    LRESULT(0)
}

/// 绘制圆角尺寸徽章（深色卡片 + 主题强调色文字）。
unsafe fn draw_badge(hwnd: HWND, hdc: HDC, text: &str, x: i32, y: i32) {
    let t = theme::current();
    let dpi = {
        use windows::Win32::UI::HiDpi::GetDpiForSystem;
        GetDpiForSystem().max(96) as i32
    };
    let chars = text.chars().count() as i32;
    let w = chars * util::scaled(13, dpi) + util::scaled(26, dpi);
    let h = util::scaled(36, dpi);
    let brush = CreateSolidBrush(COLORREF(0x00_2B2B2B));
    let pen = CreatePen(PS_SOLID, 1, t.accent);
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, pen);
    let r = util::scaled(12, dpi);
    let _ = RoundRect(hdc, x, y, x + w, y + h, r, r);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    let font = util::gui_font(hwnd, 11, 600);
    let of = SelectObject(hdc, font);
    raw::SetBkMode(hdc.0 as isize, 1);
    raw::SetTextColor(hdc.0 as isize, t.accent);
    let tw = to_wide(text);
    let mut tr = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    let _ = raw::DrawTextW(
        hdc.0 as isize,
        PCWSTR::from_raw(tw.as_ptr()),
        -1,
        &mut tr,
        0x25, // DT_CENTER | DT_VCENTER | DT_SINGLELINE
    );
    SelectObject(hdc, of);
}

/// 绘制提示文本（阴影双绘，深浅底上都可读；主题字体）。
unsafe fn draw_text(hwnd: HWND, hdc: HDC, text: &str, x: i32, y: i32) {
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    let dpi = GetDpiForSystem().max(96) as i32;
    let font = util::gui_font(hwnd, 12, 600);
    let old: HGDIOBJ = SelectObject(hdc, font);
    let tw = to_wide(text);
    let h = util::scaled(28, dpi);
    let mut r = RECT {
        left: x,
        top: y,
        right: x + util::scaled(1600, dpi),
        bottom: y + h,
    };
    // 阴影
    raw::SetBkMode(hdc.0 as isize, 1);
    raw::SetTextColor(hdc.0 as isize, COLORREF(0));
    let _ = raw::DrawTextW(hdc.0 as isize, PCWSTR::from_raw(tw.as_ptr()), -1, &mut r, 0);
    // 正文
    let mut r2 = RECT {
        left: x - 1,
        top: y - 1,
        right: r.right,
        bottom: r.bottom,
    };
    raw::SetTextColor(hdc.0 as isize, COLORREF(0x00FFFFFF));
    let _ = raw::DrawTextW(hdc.0 as isize, PCWSTR::from_raw(tw.as_ptr()), -1, &mut r2, 0);

    SelectObject(hdc, old);
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
}
