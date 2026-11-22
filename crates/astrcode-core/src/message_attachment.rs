//! 用户消息附件（图片、文本文件等），用于 prompt 与 durable `UserMessage`。

use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};

/// 单条附件内容上限（原始字节；与 TUI / 前端一致）。
///
/// 图片在线缆上以 Base64 传输，校验与 HTTP body 上限按
/// [`MAX_ATTACHMENT_WIRE_CONTENT_BYTES`] 折算，避免编码膨胀误拒合法图片。
pub const MAX_ATTACHMENT_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// 单条附件在线缆上的内容上限：Base64 每 3 字节编码为 4 个字符。
pub const MAX_ATTACHMENT_WIRE_CONTENT_BYTES: usize = MAX_ATTACHMENT_CONTENT_BYTES.div_ceil(3) * 4;

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
    InvalidImageEncoding { index: usize },
}

impl fmt::Display for AttachmentValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooMany { max } => write!(f, "at most {max} attachments allowed"),
            Self::ContentTooLarge { index, max_bytes } => write!(
                f,
                "attachment {index} exceeds size limit of {max_bytes} bytes"
            ),
            Self::InvalidImageEncoding { index } => {
                write!(f, "attachment {index} is not valid Base64 image content")
            },
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
        if !attachment.is_image() {
            if attachment.content.len() > MAX_ATTACHMENT_CONTENT_BYTES {
                return Err(AttachmentValidationError::ContentTooLarge {
                    index,
                    max_bytes: MAX_ATTACHMENT_CONTENT_BYTES,
                });
            }
            continue;
        }
        if attachment.content.len() > MAX_ATTACHMENT_WIRE_CONTENT_BYTES {
            return Err(AttachmentValidationError::ContentTooLarge {
                index,
                max_bytes: MAX_ATTACHMENT_CONTENT_BYTES,
            });
        }
        let decoded = BASE64_STANDARD
            .decode(&attachment.content)
            .map_err(|_| AttachmentValidationError::InvalidImageEncoding { index })?;
        if decoded.len() > MAX_ATTACHMENT_CONTENT_BYTES {
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

    pub(crate) fn is_image(&self) -> bool {
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
            Err(AttachmentValidationError::ContentTooLarge {
                max_bytes: MAX_ATTACHMENT_CONTENT_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn accepts_base64_image_at_raw_content_limit() {
        let attachment = MessageAttachment {
            filename: "limit.png".into(),
            content: BASE64_STANDARD.encode(vec![0_u8; MAX_ATTACHMENT_CONTENT_BYTES]),
            media_type: "image/png".into(),
        };
        assert!(validate_attachments(&[attachment]).is_ok());
    }

    #[test]
    fn rejects_image_with_invalid_base64_payload() {
        let attachment = MessageAttachment {
            filename: "broken.png".into(),
            content: "not base64 !".into(),
            media_type: "image/png".into(),
        };
        assert!(matches!(
            validate_attachments(&[attachment]),
            Err(AttachmentValidationError::InvalidImageEncoding { index: 0 })
        ));
    }

    #[test]
    fn rejects_image_decoded_above_raw_content_limit() {
        let attachment = MessageAttachment {
            filename: "padded.png".into(),
            content: "A".repeat(MAX_ATTACHMENT_WIRE_CONTENT_BYTES),
            media_type: "image/png".into(),
        };
        assert!(matches!(
            validate_attachments(&[attachment]),
            Err(AttachmentValidationError::ContentTooLarge {
                max_bytes: MAX_ATTACHMENT_CONTENT_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn rejects_base64_image_above_wire_content_limit() {
        let attachment = MessageAttachment {
            filename: "too-big.png".into(),
            content: "A".repeat(MAX_ATTACHMENT_WIRE_CONTENT_BYTES + 4),
            media_type: "image/png".into(),
        };
        assert!(matches!(
            validate_attachments(&[attachment]),
            Err(AttachmentValidationError::ContentTooLarge {
                max_bytes: MAX_ATTACHMENT_CONTENT_BYTES,
                ..
            })
        ));
    }
}
