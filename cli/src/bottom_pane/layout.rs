use ratatui::layout::{Rect, Size};

use super::BottomPaneState;
use crate::state::ActiveOverlay;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceLayout {
    pub screen: Rect,
    pub viewport: Rect,
    pub overlay: Option<Rect>,
}

impl SurfaceLayout {
    pub fn new(size: Size, overlay: ActiveOverlay, pane: &BottomPaneState) -> Self {
        let screen = Rect::new(0, 0, size.width, size.height);
        if overlay.is_open() {
            return Self {
                screen,
                viewport: screen,
                overlay: Some(screen),
            };
        }

        let viewport_height = pane.desired_height(size.height);
        let viewport = Rect::new(
            0,
            size.height.saturating_sub(viewport_height),
            size.width,
            viewport_height,
        );
        Self {
            screen,
            viewport,
            overlay: None,
        }
    }

    pub fn viewport_height(self) -> u16 {
        self.viewport.height
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Size;

    use super::SurfaceLayout;
    use crate::{bottom_pane::BottomPaneState, state::ActiveOverlay};

    #[test]
    fn overlay_uses_full_screen_viewport() {
        let layout = SurfaceLayout::new(
            Size::new(80, 24),
            ActiveOverlay::Browser,
            &BottomPaneState::default(),
        );
        assert_eq!(layout.viewport.height, 24);
        assert_eq!(layout.overlay, Some(layout.screen));
    }

    #[test]
    fn inline_layout_anchors_viewport_to_bottom() {
        let pane = BottomPaneState::default();
        let layout = SurfaceLayout::new(Size::new(80, 24), ActiveOverlay::None, &pane);
        assert_eq!(layout.viewport.bottom(), 24);
        assert!(layout.viewport.height >= 4);
    }
}
