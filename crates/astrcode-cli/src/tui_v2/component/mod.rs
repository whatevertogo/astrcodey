//! Component trait, Container, and Overlay stack.
//!
//! Design: codex-rs `Renderable` signature (render to ratatui Buffer) merged with
//! pi-mono `Component` concept (invalidate cache, handle input).

use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    prelude::Widget,
    widgets::Clear,
};

pub mod composer;
pub mod footer;
pub mod slash_palette;
pub mod tool_row;
pub mod transcript;

// ─── Core trait ──────────────────────────────────────────────────────────────

/// Result of a component handling a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Event was consumed; stop propagation.
    Handled,
    /// Event was not consumed; let parent handle it.
    NotHandled,
    /// Component requests the application to quit.
    Quit,
}

/// All renderable TUI components implement this trait.
///
/// Signature follows codex-rs `Renderable` (render to Buffer + desired_height)
/// extended with pi-mono's `handleInput` / `invalidate` concepts.
pub trait Component: Send {
    /// Draw the component into `buf` within `area`.
    fn render(&mut self, area: Rect, buf: &mut Buffer);

    /// Expected height in rows for the given terminal width.
    /// Used by `Container` to allocate `Constraint::Length` slices.
    fn desired_height(&self, width: u16) -> u16;

    /// Cursor position relative to the terminal origin, if this component
    /// should own the hardware cursor when focused.
    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    /// Handle a keyboard event. Returns `Handled` if consumed.
    fn handle_key(&mut self, _key: &KeyEvent) -> KeyOutcome {
        KeyOutcome::NotHandled
    }

    /// Handle bracketed-paste text. Returns `Handled` if consumed.
    fn handle_paste(&mut self, _text: &str) -> KeyOutcome {
        KeyOutcome::NotHandled
    }

    /// Invalidate any cached render state (e.g. when width or theme changes).
    fn invalidate(&mut self) {}
}

// ─── Container ───────────────────────────────────────────────────────────────

/// Vertical stack of components.
///
/// Mirrors pi-mono `Container { children: Component[] }` but uses ratatui
/// `Layout::vertical` with `Constraint::Length(desired_height)` for allocation.
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    pub fn push(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }

    pub fn children_mut(&mut self) -> &mut Vec<Box<dyn Component>> {
        &mut self.children
    }
}

impl Component for Container {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if self.children.is_empty() || area.height == 0 {
            return;
        }
        let constraints: Vec<Constraint> = self
            .children
            .iter()
            .map(|c| Constraint::Length(c.desired_height(area.width)))
            .collect();
        let chunks = Layout::vertical(constraints).split(area);
        for (child, chunk) in self.children.iter_mut().zip(chunks.iter()) {
            child.render(*chunk, buf);
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.children.iter().map(|c| c.desired_height(width)).sum()
    }

    fn handle_key(&mut self, key: &KeyEvent) -> KeyOutcome {
        for child in &mut self.children {
            if child.handle_key(key) == KeyOutcome::Handled {
                return KeyOutcome::Handled;
            }
        }
        KeyOutcome::NotHandled
    }

    fn handle_paste(&mut self, text: &str) -> KeyOutcome {
        for child in &mut self.children {
            if child.handle_paste(text) == KeyOutcome::Handled {
                return KeyOutcome::Handled;
            }
        }
        KeyOutcome::NotHandled
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

// ─── Overlay stack ────────────────────────────────────────────────────────────

/// Anchor position for overlay components (mirrors pi-mono `OverlayAnchor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Center,
    BottomCenter,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// A single overlay entry (mirrors pi-mono overlay stack entry).
pub struct OverlayEntry {
    pub component: Box<dyn Component>,
    pub anchor: OverlayAnchor,
    /// Higher = rendered on top.
    pub focus_order: u16,
    pub hidden: bool,
    /// Width as fraction of terminal width (0.0–1.0).
    pub width_pct: f32,
    /// Fixed height in rows (None = desired_height).
    pub height: Option<u16>,
}

/// Stack of overlay components rendered on top of the base container.
pub struct OverlayStack {
    entries: Vec<OverlayEntry>,
    counter: u16,
}

impl OverlayStack {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            counter: 0,
        }
    }

    pub fn push(&mut self, entry: OverlayEntry) {
        self.entries.push(entry);
    }

    pub fn remove_by_focus_order(&mut self, focus_order: u16) {
        self.entries.retain(|e| e.focus_order != focus_order);
    }

    pub fn next_focus_order(&mut self) -> u16 {
        self.counter += 1;
        self.counter
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render all visible overlays on top of `base_area`.
    pub fn render(&mut self, base_area: Rect, buf: &mut Buffer) {
        // Sort by focus_order ascending so highest is drawn last (on top).
        self.entries.sort_by_key(|e| e.focus_order);
        for entry in &mut self.entries {
            if entry.hidden {
                continue;
            }
            let popup = overlay_rect(base_area, entry.anchor, entry.width_pct, entry.height);
            // Clear the popup area first.
            Clear.render(popup, buf);
            entry.component.render(popup, buf);
        }
    }

    /// Forward key to the topmost visible overlay. Returns Handled if consumed.
    pub fn handle_key(&mut self, key: &KeyEvent) -> KeyOutcome {
        // Topmost = highest focus_order.
        if let Some(entry) = self
            .entries
            .iter_mut()
            .filter(|e| !e.hidden)
            .max_by_key(|e| e.focus_order)
        {
            return entry.component.handle_key(key);
        }
        KeyOutcome::NotHandled
    }
}

fn overlay_rect(base: Rect, anchor: OverlayAnchor, width_pct: f32, height: Option<u16>) -> Rect {
    let w = ((base.width as f32 * width_pct) as u16)
        .max(24)
        .min(base.width);
    let h = height.unwrap_or(10).min(base.height);
    let x = match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::BottomLeft => base.x,
        OverlayAnchor::TopRight | OverlayAnchor::BottomRight => {
            base.x + base.width.saturating_sub(w)
        },
        _ => base.x + (base.width.saturating_sub(w)) / 2,
    };
    let y = match anchor {
        OverlayAnchor::BottomCenter | OverlayAnchor::BottomLeft | OverlayAnchor::BottomRight => {
            base.y + base.height.saturating_sub(h + 3)
        },
        _ => base.y,
    };
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
