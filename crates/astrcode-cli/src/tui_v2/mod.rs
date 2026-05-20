//! TUI v2 — rewrite of crates/astrcode-cli/src/tui/.
//!
//! Architecture:
//! - Component trait (codex Renderable signature: render(Rect, &mut Buffer) + desired_height)
//! - Container + OverlayStack (pi-mono design)
//! - FrameRequester actor (codex design, 120 FPS cap)
//! - AdaptiveChunkingPolicy for streaming (codex design)
//! - ToolRenderer / MessageRenderer registries (pi-mono design)

// tui_v2 is a work-in-progress module. Dead-code warnings are expected until
// Phase 7 wires it into the main event loop.
#![allow(dead_code, unused_imports)]

pub(crate) mod component;
pub(crate) mod custom_terminal;
pub(crate) mod frame;
pub(crate) mod insert_history;
pub(crate) mod terminal_probe;
pub(crate) mod theme;

// Phases 2-7 modules (stubs until implemented)
pub(crate) mod command;
pub(crate) mod ext;
pub(crate) mod render;
pub(crate) mod store;
pub(crate) mod streaming;

mod app;
mod terminal;

use std::io;

/// TUI entry point — called from main.rs.
pub async fn run() -> io::Result<()> {
    // TODO: implement in Phase 7
    Err(io::Error::other("tui_v2 not yet implemented"))
}
