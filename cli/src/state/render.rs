use std::time::Duration;

use super::StreamRenderMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedLine {
    pub style: WrappedLineStyle,
    pub rewrap_policy: WrappedLineRewrapPolicy,
    pub spans: Vec<WrappedSpan>,
}

impl WrappedLine {
    pub fn plain(style: WrappedLineStyle, content: impl Into<String>) -> Self {
        let content = content.into();
        let spans = if content.is_empty() {
            Vec::new()
        } else {
            vec![WrappedSpan::plain(content)]
        };
        Self {
            style,
            rewrap_policy: WrappedLineRewrapPolicy::Reflow,
            spans,
        }
    }

    pub fn from_spans(style: WrappedLineStyle, spans: Vec<WrappedSpan>) -> Self {
        Self {
            style,
            rewrap_policy: WrappedLineRewrapPolicy::Reflow,
            spans,
        }
    }

    pub fn with_rewrap_policy(mut self, rewrap_policy: WrappedLineRewrapPolicy) -> Self {
        self.rewrap_policy = rewrap_policy;
        self
    }

    pub fn text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>()
    }

    pub fn is_blank(&self) -> bool {
        self.spans.is_empty() || self.text().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrappedLineRewrapPolicy {
    Reflow,
    PreserveAndCrop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedSpan {
    pub style: Option<WrappedSpanStyle>,
    pub content: String,
}

impl WrappedSpan {
    pub fn plain(content: impl Into<String>) -> Self {
        Self {
            style: None,
            content: content.into(),
        }
    }

    pub fn styled(style: WrappedSpanStyle, content: impl Into<String>) -> Self {
        Self {
            style: Some(style),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrappedSpanStyle {
    Strong,
    Emphasis,
    Heading,
    HeadingRule,
    TableBorder,
    TableHeader,
    InlineCode,
    Link,
    ListMarker,
    QuoteMarker,
    CodeFence,
    CodeText,
    TextArt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrappedLineStyle {
    Plain,
    Muted,
    Selection,
    PromptEcho,
    ThinkingLabel,
    ThinkingPreview,
    ThinkingBody,
    ToolLabel,
    ToolBody,
    Notice,
    ErrorText,
    PaletteItem,
    PaletteSelected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveOverlay {
    #[default]
    None,
    Browser,
}

impl ActiveOverlay {
    pub fn is_open(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderState {
    pub frame_dirty: bool,
    pub active_overlay: ActiveOverlay,
    pub last_known_terminal_size: Option<(u16, u16)>,
}

impl RenderState {
    pub fn note_terminal_resize(&mut self, width: u16, height: u16) -> bool {
        let next = Some((width, height));
        let changed = self.last_known_terminal_size != next;
        self.last_known_terminal_size = next;
        if changed {
            self.frame_dirty = true;
        }
        changed
    }

    pub fn set_active_overlay(&mut self, overlay: ActiveOverlay) -> bool {
        if self.active_overlay == overlay {
            return false;
        }
        self.active_overlay = overlay;
        self.frame_dirty = true;
        true
    }

    pub fn mark_dirty(&mut self) {
        self.frame_dirty = true;
    }

    pub fn take_frame_dirty(&mut self) -> bool {
        std::mem::take(&mut self.frame_dirty)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamViewState {
    pub mode: StreamRenderMode,
    pub pending_chunks: usize,
    pub oldest_chunk_age: Duration,
}

impl Default for StreamViewState {
    fn default() -> Self {
        Self {
            mode: StreamRenderMode::Smooth,
            pending_chunks: 0,
            oldest_chunk_age: Duration::ZERO,
        }
    }
}

impl StreamViewState {
    pub fn update(
        &mut self,
        mode: StreamRenderMode,
        pending_chunks: usize,
        oldest_chunk_age: Duration,
    ) {
        self.mode = mode;
        self.pending_chunks = pending_chunks;
        self.oldest_chunk_age = oldest_chunk_age;
    }
}
