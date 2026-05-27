//! 将附件输入规范化为 [`UserPromptParts`]。
//!
//! 边界层（server / CLI）负责将 protocol wire 类型映射为本模块的 [`PromptAttachment`]。

use astrcode_core::user_prompt::{UserImagePart, UserPromptParts};
use thiserror::Error;

use crate::image_processing::{self, ImageProcessingError};

/// 附件输入（与 protocol wire 解耦，由边界层映射）。
#[derive(Debug, Clone)]
pub struct PromptAttachment {
    pub filename: String,
    pub content: String,
    pub media_type: String,
}

#[derive(Debug, Error)]
pub enum PromptAttachmentError {
    #[error("prompt must include text or at least one image")]
    Empty,
    #[error(transparent)]
    Image(#[from] ImageProcessingError),
    #[error(
        "unsupported attachment `{filename}` with media type `{media_type}` (only image/* is \
         supported)"
    )]
    UnsupportedAttachment {
        filename: String,
        media_type: String,
    },
}

/// 从文本与附件构建可提交的 [`UserPromptParts`]。
pub fn build_user_prompt(
    text: String,
    attachments: &[PromptAttachment],
) -> Result<UserPromptParts, PromptAttachmentError> {
    let images = images_from_attachments(attachments)?;
    let input = UserPromptParts { text, images };
    if !input.is_submittable() {
        return Err(PromptAttachmentError::Empty);
    }
    Ok(input)
}

fn images_from_attachments(
    attachments: &[PromptAttachment],
) -> Result<Vec<UserImagePart>, PromptAttachmentError> {
    let mut images = Vec::new();
    for attachment in attachments {
        if !attachment.media_type.starts_with("image/") {
            return Err(PromptAttachmentError::UnsupportedAttachment {
                filename: attachment.filename.clone(),
                media_type: attachment.media_type.clone(),
            });
        }
        let encoded = if attachment.content.starts_with("data:") {
            image_processing::decode_from_data_url(&attachment.content)?
        } else {
            image_processing::encode_from_bytes(
                attachment.content.as_bytes(),
                &attachment.media_type,
            )?
        };
        images.push(UserImagePart {
            filename: attachment.filename.clone(),
            media_type: encoded.mime.clone(),
            base64: encoded.to_base64(),
        });
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_image_attachment() {
        let err = build_user_prompt(
            "hello".into(),
            &[PromptAttachment {
                filename: "notes.txt".into(),
                content: "plain".into(),
                media_type: "text/plain".into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PromptAttachmentError::UnsupportedAttachment { .. }
        ));
    }

    #[test]
    fn accepts_text_only_prompt() {
        let input = build_user_prompt("hello".into(), &[]).unwrap();
        assert!(input.images.is_empty());
        assert_eq!(input.text, "hello");
    }
}
