//! 统一错误类型（沿用本仓库 screen_recorder 的 error.rs 模式）。

use windows::core::Error as WinError;
use std::fmt;

#[derive(Debug)]
pub enum AppError {
    /// Windows API 调用失败。
    Win32(WinError),
    /// I/O 错误。
    Io(std::io::Error),
    /// 本地逻辑检查错误。
    Config(String),
    /// OCR 相关错误。
    Ocr(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win32(e) => write!(f, "Windows API 错误: {e}"),
            Self::Io(e) => write!(f, "I/O 错误: {e}"),
            Self::Config(msg) => write!(f, "{msg}"),
            Self::Ocr(msg) => write!(f, "OCR 错误: {msg}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Win32(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<WinError> for AppError {
    fn from(e: WinError) -> Self {
        Self::Win32(e)
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
