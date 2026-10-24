//! 图片保存（WIC，Windows Imaging Component —— 零额外依赖）
//! + 系统保存对话框（GetSaveFileNameW）。
//!
//! 支持 PNG / JPEG / BMP 三种容器（格式由配置文件决定，见 `config::SaveFormat`）：
//! - PNG / BMP：直接写 32bppBGRA（捕获阶段已把 alpha 置满）；
//! - JPEG：不支持 alpha，先转 24bppBGR 再写。
//!
//! 保存成功后把所在目录写回配置，下次对话框默认打开同一目录。

use crate::config::SaveFormat;
use crate::error::{AppError, Result};
use crate::util::to_wide;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatBmp, GUID_ContainerFormatJpeg,
    GUID_ContainerFormatPng, GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppBGRA,
    IWICBitmapEncoder, IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
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
    let default_name = default_file_name(fmt);

    let mut file_buf = [0u16; 260];
    for (i, c) in default_name.encode_utf16().take(259).enumerate() {
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

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR::from_raw(filter.as_ptr()),
        lpstrFile: PWSTR(file_buf.as_mut_ptr()),
        nMaxFile: 260,
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
    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    if !ok.as_bool() {
        return Ok(None); // 用户取消
    }
    let end = file_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(file_buf.len());
    let path = String::from_utf16_lossy(&file_buf[..end]);
    if path.is_empty() {
        return Ok(None);
    }
    save(&path, bgra, w, h, fmt)?;

    // 记住本次目录，供下次对话框使用
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let dir = dir.display().to_string();
        crate::config::update(|c| c.save_dir = Some(dir));
    }
    Ok(Some(path))
}

/// 默认文件名：截图_YYYYMMDD_HHMMSS.<ext>（本地时间，Win32 GetLocalTime，零依赖）。
fn default_file_name(fmt: SaveFormat) -> String {
    let st = unsafe { GetLocalTime() };
    format!(
        "截图_{:04}{:02}{:02}_{:02}{:02}{:02}.{}",
        st.wYear,
        st.wMonth,
        st.wDay,
        st.wHour,
        st.wMinute,
        st.wSecond,
        fmt.ext()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let n = default_file_name(SaveFormat::Jpeg);
        assert!(n.starts_with("截图_"));
        assert!(n.ends_with(".jpg"), "实际：{n}");
        assert!(default_file_name(SaveFormat::Bmp).ends_with(".bmp"));
        assert!(default_file_name(SaveFormat::Png).ends_with(".png"));
    }
}
