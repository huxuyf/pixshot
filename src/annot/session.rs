//! 标注引擎：会话的 Win32 集成（DIB 表面、统一渲染入口、导出合成，见设计文档 4.1 / 6）。
//!
//! - [`DibSurface`]：选区大小的 32bpp 自上而下 DIB，承载不可变的 `annot_base` 底图；
//! - [`render_into`]：把「已提交标注 + 预览 + 选中态」渲染到给定 DC（屏幕与导出共用）；
//! - [`composite`]：把标注合成进成品图（保存/复制/贴图接入点调用）。
//!
//! **CPU 像素算子**（马赛克/高亮，参照 SnowShot `edit_render.go` 的纯 CPU 渲染）：
//! GDI+ 无法做块平均与乘法混合，这两类标注在渲染时从 pristine 底图（[`BaseSource`]）
//! 取样、直接写目标表面像素——与 GDI+ 矢量绘制按文档 z 序交错（`GdiFlush` 分隔），
//! 预览与导出仍共用同一条渲染路径。

use std::ptr::null_mut;
use std::sync::Arc;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};

use crate::capture::CapturedImage;

use super::controller::AnnotSession;
use super::doc::AnnotDoc;
use super::geometry::{Pt, Rect};
use super::render_gdiplus;
use super::shape::Shape;
use super::style::{HighlightMode, MosaicMode, Rgba};
use windows::Win32::Graphics::GdiPlus::GpGraphics;

/// 32bpp 自上而下 DIB 表面（DC + 位图 + 像素基址），`Drop` 时释放 GDI 资源。
pub struct DibSurface {
    pub dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    pub w: i32,
    pub h: i32,
}

impl DibSurface {
    /// 新建 `w×h` 的 32bpp DIB 表面。
    pub unsafe fn new(w: i32, h: i32) -> Option<Self> {
        if w <= 0 || h <= 0 {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let mut bits: *mut core::ffi::c_void = null_mut();
        let bmp = match CreateDIBSection(
            dc,
            &top_down_32bpp(w, h),
            DIB_RGB_COLORS,
            &mut bits,
            HANDLE::default(),
            0,
        ) {
            Ok(b) if !bits.is_null() => b,
            _ => {
                let _ = DeleteDC(dc);
                return None;
            }
        };
        let old = SelectObject(dc, bmp);
        Some(Self {
            dc,
            bmp,
            old,
            bits: bits as *mut u32,
            w,
            h,
        })
    }

    pub fn bits(&self) -> *mut u32 {
        self.bits
    }

    /// 从源表面复制一块区域到本表面左上角（建立 `annot_base` 底图）。
    pub unsafe fn blit_from(&self, src: HDC, sx: i32, sy: i32, w: i32, h: i32) {
        let _ = BitBlt(self.dc, 0, 0, w, h, src, sx, sy, SRCCOPY);
    }

    /// 把 BGRA 行数据整块拷入表面。
    pub unsafe fn copy_rows(&self, data: &[u8]) {
        let n = (self.w as usize) * (self.h as usize);
        if data.len() >= n * 4 {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.bits as *mut u8, n * 4);
        }
    }

    /// 读回表面像素（BGRA）。
    pub unsafe fn read_rows(&self) -> Vec<u8> {
        let n = (self.w as usize) * (self.h as usize) * 4;
        let mut v = vec![0u8; n];
        std::ptr::copy_nonoverlapping(self.bits as *const u8, v.as_mut_ptr(), n);
        v
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            // 先恢复原对象再删位图（位图被选入 DC 时 DeleteObject 会失败并泄漏）
            let _ = SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
        }
    }
}

/// 选区大小的自上而下 32bpp BITMAPINFO。
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

/// 把脏区内所有像素的第 4 字节强制写 255（alpha 防御，见 4.5）。
///
/// 冻结表面是分层窗口的内容来源，alpha < 255 的像素会显示为半透明并穿透点击；
/// 底图 alpha 本就恒 255，这里只修补 GDI+ 写入的区域（脏区小，成本可忽略）。
pub unsafe fn alpha_fix(bits: *mut u32, stride_w: i32, r: Rect) {
    let x0 = r.x.max(0);
    let y0 = r.y.max(0);
    let x1 = r.right();
    let y1 = r.bottom();
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for row in y0..y1 {
        let base = row as usize * stride_w as usize;
        for col in x0..x1 {
            let p = bits.add(base + col as usize);
            *p = (*p & 0x00FF_FFFF) | 0xFF00_0000;
        }
    }
}

/// CPU 算子的底图取样来源（二者同为 BGRA、自上而下、行距 = width×4）。
pub enum BaseSource<'a> {
    /// 预览路径：选区大小的底图表面（局部坐标 (0,0) 起）。
    Surface(&'a DibSurface),
    /// 导出路径：成品图像（局部坐标 = 图像坐标）。
    Image(&'a CapturedImage),
}

impl BaseSource<'_> {
    /// 取局部坐标 (x,y) 处的 BGRA 像素。越界返回不透明黑（防御，不 panic）。
    unsafe fn pixel(&self, x: i32, y: i32) -> [u8; 4] {
        match self {
            BaseSource::Surface(s) => {
                if x < 0 || y < 0 || x >= s.w || y >= s.h {
                    return [0, 0, 0, 0xFF];
                }
                let p = s.bits().add(y as usize * s.w as usize + x as usize);
                (*p).to_le_bytes()
            }
            BaseSource::Image(img) => {
                if x < 0 || y < 0 || x >= img.width || y >= img.height {
                    return [0, 0, 0, 0xFF];
                }
                let off = (y as usize * img.width as usize + x as usize) * 4;
                if off + 4 > img.data.len() {
                    return [0, 0, 0, 0xFF];
                }
                [img.data[off], img.data[off + 1], img.data[off + 2], img.data[off + 3]]
            }
        }
    }
}

/// 马赛克块大小：`max(8, min(宽,高) / 80)`（参照 SnowShot `pixelBlock`，
/// 与图像尺寸成比例；预览用选区尺寸、导出用成品尺寸，二者相等，结果一致）。
pub fn mosaic_block(w: i32, h: i32) -> i32 {
    (w.min(h) / 80).max(8)
}

/// 模糊半径：`max(2, min(宽,高) / 200)`（与块大小同源；比像素化更细的尺度）。
pub fn mosaic_radius(w: i32, h: i32) -> i32 {
    (w.min(h) / 200).max(2)
}

/// 两趟可分盒模糊采样半径（相对选区；钳到 1..=24，避免过慢）。
fn blur_radius(w: i32, h: i32) -> i32 {
    mosaic_radius(w, h).clamp(1, 24)
}

/// 马赛克：bbox 内按 `block` 块做平均（取样自 pristine 底图，参照 SnowShot
/// `drawPixelate`）。写入目标表面 `origin` 偏移处；越界自动裁剪。
///
/// # Safety
/// `dst_bits` 须指向 `dst_w×dst_h` 的 32bpp 表面（u32 寻址）。
pub unsafe fn apply_mosaic(
    dst_bits: *mut u32,
    dst_w: i32,
    dst_h: i32,
    origin: Pt,
    base: &BaseSource,
    bbox: Rect,
    block: i32,
) {
    if block < 1 || bbox.w <= 0 || bbox.h <= 0 {
        return;
    }
    let (rx0, ry0, rx1, ry1) = clip_surface(origin, bbox, dst_w, dst_h);
    if rx1 <= rx0 || ry1 <= ry0 {
        return;
    }
    // 按块遍历（块边界对齐 bbox 起点，末块自动裁剪）
    let mut by = bbox.y;
    while by < bbox.bottom() {
        let mut bx = bbox.x;
        while bx < bbox.right() {
            let lx0 = bx.max(rx0 - origin.x).max(bbox.x);
            let ly0 = by.max(ry0 - origin.y).max(bbox.y);
            let lx1 = (bx + block).min(bbox.right()).min(rx1 - origin.x);
            let ly1 = (by + block).min(bbox.bottom()).min(ry1 - origin.y);
            if lx1 > lx0 && ly1 > ly0 {
                let mut sb = 0u32;
                let mut sg = 0u32;
                let mut sr = 0u32;
                let mut n = 0u32;
                for y in ly0..ly1 {
                    for x in lx0..lx1 {
                        let [b, g, r, _] = base.pixel(x, y);
                        sb += b as u32;
                        sg += g as u32;
                        sr += r as u32;
                        n += 1;
                    }
                }
                if n > 0 {
                    // 像素按小端 u32 存取：低字节 = 蓝（BGRA）
                    let avg = 0xFF00_0000u32
                        | ((sr / n) << 16)
                        | ((sg / n) << 8)
                        | (sb / n);
                    write_px(dst_bits, dst_w, origin, lx0, ly0, lx1, ly1, avg);
                }
            }
            bx += block;
        }
        by += block;
    }
}

/// 高亮：bbox 内逐像素乘法混合（取样自 pristine 底图，参照 SnowShot
/// `drawHighlight`：每通道 `× (255-(255-c)·keep)/255`，底层文字保持可读）。
///
/// # Safety
/// `dst_bits` 须指向 `dst_w×dst_h` 的 32bpp 表面（u32 寻址）。
pub unsafe fn apply_highlight(
    dst_bits: *mut u32,
    dst_w: i32,
    dst_h: i32,
    origin: Pt,
    base: &BaseSource,
    bbox: Rect,
    color: Rgba,
    keep: f32,
) {
    let m = [
        (255.0 - (255.0 - color.b as f32) * keep) / 255.0, // 蓝
        (255.0 - (255.0 - color.g as f32) * keep) / 255.0, // 绿
        (255.0 - (255.0 - color.r as f32) * keep) / 255.0, // 红
    ];
    let (rx0, ry0, rx1, ry1) = clip_surface(origin, bbox, dst_w, dst_h);
    if rx1 <= rx0 || ry1 <= ry0 {
        return;
    }
    for y in ry0..ry1 {
        let row = y as usize * dst_w as usize;
        for x in rx0..rx1 {
            let [b, g, r, _] = base.pixel(x - origin.x, y - origin.y);
            let nb = ((b as f32) * m[0]).round().min(255.0) as u32;
            let ng = ((g as f32) * m[1]).round().min(255.0) as u32;
            let nr = ((r as f32) * m[2]).round().min(255.0) as u32;
            *dst_bits.add(row + x as usize) = 0xFF00_0000 | (nr << 16) | (ng << 8) | nb;
        }
    }
}

/// 模糊：bbox 内两趟可分盒模糊（取样自 pristine 底图，幂等——与部分重绘兼容）。
///
/// # Safety
/// `dst_bits` 须指向 `dst_w×dst_h` 的 32bpp 表面（u32 寻址）。
pub unsafe fn apply_blur(
    dst_bits: *mut u32,
    dst_w: i32,
    dst_h: i32,
    origin: Pt,
    base: &BaseSource,
    bbox: Rect,
    radius: i32,
) {
    if radius < 1 || bbox.w <= 0 || bbox.h <= 0 {
        return;
    }
    let (rx0, ry0, rx1, ry1) = clip_surface(origin, bbox, dst_w, dst_h);
    if rx1 <= rx0 || ry1 <= ry0 {
        return;
    }
    let r = radius;
    let w = (rx1 - rx0) as usize;
    let h = (ry1 - ry0) as usize;
    // 局部坐标原点（surface 坐标 → 局部：减 origin）
    let lx0 = rx0 - origin.x;
    let ly0 = ry0 - origin.y;

    // 水平趟：tmp[j*w + i] = 行 ly 上 [lx-r, lx+r] 的平均（越界按 bbox 边界延伸）
    let mut tmp = vec![[0u8; 3]; w * h];
    let cw = w as i32;
    for j in 0..h {
        let ly = ly0 + j as i32;
        let mut sb = 0i64;
        let mut sg = 0i64;
        let mut sr = 0i64;
        let n = (2 * r + 1) as i64;
        for k in -r..=r {
            let [b, g, rr, _] = base.pixel(lx0 + k.clamp(0, cw - 1), ly);
            sb += b as i64;
            sg += g as i64;
            sr += rr as i64;
        }
        for i in 0..w {
            let row = &mut tmp[j * w + i];
            row[0] = (sb / n) as u8;
            row[1] = (sg / n) as u8;
            row[2] = (sr / n) as u8;
            // 滑窗：去掉左端、加上右端（下标按边界夹取）
            let out = base.pixel(lx0 + (i as i32 - r).clamp(0, cw - 1), ly);
            let inx = base.pixel(lx0 + (i as i32 + r + 1).clamp(0, cw - 1), ly);
            sb += inx[0] as i64 - out[0] as i64;
            sg += inx[1] as i64 - out[1] as i64;
            sr += inx[2] as i64 - out[2] as i64;
        }
    }
    // 垂直趟：对 tmp 逐列滑窗，写回目标表面
    let n = (2 * r + 1) as i64;
    let ch = h as i32;
    for i in 0..w {
        let mut sb = 0i64;
        let mut sg = 0i64;
        let mut sr = 0i64;
        for k in -r..=r {
            let idx = k.clamp(0, ch - 1) as usize * w + i;
            sb += tmp[idx][0] as i64;
            sg += tmp[idx][1] as i64;
            sr += tmp[idx][2] as i64;
        }
        for j in 0..h {
            let px = 0xFF00_0000u32
                | (((sr / n) as u32) << 16)
                | (((sg / n) as u32) << 8)
                | ((sb / n) as u32);
            let sy = (origin.y + ly0 + j as i32) as usize;
            let sx = (origin.x + lx0 + i as i32) as usize;
            *dst_bits.add(sy * dst_w as usize + sx) = px;
            let outi = (j as i32 - r).clamp(0, ch - 1) as usize * w + i;
            let ini = (j as i32 + r + 1).clamp(0, ch - 1) as usize * w + i;
            sb += tmp[ini][0] as i64 - tmp[outi][0] as i64;
            sg += tmp[ini][1] as i64 - tmp[outi][1] as i64;
            sr += tmp[ini][2] as i64 - tmp[outi][2] as i64;
        }
    }
}

/// 聚光灯：**整个选区**内，bbox 之外按 `dim` 压暗、bbox 内保留原亮度。
///
/// 与马赛克/高亮同一口径：取样自 pristine 底图（幂等，兼容部分重绘）。
/// `bounds` 为选区局部尺寸（0,0,w,h）——只改写选区，不碰屏幕上选区以外的冻结画面。
///
/// 局限：作为整选区滤镜，它按 pristine 底图重写整选区，因此**画在它之前的标注
/// 会被压暗/覆盖**（这与 SnowAir 合并下层的做法不同）；后画的标注正常叠在其上。
///
/// # Safety
/// `dst_bits` 须指向 `dst_w×dst_h` 的 32bpp 表面（u32 寻址）。
#[allow(clippy::too_many_arguments)]
pub unsafe fn apply_spotlight(
    dst_bits: *mut u32,
    dst_w: i32,
    dst_h: i32,
    origin: Pt,
    base: &BaseSource,
    bounds: Rect,
    bbox: Rect,
    dim: f32,
) {
    let dim = dim.clamp(0.0, 1.0);
    for ly in 0..bounds.h {
        let sy = origin.y + ly;
        if sy < 0 || sy >= dst_h {
            continue;
        }
        for lx in 0..bounds.w {
            let sx = origin.x + lx;
            if sx < 0 || sx >= dst_w {
                continue;
            }
            let [b, g, r, _] = base.pixel(lx, ly);
            let inside = bbox.contains(Pt::new(lx, ly));
            let f = if inside { 1.0 } else { dim };
            let nb = ((b as f32) * f).round().clamp(0.0, 255.0) as u32;
            let ng = ((g as f32) * f).round().clamp(0.0, 255.0) as u32;
            let nr = ((r as f32) * f).round().clamp(0.0, 255.0) as u32;
            *dst_bits.add(sy as usize * dst_w as usize + sx as usize) =
                0xFF00_0000 | (nr << 16) | (ng << 8) | nb;
        }
    }
}

/// 局部坐标 bbox → 表面坐标范围（裁剪到 `dst_w×dst_h` 与 bbox 内）。
fn clip_surface(origin: Pt, bbox: Rect, dst_w: i32, dst_h: i32) -> (i32, i32, i32, i32) {
    (
        (origin.x + bbox.x).max(0),
        (origin.y + bbox.y).max(0),
        (origin.x + bbox.right()).min(dst_w),
        (origin.y + bbox.bottom()).min(dst_h),
    )
}

/// 把表面坐标 `[lx0,ly0)..[lx1,ly1)`（局部坐标）填成 `px`。
unsafe fn write_px(
    dst_bits: *mut u32,
    dst_w: i32,
    origin: Pt,
    lx0: i32,
    ly0: i32,
    lx1: i32,
    ly1: i32,
    px: u32,
) {
    for y in ly0..ly1 {
        let row = (origin.y + y) as usize * dst_w as usize;
        for x in lx0..lx1 {
            *dst_bits.add(row + (origin.x + x) as usize) = px;
        }
    }
}

/// 把「已提交标注 + 预览 + 选中态」渲染到一个表面（选区局部坐标）。
///
/// 调用方须保证 `dc`/`bits` 指向同一块 `dst_w×dst_h` 表面，`origin` 为选区原点
/// 在该表面中的坐标（见 4.1）。`base` 供马赛克/高亮取样 pristine 底图：
/// 预览路径传 `Some(BaseSource::Surface(&annot_base))`，导出传
/// `Some(BaseSource::Image(&成品))`；无底图时这两类标注退化为不绘制。
pub unsafe fn render_into(
    dc: HDC,
    bits: *mut u32,
    dst_w: i32,
    dst_h: i32,
    origin: Pt,
    session: &AnnotSession,
    dpi: i32,
    base: Option<&BaseSource>,
) {
    let Some(g) = render_gdiplus::graphics_from(dc) else {
        return;
    };
    render_gdiplus::set_origin(g, origin.x as f32, origin.y as f32);
    render_gdiplus::clip(g, session.bounds());

    let hidden = session.hidden_id();
    for ann in session.doc.shapes() {
        if Some(ann.id) == hidden {
            continue;
        }
        draw_one(
            g, bits, dst_w, dst_h, origin, &ann.shape, session.bounds(), base,
        );
    }
    if session.drawing() || session.typing() {
        if let Some(pv) = session.preview_shape() {
            draw_one(
                g, bits, dst_w, dst_h, origin, &pv, session.bounds(), base,
            );
        }
        // 输入中的文字：末尾光标（仅预览；已提交文字不带光标）
        if let Some((anchor, text, style)) = session.typing_state() {
            render_gdiplus::draw_caret(g, anchor, &text, &style);
        }
    } else if let Some(ch) = session.chrome_shape() {
        render_gdiplus::draw_shape(g, &ch);
        render_gdiplus::draw_chrome(g, &ch, dpi);
    }

    render_gdiplus::reset_origin(g);
    render_gdiplus::free_graphics(g);
    let _ = GdiFlush();
}

/// 绘制单个形状：马赛克/高亮走 CPU 像素合成（需底图取样），其余走 GDI+。
/// z 序 = 调用序；CPU 写入与 GDI+ 绘制交错时以 `GdiFlush` 分隔，保证顺序正确。
unsafe fn draw_one(
    g: *mut GpGraphics,
    bits: *mut u32,
    dst_w: i32,
    dst_h: i32,
    origin: Pt,
    shape: &Shape,
    bounds: Rect,
    base: Option<&BaseSource>,
) {
    // 高亮/聚光灯系数来自配置（渲染路径非逐像素，读一次即可）。
    let cfg = crate::config::get();
    match shape {
        Shape::Mosaic { bbox, mode } => {
            if let Some(base) = base {
                let _ = GdiFlush();
                match mode {
                    MosaicMode::Pixelate => apply_mosaic(
                        bits,
                        dst_w,
                        dst_h,
                        origin,
                        base,
                        *bbox,
                        mosaic_block(bounds.w, bounds.h),
                    ),
                    MosaicMode::Blur => apply_blur(
                        bits,
                        dst_w,
                        dst_h,
                        origin,
                        base,
                        *bbox,
                        blur_radius(bounds.w, bounds.h),
                    ),
                }
            }
        }
        Shape::Highlight { bbox, color, mode } => {
            if let Some(base) = base {
                let _ = GdiFlush();
                match mode {
                    HighlightMode::Marker => {
                        apply_highlight(bits, dst_w, dst_h, origin, base, *bbox, *color, cfg.highlight_keep)
                    }
                    HighlightMode::Spotlight => apply_spotlight(
                        bits,
                        dst_w,
                        dst_h,
                        origin,
                        base,
                        bounds,
                        *bbox,
                        cfg.spotlight_dim,
                    ),
                }
            }
        }
        _ => render_gdiplus::draw_shape(g, shape),
    }
}

/// 把标注合成进成品图（见 6）。无标注时零拷贝返回 `base` 的共享引用。
pub fn composite(base: &CapturedImage, doc: &AnnotDoc) -> CapturedImage {
    if doc.is_empty() {
        return base.clone();
    }
    unsafe {
        let Some(surf) = DibSurface::new(base.width, base.height) else {
            return base.clone();
        };
        surf.copy_rows(&base.data);
        let src = BaseSource::Image(base);
        let cfg = crate::config::get();
        if let Some(g) = render_gdiplus::graphics_from(surf.dc) {
            for ann in doc.shapes() {
                let bounds = Rect::new(0, 0, base.width, base.height);
                match &ann.shape {
                    Shape::Mosaic { bbox, mode } => {
                        let _ = GdiFlush();
                        match mode {
                            MosaicMode::Pixelate => apply_mosaic(
                                surf.bits(),
                                base.width,
                                base.height,
                                Pt::new(0, 0),
                                &src,
                                *bbox,
                                mosaic_block(base.width, base.height),
                            ),
                            MosaicMode::Blur => apply_blur(
                                surf.bits(),
                                base.width,
                                base.height,
                                Pt::new(0, 0),
                                &src,
                                *bbox,
                                blur_radius(base.width, base.height),
                            ),
                        }
                    }
                    Shape::Highlight { bbox, color, mode } => {
                        let _ = GdiFlush();
                        match mode {
                            HighlightMode::Marker => apply_highlight(
                                surf.bits(),
                                base.width,
                                base.height,
                                Pt::new(0, 0),
                                &src,
                                *bbox,
                                *color,
                                cfg.highlight_keep,
                            ),
                            HighlightMode::Spotlight => apply_spotlight(
                                surf.bits(),
                                base.width,
                                base.height,
                                Pt::new(0, 0),
                                &src,
                                bounds,
                                *bbox,
                                cfg.spotlight_dim,
                            ),
                        }
                    }
                    _ => render_gdiplus::draw_shape(g, &ann.shape),
                }
            }
            render_gdiplus::free_graphics(g);
        }
        let _ = GdiFlush();
        alpha_fix(
            surf.bits(),
            base.width,
            Rect::new(0, 0, base.width, base.height),
        );
        let data = surf.read_rows();
        CapturedImage {
            x: base.x,
            y: base.y,
            width: base.width,
            height: base.height,
            data: Arc::from(data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_fix_forces_opaque_bytes() {
        let (w, h) = (4i32, 3i32);
        let mut buf = vec![0u32; (w * h) as usize];
        unsafe {
            alpha_fix(buf.as_mut_ptr(), w, Rect::new(1, 1, 2, 2));
        }
        // 只有 (1,1)-(2,2) 被置满 alpha
        assert_eq!(buf[(1 * w + 1) as usize] >> 24, 0xFF);
        assert_eq!(buf[(2 * w + 2) as usize] >> 24, 0xFF);
        assert_eq!(buf[0] >> 24, 0x00, "脏区外不应改动");
    }

    #[test]
    fn composite_without_annotations_is_zero_copy() {
        let base = CapturedImage {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            data: Arc::from(vec![7u8; 16]),
        };
        let doc = AnnotDoc::new();
        let out = composite(&base, &doc);
        assert_eq!(&out.data[..], &base.data[..]);
        assert!(Arc::ptr_eq(&out.data, &base.data), "无标注必须零拷贝共享像素");
    }

    /// 合成端到端：马赛克/高亮/文字经 `composite` 完成（CPU 算子 + GDI+ 全路径）。
    /// 需要 GDI/GDI+，标记 #[ignore] 供显式运行验证。
    #[test]
    #[ignore = "需要 GDI+/DIB，供显式运行验证"]
    fn composite_applies_mosaic_highlight_text() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        let base = fake_img(8, 8);

        // 1. 马赛克：整幅应变为平均色（B=3, G=3, R=0x40）
        let mut doc = AnnotDoc::new();
        doc.add(Shape::Mosaic {
            bbox: Rect::new(0, 0, 8, 8),
            mode: MosaicMode::Pixelate,
        });
        let out = composite(&base, &doc);
        let avg = 0xFF00_0000u32 | (0x40 << 16) | (3 << 8) | 3;
        let px: Vec<u32> = out
            .data
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert!(px.iter().all(|&p| p == avg), "马赛克应整幅为平均色");

        // 2. 高亮：白底 × 高亮系数（像素变化且不等于原值）
        let mut white = vec![0u8; 8 * 8 * 4];
        for p in white.chunks_exact_mut(4) {
            p.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        }
        let white_base = CapturedImage {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            data: Arc::from(white),
        };
        let mut doc2 = AnnotDoc::new();
        doc2.add(Shape::Highlight {
            bbox: Rect::new(1, 1, 4, 4),
            color: super::super::style::HIGHLIGHT,
            mode: HighlightMode::Marker,
        });
        let out2 = composite(&white_base, &doc2);
        // bbox 内像素应为高亮色 × 白 = (115, 227, 255)（B,G,R）；bbox 外仍为白
        // （蓝 ← 0×0.45=115，绿 ← 204×0.89=227，红 ← 255×1=255）
        let at = |x: i32, y: i32| {
            let off = ((y * 8 + x) * 4) as usize;
            (out2.data[off], out2.data[off + 1], out2.data[off + 2])
        };
        assert_eq!(at(2, 2), (115, 227, 255), "bbox 内应为高亮乘法结果");
        assert_eq!(at(6, 6), (0xFF, 0xFF, 0xFF), "bbox 外应保持白");

        // 3. 文字：GDI+ 绘制后像素应发生变化（字形落到图上）
        let mut doc3 = AnnotDoc::new();
        doc3.add(Shape::Text {
            origin: Pt::new(0, 0),
            text: "Hi".into(),
            bbox: Rect::new(0, 0, 20, 25),
            style: super::super::style::TextStyle {
                color: super::super::style::DEF_STROKE,
                font_px: 20,
                bold: false,
                italic: false,
            },
            frame: super::super::style::TextFrame::None,
            attach: None,
        });
        let out3 = composite(&white_base, &doc3);
        assert_ne!(
            &out3.data[..4 * 8 * 8],
            &white_base.data[..],
            "文字应改变像素"
        );
        // alpha 全图置满（分层窗口/导出契约）
        assert!(out3.data.chunks_exact(4).all(|c| c[3] == 0xFF));
    }

    /// 与快照同尺寸、内容可辨认的假图像（BGRA：B=x, G=y, R=0x40, A=0xFF）。
    fn fake_img(w: i32, h: i32) -> CapturedImage {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = x as u8;
                data[i + 1] = y as u8;
                data[i + 2] = 0x40;
                data[i + 3] = 0xFF;
            }
        }
        CapturedImage {
            x: 0,
            y: 0,
            width: w,
            height: h,
            data: Arc::from(data),
        }
    }

    #[test]
    fn mosaic_block_size_follows_min_dim() {
        assert_eq!(mosaic_block(1600, 900), 11, "min/80 = 900/80 = 11");
        assert_eq!(mosaic_block(800, 640), 8, "640/80 = 8");
        assert_eq!(mosaic_block(4000, 4000), 50);
        assert_eq!(mosaic_block(100, 100), 8, "下限 8");
    }

    /// 整块马赛克：块内所有像素变成该块的平均色。
    #[test]
    fn mosaic_averages_block_pixels() {
        let img = fake_img(8, 8);
        let base = BaseSource::Image(&img);
        let mut dst = vec![0u32; 64];
        unsafe {
            apply_mosaic(
                dst.as_mut_ptr(),
                8,
                8,
                Pt::new(0, 0),
                &base,
                Rect::new(0, 0, 8, 8),
                8,
            );
        }
        // 8×8 平均：B = mean(0..8)=3.5→3, G = mean(0..8)=3, R = 0x40
        let avg = 0xFF00_0000u32 | (0x40 << 16) | (3 << 8) | 3;
        assert!(dst.iter().all(|&px| px == avg), "整块应为平均色");
    }

    /// 分块马赛克：各块独立平均（取整向零）。
    #[test]
    fn mosaic_splits_into_blocks() {
        let img = fake_img(4, 4);
        let base = BaseSource::Image(&img);
        let mut dst = vec![0u32; 16];
        unsafe {
            apply_mosaic(
                dst.as_mut_ptr(),
                4,
                4,
                Pt::new(0, 0),
                &base,
                Rect::new(0, 0, 4, 4),
                2,
            );
        }
        // 左上块 (0..2)×(0..2)：B = (0+1)/2=0, G = 0
        let tl = dst[0];
        assert_eq!(tl & 0xFF, 0, "左上块蓝通道 = 0");
        // 右下块 x∈{2,3}, y∈{2,3}：B = (2+3)/2=2, G = 2
        let br = dst[(3 * 4 + 3) as usize];
        assert_eq!(br & 0xFF, 2, "右下块蓝通道 = 2");
        assert_eq!((br >> 8) & 0xFF, 2, "右下块绿通道 = 2");
    }

    /// bbox 外不写、越界不 panic。
    #[test]
    fn mosaic_clips_out_of_bounds() {
        let img = fake_img(4, 4);
        let base = BaseSource::Image(&img);
        // 完全在外：不应改动
        let mut dst = vec![0x11u32; 16];
        unsafe {
            apply_mosaic(
                dst.as_mut_ptr(),
                4,
                4,
                Pt::new(0, 0),
                &base,
                Rect::new(8, 8, 4, 4),
                2,
            );
        }
        assert!(dst.iter().all(|&px| px == 0x11), "bbox 外不应改动");
        // 部分越界（负坐标）：只写界内部分，不 panic；越界区外保持原样
        unsafe {
            apply_mosaic(
                dst.as_mut_ptr(),
                4,
                4,
                Pt::new(0, 0),
                &base,
                Rect::new(-4, -4, 6, 6),
                2,
            );
        }
        // bbox (-4..2)×(-4..2) 裁剪到 (0..2)×(0..2)；(3,3) 不在范围内
        assert_eq!(dst[(3 * 4 + 3) as usize], 0x11, "界外像素不应改动");
    }

    /// 高亮：每通道乘法混合（HIGHLIGHT_KEEP=0.55），alpha 保持 255。
    #[test]
    fn highlight_multiplies_channels() {
        // 底图纯黑 (0,0,0)：乘法后仍黑；纯白 (255,255,255)：变为高亮色的 45%+…
        let mut data = vec![0u8; 2 * 4];
        // 像素 0：白；像素 1：黑
        data[..4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        data[4..8].copy_from_slice(&[0, 0, 0, 0xFF]);
        let img = CapturedImage {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            data: Arc::from(data),
        };
        let base = BaseSource::Image(&img);
        let mut dst = vec![0u32; 2];
        unsafe {
            apply_highlight(
                dst.as_mut_ptr(),
                2,
                1,
                Pt::new(0, 0),
                &base,
                Rect::new(0, 0, 2, 1),
                super::super::style::HIGHLIGHT,
                super::super::style::HIGHLIGHT_KEEP,
            );
        }
        // 白像素：每通道 × m —— 蓝 ← color.b(0)、绿 ← color.g(0xCC)、红 ← color.r(0xFF)
        let white_px = dst[0];
        let (b, g, r) = (white_px & 0xFF, (white_px >> 8) & 0xFF, (white_px >> 16) & 0xFF);
        let expect = |c: u8| ((255.0 - (255.0 - c as f32) * 0.55) / 255.0 * 255.0).round() as u32;
        assert_eq!(b, expect(0x00));
        assert_eq!(g, expect(0xCC));
        assert_eq!(r, expect(0xFF));
        assert_eq!(white_px >> 24, 0xFF, "alpha 必须为 255");
        // 黑像素乘法后仍为黑
        assert_eq!(dst[1] & 0x00FF_FFFF, 0, "黑底乘法后保持黑");
        assert_eq!(dst[1] >> 24, 0xFF);
    }

    /// 模糊：均匀块模糊后仍为原色（不变）；单点噪声被邻域平均掉。
    #[test]
    fn blur_smooths_and_preserves_uniform() {
        // 均匀 4×4（B=10,G=20,R=30）：模糊后应保持同色
        let mut data = vec![0u8; 4 * 4 * 4];
        for p in data.chunks_exact_mut(4) {
            p.copy_from_slice(&[10, 20, 30, 0xFF]);
        }
        let img = CapturedImage {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
            data: Arc::from(data),
        };
        let base = BaseSource::Image(&img);
        let mut dst = vec![0u32; 16];
        unsafe { apply_blur(dst.as_mut_ptr(), 4, 4, Pt::new(0, 0), &base, Rect::new(0, 0, 4, 4), 1) };
        for &px in &dst {
            assert_eq!(px, 0xFF00_0000 | (30 << 16) | (20 << 8) | 10, "均匀块应保持原色");
        }
    }

    /// 聚光灯：bbox 内保持原亮度，bbox 外按系数压暗；alpha 恒 255；选区外不改写。
    #[test]
    fn spotlight_dims_outside_keeps_inside() {
        let mut data = vec![0u8; 4 * 4 * 4];
        for p in data.chunks_exact_mut(4) {
            p.copy_from_slice(&[200, 200, 200, 0xFF]);
        }
        let img = CapturedImage {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
            data: Arc::from(data),
        };
        let base = BaseSource::Image(&img);
        let mut dst = vec![0x11u32; 16];
        unsafe {
            apply_spotlight(
                dst.as_mut_ptr(),
                4,
                4,
                Pt::new(0, 0),
                &base,
                Rect::new(0, 0, 4, 4),
                Rect::new(1, 1, 2, 2),
                super::super::style::SPOTLIGHT_DIM,
            );
        }
        // (1,1) 在 bbox 内 → 原亮度
        assert_eq!(dst[(1 * 4 + 1) as usize] & 0xFF, 200, "bbox 内保持原亮度");
        // (0,0) 在外 → 200 × 0.55 = 110
        assert_eq!(dst[0] & 0xFF, 110, "bbox 外压暗");
        assert_eq!(dst[0] >> 24, 0xFF, "alpha 恒 255");
    }
}
