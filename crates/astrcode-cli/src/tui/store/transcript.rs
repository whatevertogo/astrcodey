//! Transcript data types: Message, MessageBody, ScrollbackEntry.

use astrcode_core::render::RenderSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
pub struct MessageBody {
    plain: String,
    render: Option<RenderSpec>,
}

impl MessageBody {
    pub fn text(text: String) -> Self {
        Self {
            plain: text,
            render: None,
        }
    }

    pub fn plain_text(&self) -> &str {
        &self.plain
    }

    pub fn render_spec(&self) -> Option<&RenderSpec> {
        self.render.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.plain.is_empty()
    }

    pub fn contains_text(&self, text: &str) -> bool {
        self.plain.contains(text)
    }

    pub fn set_text(&mut self, text: String) {
        self.plain = text;
        self.render = None;
    }

    pub fn append_text(&mut self, text: &str) {
        self.plain.push_str(text);
    }

    pub fn set_render(&mut self, spec: RenderSpec, fallback: String) {
        self.plain = if fallback.is_empty() {
            spec.plain_text_fallback()
        } else {
            fallback
        };
        self.render = Some(spec);
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub label: String,
    pub body: MessageBody,
    pub is_streaming: bool,
    pub key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ScrollbackEntry {
    Message(Message),
    StreamHeader { role: MessageRole, label: String },
    StreamText { role: MessageRole, text: String },
    BlankLine,
}
