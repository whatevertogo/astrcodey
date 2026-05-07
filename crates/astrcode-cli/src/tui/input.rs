//! 键盘输入过滤。
//!
//! 过滤掉按键释放等不需要的事件类型。

use crossterm::event::{KeyEvent, KeyEventKind};

/// 将 crossterm 键盘事件过滤为有效的按键事件。
///
/// 仅处理 `Press`（按下）和 `Repeat`（长按重复）事件，
/// 忽略 `Release`（释放）事件以避免重复触发。
pub fn is_press_event(event: &KeyEvent) -> bool {
    matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
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
        assert!(!is_press_event(&event));
    }

    #[test]
    fn keeps_press_events() {
        let event =
            KeyEvent::new_with_kind(KeyCode::Char('好'), KeyModifiers::NONE, KeyEventKind::Press);
        assert!(is_press_event(&event));
    }

    #[test]
    fn ctrl_c_is_not_filtered() {
        let event = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        );
        assert!(is_press_event(&event));
    }
}
