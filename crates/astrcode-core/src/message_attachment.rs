//! 用户消息附件（图片、文本文件等），用于 prompt 与 durable `UserMessage`。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 单条附件内容上限（与 TUI / 前端一致）。
pub const MAX_ATTACHMENT_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// 单次 prompt 附件数量上限。
pub const MAX_ATTACHMENTS: usize = 4;

/// 用户消息或 `SubmitPrompt` 附带的资源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageAttachment {
    /// 文件名（含扩展名）。
    pub filename: String,
    /// 载荷：图片/二进制为 Base64；纯文本文件为 UTF-8 文本。
    pub content: String,
    /// MIME 类型（如 `image/png`、`text/plain`）。
    pub media_type: String,
}

/// 附件校验失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentValidationError {
    TooMany { max: usize },
    ContentTooLarge { index: usize, max_bytes: usize },
}

impl fmt::Display for AttachmentValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooMany { max } => write!(f, "at most {max} attachments allowed"),
            Self::ContentTooLarge { index, max_bytes } => write!(
                f,
                "attachment {index} exceeds size limit of {max_bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for AttachmentValidationError {}

/// 校验 prompt 附件数量与单条大小（服务端权威上限）。
pub fn validate_attachments(
    attachments: &[MessageAttachment],
) -> Result<(), AttachmentValidationError> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(AttachmentValidationError::TooMany {
            max: MAX_ATTACHMENTS,
        });
    }
    for (index, attachment) in attachments.iter().enumerate() {
        if attachment.content.len() > MAX_ATTACHMENT_CONTENT_BYTES {
            return Err(AttachmentValidationError::ContentTooLarge {
                index,
                max_bytes: MAX_ATTACHMENT_CONTENT_BYTES,
            });
        }
    }
    Ok(())
}

impl MessageAttachment {
    pub fn image_png(filename: impl Into<String>, base64: impl Into<String>) -> Self {
        Self {
            filename: filename.into(),
            content: base64.into(),
            media_type: "image/png".into(),
        }
    }

    pub fn is_image(&self) -> bool {
        self.media_type.starts_with("image/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_too_many_attachments() {
        let attachments = vec![MessageAttachment::image_png("a.png", "x"); MAX_ATTACHMENTS + 1];
        assert!(matches!(
            validate_attachments(&attachments),
            Err(AttachmentValidationError::TooMany { .. })
        ));
    }

    #[test]
    fn rejects_oversized_content() {
        let attachment = MessageAttachment {
            filename: "big.bin".into(),
            content: "x".repeat(MAX_ATTACHMENT_CONTENT_BYTES + 1),
            media_type: "application/octet-stream".into(),
        };
        assert!(matches!(
            validate_attachments(&[attachment]),
            Err(AttachmentValidationError::ContentTooLarge { .. })
        ));
    }
}
