//! astrcode-extensions: 扩展/钩子系统。
//!
//! 负责生命周期事件分发、扩展加载、钩子模式强制执行以及扩展上下文提供。
//! 磁盘扩展为 s5r 子进程（stdio 长度前缀帧）；内置扩展为进程内 Rust crate。

mod extension_manifest;
pub mod host_router;
pub mod loader;
mod process_supervision;
mod remote_manifest;
pub mod runner;
pub mod s5r_ext;

pub use astrcode_extension_sdk::extension::Extension;
pub use host_router::{
    HostBackends, HostRouter, PublicHttpDispatcher, build_host_router,
    build_host_router_with_public_http_dispatcher,
};
