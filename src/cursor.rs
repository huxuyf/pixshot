//! 鼠标指针合成：把当前系统光标画进已捕获的屏幕像素里（"截图含指针"）。
//!
//! 背景：`BitBlt` **不包含光标**——Windows 光标由硬件/GDI 叠加层单独绘制，
//! 抓屏拿到的永远是无光标画面。因此要"含指针"必须手动取值再画上去。
//!
//! 做法：把光标用 `DrawIconEx(DI_NORMAL)` 直接画到**已经装着屏幕内容的 DC** 上
//! （`capture.rs` 里 `BitBlt` 之后、读出像素之前）。这条路径对两类光标都正确：
//! - 带 alpha 的现代光标：GDI 走 AlphaBlend，与不透明底图混合出正确 RGB；
//! - 老的 1 位掩码光标：GDI 走 AND/XOR，同样得到正确 RGB。
//!
//! 因为底图本身不透明，混合结果仍不透明——下游「alpha 恒为 255」的约定不受影响
//! （`capture.rs` 在读出后统一把 alpha 置满）。
//!
//! **多 DPI**：系统光标位图按 96 DPI 取形，而 Windows 显示时会按所在显示器的
//! 有效 DPI 把光标放大。这里取光标所在显示器的 DPI，对绘制尺寸和热点偏移都做
//! 同样的缩放，保证合成结果与用户屏幕上看到的指针一致（96 DPI 下缩放为 1，无副作用）。

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetObjectW, MonitorFromPoint, BITMAP, HBRUSH, HDC, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, CURSOR_SHOWING, CURSORINFO, DI_NORMAL,
    ICONINFO, SM_CXCURSOR, SM_CYCURSOR,
};

/// 计算光标绘制矩形 `(x, y, w, h)`（图像局部坐标）。
///
/// 缩放因子 `s = dpi / 96`：位图尺寸放大 `s` 倍，热点偏移也放大 `s` 倍。
/// `GetCursorInfo` 给的是**热点**所在屏幕坐标，`DrawIconEx` 画的是光标**左上角**，
/// 故左上角 = 热点屏幕坐标 − 缩放后的热点偏移 − 图像原点。纯函数，便于单测。
fn scaled_draw(
    cursor_screen: POINT,
    origin: (i32, i32),
    hotspot: (u32, u32),
    bitmap: (i32, i32),
    dpi: u32,
) -> (i32, i32, i32, i32) {
    let s = dpi as f32 / 96.0;
    let w = (bitmap.0 as f32 * s).round() as i32;
    let h = (bitmap.1 as f32 * s).round() as i32;
    let hs_x = (hotspot.0 as f32 * s).round() as i32;
    let hs_y = (hotspot.1 as f32 * s).round() as i32;
    (
        cursor_screen.x - hs_x - origin.0,
        cursor_screen.y - hs_y - origin.1,
        w,
        h,
    )
}

/// 光标位图尺寸（96 DPI 基准）。
///
/// 单色光标只有 mask，其位图高度是实际高度的两倍（上半 AND 掩码 + 下半 XOR 掩码），
/// 故无 color 位图时高度取半。两者都取不到时回退系统光标尺寸。
unsafe fn cursor_bitmap_size(ii: &ICONINFO) -> (i32, i32) {
    let hbm = if !ii.hbmColor.is_invalid() {
        ii.hbmColor
    } else {
        ii.hbmMask
    };
    if !hbm.is_invalid() {
        let mut bm = BITMAP::default();
        let n = GetObjectW(
            hbm,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut core::ffi::c_void),
        );
        if n != 0 && bm.bmWidth > 0 && bm.bmHeight > 0 {
            let h = if ii.hbmColor.is_invalid() {
                bm.bmHeight / 2
            } else {
                bm.bmHeight
            };
            return (bm.bmWidth, h.max(1));
        }
    }
    let (cw, ch) = (
        GetSystemMetrics(SM_CXCURSOR),
        GetSystemMetrics(SM_CYCURSOR),
    );
    (cw.max(1), ch.max(1))
}

/// 光标所在显示器的有效 DPI（取值失败回退 96，即不做缩放）。
unsafe fn monitor_dpi(pt: POINT) -> u32 {
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let (mut dx, mut dy) = (0u32, 0u32);
    if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 {
        dx
    } else {
        96
    }
}

/// 把当前光标画到 `mem_dc` 上。
///
/// 前置条件：`mem_dc` 已选中一张装着区域屏幕像素的位图，(0,0) 对应虚拟屏幕的
/// `(origin_x, origin_y)`。光标超出区域时由位图边界自动裁剪。
///
/// 光标隐藏（如全屏游戏）或取值失败时静默跳过——不合成任何东西，不影响截图本身。
pub unsafe fn draw_cursor_on(mem_dc: HDC, origin_x: i32, origin_y: i32) {
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if GetCursorInfo(&mut ci).is_err() {
        return;
    }
    if ci.flags.0 & CURSOR_SHOWING.0 == 0 || ci.hCursor.is_invalid() {
        crate::diag::log("[cursor] 光标隐藏或句柄无效 -> 跳过合成");
        return;
    }

    // 热点坐标只有 GetIconInfo 能拿到（GetCursorInfo 不给）
    let mut ii = ICONINFO::default();
    if GetIconInfo(ci.hCursor, &mut ii).is_err() {
        crate::diag::log("[cursor] GetIconInfo 失败 -> 跳过合成");
        return;
    }

    let bitmap = cursor_bitmap_size(&ii);
    let dpi = monitor_dpi(ci.ptScreenPos);
    let (x, y, w, h) = scaled_draw(
        ci.ptScreenPos,
        (origin_x, origin_y),
        (ii.xHotspot, ii.yHotspot),
        bitmap,
        dpi,
    );
    // 传空画刷表示不做闪烁消除；显式传入缩放后的 w/h（DC 无 DPI 关联，不会二次缩放）
    let r = DrawIconEx(mem_dc, x, y, ci.hCursor, w, h, 0, HBRUSH::default(), DI_NORMAL);
    crate::diag::log(&format!(
        "[cursor] 合成光标 屏幕({},{}) 热点({},{}) 位图{}x{} dpi={dpi} -> 图像({x},{y}) {w}x{h} {:?}",
        ci.ptScreenPos.x, ci.ptScreenPos.y, ii.xHotspot, ii.yHotspot, bitmap.0, bitmap.1, r
    ));

    // GetIconInfo 会创建两张位图，由调用方负责释放（hCursor 本身是共享的，勿销毁）
    if !ii.hbmColor.is_invalid() {
        let _ = DeleteObject(ii.hbmColor);
    }
    if !ii.hbmMask.is_invalid() {
        let _ = DeleteObject(ii.hbmMask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_draw_is_identity_at_96_dpi() {
        // 96 DPI：缩放为 1，尺寸/热点原样
        let p = POINT { x: 500, y: 400 };
        assert_eq!(scaled_draw(p, (100, 100), (2, 3), (32, 32), 96), (398, 297, 32, 32));
    }

    #[test]
    fn scaled_draw_scales_size_and_hotspot() {
        // 144 DPI（150%）：尺寸 32->48，热点 (2,4)->(3,6)
        let p = POINT { x: 500, y: 400 };
        assert_eq!(scaled_draw(p, (100, 100), (2, 4), (32, 32), 144), (397, 294, 48, 48));
    }

    #[test]
    fn scaled_draw_zero_origin_and_hotspot() {
        let p = POINT { x: 42, y: 7 };
        assert_eq!(scaled_draw(p, (0, 0), (0, 0), (32, 32), 96), (42, 7, 32, 32));
    }
}
