//! 进入 session 的领域输入。

use serde::{Deserialize, Serialize};

use crate::message_attachment::MessageAttachment;

/// 一条用户输入；传输层 DTO 在边界映射为此类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
    pub attachments: Vec<MessageAttachment>,
}

impl UserInput {
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    pub fn can_submit(&self) -> bool {
        !self.text.trim().is_empty() || !self.attachments.is_empty()
    }
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self::text_only(text)
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::text_only(text)
    }
}
