//! OCR 引擎抽象层：把「用哪个引擎识别」与识别管线（上采样、多变体跑分、按行重建）解耦。
//!
//! 两种引擎：
//! - **本地 ONNX 推理链**（[`super::onnx`]，首选）：DB 文本检测 + CTC 文本识别，自带模型，
//!   能给出**逐行真实置信度**与**文本框几何**，因此标记为 [`OcrEngineImpl::self_contained`]，
//!   由管线直接取行结构做版面聚类，不再走上采样/多候选那套启发式。
//! - **系统 WinRT**（[`super::winrt`]，回退）：Windows.Media.Ocr，无需安装任何引擎、不打包
//!   模型；但只返回无置信度的纯文本，故回退到「多变体跑分取优」的启发式管线。
//!
//! 挑选顺序见 [`create`]：ONNX（模型/运行时齐备）→ WinRT。

use crate::error::Result;

/// 一行识别结果（含真实置信度与原始图像中的轴对齐包围盒）。
///
/// 自包含引擎（ONNX）逐框产出；回退引擎（WinRT）无几何信息，`rect` 为 `None`、
/// `conf` 取 0（表示「引擎不提供置信度」，而非「置信度低」）。
#[derive(Clone, Debug)]
pub struct TextLine {
    /// 该行文本（已清理中文假空格）。
    pub text: String,
    /// 行置信度 0.0–1.0；`0.0` 亦用于「引擎不给置信度」。
    pub conf: f32,
    /// 原始图像像素坐标下的包围盒 `(x, y, w, h)`；无几何信息时为 `None`。
    pub rect: Option<(i32, i32, i32, i32)>,
}

/// 单个 OCR 引擎需要提供的最小能力。
pub trait OcrEngineImpl {
    /// 引擎标识，用于日志与诊断（如 `"onnx"`、`"winrt"`）。
    fn id(&self) -> &'static str;

    /// 引擎能接受的最大图像边长（像素）。管线据此决定上采样/下采样目标。
    fn max_dimension(&self) -> i32;

    /// 识别一幅 BGRA 图像（自上而下，行距 = width×4），返回重建后的文本。
    ///
    /// 实现方负责把引擎返回的行结构还原成换行文本；清理中文假空格由
    /// [`super::collapse_cjk_spaces`] 统一处理，各引擎保持一致。
    fn recognize(&self, bgra: &[u8], w: i32, h: i32) -> Result<String>;

    /// 引擎是否自带检测 + 识别 + 版面（自行处理原始图像）。
    ///
    /// 为 `true` 时管线**不做**上采样与多候选跑分，而是直接取 [`Self::recognize_lines`] 的
    /// 行结构做版面聚类与后处理；为 `false`（默认）时走上采样 + 多变体启发式老路。
    fn self_contained(&self) -> bool {
        false
    }

    /// 返回带几何/置信度的行结构。默认把 [`Self::recognize`] 的结果包成单行（无几何、无置信度），
    /// 因此只有自包含引擎需要覆写。
    fn recognize_lines(&self, bgra: &[u8], w: i32, h: i32) -> Result<Vec<TextLine>> {
        Ok(vec![TextLine {
            text: self.recognize(bgra, w, h)?,
            conf: 0.0,
            rect: None,
        }])
    }
}

/// 选择并创建可用引擎；无可用引擎时返回 `None`（调用方转成面向用户的提示）。
///
/// 挑选顺序：本地 ONNX（`onnxruntime.dll` 与 `model/` 齐备）→ 系统 WinRT
/// （仅 `winrt-ocr` feature 开启时存在）。这是唯一的引擎挑选点。
pub fn create() -> Option<Box<dyn OcrEngineImpl>> {
    match super::onnx::create() {
        Ok(e) => {
            crate::diag::log("[ocr] 选用本地 ONNX 引擎");
            return Some(Box::new(e) as Box<dyn OcrEngineImpl>);
        }
        Err(e) => {
            crate::diag::log(&format!("[ocr] 本地 ONNX 引擎不可用（回退系统 OCR）：{e}"));
        }
    }
    #[cfg(feature = "winrt-ocr")]
    {
        return super::winrt::create().map(|e| Box::new(e) as Box<dyn OcrEngineImpl>);
    }
    #[cfg(not(feature = "winrt-ocr"))]
    {
        crate::diag::log("[ocr] 未启用 winrt-ocr feature，无系统 OCR 回退可用");
        None
    }
}
