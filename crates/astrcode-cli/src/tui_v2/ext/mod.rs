//! Extension renderer traits and registries.
pub mod builtin;
pub mod fallback;
pub mod message;
pub mod tool;

pub use message::{MessageRenderOpts, MessageRenderer, MessageRendererRegistry};
pub use tool::{RenderShell, ToolRenderCtx, ToolRenderer, ToolRendererRegistry};
