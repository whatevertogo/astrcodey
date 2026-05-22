//! Render pipeline: RenderSpec → terminal Lines, scrollback entry → Lines.
//!
//! - `render_spec` — Pure functions: `RenderSpec` tree → styled `Line`s, markdown parser, visual
//!   layout engine. No knowledge of Message/ScrollbackEntry.
//! - `scrollback` — Message-aware: `ScrollbackEntry` → `Line`s with role-aware
//!   header/body/separator rendering. Delegates to `render_spec` for rich content.

pub mod render_spec;
pub mod scrollback;

// Re-export public API for external callers.
pub use render_spec::{VisualLayout, layout_visual_text, render_spec_to_lines, visual_lines};
pub use scrollback::scrollback_entry_to_lines;
