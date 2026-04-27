//! astrcode-extensions: Extension/hook system.
//!
//! Lifecycle event dispatch, extension loading, hook mode enforcement,
//! and extension context provisioning. This is the primary extensibility
//! mechanism — skills, agent profiles, custom tools are all extensions.

pub mod context;
pub mod events;
pub mod ffi;
pub mod loader;
pub mod native_ext;
pub mod runtime;
pub mod runner;
