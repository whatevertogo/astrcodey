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
    /// 扩展自定义消息类型标识，用于分发到 MessageRendererRegistry。
    pub custom_type: Option<String>,
    /// 扩展自定义消息 payload（JSON），供 MessageRenderer 消费。
    pub payload: Option<serde_json::Value>,
}

impl MessageBody {
    pub fn text(text: String) -> Self {
        Self {
            plain: text,
            render: None,
            custom_type: None,
            payload: None,
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

    pub fn set_text(&mut self, text: String) {
        self.plain = text;
        self.render = None;
        self.custom_type = None;
        self.payload = None;
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

    /// 创建携带自定义类型的消息体，供 `MessageRendererRegistry` 分发渲染。
    ///
    /// `custom_type` 对应 [`MessageRendererRegistry`] 中注册的键。
    /// `payload` 是传给渲染器的 JSON 数据。
    /// `fallback` 是渲染器不可用时的纯文本降级内容。
    pub fn with_custom(custom_type: String, payload: serde_json::Value, fallback: String) -> Self {
        Self {
            plain: fallback,
            render: None,
            custom_type: Some(custom_type),
            payload: Some(payload),
        }
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
    StreamHeader,
    StreamText { role: MessageRole, text: String },
    BlankLine,
}
