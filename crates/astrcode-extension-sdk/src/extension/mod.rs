//! 扩展系统类型定义。
//!
//! 扩展是 astrcode 的主要扩展机制。技能、Agent 配置、自定义工具和斜杠命令
//! 都通过这里定义的稳定契约挂入宿主。
//!
//! 本模块只定义契约（trait、capability、hook 类型）：扩展的发现、加载、
//! 路由与进程管理位于 `astrcode-extensions`。

mod events;
mod hooks;
mod http;
mod registrar;
mod runtime;
mod tool_context;

pub use astrcode_core::{
    compaction::{CompactStrategy, CompactTrigger},
    tool::SessionToolSelection,
};
pub use events::*;
pub use hooks::*;
pub use http::*;
pub use registrar::*;
pub use runtime::*;
pub use tool_context::*;

pub use crate::authoring_runtime::{Extension, ExtensionCtx};
