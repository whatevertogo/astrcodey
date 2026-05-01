//! 键盘输入到 Action 的映射。
//!
//! 将 crossterm 的底层键盘事件转换为 TUI 事件循环可处理的 Action 枚举，
/// 过滤掉按键释放等不需要的事件类型。
use crossterm::event::{KeyEvent, KeyEventKind};

/// 驱动 TUI 事件循环的动作枚举。
#[derive(Debug, Clone)]
pub enum Action {
    /// 退出 TUI
    Quit,
    /// 刷新时钟周期（用于触发重绘，如终端窗口大小变化）
    Tick,
    /// 键盘按键事件
    Key(KeyEvent),
    /// bracketed paste 文本
    Paste(String),
}

/// 将 crossterm 键盘事件映射为 Action。
///
/// 仅处理 `Press`（按下）和 `Repeat`（长按重复）事件，
/// 忽略 `Release`（释放）事件以避免重复触发。
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
