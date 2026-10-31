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
    /// 标注：矩形。
    Rect,
    /// 标注：箭头。
    Arrow,
    /// 标注：步骤序号。
    StepNo,
    /// 标注：椭圆。
    Ellipse,
    /// 标注：线段。
    Line,
    /// 标注：自由画笔。
    Pen,
    /// 标注：马赛克（打码）。
    Mosaic,
    /// 标注：高亮（荧光笔）。
    Highlight,
    /// 标注：文字。
    Text,
    /// 标注：对象橡皮。
    Eraser,
    /// 标注：水印。
    Watermark,
    /// 颜色面板（调色盘）。
    Palette,
    /// 「更多」：三个点（展开子工具条）。
    More,
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
    // square（圆角矩形，等价 lucide `square`）
    const SQUARE: &str = "m5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-14a2 2 0 0 1-2-2v-14a2 2 0 0 1 2-2z";
    // arrow-up-right
    const ARROW_UP_RIGHT: [&str; 2] = ["M7 7h10v10", "M7 17 17 7"];
    // circle + 数字 "1"（步骤序号）
    const STEP_NO: [&str; 3] = ["M12 2a10 10 0 1 0 0 20a10 10 0 1 0 0-20z", "M12 8v8", "M10 10l2-2"];
    // circle（椭圆；与序号外形同为整圆，语义靠提示区分）
    const CIRCLE: &str = "M12 2a10 10 0 1 0 0 20a10 10 0 1 0 0-20z";
    // minus（线段）
    const MINUS: &str = "M5 12h14";
    // pencil（画笔）
    const PENCIL: [&str; 2] = [
        "M21.17 6.81a1 1 0 0 0-3.99-3.99L3.84 16.17a2 2 0 0 0-.5.83l-1.32 4.35a.5.5 0 0 0 .62.62l4.35-1.32a2 2 0 0 0 .83-.5z",
        "m15 5 4 4",
    ];
    // grid-3x3（马赛克/打码）：square + 十字网格线
    const MOSAIC: [&str; 5] = [
        "m5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-14a2 2 0 0 1-2-2v-14a2 2 0 0 1 2-2z",
        "M3 9h18",
        "M3 15h18",
        "M9 3v18",
        "M15 3v18",
    ];
    // highlighter（高亮/荧光笔）
    const HIGHLIGHTER: [&str; 2] = [
        "m9 11-6 6v3h9l3-3",
        "m22 12-4.6 4.6a2 2 0 0 1-2.8 0l-5.2-5.2a2 2 0 0 1 0-2.8L14 4",
    ];
    // type（文字）
    const TYPE: [&str; 3] = ["M4 7V4h16v3", "M9 20h6", "M12 4v16"];
    // eraser（对象橡皮）
    const ERASER: [&str; 3] = [
        "m7 21-4.3-4.3c-1-1-1-2.5 0-3.4l9.6-9.6c1-1 2.5-1 3.4 0l5.6 5.6c1 1 1 2.5 0 3.4L13 21",
        "M22 21H7",
        "m5 11 9 9",
    ];
    // watermark（倾斜文字线条，提示语义）
    const WATERMARK: [&str; 4] = ["M4 6h16", "M8 12h8", "M10 18h4", "M3 3l18 18"];
    // palette（调色盘 + 三个色点）
    const PALETTE: [&str; 4] = [
        "M12 22a10 10 0 1 1 0-20c5.5 0 10 4.5 10 10 0 1.5-1.5 3-3 3h-2.5a2.5 2.5 0 0 0 0 5c0 1.1-.9 2-2 2z",
        "M7 9a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
        "M11 5a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
        "M16 8a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
    ];
    // more-horizontal（三个等距圆点）：三点各为一枚 r=1 的整圆
    const MORE: [&str; 3] = [
        "M5 13a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
        "M12 13a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
        "M19 13a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
    ];

    match icon {
        Icon::Pin => std::slice::from_ref(&PIN),
        Icon::Save => &SAVE,
        Icon::Copy => &COPY,
        Icon::Ocr => &SCAN_TEXT,
        Icon::Close => &X,
        Icon::Rect => std::slice::from_ref(&SQUARE),
        Icon::Arrow => &ARROW_UP_RIGHT,
        Icon::StepNo => &STEP_NO,
        Icon::Ellipse => std::slice::from_ref(&CIRCLE),
        Icon::Line => std::slice::from_ref(&MINUS),
        Icon::Pen => &PENCIL,
        Icon::Mosaic => &MOSAIC,
        Icon::Highlight => &HIGHLIGHTER,
        Icon::Text => &TYPE,
        Icon::Eraser => &ERASER,
        Icon::Watermark => &WATERMARK,
        Icon::Palette => &PALETTE,
        Icon::More => &MORE,
    }
}

/// 惰性初始化 GDI+（进程级一次）。供标注引擎（`annot::render_gdiplus`）复用。
pub(crate) fn gdiplus_ready() -> bool {
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

// 扁平化缓存（线程局部）：图标路径解析结果只算一次。
thread_local! {
    static FLATTEN_CACHE: std::cell::RefCell<
        std::collections::HashMap<&'static str, std::rc::Rc<Vec<Vec<(f32, f32)>>>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// 取路径的扁平化折线（带缓存；Rc 克隆 O(1)）。
fn flattened(d: &'static str) -> std::rc::Rc<Vec<Vec<(f32, f32)>>> {
    FLATTEN_CACHE.with(|c| {
        c.borrow_mut()
            .entry(d)
            .or_insert_with(|| std::rc::Rc::new(svgpath::flatten(d)))
            .clone()
    })
}

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
        // 扁平化结果按路径串缓存（viewBox 坐标与 DPI/尺寸无关，天然可复用）——
        // HoverFx 动画 16ms 节拍期间每个图标每帧都画，逐帧重新解析 SVG 不值
        let subs = flattened(d);
        for sub in subs.iter() {
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
        let (cell, w, h) = (96i32, 96 * 14, 96 * 2);
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
            let all = [
                Icon::Pin,
                Icon::Save,
                Icon::Copy,
                Icon::Ocr,
                Icon::Rect,
                Icon::Arrow,
                Icon::StepNo,
                Icon::Ellipse,
                Icon::Line,
                Icon::Pen,
                Icon::Mosaic,
                Icon::Highlight,
                Icon::Text,
                Icon::Close,
            ];
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
