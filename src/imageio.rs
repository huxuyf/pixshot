//! 图片保存（WIC，Windows Imaging Component —— 零额外依赖）
//! + 系统保存对话框（GetSaveFileNameW）。
//!
//! 支持 PNG / JPEG / BMP 三种容器（格式由配置文件决定，见 `config::SaveFormat`）：
//! - PNG / BMP：直接写 32bppBGRA（捕获阶段已把 alpha 置满）；
//! - JPEG：不支持 alpha，先转 24bppBGR 再写。
//!
//! 保存成功后把所在目录写回配置，下次对话框默认打开同一目录。
//!
//! [`encode_png`]：内存内 PNG 编码（WIC 内存流），供剪贴板附加
//! 注册的 "PNG" 格式使用（见 `clipboard::copy_image_with_png`）。

use crate::config::SaveFormat;
use crate::error::{AppError, Result};
use crate::util::to_wide;
use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HGLOBAL, HWND};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatBmp, GUID_ContainerFormatJpeg,
    GUID_ContainerFormatPng, GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppBGRA,
    IWICBitmapDecoder, IWICBitmapEncoder, IWICBitmapFrameDecode, IWICBitmapFrameEncode,
    IWICFormatConverter, IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CLSCTX_INPROC_SERVER, STREAM_SEEK_SET,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Memory::GlobalSize;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};

/// 保存为 PNG（`--smoke` 与固定 PNG 场景使用）。
/// 兼容入口：固定以 PNG 保存。
#[allow(dead_code)]
pub fn save_png(path: &str, bgra: &[u8], w: i32, h: i32) -> Result<()> {
    save(path, bgra, w, h, SaveFormat::Png)
}

/// 内存内 PNG 编码（WIC 内存流，不经磁盘）。供剪贴板附加注册的
/// "PNG" 格式使用；与文件保存共用同一条 WIC 编码路径。
pub fn encode_png(bgra: &[u8], w: i32, h: i32) -> Result<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return Err(AppError::Config("图像尺寸无效".into()));
    }
    let stride = (w as usize) * 4;
    if bgra.len() < stride * h as usize {
        return Err(AppError::Config("图像数据不完整".into()));
    }
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        // fDeleteOnRelease=true：IStream 释放时回收全局内存
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let container = GUID_ContainerFormatPng;
        let encoder: IWICBitmapEncoder = factory.CreateEncoder(&container, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;

        let mut frame_opt: Option<IWICBitmapFrameEncode> = None;
        encoder.CreateNewFrame(&mut frame_opt, std::ptr::null_mut())?;
        let frame =
            frame_opt.ok_or_else(|| AppError::Config("WIC CreateNewFrame 返回空".into()))?;
        frame.Initialize(None)?;
        frame.SetSize(w as u32, h as u32)?;
        let mut pf = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut pf)?;
        if pf != GUID_WICPixelFormat32bppBGRA {
            return Err(AppError::Config("WIC 编码器未采用 BGRA 像素格式".into()));
        }
        frame.WritePixels(h as u32, (w as u32) * 4, bgra)?;
        frame.Commit()?;
        encoder.Commit()?;

        // 读回编码结果：经 GetHGlobalFromStream 拿到内存块尺寸，先回到流起点
        // （编码后位置在末尾，直接 Read 会得到 0 字节）再整块读出
        // GlobalSize 是 HGLOBAL 的分配尺寸（≥ 流有效长度），只作容量提示；
        // 循环 Read 直到短读（0）——单次 Read 不保证返回全部字节
        let hg = GetHGlobalFromStream(&stream)?;
        let cap = GlobalSize(hg) as usize;
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        if cap == 0 {
            return Err(AppError::Config("PNG 编码结果为空".into()));
        }
        let mut out = vec![0u8; cap];
        let mut filled = 0usize;
        while filled < cap {
            let mut read: u32 = 0;
            let hr = stream.Read(
                out[filled..].as_mut_ptr() as *mut core::ffi::c_void,
                (cap - filled) as u32,
                Some(&mut read),
            );
            // ISequentialStream::Read 返回 HRESULT（非 Result），手动检查
            if hr.is_err() {
                return Err(AppError::Config("读取 PNG 编码结果失败".into()));
            }
            if read == 0 {
                break;
            }
            filled += read as usize;
        }
        out.truncate(filled);
        if out.is_empty() {
            return Err(AppError::Config("PNG 编码结果为空".into()));
        }
        Ok(out)
    }
}

/// 内存内 PNG 解码（WIC 内存流）→ BGRA 自上而下。供剪贴板注册的 "PNG" 格式
/// （现代应用复制图像时优先提供的格式）使用，见 `clipboard::read_clipboard`。
pub fn decode_png(png: &[u8]) -> Result<(i32, i32, Vec<u8>)> {
    if png.is_empty() {
        return Err(AppError::Config("PNG 数据为空".into()));
    }
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        if png.len() > u32::MAX as usize {
            return Err(AppError::Config("PNG 数据过大".into()));
        }
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        // Write 可能短写/失败：检查 HRESULT 与实际写入量（否则后续报误导性的"解码失败"）
        let mut written = 0u32;
        let hr = stream.Write(png.as_ptr() as *const c_void, png.len() as u32, Some(&mut written));
        if hr.is_err() || written as usize != png.len() {
            return Err(AppError::Config(format!(
                "PNG 写入内存流失败（{written}/{} 字节）",
                png.len()
            )));
        }
        stream.Seek(0, STREAM_SEEK_SET, None)?;

        let decoder: IWICBitmapDecoder =
            factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame: IWICBitmapFrameDecode = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        if w == 0 || h == 0 {
            return Err(AppError::Config("PNG 尺寸无效".into()));
        }
        // 尺寸上限：解压炸弹（数万乘数万的合法 PNG）不得直接分配数 GB
        if w > 32767 || h > 32767 || (w as u64) * (h as u64) * 4 > (1 << 30) {
            return Err(AppError::Config(format!("PNG 尺寸超出上限: {w}×{h}")));
        }
        let conv: IWICFormatConverter = factory.CreateFormatConverter()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let stride = (w as usize) * 4;
        let mut buf = vec![0u8; stride * h as usize];
        conv.CopyPixels(std::ptr::null(), stride as u32, &mut buf)?;
        Ok((w as i32, h as i32, buf))
    }
}

/// 按指定格式保存。
pub fn save(path: &str, bgra: &[u8], w: i32, h: i32, fmt: SaveFormat) -> Result<()> {
    if w <= 0 || h <= 0 {
        return Err(AppError::Config("图像尺寸无效".into()));
    }
    let stride = (w as usize) * 4;
    if bgra.len() < stride * h as usize {
        return Err(AppError::Config("图像数据不完整".into()));
    }
    unsafe { save_wic(path, bgra, w, h, fmt) }
}

unsafe fn save_wic(path: &str, bgra: &[u8], w: i32, h: i32, fmt: SaveFormat) -> Result<()> {
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

    let stream = factory.CreateStream()?;
    let wide = to_wide(path);
    // 0x40000000 = GENERIC_WRITE（windows 0.58 中无此常量）
    stream.InitializeFromFilename(PCWSTR::from_raw(wide.as_ptr()), 0x4000_0000)?;

    let container = match fmt {
        SaveFormat::Png => GUID_ContainerFormatPng,
        SaveFormat::Jpeg => GUID_ContainerFormatJpeg,
        SaveFormat::Bmp => GUID_ContainerFormatBmp,
    };
    let encoder: IWICBitmapEncoder = factory.CreateEncoder(&container, std::ptr::null())?;
    encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;

    let mut frame_opt: Option<IWICBitmapFrameEncode> = None;
    encoder.CreateNewFrame(&mut frame_opt, std::ptr::null_mut())?;
    let frame = frame_opt.ok_or_else(|| AppError::Config("WIC CreateNewFrame 返回空".into()))?;
    frame.Initialize(None)?;
    frame.SetSize(w as u32, h as u32)?;

    match fmt {
        SaveFormat::Jpeg => {
            // JPEG 不支持 alpha：转 24bppBGR 写入
            let mut pf = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut pf)?;
            if pf != GUID_WICPixelFormat24bppBGR {
                return Err(AppError::Config("WIC 编码器未采用 24bppBGR 像素格式".into()));
            }
            let bgr = to_bgr24(bgra, w, h);
            frame.WritePixels(h as u32, (w as u32) * 3, &bgr)?;
        }
        _ => {
            let mut pf = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut pf)?;
            if pf != GUID_WICPixelFormat32bppBGRA {
                return Err(AppError::Config("WIC 编码器未采用 BGRA 像素格式".into()));
            }
            frame.WritePixels(h as u32, (w as u32) * 4, bgra)?;
        }
    }

    frame.Commit()?;
    encoder.Commit()?;
    Ok(())
}

/// BGRA → BGR24（丢弃 alpha 通道）。
fn to_bgr24(bgra: &[u8], w: i32, h: i32) -> Vec<u8> {
    let pixels = (w as usize) * (h as usize);
    let mut out = Vec::with_capacity(pixels * 3);
    for px in bgra.chunks_exact(4).take(pixels) {
        out.extend_from_slice(&px[..3]);
    }
    out
}

/// 弹出系统保存对话框并保存；用户取消返回 Ok(None)。
///
/// `owner` 传当前活动窗口（工具栏/贴图），避免对话框被其他窗口压在后面；
/// `default_dir` 为上次保存目录（空则用系统默认）。
pub fn save_with_dialog(
    owner: HWND,
    bgra: &[u8],
    w: i32,
    h: i32,
    fmt: SaveFormat,
    default_dir: Option<&str>,
) -> Result<Option<String>> {
    let default_name = default_file_name(fmt, default_dir);

    let mut file_buf = [0u16; 1024]; // 长路径（超过 MAX_PATH 的目录）也能容纳
    for (i, c) in default_name.encode_utf16().take(1023).enumerate() {
        file_buf[i] = c;
    }

    // 过滤器："PNG 图片 (*.png)\0*.png\0…\0\0"
    let mut filter: Vec<u16> = Vec::new();
    for f in [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Bmp] {
        let item = format!("{} (*.{})\0*.{}\0", f.label(), f.ext(), f.ext());
        filter.extend(item.encode_utf16());
    }
    filter.push(0); // 双 NUL 结束

    let def_ext = to_wide(fmt.ext());
    let title = to_wide("保存截图");
    let init_dir = default_dir
        .filter(|d| !d.is_empty())
        .map(to_wide)
        .unwrap_or_default();

    // 初始选中配置格式对应的过滤器项（1 起）
    let initial_index = [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Bmp]
        .iter()
        .position(|f| *f == fmt)
        .map(|i| i as u32 + 1)
        .unwrap_or(1);

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR::from_raw(filter.as_ptr()),
        lpstrFile: PWSTR(file_buf.as_mut_ptr()),
        nMaxFile: 1024,
        lpstrDefExt: PCWSTR::from_raw(def_ext.as_ptr()),
        lpstrTitle: PCWSTR::from_raw(title.as_ptr()),
        lpstrInitialDir: if init_dir.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR::from_raw(init_dir.as_ptr())
        },
        Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    ofn.nFilterIndex = initial_index;
    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    if !ok.as_bool() {
        // 区分「用户取消」与「对话框出错」（错误码 0 = 取消）
        let err = unsafe {
            windows::Win32::UI::Controls::Dialogs::CommDlgExtendedError()
        };
        if err.0 != 0 {
            crate::diag::log(&format!("[imageio] 保存对话框错误，CommDlgExtendedError={}", err.0));
        }
        return Ok(None);
    }
    let end = file_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(file_buf.len());
    let path = String::from_utf16_lossy(&file_buf[..end]);
    if path.is_empty() {
        return Ok(None);
    }
    // 保存格式跟随用户在对话框里选的过滤器（否则选了 JPEG 仍存 PNG，扩展名与内容不符）
    let fmt = match ofn.nFilterIndex {
        2 => SaveFormat::Jpeg,
        3 => SaveFormat::Bmp,
        _ => SaveFormat::Png,
    };
    let path = match_extension(&path, fmt);
    save(&path, bgra, w, h, fmt)?;

    // 记住本次目录，供下次对话框使用
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let dir = dir.display().to_string();
        crate::config::update(|c| c.save_dir = Some(dir));
    }
    Ok(Some(path))
}

/// 让文件扩展名与保存格式一致：扩展名已是目标格式则不动；是其它已知图片
/// 扩展名（如对话框按默认扩展名补的 `.png`）则替换；没有扩展名则补上。
fn match_extension(path: &str, fmt: SaveFormat) -> String {
    let want = fmt.ext();
    let p = std::path::Path::new(path);
    let cur = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
    match cur.as_deref() {
        Some(e) if e == want || (want == "jpg" && e == "jpeg") => path.to_string(),
        Some(e) if ["png", "jpg", "jpeg", "bmp"].contains(&e) => {
            let mut pb = p.to_path_buf();
            pb.set_extension(want);
            pb.to_string_lossy().into_owned()
        }
        Some(_) | None => format!("{path}.{want}"),
    }
}

/// 静默保存目录：优先配置里的 `imageio_autosave_dir`，其次 `save_dir`，
/// 否则 `%USERPROFILE%\Pictures\PixShot`（取不到用户目录时回退当前目录）。
/// 用于托盘「截取全屏」等无对话框保存。
pub fn autosave_dir() -> PathBuf {
    let cfg = crate::config::get();
    if let Some(d) = cfg.imageio_autosave_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = cfg.save_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            return PathBuf::from(profile).join("Pictures").join("PixShot");
        }
    }
    PathBuf::from(".")
}

/// 静默保存到 [`autosave_dir`]，文件名沿用当天递增的 `截图_YYYYMMDD_NN.<ext>`。
/// 返回完整落盘路径（供提示展示）。
pub fn autosave(bgra: &[u8], w: i32, h: i32, fmt: SaveFormat) -> Result<String> {
    let dir = autosave_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::Config(format!("创建目录失败 {}：{e}", dir.display())))?;
    // 序号已被占用（并发实例/同秒重复触发）时递增避让，到上限为止
    let cfg = crate::config::get();
    let max_seq = cfg.imageio_max_seq.max(1);
    let st = unsafe { GetLocalTime() };
    let ymd = format!("{:04}{:02}{:02}", st.wYear, st.wMonth, st.wDay);
    let mut seq = next_seq(Some(&dir.to_string_lossy()), &ymd);
    let path = loop {
        let name = format!("{}_{ymd}_{seq:02}.{}", cfg.imageio_prefix, fmt.ext());
        let p = dir.join(name);
        if !p.exists() || seq >= max_seq {
            break p;
        }
        seq += 1;
    };
    let path_s = path.to_string_lossy().into_owned();
    save(&path_s, bgra, w, h, fmt)?;
    Ok(path_s)
}

/// 默认文件名：`<prefix>_YYYYMMDD_NN.<ext>`（本地时间，Win32 GetLocalTime，零依赖）。
/// `dir` 为上次保存目录：在其中扫描当日已存的序列，取最大编号 +1，使当日截图
/// 依次累加（`01`..`max_seq`，满额后固定为上限，由文件覆盖提示兜底）。
fn default_file_name(fmt: SaveFormat, dir: Option<&str>) -> String {
    let st = unsafe { GetLocalTime() };
    let ymd = format!("{:04}{:02}{:02}", st.wYear, st.wMonth, st.wDay);
    let seq = next_seq(dir, &ymd);
    format!("{}_{ymd}_{seq:02}.{}", crate::config::get().imageio_prefix, fmt.ext())
}

/// 扫描 `dir` 下形如 `<prefix>_<ymd>_NN.*` 的文件，返回下一个可用编号。
/// 序号上限来自配置（默认 99）；`dir` 为空或不可读时按首个（1）处理。
fn next_seq(dir: Option<&str>, ymd: &str) -> u32 {
    let cfg = crate::config::get();
    let max_seq = cfg.imageio_max_seq.max(1);
    let prefix = format!("{}_{ymd}_", cfg.imageio_prefix);
    let mut max = 0u32;
    if let Some(dir) = dir.filter(|d| !d.is_empty()) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                let Some(rest) = name.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(n) = digits.parse::<u32>() {
                    max = max.max(n);
                }
            }
        }
    }
    (max + 1).min(max_seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PNG 内存编码（WIC 内存流）冒烟：结果必须是合法 PNG（签名 + IHDR 宽高）。
    /// 需要 COM/WIC，标记 #[ignore] 供显式运行验证（`cargo test encode_png -- --ignored`）。
    #[test]
    #[ignore = "需要 COM/WIC，供显式运行验证"]
    fn encode_png_produces_valid_png() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        // 2×2 纯色 BGRA
        let bgra = [1u8, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255];
        let out = encode_png(&bgra, 2, 2).expect("encode_png 应成功");
        // PNG 签名
        assert_eq!(&out[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // IHDR 块类型 + 宽高（大端）
        assert_eq!(&out[12..16], b"IHDR");
        let w = u32::from_be_bytes([out[16], out[17], out[18], out[19]]);
        let h = u32::from_be_bytes([out[20], out[21], out[22], out[23]]);
        assert_eq!((w, h), (2, 2));
        // 解码回读：像素应与输入一致（WIC PNG 无损）
        // （这里只验容器正确性；像素级由 --smoke / E2E 覆盖）
    }

    /// encode→decode 往返：像素与尺寸应一致（WIC PNG 无损）。需 COM/WIC，显式运行。
    #[test]
    #[ignore = "需要 COM/WIC，供显式运行验证"]
    fn decode_png_roundtrip() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        let bgra: Vec<u8> = (0..4u8).flat_map(|i| [i, i + 10, i + 20, 255]).collect();
        let png = encode_png(&bgra, 2, 2).expect("encode_png");
        let (w, h, back) = decode_png(&png).expect("decode_png");
        assert_eq!((w, h), (2, 2));
        assert_eq!(back, bgra);
    }

    #[test]
    fn bgr24_drops_alpha_and_keeps_order() {
        // 两个像素：BGRA = (1,2,3,255) 与 (4,5,6,255)
        let bgra = [1u8, 2, 3, 255, 4, 5, 6, 255];
        let out = to_bgr24(&bgra, 2, 1);
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(out.len(), 2 * 3);
    }

    #[test]
    fn bgr24_handles_odd_width() {
        // 宽 3（stride 非 4 对齐）也必须逐像素正确
        let bgra: Vec<u8> = (0..3u8).flat_map(|i| [i, i + 1, i + 2, 255]).collect();
        let out = to_bgr24(&bgra, 3, 1);
        assert_eq!(out.len(), 9);
        assert_eq!(&out[..3], &[0, 1, 2]);
        assert_eq!(&out[6..9], &[2, 3, 4]);
    }

    #[test]
    fn default_name_uses_format_extension() {
        let n = default_file_name(SaveFormat::Jpeg, None);
        assert!(n.starts_with("截图_"), "实际：{n}");
        assert!(n.ends_with(".jpg"), "实际：{n}");
        assert!(n.contains('_'), "实际：{n}");
        assert!(default_file_name(SaveFormat::Bmp, None).ends_with(".bmp"));
        assert!(default_file_name(SaveFormat::Png, None).ends_with(".png"));
    }

    #[test]
    fn next_seq_counts_existing_files_of_the_day() {
        let dir = std::env::temp_dir().join(format!("pixshot_seq_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().into_owned();

        // 空目录 → 01
        assert_eq!(next_seq(Some(&d), "20261025"), 1);

        for name in [
            "截图_20261025_01.png",
            "截图_20261025_02.png",
            "截图_20261025_07.PNG",
            "截图_20261025_x.png",   // 非数字，忽略
            "截图_20261024_09.png",  // 非当日，忽略
            "其他_20261025_50.png",  // 前缀不符，忽略
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        assert_eq!(next_seq(Some(&d), "20261025"), 8);

        // 达到上限后固定 99
        std::fs::write(dir.join("截图_20261025_99.png"), b"x").unwrap();
        assert_eq!(next_seq(Some(&d), "20261025"), 99);

        // 目录缺失时回退 01
        assert_eq!(next_seq(None, "20261025"), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
