//! Render pipeline: RenderSpec → terminal Lines, scrollback entry → Lines.
//!
//! - `render_spec` — Pure functions: `RenderSpec` tree → styled `Line`s, markdown parser, visual
//!   layout engine. No knowledge of Message/ScrollbackEntry.
//! - `scrollback` — Message-aware: `ScrollbackEntry` → `Line`s with role-aware
//!   header/body/separator rendering. Delegates to `render_spec` for rich content.

pub mod render_spec;
pub mod scrollback;

pub use render_spec::{RenderKeyValue, RenderSpec, RenderTone, layout_visual_text, visual_lines};
pub use scrollback::scrollback_entry_to_lines;

pub(super) fn inline_preview(text: &str, max_chars: usize) -> String {
    let mut preview = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((byte_index, _)) = preview.char_indices().nth(max_chars) {
        preview.truncate(byte_index);
        preview.push('…');
    }
    preview
}
