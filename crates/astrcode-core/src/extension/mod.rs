//! 扩展系统类型定义。
//!
//! 扩展是 astrcode 的主要扩展机制。技能、Agent 配置、自定义工具和斜杠命令
//! 都通过这里定义的稳定契约挂入宿主。

mod events;
mod hooks;
mod http;
mod registrar;
mod runtime;

pub use events::*;
pub use hooks::*;
pub use http::*;
pub use registrar::*;
pub use runtime::*;
