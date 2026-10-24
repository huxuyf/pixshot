//! 工具条图标：GDI+ 矢量自绘（线性描边图标，零图片资源）。
//!
//! 为什么用 GDI+ 而不是纯 GDI：GDI 画线/圆弧**不做抗锯齿**，图钉的 45° 斜线、
//! ✕ 的对角线以及圆角矩形都会出现明显锯齿。GDI+ 的 `SmoothingModeAntiAlias`
//! 配圆头圆角笔可得到平滑边缘，且可直接绘制在按钮 DC 上（无需离屏合成）。
//!
//! GDI+ 用 `GdiplusStartup` 惰性初始化一次（进程级，随进程退出回收；初始化失败时
//! 图标不绘制，不影响其它功能）。
//!
//! 图标在「以按钮中心为心的正方形」内按比例定义（局部坐标 [-0.5, 0.5]），
//! 笔宽与尺寸随 DPI 缩放。图标语义：
//! [`Icon::Pin`] 贴图 / [`Icon::Save`] 保存图像 / [`Icon::Copy`] 复制并关闭 /
//! [`Icon::Ocr`] 复制识别到的文字 / [`Icon::Close`] 关闭。

use std::sync::OnceLock;

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::GdiPlus::{
    DashCapRound, FillModeWinding, GdipAddPathArcI, GdipAddPathLineI, GdipAddPathRectangleI,
    GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1, GdipCreateSolidFill,
    GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDrawLinesI,
    GdipDrawPath, GdipFillPath, GdipSetPenLineCap197819, GdipSetPenLineJoin, GdipSetPixelOffsetMode,
    GdipSetSmoothingMode, GdiplusStartup, GdiplusStartupInput, GpBrush,
    GpGraphics, GpPath, GpPen, GpSolidFill, LineCapRound, LineJoinRound, PixelOffsetModeHighQuality,
    Point, SmoothingModeAntiAlias, UnitPixel,
};

/// 工具条图标种类。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Pin,
    Save,
    Copy,
    Ocr,
    Close,
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

/// 构建圆角矩形路径（r<=0 时为直角矩形）。调用方负责 `GdipDeletePath`。
unsafe fn rrect_path(x: i32, y: i32, w: i32, h: i32, r: i32) -> *mut GpPath {
    let mut p: *mut GpPath = std::ptr::null_mut();
    if GdipCreatePath(FillModeWinding, &mut p).0 != 0 {
        return std::ptr::null_mut();
    }
    if r <= 0 || w <= 2 * r || h <= 2 * r {
        let _ = GdipAddPathRectangleI(p, x, y, w, h);
        return p;
    }
    // GDI+ 角度：0° = 3 点钟方向，顺时针为正；四角圆弧 + 四边直线
    let d = r * 2;
    let _ = GdipAddPathArcI(p, x, y, d, d, 180.0, 90.0); // 左上
    let _ = GdipAddPathLineI(p, x + r, y, x + w - r, y);
    let _ = GdipAddPathArcI(p, x + w - d, y, d, d, 270.0, 90.0); // 右上
    let _ = GdipAddPathLineI(p, x + w, y + r, x + w, y + h - r);
    let _ = GdipAddPathArcI(p, x + w - d, y + h - d, d, d, 0.0, 90.0); // 右下
    let _ = GdipAddPathLineI(p, x + w - r, y + h, x + r, y + h);
    let _ = GdipAddPathArcI(p, x, y + h - d, d, d, 90.0, 90.0); // 左下
    let _ = GdipAddPathLineI(p, x, y + h - r, x, y + r);
    let _ = GdipClosePathFigure(p);
    p
}

/// 在按钮矩形内居中绘制指定图标。
/// `ink` 描边色；`bg` 按钮当前填充色（供 [`Icon::Copy`] 遮住两页重叠处）。
pub unsafe fn draw(hdc: HDC, rc: RECT, icon: Icon, ink: COLORREF, bg: COLORREF) {
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
    // 图标视觉边长：取按钮短边的 ~60%（紧凑按钮下仍清晰），四周留一点呼吸空间
    let s = (w.min(h) * 0.60).max(10.0);
    let ox = rc.left as f32 + (w - s) * 0.5;
    let oy = rc.top as f32 + (h - s) * 0.5;
    // 笔宽不取整：小数宽度配合抗锯齿更平滑
    let stroke = (s * 0.095).max(1.0);

    // 局部 [-0.5,0.5] → 设备像素
    let map = |lx: f32, ly: f32| Point {
        X: (ox + (lx + 0.5) * s).round() as i32,
        Y: (oy + (ly + 0.5) * s).round() as i32,
    };

    let pen = make_pen(stroke, ink);
    if pen.is_null() {
        let _ = GdipDeleteGraphics(g);
        return;
    }

    // 折线（各段独立，端点圆头）
    let path = |pts: &[(f32, f32)]| {
        let v: Vec<Point> = pts.iter().map(|&(x, y)| map(x, y)).collect();
        let _ = GdipDrawLinesI(g, pen, v.as_ptr(), v.len() as i32);
    };
    // 绕中心旋转（屏幕坐标系 y 向下，正角为顺时针）
    let rot = |deg: f32, x: f32, y: f32| -> (f32, f32) {
        let a = deg.to_radians();
        (x * a.cos() - y * a.sin(), x * a.sin() + y * a.cos())
    };

    match icon {
        // 图钉：钉帽（上宽下收）+ 竖直针，整体顺时针旋转 45° 呈斜插姿态
        Icon::Pin => {
            let k = 0.84;
            let r = |x: f32, y: f32| {
                let (rx, ry) = rot(45.0, x * k, y * k);
                (rx, ry)
            };
            path(&[
                r(-0.30, -0.46),
                r(0.30, -0.46),
                r(0.30, -0.22),
                r(0.11, -0.08),
                r(-0.11, -0.08),
                r(-0.30, -0.22),
                r(-0.30, -0.46),
            ]);
            path(&[r(0.0, -0.08), r(0.0, 0.48)]);
        }
        // 保存：向下的箭头 + 底部承接托盘
        Icon::Save => {
            path(&[(0.0, -0.42), (0.0, 0.10)]);
            path(&[(-0.21, -0.12), (0.0, 0.10), (0.21, -0.12)]);
            path(&[(-0.32, 0.06), (-0.32, 0.42), (0.32, 0.42), (0.32, 0.06)]);
        }
        // 复制：前后两张圆角卡片（前页用底色实心填充，遮住后页被压住的部分）
        Icon::Copy => {
            let radius = (s * 0.16).round() as i32;
            let bl = map(-0.08, -0.48);
            let br = map(0.48, 0.08);
            let back = rrect_path(bl.X, bl.Y, br.X - bl.X, br.Y - bl.Y, radius);
            if !back.is_null() {
                let _ = GdipDrawPath(g, pen, back);
                let _ = GdipDeletePath(back);
            }
            let fl = map(-0.48, -0.08);
            let fr = map(0.08, 0.48);
            let front = rrect_path(fl.X, fl.Y, fr.X - fl.X, fr.Y - fl.Y, radius);
            if !front.is_null() {
                let mut brush: *mut GpSolidFill = std::ptr::null_mut();
                if GdipCreateSolidFill(argb(bg), &mut brush).0 == 0 {
                    let _ = GdipFillPath(g, brush as *mut GpBrush, front);
                    let _ = GdipDeleteBrush(brush as *mut GpBrush);
                }
                let _ = GdipDrawPath(g, pen, front);
                let _ = GdipDeletePath(front);
            }
        }
        // OCR：四角取景括号 + 中央字母 A
        Icon::Ocr => {
            let e = 0.44;
            let a = 0.18;
            path(&[(-e, -e + a), (-e, -e), (-e + a, -e)]);
            path(&[(e - a, -e), (e, -e), (e, -e + a)]);
            path(&[(-e, e - a), (-e, e), (-e + a, e)]);
            path(&[(e - a, e), (e, e), (e, e - a)]);
            path(&[(-0.17, 0.22), (0.0, -0.24), (0.17, 0.22)]);
            path(&[(-0.10, 0.08), (0.10, 0.08)]);
        }
        // 关闭：✕
        Icon::Close => {
            path(&[(-0.34, -0.34), (0.34, 0.34)]);
            path(&[(0.34, -0.34), (-0.34, 0.34)]);
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
                    draw(mem, rc, *icon, ink, bg);
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
