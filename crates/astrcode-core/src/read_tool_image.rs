//! read 工具成功时内联图片的 tool result content 契约。
//!
//! 生产方：`astrcode_tools` 的 `read_image_file_result`。
//! 消费方：`astrcode_ai::tool_result_wire` 在送模边界解析为 provider 图片块。

use serde::{Deserialize, Serialize};

/// read 工具内联图片载荷的 `type` 判别值。
pub const READ_TOOL_INLINE_IMAGE_TYPE: &str = "image";

/// read 工具内联载荷；当前仅 [`Self::Image`]，扩展新 variant 会强制更新消费方 match。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReadToolInlinePayload {
    /// 内联 base64 图片。
    #[serde(rename = "image")]
    Image {
        #[serde(rename = "mediaType")]
        media_type: String,
        /// Standard base64（RFC 4648），无 `data:` URL 前缀。
        data: String,
    },
}

impl ReadToolInlinePayload {
    pub fn image(media_type: impl Into<String>, base64_data: impl Into<String>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            data: base64_data.into(),
        }
    }

    pub fn to_content_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 从 tool result content 解析内联图片；非图片或字段无效时返回 `None`。
    pub fn parse_image(content: &str) -> Option<(String, String)> {
        match serde_json::from_str(content).ok()? {
            Self::Image { media_type, data } if !media_type.is_empty() && !data.is_empty() => {
                Some((data, media_type))
            },
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_wire_shape() {
        let payload = ReadToolInlinePayload::image("image/png", "aGVsbG8=");
        let json = payload.to_content_string().expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(value["type"], READ_TOOL_INLINE_IMAGE_TYPE);
        assert_eq!(value["mediaType"], "image/png");
        assert_eq!(value["data"], "aGVsbG8=");
    }

    #[test]
    fn parse_image_extracts_base64_and_media_type() {
        let json = ReadToolInlinePayload::image("image/png", "aGVsbG8=")
            .to_content_string()
            .expect("serialize");
        let (data, media) = ReadToolInlinePayload::parse_image(&json).expect("image");
        assert_eq!(data, "aGVsbG8=");
        assert_eq!(media, "image/png");
    }

    #[test]
    fn parse_image_rejects_plain_text() {
        assert!(ReadToolInlinePayload::parse_image("line 1\nline 2").is_none());
    }
}
