//! 标注引擎：会话的 Win32 集成（DIB 表面、统一渲染入口、导出合成，见设计文档 4.1 / 6）。
//!
//! - [`DibSurface`]：选区大小的 32bpp 自上而下 DIB，承载不可变的 `annot_base` 底图；
//! - [`render_into`]：把「已提交标注 + 预览 + 选中态」渲染到给定 DC（屏幕与导出共用）；
//! - [`composite`]：把标注合成进成品图（保存/复制/贴图接入点调用）。

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

/// 把「已提交标注 + 预览 + 选中态」渲染到一个 DC（选区局部坐标）。
///
/// 调用方须保证 `dc` 指向选区大小的表面，`origin` 为其在屏幕表面中的左上角（见 4.1）。
pub unsafe fn render_into(dc: HDC, origin: Pt, session: &AnnotSession, dpi: i32) {
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
        render_gdiplus::draw_shape(g, &ann.shape);
    }
    if session.drawing() {
        if let Some(pv) = session.preview_shape() {
            render_gdiplus::draw_shape(g, &pv);
        }
    } else if let Some(ch) = session.chrome_shape() {
        render_gdiplus::draw_shape(g, &ch);
        render_gdiplus::draw_chrome(g, &ch, dpi);
    }

    render_gdiplus::reset_origin(g);
    render_gdiplus::free_graphics(g);
    let _ = GdiFlush();
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
        if let Some(g) = render_gdiplus::graphics_from(surf.dc) {
            for ann in doc.shapes() {
                render_gdiplus::draw_shape(g, &ann.shape);
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
}
