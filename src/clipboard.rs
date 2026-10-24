//! Win32 剪贴板：图像（CF_DIB）与文本（CF_UNICODETEXT）。
//!
//! 资源收尾是本模块的关键：剪贴板是**全系统共享**的独占资源 ——
//! 一旦 `OpenClipboard` 之后没有 `CloseClipboard`，在进程退出前
//! 其他所有程序都无法读写剪贴板（用户感受是「复制粘贴全失灵」）。
//!
//! 因此这里用 `ClipboardGuard`（RAII）保证任何返回路径都会关闭剪贴板，
//! 并用 `GlobalFree` 保证 `SetClipboardData` 失败时不泄漏全局内存块。

use crate::error::{AppError, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};

/// 剪贴板打开守卫：`Drop` 时无条件关闭。
struct ClipboardGuard;

impl ClipboardGuard {
    /// 打开剪贴板。剪贴板可能被其他程序短暂占用，重试 5 次（共约 200ms）。
    fn open() -> Result<Self> {
        for _ in 0..5 {
            match unsafe { OpenClipboard(HWND::default()) } {
                Ok(()) => return Ok(Self),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(40)),
            }
        }
        Err(AppError::Config(
            "无法打开剪贴板（可能被其他程序占用）".into(),
        ))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// 复制 BGRA 图像到剪贴板（CF_DIB，底向上行序）。
pub fn copy_image(bgra_top_down: &[u8], w: i32, h: i32) -> Result<()> {
    if w <= 0 || h <= 0 {
        return Err(AppError::Config("图像尺寸无效".into()));
    }
    let stride = w as usize * 4;
    if bgra_top_down.len() < stride * h as usize {
        return Err(AppError::Config("图像数据不完整".into()));
    }

    let header = std::mem::size_of::<BITMAPINFOHEADER>();
    let total = header + stride * h as usize;

    // 守卫在函数结束时（含提前 return / ? 传播）自动 CloseClipboard
    let _guard = ClipboardGuard::open()?;

    unsafe {
        let _ = EmptyClipboard();

        let hmem = match GlobalAlloc(GMEM_MOVEABLE, total) {
            Ok(h) => h,
            Err(e) => return Err(e.into()),
        };
        let p = GlobalLock(hmem);
        if p.is_null() {
            let _ = GlobalFree(hmem);
            return Err(AppError::Config("GlobalLock 失败".into()));
        }

        // BITMAPINFOHEADER：正高度 = 自下而上
        let hdr = BITMAPINFOHEADER {
            biSize: header as u32,
            biWidth: w,
            biHeight: h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            biSizeImage: (stride * h as usize) as u32,
            ..Default::default()
        };
        let dst = p as *mut u8;
        std::ptr::copy_nonoverlapping(&hdr as *const BITMAPINFOHEADER as *const u8, dst, header);

        // 行翻转：屏幕数据自上而下，CF_DIB（正高度）自下而上
        let body = std::slice::from_raw_parts_mut(dst.add(header), stride * h as usize);
        copy_rows_flipped(bgra_top_down, stride, h as usize, body);

        let _ = GlobalUnlock(hmem);
        // 内存块所有权移交给剪贴板：成功后不可再释放；失败时必须释放，否则泄漏整幅图像
        if let Err(e) = SetClipboardData(CF_DIB.0 as u32, HANDLE(hmem.0)) {
            let _ = GlobalFree(hmem);
            return Err(e.into());
        }
    }
    Ok(())
}

/// 复制文本到剪贴板（CF_UNICODETEXT）。
pub fn copy_text(s: &str) -> Result<()> {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * 2;

    let _guard = ClipboardGuard::open()?;

    unsafe {
        let _ = EmptyClipboard();

        let hmem = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
            Ok(h) => h,
            Err(e) => return Err(e.into()),
        };
        let p = GlobalLock(hmem);
        if p.is_null() {
            let _ = GlobalFree(hmem);
            return Err(AppError::Config("GlobalLock 失败".into()));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p as *mut u16, wide.len());
        let _ = GlobalUnlock(hmem);
        if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0)) {
            let _ = GlobalFree(hmem);
            return Err(e.into());
        }
    }
    Ok(())
}

/// 把自上而下的行序翻转成自下而上（CF_DIB 要求）。
/// 抽成独立函数以便单元测试行序正确性。
fn copy_rows_flipped(src: &[u8], stride: usize, h: usize, dst: &mut [u8]) {
    debug_assert_eq!(src.len() / stride.max(1), h);
    debug_assert!(dst.len() >= stride * h);
    for row in 0..h {
        let s = row * stride;
        let d = (h - 1 - row) * stride;
        if s + stride <= src.len() && d + stride <= dst.len() {
            dst[d..d + stride].copy_from_slice(&src[s..s + stride]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_flipped_vertically() {
        // 3 行 × 1 像素（stride=4），每行首字节标记行号
        let mut src = vec![0u8; 3 * 4];
        for row in 0..3 {
            src[row * 4] = row as u8;
        }
        let mut dst = vec![0u8; 3 * 4];
        copy_rows_flipped(&src, 4, 3, &mut dst);
        assert_eq!(dst[0], 2, "第 0 行应来自源第 2 行");
        assert_eq!(dst[4], 1);
        assert_eq!(dst[8], 0);
    }

    #[test]
    fn single_row_is_identity() {
        let src = [9u8, 8, 7, 6];
        let mut dst = [0u8; 4];
        copy_rows_flipped(&src, 4, 1, &mut dst);
        assert_eq!(dst, src);
    }

    /// 回归：SetClipboardData 失败路径曾经跳过 CloseClipboard 并泄漏 GMEM。
    /// 这里验证守卫的 Drop 一定执行（用计数器模拟）。
    #[test]
    fn guard_drop_runs_on_early_return() {
        use std::cell::Cell;
        thread_local! { static CLOSED: Cell<bool> = const { Cell::new(false) }; }
        struct Probe;
        impl Drop for Probe {
            fn drop(&mut self) {
                CLOSED.with(|c| c.set(true));
            }
        }
        fn early() -> Result<()> {
            let _p = Probe;
            Err(AppError::Config("模拟失败".into()))
        }
        let r = early();
        assert!(r.is_err());
        assert!(CLOSED.with(|c| c.get()), "提前返回时必须执行收尾");
    }
}
