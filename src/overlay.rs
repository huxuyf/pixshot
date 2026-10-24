//! 区域框选：拖动框选 / Esc 取消（只保留区域截图，不支持选窗口与全屏）。
//!
//! **无遮罩**：框选期间不对屏幕做任何暗化 —— 桌面保持原样可见，只叠加一圈
//! 红色框线。实现方式是逐像素 alpha 的分层窗口（`UpdateLayeredWindow`）：
//! 表面整块为"透明底"（alpha=1），仅在选区四条边上写入不透明纯红像素。
//!
//! 为什么透明底用 alpha=1 而不是 0：分层窗口的鼠标命中按 alpha 判定，
//! alpha=0 的像素会让点击**穿透**到桌面上的其它窗口，拖拽根本发不起来；
//! alpha=1 的视觉贡献是 1/255（不可察觉），但窗口整块仍然接收鼠标。
//!
//! 重要设计（截屏管线）：本模块在进入时截取**一次**虚拟屏幕快照，并把
//! `(快照, 选区)` 一起返回给调用方裁剪成品。调用方不再重新截屏 ——
//! 既省掉一次全屏 BitBlt 与一份全屏缓冲，也保证成品来自同一帧。
//! 由于红框是画在分层窗口上、成品是从快照裁剪的，**红框不会出现在截图里**。
//!
//! 状态存放：`GWLP_USERDATA` 持有 `Box<State>` 指针（与 pin/results 同一模式），
//! 不再使用 `static mut`；`State` 由 `run()` 在消息循环结束后释放，因此
//! 窗口销毁后仍能读回选区结果。

use std::sync::Arc;

use crate::capture::{self, CapturedImage};
use crate::util::{self, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint, GetDC,
    ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT,
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

struct State {
    result: Option<SelectedRegion>,
    dragging: bool,
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
/// 确认时返回「整屏快照 + 选区」；取消返回 None。
pub fn select_region() -> Option<(Arc<CapturedImage>, SelectedRegion)> {
    unsafe { run() }
}

unsafe fn run() -> Option<(Arc<CapturedImage>, SelectedRegion)> {
    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    if vw <= 0 || vh <= 0 {
        return None;
    }

    // 预截取整个虚拟屏幕（唯一一次全屏捕获），供最终裁剪
    let shot = Arc::new(capture::capture_region(vx, vy, vw, vh).ok()?);

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
        result: None,
        dragging: false,
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

    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    {
        let s = &*state;
        clear_rect(s.surface_bits, vw, vh, (0, 0, vw, vh));
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

    // 窗口此时已销毁；state 指针仍有效（由本函数持有）
    let s = Box::from_raw(state);
    let result = s.result;
    cleanup(&s);
    result.map(|r| (shot, r))
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
                if s.dragging {
                    s.cur = point_from_lparam(lp, s.vx, s.vy);
                    repaint_frame(hwnd, s);
                }
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            finish_selection(hwnd, lp);
            LRESULT(0)
        }

        WM_KEYDOWN => {
            if wp.0 == VK_ESCAPE {
                let _ = DestroyWindow(hwnd);
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

/// 鼠标松开：拖动够大则取选区；过小（含单击）则擦掉框线，留在框选状态。
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

    // 单击 / 过小的拖动：不产生选区，擦掉已画的框线，继续等待用户框选
    if let Some(last) = s.last.take() {
        clear_rect(s.surface_bits, s.vw, s.vh, last);
        commit(hwnd, s, last);
    }
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

    // 先清上一帧、再画当前帧：顺序保证两帧重叠时结果正确
    if let Some(last) = s.last {
        clear_rect(s.surface_bits, s.vw, s.vh, last);
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
