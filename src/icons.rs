//! 工具条图标：**Lucide 图标集**的 GDI+ 矢量自绘（线性描边，零图片资源）。
//!
//! 图标形状不再手绘，而是直接采用 [Lucide](https://lucide.dev) 官方图标的
//! **24×24 viewBox 路径数据**（`stroke-width: 2`、圆头圆角笔），由 [`crate::svgpath`]
//! 展平成折线后经 GDI+ 抗锯齿描边绘制 —— 单一来源、易与设计稿对齐，仍是零图片资源。
//!
//! 为什么用 GDI+ 而不是纯 GDI：GDI 画线/圆弧**不做抗锯齿**，斜线与圆角会出现明显锯齿。
//! GDI+ 的 `SmoothingModeAntiAlias` 配圆头圆角笔可得到平滑边缘，且可直接绘制在按钮 DC 上。
//! GDI+ 用 `GdiplusStartup` 惰性初始化一次（进程级，随进程退出回收；初始化失败时
//! 图标不绘制，不影响其它功能）。
//!
//! 图标语义（括号内为对应的 Lucide 图标名）：
//! [`Icon::Pin`] 贴图（`pin`）/ [`Icon::Save`] 保存图像（`download`）/
//! [`Icon::Copy`] 复制并关闭（`copy`）/ [`Icon::Ocr`] 复制识别到的文字（`scan-text`）/
//! [`Icon::Close`] 关闭（`x`）。

use std::sync::OnceLock;

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::GdiPlus::{
    DashCapRound, GdipCreateFromHDC, GdipCreatePen1, GdipDeleteGraphics, GdipDeletePen,
    GdipDrawLines, GdipSetPenLineCap197819, GdipSetPenLineJoin, GdipSetPixelOffsetMode,
    GdipSetSmoothingMode, GdiplusStartup, GdiplusStartupInput, GpGraphics, GpPen, LineCapRound,
    LineJoinRound, PixelOffsetModeHighQuality, PointF, SmoothingModeAntiAlias, UnitPixel,
};

use crate::svgpath;

/// 工具条图标种类。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Pin,
    Save,
    Copy,
    Ocr,
    Close,
}

/// 图标对应的 Lucide 路径数据（24×24 viewBox，`stroke-width: 2`）。
///
/// 每条字符串是 Lucide SVG 中一个 `<path>` 的 `d`；矩形（如 `copy` 的前页）已按其
/// `x/y/width/height/rx` 等价转换为圆角路径，以便统一走 [`svgpath::flatten`]。
fn lucide_paths(icon: Icon) -> &'static [&'static str] {
    const PIN: &str = "M12 17v5 M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z";
    const SAVE: [&str; 3] = [
        "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4",
        "M7 10l5 5 5-5",
        "M12 15V3",
    ];
    // copy：rect(8,8,14,14,r=2) 转为等价圆角路径 + 后页轮廓
    const COPY: [&str; 2] = [
        "M8 10a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H10a2 2 0 0 1-2-2z",
        "M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2",
    ];
    const SCAN_TEXT: [&str; 7] = [
        "M3 7V5a2 2 0 0 1 2-2h2",
        "M17 3h2a2 2 0 0 1 2 2v2",
        "M21 17v2a2 2 0 0 1-2 2h-2",
        "M7 21H5a2 2 0 0 1-2-2v-2",
        "M7 8h8",
        "M7 12h10",
        "M7 16h6",
    ];
    const X: [&str; 2] = ["M18 6 6 18", "m6 6 12 12"];

    match icon {
        Icon::Pin => std::slice::from_ref(&PIN),
        Icon::Save => &SAVE,
        Icon::Copy => &COPY,
        Icon::Ocr => &SCAN_TEXT,
        Icon::Close => &X,
    }
}

/// 惰性初始化 GDI+（进程级一次）。
fn gdiplus_ready() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| unsafe {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token: usize = 0;
        let status = GdiplusStartup(&mut token, &input, std::ptr::null_mut());
        if status.0 != 0 {
            crate::diag::log(&format!("[icons] GdiplusStartup 失败 status={}", status.0));
            return false;
        }
        // 进程退出时由系统回收；此处只初始化一次，不做 GdiplusShutdown
        true
    })
}

/// COLORREF(0x00BBGGRR) → GDI+ ARGB(0xAARRGGBB)。
fn argb(c: COLORREF) -> u32 {
    let r = c.0 & 0xFF;
    let g = (c.0 >> 8) & 0xFF;
    let b = (c.0 >> 16) & 0xFF;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// 创建圆头圆角抗锯齿笔。
unsafe fn make_pen(width: f32, ink: COLORREF) -> *mut GpPen {
    let mut p: *mut GpPen = std::ptr::null_mut();
    if GdipCreatePen1(argb(ink), width.max(1.0), UnitPixel, &mut p).0 != 0 {
        return std::ptr::null_mut();
    }
    let _ = GdipSetPenLineCap197819(p, LineCapRound, LineCapRound, DashCapRound);
    let _ = GdipSetPenLineJoin(p, LineJoinRound);
    p
}

/// Lucide 图标坐标盒（24×24 viewBox）。
const BOX: f32 = 24.0;
/// Lucide 描边宽度（viewBox 单位），对应 `stroke-width: 2`。
const LUCIDE_STROKE: f32 = 2.0;
/// 图标占按钮短边的比例：Lucide 内容约占 viewBox 的 20/24，乘 0.72 后视觉边长 ≈ 0.60。
const ICON_RATIO: f32 = 0.72;

/// 在按钮矩形内居中绘制指定图标（Lucide 路径，GDI+ 抗锯齿描边）。
/// `ink` 描边色。
pub unsafe fn draw(hdc: HDC, rc: RECT, icon: Icon, ink: COLORREF) {
    if !gdiplus_ready() {
        return;
    }
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 != 0 || g.is_null() {
        return;
    }
    let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let _ = GdipSetPixelOffsetMode(g, PixelOffsetModeHighQuality);

    let w = (rc.right - rc.left) as f32;
    let h = (rc.bottom - rc.top) as f32;
    // 图标盒边长（含 Lucide 自带的内边距），四周留出呼吸空间
    let s = (w.min(h) * ICON_RATIO).max(10.0);
    let ox = rc.left as f32 + (w - s) * 0.5;
    let oy = rc.top as f32 + (h - s) * 0.5;
    // 笔宽按 viewBox 比例换算；不取整：小数宽度配合抗锯齿更平滑
    let stroke = (s * (LUCIDE_STROKE / BOX)).max(1.0);

    // viewBox(0..24) → 设备像素（保留小数，交给抗锯齿做亚像素平滑）
    let map = |lx: f32, ly: f32| PointF {
        X: ox + lx / BOX * s,
        Y: oy + ly / BOX * s,
    };

    let pen = make_pen(stroke, ink);
    if pen.is_null() {
        let _ = GdipDeleteGraphics(g);
        return;
    }

    for d in lucide_paths(icon) {
        for sub in svgpath::flatten(d) {
            if sub.len() < 2 {
                continue;
            }
            let v: Vec<PointF> = sub.iter().map(|&(x, y)| map(x, y)).collect();
            let _ = GdipDrawLines(g, pen, v.as_ptr(), v.len() as i32);
        }
    }

    let _ = GdipDeletePen(pen);
    let _ = GdipDeleteGraphics(g);
}

#[cfg(test)]
mod preview {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, FillRect, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };

    #[test]
    #[ignore = "仅在需要人工核对图标外观（抗锯齿）时运行：--ignored render_preview"]
    fn render_preview() {
        // 与真实工具条一致：32 逻辑像素方形按钮（此处按 3x 渲染便于观察锯齿）
        let (cell, w, h) = (96i32, 96 * 5, 96 * 2);
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
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

            // 与工具条实际皮肤一致：白底 + 纯黑图标（第一行默认态，第二行悬停态）
            let card = COLORREF(0x00FF_FFFF);
            let hover = COLORREF(0x00EC_ECEC);
            let ink = COLORREF(0x0000_0000);
            let all = [Icon::Pin, Icon::Save, Icon::Copy, Icon::Ocr, Icon::Close];
            for (i, icon) in all.iter().enumerate() {
                for row in 0..2 {
                    let bg = if row == 0 { card } else { hover };
                    let rc = RECT {
                        left: i as i32 * cell,
                        top: row * cell,
                        right: (i as i32 + 1) * cell,
                        bottom: (row + 1) * cell,
                    };
                    let b = windows::Win32::Graphics::Gdi::CreateSolidBrush(bg);
                    FillRect(mem, &rc, b);
                    let _ = DeleteObject(b);
                    draw(mem, rc, *icon, ink);
                }
            }

            let n = (w * h) as usize;
            let mut px = std::slice::from_raw_parts(bits as *const u8, n * 4).to_vec();
            for p in px.chunks_exact_mut(4) {
                p[3] = 255; // DIB 未写 alpha，置满避免 PNG 全透明
            }
            SelectObject(mem, old);
            let _ = DeleteObject(hbmp);
            let _ = DeleteDC(mem);

            let _ = crate::imageio::save_png("target/icon_preview.png", &px, w, h);
        }
    }
}
