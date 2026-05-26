//! astrcode-extensions: 扩展/钩子系统。
//!
//! 负责生命周期事件分发、扩展加载、钩子模式强制执行以及扩展上下文提供。
//! 磁盘扩展为 s5r 子进程（stdio 长度前缀帧）；内置扩展为进程内 Rust crate。

pub mod extension_manifest;
pub mod host_router;
pub mod loader;
pub mod remote_manifest;
pub mod runner;
pub mod s5r_ext;

pub use host_router::{HostRouter, build_host_router};
