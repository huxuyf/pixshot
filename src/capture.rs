//! 屏幕捕获（GDI BitBlt + DIB section 直读）——单帧截图可靠，支持虚拟屏幕（多显示器）坐标。
//!
//! 相比「CreateCompatibleBitmap + GetDIBits」的经典写法，这里直接把 BitBlt 的目标
//! 设成 `CreateDIBSection` 的位图：像素由 GDI 直接写进我们持有的内存，省掉一次
//! 位图对象创建与一次整幅像素拷贝/格式转换。
//!
//! 像素格式约定（全项目统一）：**BGRA、自上而下、行距 = width × 4、alpha 恒为 255**。
//! 在捕获阶段就把 alpha 置满，是为了让下游（PNG 编码 / CF_DIB / OCR）都不必再做
//! 「整幅拷贝 + 逐像素改 alpha」——同时也修掉了 CF_DIB 中 alpha=0 时部分应用
//! 把贴图当全透明处理的兼容问题。

use std::sync::Arc;

use crate::error::{AppError, Result};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HDC, SRCCOPY,
};

/// 一张已捕获的屏幕图像。
#[derive(Clone)]
pub struct CapturedImage {
    /// 图像左上角在虚拟屏幕中的坐标（物理像素）。
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// BGRA 像素，自上而下，行距 = width * 4，alpha 恒为 255。
    ///
    /// 用 `Arc<[u8]>` 承载：从框选到贴图/OCR/保存，多个消费者共享同一份像素，
    /// 不再各自 `to_vec()` 复制（1080p 每次省 8 MB 拷贝与相应的分配开销）。
    pub data: Arc<[u8]>,
}

impl CapturedImage {
    /// 从本图裁出一块区域（虚拟屏幕绝对坐标）。越界自动钳制到图像内。
    pub fn crop(&self, x: i32, y: i32, w: i32, h: i32) -> CapturedImage {
        let stride = self.width.max(1) as usize * 4;
        let dx = (x - self.x).clamp(0, self.width.max(0)) as usize;
        let dy = (y - self.y).clamp(0, self.height.max(0)) as usize;
        let cw = w.min(self.width - dx as i32).max(1) as usize;
        let ch = h.min(self.height - dy as i32).max(1) as usize;
        let row = cw * 4;
        let mut out = vec![0u8; row * ch];
        for r in 0..ch {
            let src = (dy + r) * stride + dx * 4;
            let dst = r * row;
            // 越界保护：极端情况下（外部传入非法矩形）宁可留黑也不越界读
            if src + row <= self.data.len() {
                out[dst..dst + row].copy_from_slice(&self.data[src..src + row]);
            }
        }
        CapturedImage {
            x: self.x + dx as i32,
            y: self.y + dy as i32,
            width: cw as i32,
            height: ch as i32,
            data: Arc::from(out),
        }
    }
}

/// 虚拟屏幕（所有显示器的外接矩形）：(x, y, width, height)。
pub fn virtual_screen_rect() -> (i32, i32, i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// 20 位宽高（BITMAPINFOHEADER 约定，负高度 = 自上而下）。
fn top_down_header(w: i32, h: i32) -> BITMAPINFO {
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

/// 截取虚拟屏幕上 (x, y, w, h) 区域（物理像素）。
pub fn capture_region(x: i32, y: i32, w: i32, h: i32) -> Result<CapturedImage> {
    if w <= 0 || h <= 0 {
        return Err(AppError::Config("截屏区域为空".into()));
    }
    unsafe {
        let screen_dc: HDC = GetDC(HWND::default());
        if screen_dc.is_invalid() {
            return Err(AppError::Config("GetDC(屏幕) 失败".into()));
        }

        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_invalid() {
            let _ = ReleaseDC(HWND::default(), screen_dc);
            return Err(AppError::Config("CreateCompatibleDC 失败".into()));
        }

        // 直接把 DIB section 作为 BitBlt 目标：像素落在我们自己持有的内存里
        let bmi = top_down_header(w, h);
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib: HBITMAP = match CreateDIBSection(
            mem_dc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            HANDLE::default(),
            0,
        ) {
            Ok(b) => b,
            Err(e) => {
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(HWND::default(), screen_dc);
                return Err(AppError::Config(format!(
                    "CreateDIBSection 失败（{w}×{h}，可能内存不足）: {e}"
                )));
            }
        };
        if bits.is_null() {
            let _ = DeleteObject(dib);
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(HWND::default(), screen_dc);
            return Err(AppError::Config("CreateDIBSection 返回空像素指针".into()));
        }

        let old = SelectObject(mem_dc, dib);
        let blt = BitBlt(mem_dc, 0, 0, w, h, screen_dc, x, y, SRCCOPY);
        SelectObject(mem_dc, old);

        let out = if let Err(e) = blt {
            // 明确报「BitBlt 失败」并带上区域，便于区分"区域非法"与"资源不足"
            Err(AppError::Config(format!("BitBlt 失败（{w}×{h} @ {x},{y}）: {e}")))
        } else {
            // GDI 批处理可能尚未真正落盘，读取前必须 flush
            let _ = GdiFlush();
            let len = w as usize * h as usize * 4;
            let mut data = vec![0u8; len];
            std::ptr::copy_nonoverlapping(bits as *const u8, data.as_mut_ptr(), len);
            // 屏幕像素本身不透明：alpha 统一置满（见模块头说明）
            for px in data.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
            Ok(CapturedImage {
                x,
                y,
                width: w,
                height: h,
                data: Arc::from(data),
            })
        };

        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(HWND::default(), screen_dc);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: i32, h: i32) -> CapturedImage {
        // 每行像素值 = 列号，便于断言裁剪偏移
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = x as u8;
                data[i + 1] = y as u8;
                data[i + 3] = 0xFF;
            }
        }
        CapturedImage {
            x: 100,
            y: 200,
            width: w,
            height: h,
            data: Arc::from(data),
        }
    }

    #[test]
    fn header_uses_negative_height_for_top_down() {
        let h = top_down_header(640, 480);
        assert_eq!(h.bmiHeader.biWidth, 640);
        assert_eq!(h.bmiHeader.biHeight, -480, "负高度 = 自上而下");
        assert_eq!(h.bmiHeader.biBitCount, 32);
        assert_eq!(h.bmiHeader.biPlanes, 1);
    }

    #[test]
    fn crop_keeps_absolute_origin() {
        let im = img(10, 10);
        let c = im.crop(103, 205, 4, 3);
        assert_eq!((c.x, c.y, c.width, c.height), (103, 205, 4, 3));
        assert_eq!(c.data.len(), 4 * 3 * 4);
        // 第一行第一列应对应原图 (3,5)
        assert_eq!(c.data[0], 3);
        assert_eq!(c.data[1], 5);
    }

    #[test]
    fn crop_clamps_to_bounds() {
        let im = img(10, 10);
        // 超出右下角：应被裁到剩余区域，不越界、不 panic
        let c = im.crop(108, 208, 100, 100);
        assert_eq!((c.width, c.height), (2, 2));
        // 完全在图像之外：钳到 1×1（最小合法图像），不产生空数据
        let c2 = im.crop(0, 0, 4, 4);
        assert!(c2.width >= 1 && c2.height >= 1);
        assert_eq!(c2.data.len(), (c2.width * c2.height * 4) as usize);
    }

    #[test]
    fn crop_full_frame_is_identity() {
        let im = img(8, 6);
        let c = im.crop(100, 200, 8, 6);
        assert_eq!((c.x, c.y, c.width, c.height), (100, 200, 8, 6));
        assert_eq!(&c.data[..], &im.data[..]);
    }
}
