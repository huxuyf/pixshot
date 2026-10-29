//! Win32 剪贴板：图像（CF_DIB + 注册的 "PNG" 格式）与文本（CF_UNICODETEXT）。
//!
//! 资源收尾是本模块的关键：剪贴板是**全系统共享**的独占资源 ——
//! 一旦 `OpenClipboard` 之后没有 `CloseClipboard`，在进程退出前
//! 其他所有程序都无法读写剪贴板（用户感受是「复制粘贴全失灵」）。
//!
//! 因此这里用 `ClipboardGuard`（RAII）保证任何返回路径都会关闭剪贴板，
//! 并用 `GlobalFree` 保证 `SetClipboardData` 失败时不泄漏全局内存块。
//!
//! **双格式**（参照 SnowShot `copyToClipboard`）：图像先写经典 `CF_DIB`
//! （Paint/Word/Discord 等通用），再注册 "PNG" 格式附加编码好的 PNG
//! （Chrome/Slack/Figma 等现代应用粘贴更可靠）；PNG 为附加格式，
//! 写入失败不影响已成功的 DIB。

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::error::{AppError, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW,
    PlayEnhMetaFile, ReleaseDC, SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    ENHMETAHEADER, GdiFlush, HBITMAP, HENHMETAFILE, HGDIOBJ,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_DIBV5, CF_ENHMETAFILE, CF_UNICODETEXT};

/// 剪贴板打开守卫：`Drop` 时无条件关闭。
struct ClipboardGuard;

impl ClipboardGuard {
    /// 打开剪贴板。剪贴板可能被其他程序短暂占用，重试 15 次（共约 375ms，
    /// 与 SnowShot 的容错窗口一致）。
    fn open() -> Result<Self> {
        for _ in 0..15 {
            match unsafe { OpenClipboard(HWND::default()) } {
                Ok(()) => return Ok(Self),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
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

/// 复制 BGRA 图像到剪贴板（CF_DIB，底向上行序）。兼容入口：不附加 PNG 格式。
pub fn copy_image(bgra_top_down: &[u8], w: i32, h: i32) -> Result<()> {
    copy_image_with_png(bgra_top_down, w, h, None)
}

/// 同 [`copy_image`]，`png` 为 `Some` 时额外注册 "PNG" 格式并写入编码好的
/// PNG 字节（现代应用粘贴优先取用）。PNG 写入失败不影响 DIB。
pub fn copy_image_with_png(
    bgra_top_down: &[u8],
    w: i32,
    h: i32,
    png: Option<&[u8]>,
) -> Result<()> {
    if w <= 0 || h <= 0 {
        return Err(AppError::Config("图像尺寸无效".into()));
    }
    let stride = w as usize * 4;
    if bgra_top_down.len() < stride * h as usize {
        return Err(AppError::Config("图像数据不完整".into()));
    }

    let header = std::mem::size_of::<BITMAPINFOHEADER>();
    let total = stride
        .checked_mul(h as usize)
        .and_then(|n| n.checked_add(header))
        .ok_or_else(|| AppError::Config("图像尺寸溢出".into()))?;

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

    // 附加 PNG 格式（尽力而为：任何失败都不影响已写入的 DIB）
    if let Some(png) = png.filter(|p| !p.is_empty()) {
        set_png_data(png);
    }
    Ok(())
}

/// 注册 "PNG" 剪贴板格式并写入数据（须在剪贴板已打开时调用）。
fn set_png_data(png: &[u8]) {
    static PNG_FMT: OnceLock<u32> = OnceLock::new();
    let fmt = *PNG_FMT.get_or_init(|| unsafe {
        let name = crate::util::to_wide("PNG");
        RegisterClipboardFormatW(PCWSTR::from_raw(name.as_ptr()))
    });
    if fmt == 0 {
        return;
    }
    unsafe {
        let hmem = match GlobalAlloc(GMEM_MOVEABLE, png.len()) {
            Ok(h) => h,
            Err(_) => return,
        };
        let p = GlobalLock(hmem);
        if p.is_null() {
            let _ = GlobalFree(hmem);
            return;
        }
        std::ptr::copy_nonoverlapping(png.as_ptr(), p as *mut u8, png.len());
        let _ = GlobalUnlock(hmem);
        if SetClipboardData(fmt, HANDLE(hmem.0)).is_err() {
            let _ = GlobalFree(hmem); // Windows 未接受，内存块仍归我们
        }
    }
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
    debug_assert!(src.len() >= stride * h, "源缓冲必须至少容纳 stride×h");
    debug_assert!(dst.len() >= stride * h);
    for row in 0..h {
        let s = row * stride;
        let d = (h - 1 - row) * stride;
        if s + stride <= src.len() && d + stride <= dst.len() {
            dst[d..d + stride].copy_from_slice(&src[s..s + stride]);
        }
    }
}

// ─── 读取：剪贴板 → 可贴图内容（F3 贴图，见 main::pin_from_clipboard）───────
//
// 与写路径相反的资源规则：`GetClipboardData` 返回的 `HANDLE` **归剪贴板所有**，
// 调用方绝不能 `GlobalFree`；读出后立即拷进自有 `Vec`。所有读取都在同一个
// `ClipboardGuard` 生命周期内完成（句柄在 `CloseClipboard` 后失效）。

/// 剪贴板可用格式集合（供纯函数 [`classify`] 判定，便于单测）。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Formats {
    pub png: bool,
    pub dibv5: bool,
    pub dib: bool,
    pub bitmap: bool,
    pub emf: bool,
    pub rtf: bool,
    pub html: bool,
    pub text: bool,
}

/// 判定结果（按优先级）。`Empty` 表示什么都没有。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Png,
    DibV5,
    Dib,
    Bitmap,
    Emf,
    Rtf,
    Html,
    Text,
    Empty,
}

/// 格式优先级：**图像优先于文本**，保证 Excel 复制（同时带 DIB/BITMAP 与
/// HTML/RTF/文本）走图片分支而非被当成纯文本。
pub fn classify(f: &Formats) -> Kind {
    if f.png {
        Kind::Png
    } else if f.dibv5 {
        Kind::DibV5
    } else if f.dib {
        Kind::Dib
    } else if f.bitmap {
        Kind::Bitmap
    } else if f.emf {
        Kind::Emf
    } else if f.rtf {
        Kind::Rtf
    } else if f.html {
        Kind::Html
    } else if f.text {
        Kind::Text
    } else {
        Kind::Empty
    }
}

/// 读取到的剪贴板内容（图像已统一转成 BGRA 自上而下）。
pub enum ClipContent {
    /// 已转换好的位图（PNG/DIB/BITMAP/EMF 统一入口）。
    Image { w: i32, h: i32, bgra: Vec<u8> },
    /// RTF 原始字节（交由 RichEdit 渲染）。
    Rtf(Vec<u8>),
    /// HTML 片段（无渲染器，主流程提取可见文本后走文本路径）。
    Html(String),
    /// 纯文本。
    Text(String),
}

/// 注册并缓存一个剪贴板格式 id（0 = 注册失败）。
fn registered_format(name: &str) -> u32 {
    unsafe {
        let n = crate::util::to_wide(name);
        RegisterClipboardFormatW(PCWSTR::from_raw(n.as_ptr()))
    }
}

fn fmt_png() -> u32 {
    static F: OnceLock<u32> = OnceLock::new();
    *F.get_or_init(|| registered_format("PNG"))
}
fn fmt_rtf() -> u32 {
    static F: OnceLock<u32> = OnceLock::new();
    *F.get_or_init(|| registered_format("Rich Text Format"))
}
fn fmt_html() -> u32 {
    static F: OnceLock<u32> = OnceLock::new();
    *F.get_or_init(|| registered_format("HTML Format"))
}

/// 探测当前剪贴板可用格式（须在剪贴板已打开时调用）。
fn probe_formats() -> Formats {
    unsafe {
        let avail = |f: u32| f != 0 && IsClipboardFormatAvailable(f).is_ok();
        Formats {
            png: avail(fmt_png()),
            dibv5: avail(CF_DIBV5.0 as u32),
            dib: avail(CF_DIB.0 as u32),
            bitmap: avail(CF_BITMAP.0 as u32),
            emf: avail(CF_ENHMETAFILE.0 as u32),
            rtf: avail(fmt_rtf()),
            html: avail(fmt_html()),
            text: avail(CF_UNICODETEXT.0 as u32),
        }
    }
}

/// 把 `HGLOBAL` 剪贴板数据整块拷进 `Vec`（不释放句柄）。
/// 巨型剪贴板项直接报错（`vec!` 分配失败是 abort 而非可恢复错误）。
fn read_hglobal(fmt: u32) -> Result<Vec<u8>> {
    const MAX_CLIP_BYTES: usize = 512 * 1024 * 1024; // 512MB，足以容纳 8K 截图
    unsafe {
        let h = GetClipboardData(fmt)
            .map_err(|e| AppError::Config(format!("读取剪贴板格式 {fmt} 失败：{e}")))?;
        if h.is_invalid() {
            return Err(AppError::Config(format!("剪贴板格式 {fmt} 无数据")));
        }
        let hg = HGLOBAL(h.0);
        let p = GlobalLock(hg);
        if p.is_null() {
            return Err(AppError::Config("GlobalLock 失败".into()));
        }
        let size = GlobalSize(hg);
        if size > MAX_CLIP_BYTES {
            let _ = GlobalUnlock(hg);
            return Err(AppError::Config(format!(
                "剪贴板数据过大（{}MB），拒绝读取",
                size / 1024 / 1024
            )));
        }
        let mut v = vec![0u8; size];
        if size > 0 {
            std::ptr::copy_nonoverlapping(p as *const u8, v.as_mut_ptr(), size);
        }
        let _ = GlobalUnlock(hg);
        Ok(v)
    }
}

/// 读取 UTF-16 NUL 结尾文本。
fn read_unicode_text() -> Result<String> {
    let bytes = read_hglobal(CF_UNICODETEXT.0 as u32)?;
    let mut u16s: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for c in bytes.chunks_exact(2) {
        let v = u16::from_le_bytes([c[0], c[1]]);
        if v == 0 {
            break;
        }
        u16s.push(v);
    }
    Ok(String::from_utf16_lossy(&u16s))
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn i32_at(b: &[u8], o: usize) -> i32 {
    u32_at(b, o) as i32
}

/// CF_DIB / CF_DIBV5 数据块 → BGRA 自上而下。
///
/// 手动按字节解析位图头字段（避免依赖不同 V4/V5 结构体布局）。
/// **这是不可信输入**（剪贴板任何进程可写）：尺寸先钳制再换算（checked 运算，
/// 乘法溢出不得绕过长度校验）；只接受 BI_RGB 与 32bpp BI_BITFIELDS 的常见
/// 掩码布局（00RRGGBB，内存字节序与 BI_RGB 相同）；BI_JPEG/BI_PNG 等压缩
/// 格式明确报错，而不是把压缩字节当像素解析（花屏）。
/// 支持 BITMAPINFOHEADER+（biSize 40..=124）与 BITMAPCOREHEADER（biSize=12）。
pub fn dib_to_bgra(blob: &[u8]) -> Result<(i32, i32, Vec<u8>)> {
    /// 单条边上限（8K 截图 7680 远低于此；防御畸形头）
    const MAX_DIM: i32 = 32767;
    /// 输出总字节上限（32767²×4 ≈ 4GB，分配失败会 abort——先拦住）
    const MAX_OUT_BYTES: u64 = 1 << 30; // 1 GiB
    if blob.len() < 12 {
        return Err(AppError::Config("DIB 数据过短".into()));
    }
    let bi_size = u32_at(blob, 0) as usize;
    let (w, h_raw, bpp, compression, pixel_off);
    if bi_size == 12 {
        // BITMAPCOREHEADER：宽/高为 u16，bpp 在偏移 10；无压缩字段（恒 BI_RGB）
        w = u16_at(blob, 4) as i32;
        h_raw = u16_at(blob, 6) as i32;
        bpp = u16_at(blob, 10);
        compression = 0u32;
        pixel_off = 12usize;
    } else if (40..=124).contains(&bi_size) {
        if blob.len() < bi_size {
            return Err(AppError::Config("DIB 头不完整".into()));
        }
        w = i32_at(blob, 4);
        h_raw = i32_at(blob, 8);
        bpp = u16_at(blob, 14);
        compression = u32_at(blob, 16);
        // BI_BITFIELDS + 40 字节头：3 个掩码 DWORD 紧随头部（在 biSize 之外）
        pixel_off = if compression == 3 && bi_size == 40 { bi_size + 12 } else { bi_size };
    } else {
        return Err(AppError::Config(format!("DIB 头长度异常: biSize={bi_size}")));
    }
    if w <= 0 || h_raw == 0 {
        return Err(AppError::Config("DIB 尺寸无效".into()));
    }
    let h_abs = h_raw.unsigned_abs() as i64;
    if w > MAX_DIM || h_abs > MAX_DIM as i64 {
        return Err(AppError::Config(format!("DIB 尺寸超出上限: {w}×{h_abs}")));
    }
    if bpp != 24 && bpp != 32 {
        return Err(AppError::Config(format!("不支持的 DIB 位深：{bpp}bpp")));
    }
    // 只接受未压缩；32bpp 兼容 BI_BITFIELDS 的常见掩码（字节序与 BI_RGB 相同）
    if compression != 0 && !(compression == 3 && bpp == 32) {
        return Err(AppError::Config(format!("不支持的 DIB 压缩格式: {compression}")));
    }
    let top_down = h_raw < 0;
    let h = h_raw.unsigned_abs() as usize;
    let wu = w as usize;
    if (wu as u64) * (h as u64) * 4 > MAX_OUT_BYTES {
        return Err(AppError::Config("DIB 图像过大".into()));
    }
    let src_stride = ((wu * bpp as usize + 31) / 32) * 4;
    let need = src_stride
        .checked_mul(h)
        .and_then(|n| n.checked_add(pixel_off))
        .ok_or_else(|| AppError::Config("DIB 尺寸溢出".into()))?;
    if blob.len() < need {
        return Err(AppError::Config("DIB 像素数据不完整".into()));
    }

    let mut out = vec![0u8; wu * h * 4];
    if bpp == 32 {
        for row in 0..h {
            let src_row = if top_down { row } else { h - 1 - row };
            let s = pixel_off + src_row * src_stride;
            let d = row * wu * 4;
            out[d..d + wu * 4].copy_from_slice(&blob[s..s + wu * 4]);
        }
        // alpha 整图决策：BI_RGB 下 alpha 常全为 0（应整体视为不透明）；
        // 若存在真实透明像素（DIBV5），则保留原 alpha，不做逐像素改写
        let all_zero = out.chunks_exact(4).all(|px| px[3] == 0);
        if all_zero {
            for px in out.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
    } else {
        for row in 0..h {
            let src_row = if top_down { row } else { h - 1 - row };
            let s = pixel_off + src_row * src_stride;
            let d = row * wu * 4;
            for x in 0..wu {
                let si = s + x * 3;
                let di = d + x * 4;
                out[di] = blob[si];
                out[di + 1] = blob[si + 1];
                out[di + 2] = blob[si + 2];
                out[di + 3] = 255;
            }
        }
    }
    Ok((w as i32, h as i32, out))
}

/// CF_BITMAP（HBITMAP）→ BGRA 自上而下。
pub fn hbitmap_to_bgra(hbmp: HBITMAP) -> Result<(i32, i32, Vec<u8>)> {
    unsafe {
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(hbmp.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut c_void),
        );
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            return Err(AppError::Config("HBITMAP 尺寸无效".into()));
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        // 防御：超大位图（不可信来源）。`w * h * 4` 必须先在 usize 域相乘——
        // i32 乘法溢出回绕会让缓冲远小于 GetDIBits 的实际写入量（堆越界写）。
        const MAX_DIM: i32 = 32767;
        const MAX_BYTES: u64 = 1 << 30; // 1 GiB
        if w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) * 4 > MAX_BYTES {
            return Err(AppError::Config(format!("HBITMAP 尺寸超出上限: {w}×{h}")));
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 自上而下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bmi = bmi;
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(AppError::Config("CreateCompatibleDC 失败".into()));
        }
        let lines = GetDIBits(
            dc,
            hbmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        if lines == 0 {
            return Err(AppError::Config("GetDIBits(HBITMAP) 失败".into()));
        }
        for px in buf.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok((w, h, buf))
    }
}

/// CF_ENHMETAFILE（HENHMETAFILE）→ BGRA 自上而下（白底播放）。
pub fn enhmetafile_to_bgra(hemf: HENHMETAFILE) -> Result<(i32, i32, Vec<u8>)> {
    unsafe {
        let mut hdr = ENHMETAHEADER::default();
        let n = windows::Win32::Graphics::Gdi::GetEnhMetaFileHeader(
            hemf,
            std::mem::size_of::<ENHMETAHEADER>() as u32,
            Some(&mut hdr),
        );
        if n == 0 {
            return Err(AppError::Config("GetEnhMetaFileHeader 失败".into()));
        }
        let w = (hdr.rclBounds.right - hdr.rclBounds.left).max(1);
        let h = (hdr.rclBounds.bottom - hdr.rclBounds.top).max(1);
        if w > 20000 || h > 20000 {
            return Err(AppError::Config("增强图元文件尺寸异常".into()));
        }

        let screen = GetDC(HWND::default());
        if screen.is_invalid() {
            return Err(AppError::Config("GetDC 失败".into()));
        }
        let mem = CreateCompatibleDC(screen);
        if mem.is_invalid() {
            let _ = ReleaseDC(HWND::default(), screen);
            return Err(AppError::Config("CreateCompatibleDC 失败".into()));
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0) {
            Ok(b) => b,
            Err(e) => {
                let _ = DeleteDC(mem);
                let _ = ReleaseDC(HWND::default(), screen);
                return Err(AppError::Config(format!("CreateDIBSection 失败：{e}")));
            }
        };
        if bits.is_null() {
            let _ = DeleteObject(dib);
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(HWND::default(), screen);
            return Err(AppError::Config("CreateDIBSection 返回空指针".into()));
        }
        let old = SelectObject(mem, dib);
        // 白底不透明打底（BGRA 全 0xFF = 白），再把图元文件播到上方
        std::ptr::write_bytes(bits as *mut u8, 0xFF, (w as usize) * (h as usize) * 4);
        let rect = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = PlayEnhMetaFile(mem, hemf, &rect);
        let _ = GdiFlush();
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());
        SelectObject(mem, old);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(HWND::default(), screen);
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok((w, h, out))
    }
}

/// 读取剪贴板并转成可贴图内容。`Ok(None)` 表示剪贴板为空/无可贴内容。
///
/// 全部读取在同一个 `ClipboardGuard` 内完成（句柄在关闭后失效）。
pub fn read_clipboard() -> Result<Option<ClipContent>> {
    let _guard = ClipboardGuard::open()?;
    let f = probe_formats();
    match classify(&f) {
        Kind::Empty => Ok(None),
        Kind::Png => {
            let bytes = read_hglobal(fmt_png())?;
            let (w, h, bgra) = crate::imageio::decode_png(&bytes)?;
            Ok(Some(ClipContent::Image { w, h, bgra }))
        }
        Kind::DibV5 => {
            let blob = read_hglobal(CF_DIBV5.0 as u32)?;
            let (w, h, bgra) = dib_to_bgra(&blob)?;
            Ok(Some(ClipContent::Image { w, h, bgra }))
        }
        Kind::Dib => {
            let blob = read_hglobal(CF_DIB.0 as u32)?;
            let (w, h, bgra) = dib_to_bgra(&blob)?;
            Ok(Some(ClipContent::Image { w, h, bgra }))
        }
        Kind::Bitmap => {
            let h = unsafe {
                GetClipboardData(CF_BITMAP.0 as u32)
                    .map_err(|e| AppError::Config(format!("读取 CF_BITMAP 失败：{e}")))?
            };
            if h.is_invalid() {
                return Ok(None);
            }
            let (w, hh, bgra) = hbitmap_to_bgra(HBITMAP(h.0))?;
            Ok(Some(ClipContent::Image { w, h: hh, bgra }))
        }
        Kind::Emf => {
            let h = unsafe {
                GetClipboardData(CF_ENHMETAFILE.0 as u32)
                    .map_err(|e| AppError::Config(format!("读取 CF_ENHMETAFILE 失败：{e}")))?
            };
            if h.is_invalid() {
                return Ok(None);
            }
            let (w, hh, bgra) = enhmetafile_to_bgra(HENHMETAFILE(h.0))?;
            Ok(Some(ClipContent::Image { w, h: hh, bgra }))
        }
        Kind::Rtf => Ok(Some(ClipContent::Rtf(read_hglobal(fmt_rtf())?))),
        Kind::Html => {
            let bytes = read_hglobal(fmt_html())?;
            Ok(Some(ClipContent::Html(
                String::from_utf8_lossy(&bytes).into_owned(),
            )))
        }
        Kind::Text => Ok(Some(ClipContent::Text(read_unicode_text()?))),
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

    /// 图像格式必须优先于文本/富文本——否则 Excel 复制（同时带 DIB 与 HTML/RTF）
    /// 会被误判为纯文本。
    #[test]
    fn classify_prefers_image_over_text() {
        let excel = Formats {
            dib: true,
            bitmap: true,
            html: true,
            rtf: true,
            text: true,
            ..Default::default()
        };
        assert_eq!(classify(&excel), Kind::Dib);

        // PNG 优先级最高
        let all = Formats {
            png: true,
            dibv5: true,
            dib: true,
            bitmap: true,
            emf: true,
            rtf: true,
            html: true,
            text: true,
        };
        assert_eq!(classify(&all), Kind::Png);

        // 无图像时才落到富文本，再无才落到纯文本
        assert_eq!(
            classify(&Formats { rtf: true, text: true, ..Default::default() }),
            Kind::Rtf
        );
        assert_eq!(
            classify(&Formats { text: true, ..Default::default() }),
            Kind::Text
        );
        assert_eq!(classify(&Formats::default()), Kind::Empty);
    }

    /// 构造一个最小 2×2 的 32bpp 自下而上 DIB（BI_RGB，alpha=0）。
    fn make_dib_2x2() -> Vec<u8> {
        let mut b = vec![0u8; 40 + 2 * 8]; // header + 2 行 × stride8
        b[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
        b[4..8].copy_from_slice(&2i32.to_le_bytes()); // width
        b[8..12].copy_from_slice(&2i32.to_le_bytes()); // height（正=自下而上）
        b[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        b[14..16].copy_from_slice(&32u16.to_le_bytes()); // bpp
        // 行0（DIB 底行）在文件里先出现 → 翻转后应成为输出最后一行
        // DIB 行序：先是 image 底行(y=1)，再是顶行(y=0)
        // 底行像素：B=0x10；顶行像素：B=0x20
        b[40] = 0x10;
        b[48] = 0x20;
        b
    }

    #[test]
    fn dib_bottom_up_is_flipped_and_alpha_opaque() {
        let (w, h, out) = dib_to_bgra(&make_dib_2x2()).unwrap();
        assert_eq!((w, h), (2, 2));
        // 顶行（输出第一行）应来自 DIB 的最后一行 0x20
        assert_eq!(out[0], 0x20);
        // 底行（输出最后一行）应来自 DIB 的第一行 0x10
        assert_eq!(out[(1 * 2) * 4], 0x10);
        // alpha=0 时按不透明处理
        assert_eq!(out[3], 255);
    }

    /// 真实剪贴板往返（会覆盖用户剪贴板，故标记 ignore，供显式运行）。
    /// 图像与文本合并为一个用例：剪贴板是全系统共享资源，两个用例并行会互相覆盖。
    #[test]
    #[ignore = "会占用系统剪贴板，供显式运行验证"]
    fn clipboard_roundtrip_real() {
        // 图像：copy_image → read_clipboard 应取回同尺寸像素
        let (w, h) = (4, 3);
        let bgra: Vec<u8> = (0..(w * h)).flat_map(|i| [i as u8, 0x80, 0x40, 0xFF]).collect();
        copy_image(&bgra, w, h).expect("写入图像");
        match read_clipboard().expect("读取剪贴板") {
            Some(ClipContent::Image { w: rw, h: rh, bgra: back }) => {
                assert_eq!((rw, rh), (w, h));
                assert_eq!(back, bgra, "DIB 往返像素应一致");
            }
            _ => panic!("应为图像"),
        }

        // 文本：copy_text → read_clipboard 应取回同文本
        copy_text("你好 PixShot\nline2").expect("写入文本");
        match read_clipboard().expect("读取剪贴板") {
            Some(ClipContent::Text(t)) => assert_eq!(t, "你好 PixShot\nline2"),
            _ => panic!("应为纯文本"),
        }
    }

    #[test]
    fn dib_rejects_unsupported_bitdepth() {
        let mut b = make_dib_2x2();
        b[14..16].copy_from_slice(&8u16.to_le_bytes());
        assert!(dib_to_bgra(&b).is_err());
    }

    #[test]
    fn dib_rejects_huge_dims_and_compressed() {
        // 畸形头：巨大尺寸必须被拒绝（乘法溢出不得绕过长度校验）
        let mut b = make_dib_2x2();
        b[4..8].copy_from_slice(&i32::MAX.to_le_bytes());
        b[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(dib_to_bgra(&b).is_err(), "巨大尺寸必须拒绝");
        // BI_PNG（compression=5）：压缩数据不得当像素解析
        let mut b = make_dib_2x2();
        b[16..20].copy_from_slice(&5u32.to_le_bytes());
        assert!(dib_to_bgra(&b).is_err(), "压缩格式必须拒绝");
        // 异常 biSize
        let mut b = make_dib_2x2();
        b[0..4].copy_from_slice(&7u32.to_le_bytes());
        assert!(dib_to_bgra(&b).is_err(), "异常 biSize 必须拒绝");
    }

    #[test]
    fn dib_bitfields_offset_and_coreheader() {
        // 32bpp BI_BITFIELDS + 40 字节头：像素在 biSize+12（3 个掩码 DWORD）之后
        let mut b = make_dib_2x2();
        b[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        b.splice(40..40, [0u8; 12]); // 插入掩码（值不读，布局与 BI_RGB 相同）
        let (w, h, out) = dib_to_bgra(&b).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(out[0], 0x20, "掩码后的像素偏移必须正确");
        // 全零 alpha → 整体置不透明
        assert_eq!(out[3], 255);

        // BITMAPCOREHEADER（biSize=12）：宽高为 u16
        let mut c = vec![0u8; 12 + 2 * 8]; // 头 + 2 行（24bpp stride = 8）
        c[0..4].copy_from_slice(&12u32.to_le_bytes());
        c[4..6].copy_from_slice(&2u16.to_le_bytes()); // w
        c[6..8].copy_from_slice(&2u16.to_le_bytes()); // h
        c[8..10].copy_from_slice(&1u16.to_le_bytes()); // planes
        c[10..12].copy_from_slice(&24u16.to_le_bytes()); // bpp
        c[12] = 0x10; // 底行 B
        c[20] = 0x20; // 顶行 B
        let (w, h, out) = dib_to_bgra(&c).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(out[0], 0x20);
        assert_eq!(out[3], 255);
    }

    #[test]
    fn dib_preserves_real_alpha() {
        // 存在真实透明像素（非全零 alpha）时保留原 alpha
        let mut b = make_dib_2x2();
        b[43] = 0x80; // 某像素 alpha=0x80（非全零）
        let (_, _, out) = dib_to_bgra(&b).unwrap();
        // 该像素在 DIB 底行第一个 → 翻转后是输出最后一行第一个
        assert_eq!(out[(1 * 2) * 4 + 3], 0x80, "真实 alpha 必须保留");
        assert_eq!(out[3], 0, "其余像素的 alpha 不被改写");
    }
}
