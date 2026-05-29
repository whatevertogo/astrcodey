//! Shared viewport helpers for panel and streaming layout.

/// Content width for transcript wrapping (matches `build_panel`).
pub(crate) fn content_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .saturating_sub(4)
}

/// Half-open `[start, end)` range keeping `selected` visible in a sliding window.
pub(crate) fn sliding_window_range(
    total: usize,
    selected: usize,
    max_visible: usize,
) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let selected = selected.min(total - 1);
    let window_start = if total <= max_visible || selected < max_visible / 2 {
        0
    } else if selected >= total.saturating_sub(max_visible / 2) {
        total.saturating_sub(max_visible)
    } else {
        selected.saturating_sub(max_visible / 2)
    };
    let window_end = (window_start + max_visible).min(total);
    (window_start, window_end)
}

#[cfg(test)]
mod tests {
    use super::sliding_window_range;

    #[test]
    fn sliding_window_keeps_selection_centered() {
        let (start, end) = sliding_window_range(20, 10, 8);
        assert_eq!(start, 6);
        assert_eq!(end, 14);
    }

    #[test]
    fn sliding_window_clamps_at_start_and_end() {
        assert_eq!(sliding_window_range(20, 1, 8), (0, 8));
        assert_eq!(sliding_window_range(20, 19, 8), (12, 20));
    }

    #[test]
    fn sliding_window_empty_total() {
        assert_eq!(sliding_window_range(0, 0, 8), (0, 0));
    }
}
