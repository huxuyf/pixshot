//! 标注引擎（矩形 / 箭头 / 步骤序号）——见设计文档《标注引擎 MVP 设计文档》。
//!
//! 分层：
//! - **纯逻辑层**（geometry / style / shape / doc / hit_test / controller）：不依赖 Win32，
//!   全部单元可测；
//! - **Win32 集成层**（render_gdiplus / session）：GDI+ 绘制、表面管理、合成导出。
//!
//! 对外入口：[`AnnotSession`]（状态机）、[`composite`]（导出合成）、[`Tool`]（工具）。

pub mod controller;
pub mod doc;
pub mod geometry;
pub mod hit_test;
pub mod render_gdiplus;
pub mod session;
pub mod shape;
pub mod style;

pub use controller::{Action, AnnotSession, CursorKind, Tool};
pub use doc::AnnotDoc;
pub use geometry::{Pt, Rect};
pub use session::{composite, render_into, DibSurface};
