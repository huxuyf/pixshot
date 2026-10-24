//! OCR：Windows.Media.Ocr（WinRT，系统自带，支持中英文等多语言）。
//!
//! 引擎查找顺序：
//! 1. 配置文件中指定的 `ocr_lang`（如 `zh-Hans-CN`）；
//! 2. 系统用户配置语言；
//! 3. `zh-Hans-CN` → `zh-Hans` → `en-US`；
//! 4. 任意可用语言。
//!
//! 本模块不含 GUI，可在任意后台线程调用（自行初始化 COM/MTA）。

use crate::error::{AppError, Result};
use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// 对 BGRA（自上而下）图像执行 OCR，返回识别文本。
/// 须在非 GUI 线程调用（本函数自行初始化 COM）。
pub fn recognize(bgra: &[u8], w: i32, h: i32) -> Result<String> {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() {
        let r = recognize_inner(bgra, w, h);
        unsafe { CoUninitialize() };
        r
    } else {
        Err(AppError::Ocr(format!("CoInitializeEx 失败: {hr}")))
    }
}

fn recognize_inner(bgra: &[u8], w: i32, h: i32) -> Result<String> {
    if w <= 0 || h <= 0 {
        return Err(AppError::Ocr("图像尺寸无效".into()));
    }
    if bgra.len() < (w as usize) * (h as usize) * 4 {
        return Err(AppError::Ocr("图像数据不完整".into()));
    }
    let engine = create_engine().ok_or_else(|| {
        AppError::Ocr(
            "系统未安装可用的 OCR 语言包。请在 设置 → 时间和语言 → 语言和区域 中添加\
             中文（简体）或英文语言，并包含“文字识别”可选功能。"
                .into(),
        )
    })?;

    // Windows.Media.Ocr 有最大尺寸限制，超出时等比缩小
    let max = OcrEngine::MaxImageDimension().unwrap_or(4096) as i32;
    let (sw, sh, data): (i32, i32, Vec<u8>) = if w > max || h > max {
        let f = (max as f32 / w.max(h) as f32).min(1.0);
        let nw = ((w as f32 * f) as i32).max(1);
        let nh = ((h as f32 * f) as i32).max(1);
        (
            nw,
            nh,
            resize_nearest(bgra, w as usize, h as usize, nw as usize, nh as usize),
        )
    } else {
        (w, h, bgra.to_vec())
    };

    let buf = CryptographicBuffer::CreateFromByteArray(&data)?;
    let bmp = SoftwareBitmap::CreateCopyFromBuffer(&buf, BitmapPixelFormat::Bgra8, sw, sh)?;
    let result = engine.RecognizeAsync(&bmp)?.get()?;
    let text = result.Text()?.to_string();
    Ok(text.trim().to_string())
}

/// 依次尝试多种方式创建 OCR 引擎。
fn create_engine() -> Option<OcrEngine> {
    // 1. 配置文件指定的语言
    if let Some(tag) = crate::config::get().ocr_lang {
        if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(tag.as_str())) {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                crate::diag::log(&format!("[ocr] 使用配置语言 {tag}"));
                return Some(e);
            }
            crate::diag::log(&format!("[ocr] 配置语言 {tag} 不可用，继续回退"));
        }
    }
    // 2. 系统用户配置的语言
    if let Ok(e) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Some(e);
    }
    // 3. 常用语言回退
    for tag in ["zh-Hans-CN", "zh-Hans", "en-US"] {
        if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(tag)) {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                return Some(e);
            }
        }
    }
    // 4. 任意可用语言
    if let Ok(langs) = OcrEngine::AvailableRecognizerLanguages() {
        if let Ok(size) = langs.Size() {
            for i in 0..size {
                if let Ok(lang) = langs.GetAt(i) {
                    if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                        return Some(e);
                    }
                }
            }
        }
    }
    None
}

/// 最近邻缩放（仅用于超出 OCR 尺寸限制时的降采样）。
fn resize_nearest(bgra: &[u8], w: usize, h: usize, nw: usize, nh: usize) -> Vec<u8> {
    let mut out = vec![0u8; nw * nh * 4];
    if w == 0 || h == 0 || nw == 0 || nh == 0 {
        return out;
    }
    for y in 0..nh {
        let sy = (y * h / nh).min(h - 1);
        for x in 0..nw {
            let sx = (x * w / nw).min(w - 1);
            let s = (sy * w + sx) * 4;
            let d = (y * nw + x) * 4;
            if s + 4 <= bgra.len() {
                out[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每像素的值编码为 (x, y) 便于验证采样点
    fn grid(w: usize, h: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                v[i] = x as u8;
                v[i + 1] = y as u8;
                v[i + 3] = 255;
            }
        }
        v
    }

    #[test]
    fn resize_same_size_is_copy() {
        let src = grid(4, 4);
        let out = resize_nearest(&src, 4, 4, 4, 4);
        assert_eq!(out, src);
    }

    #[test]
    fn resize_downscales_and_picks_nearest() {
        let src = grid(8, 8);
        let out = resize_nearest(&src, 8, 8, 4, 4);
        assert_eq!(out.len(), 4 * 4 * 4);
        // 目标 (1,1) 对应源 (2,2)
        let i = (1 * 4 + 1) * 4;
        assert_eq!(out[i], 2);
        assert_eq!(out[i + 1], 2);
    }

    #[test]
    fn resize_never_panics_on_degenerate_input() {
        // 0 尺寸 / 数据不足时不得 panic（OCR 链路的边界保险）
        assert!(resize_nearest(&[], 0, 0, 0, 0).is_empty());
        assert_eq!(resize_nearest(&[0u8; 16], 2, 2, 1, 1).len(), 4);
    }
}
