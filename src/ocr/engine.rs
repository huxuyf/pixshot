//! OCR 引擎抽象层：把「用哪个引擎识别」与识别管线（上采样、多变体跑分、按行重建）解耦。
//!
//! 当前只有系统自带的 `Windows.Media.Ocr` 一种实现（见 [`super::winrt`]）。增强版后续
//! 要内置体积更大的高精度引擎（自带模型、识别率更高）时，只需三步：
//!
//! 1. 新增 `src/ocr/<引擎名>.rs`，实现 [`OcrEngineImpl`]；
//! 2. 在 [`create`] 中按配置/能力挑选引擎（例如「配置指定内置 → 内置 → 回退系统」）；
//! 3. 把该引擎的依赖写进 `Cargo.toml` 并**设为 optional**（或单独 profile），
//!    这样不启用时不会进入产物，精简版的体积不受影响。
//!
//! 上层管线只依赖本 trait，不感知具体引擎。**注意**：引擎返回的是无置信度的纯文本，
//! 因此仍由管线用启发式评分在候选图之间取优；新引擎若能给出字符级置信度，可在
//! [`OcrEngineImpl`] 之上另加一条打分路径，而不必改动现有启发式。

use crate::error::Result;

/// 单个 OCR 引擎需要提供的最小能力。
pub trait OcrEngineImpl {
    /// 引擎标识，用于日志与诊断（如 `"winrt"`、将来的 `"paddle"`）。
    fn id(&self) -> &'static str;

    /// 引擎能接受的最大图像边长（像素）。管线据此决定上采样/下采样目标。
    fn max_dimension(&self) -> i32;

    /// 识别一幅 BGRA 图像（自上而下，行距 = width×4），返回重建后的文本。
    ///
    /// 实现方负责把引擎返回的行结构还原成换行文本；清理中文假空格由
    /// [`super::collapse_cjk_spaces`] 统一处理，各引擎保持一致。
    fn recognize(&self, bgra: &[u8], w: i32, h: i32) -> Result<String>;
}

/// 选择并创建可用引擎；无可用引擎时返回 `None`（调用方转成面向用户的提示）。
///
/// 目前恒为系统 WinRT 引擎。将来接入内置引擎时，本函数是唯一的挑选点。
pub fn create() -> Option<Box<dyn OcrEngineImpl>> {
    super::winrt::create().map(|e| Box::new(e) as Box<dyn OcrEngineImpl>)
}
