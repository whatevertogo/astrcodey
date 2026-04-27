//! Keyboard input → Action mapping.

use crossterm::event::{KeyEvent, KeyEventKind};

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

    Some(Action::Key(event))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

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

    #[test]
    fn ctrl_c_is_not_a_quit_shortcut() {
        let event = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        );
        assert!(matches!(map_key(event), Some(Action::Key(_))));
    }
}
