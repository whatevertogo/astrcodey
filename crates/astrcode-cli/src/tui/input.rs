//! Keyboard input → Action mapping.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Actions that drive the TUI event loop.
#[derive(Debug, Clone)]
pub enum Action {
    Quit,
    Tick,
    Key(KeyEvent),
}

/// Map a crossterm key event to an Action.
pub fn map_key(event: KeyEvent) -> Option<Action> {
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    match event {
        KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => Some(Action::Quit),
        KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => Some(Action::Quit),
        _ => Some(Action::Key(event)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_key_release_events() {
        let event = KeyEvent::new_with_kind(
            KeyCode::Char('你'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(map_key(event).is_none());
    }

    #[test]
    fn keeps_press_events() {
        let event =
            KeyEvent::new_with_kind(KeyCode::Char('好'), KeyModifiers::NONE, KeyEventKind::Press);
        assert!(matches!(map_key(event), Some(Action::Key(_))));
    }
}
