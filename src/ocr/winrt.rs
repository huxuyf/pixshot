//! 系统自带 OCR 引擎：Windows.Media.Ocr（WinRT）。
//!
//! 无需安装任何引擎、不打包模型，支持中英文等多语言，代价是识别率受系统语言包限制。
//! 语言查找顺序：配置文件的 `ocr_lang` → 系统用户配置语言 → `zh-Hans-CN` / `zh-Hans` /
//! `en-US` → 任意可用语言。

use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;

use super::engine::OcrEngineImpl;
use super::collapse_cjk_spaces;
use crate::error::Result;

/// WinRT OCR 引擎的轻封装（持有已创建的 `OcrEngine`）。
pub struct WinrtEngine {
    engine: OcrEngine,
}

impl OcrEngineImpl for WinrtEngine {
    fn id(&self) -> &'static str {
        "winrt"
    }

    fn max_dimension(&self) -> i32 {
        OcrEngine::MaxImageDimension().unwrap_or(4096) as i32
    }

    /// 按 `OcrResult::Lines()` 逐行重建文本。
    ///
    /// 用 `Lines()` 而非 `result.Text()`：后者把各行用空格拼成一行，中文会丢换行、插空格，
    /// 用户很容易把这当成识别错误。逐行 join 成 `\n` 后，再清掉中文之间被引擎插入的假空格。
    fn recognize(&self, data: &[u8], w: i32, h: i32) -> Result<String> {
        let buf = CryptographicBuffer::CreateFromByteArray(data)?;
        let bmp = SoftwareBitmap::CreateCopyFromBuffer(&buf, BitmapPixelFormat::Bgra8, w, h)?;
        let result = self.engine.RecognizeAsync(&bmp)?.get()?;
        let lines = result.Lines()?;
        let mut parts: Vec<String> = Vec::new();
        for i in 0..lines.Size()? {
            let line = lines.GetAt(i)?;
            parts.push(line.Text()?.to_string());
        }
        Ok(collapse_cjk_spaces(&parts.join("\n")).trim().to_string())
    }
}

/// 依次尝试多种方式创建系统 OCR 引擎；系统未安装可用语言包时返回 `None`。
pub fn create() -> Option<WinrtEngine> {
    // 1. 配置文件指定的语言
    if let Some(tag) = crate::config::get().ocr_lang {
        if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(tag.as_str())) {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                crate::diag::log(&format!("[ocr] 使用配置语言 {tag}"));
                return Some(WinrtEngine { engine: e });
            }
            crate::diag::log(&format!("[ocr] 配置语言 {tag} 不可用，继续回退"));
        }
    }
    // 2. 系统用户配置的语言
    if let Ok(e) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Some(WinrtEngine { engine: e });
    }
    // 3. 常用语言回退
    for tag in ["zh-Hans-CN", "zh-Hans", "en-US"] {
        if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(tag)) {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                return Some(WinrtEngine { engine: e });
            }
        }
    }
    // 4. 任意可用语言
    if let Ok(langs) = OcrEngine::AvailableRecognizerLanguages() {
        if let Ok(size) = langs.Size() {
            for i in 0..size {
                if let Ok(lang) = langs.GetAt(i) {
                    if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                        return Some(WinrtEngine { engine: e });
                    }
                }
            }
        }
    }
    None
}
