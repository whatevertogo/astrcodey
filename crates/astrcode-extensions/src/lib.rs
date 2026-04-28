//! astrcode-extensions: 扩展/钩子系统。
//!
//! 负责生命周期事件分发、扩展加载、钩子模式强制执行以及扩展上下文提供。
//! 这是主要的可扩展性机制 — 技能、Agent 配置文件、自定义工具都是扩展。

pub mod context;
pub mod events;
pub mod ffi;
pub mod loader;
pub mod native_ext;
pub mod runner;
pub mod runtime;
