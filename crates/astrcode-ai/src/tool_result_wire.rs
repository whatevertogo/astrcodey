//! 将 read 工具的内联图片 JSON 映射为各 provider 的线缆格式。
//!
//! read 工具结果在 session 中仍以 JSON 字符串存储；仅在送模边界解析为图片块。

use astrcode_core::read_tool_image::ReadToolInlinePayload;

/// 解析 read 工具的图片结果；非图片或格式无效时返回 `None`。
pub(crate) fn parse_read_tool_image(content: &str) -> Option<(String, String)> {
    ReadToolInlinePayload::parse_image(content)
}

/// Anthropic `tool_result.content`：图片为 content 数组中的 image 块。
pub(crate) fn anthropic_tool_result_content(content: &str) -> serde_json::Value {
    if let Some((base64, media_type)) = parse_read_tool_image(content) {
        return serde_json::json!([{
            "type": "image",
            "source": {
                "type": "base64",
                "data": base64,
                "media_type": media_type,
            }
        }]);
    }
    serde_json::json!(content)
}

/// OpenAI Responses `function_call_output.output`（支持 input_text + input_image 数组）。
pub(crate) fn openai_responses_tool_result_output(content: &str) -> serde_json::Value {
    if let Some((base64, media_type)) = parse_read_tool_image(content) {
        return serde_json::json!([
            {"type": "input_text", "text": "Read image file."},
            {
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{base64}")
            }
        ]);
    }
    serde_json::json!(content)
}

/// OpenAI Chat Completions `role: tool` 的 content（支持 text + image_url 数组）。
pub(crate) fn openai_chat_tool_result_content(content: &str) -> serde_json::Value {
    if let Some((base64, media_type)) = parse_read_tool_image(content) {
        return serde_json::json!([
            {"type": "text", "text": "Read image file."},
            {
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{media_type};base64,{base64}")
                }
            }
        ]);
    }
    serde_json::json!(content)
}

/// Gemini user `parts`：在 functionResponse 之后追加 inlineData 图片块。
pub(crate) fn gemini_tool_result_parts(
    tool_name: &str,
    result_text: &str,
    is_error: bool,
) -> Vec<serde_json::Value> {
    let function_response = serde_json::json!({
        "functionResponse": {
            "name": tool_name,
            "response": {"output": result_text, "error": is_error}
        }
    });
    let Some((base64, media_type)) = parse_read_tool_image(result_text) else {
        return vec![function_response];
    };
    vec![
        serde_json::json!({
            "functionResponse": {
                "name": tool_name,
                "response": {
                    "output": "Read image file.",
                    "error": is_error
                }
            }
        }),
        serde_json::json!({
            "inlineData": {"mimeType": media_type, "data": base64}
        }),
    ]
}

#[cfg(test)]
mod tests {
    use astrcode_core::read_tool_image::ReadToolInlinePayload;

    use super::*;

    fn sample_image_json() -> String {
        ReadToolInlinePayload::image("image/png", "aGVsbG8=")
            .to_content_string()
            .expect("serialize sample image")
    }

    #[test]
    fn parse_read_tool_image_accepts_valid_payload() {
        let (data, media) = parse_read_tool_image(&sample_image_json()).expect("image");
        assert_eq!(data, "aGVsbG8=");
        assert_eq!(media, "image/png");
    }

    #[test]
    fn parse_read_tool_image_rejects_plain_text() {
        assert!(parse_read_tool_image("line 1\nline 2").is_none());
    }

    #[test]
    fn anthropic_content_uses_image_block() {
        let content = anthropic_tool_result_content(&sample_image_json());
        let blocks = content.as_array().expect("array");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
    }

    #[test]
    fn openai_content_uses_image_url() {
        let content = openai_chat_tool_result_content(&sample_image_json());
        let parts = content.as_array().expect("array");
        assert!(parts.iter().any(|p| p["type"] == "image_url"));
    }

    #[test]
    fn responses_output_uses_input_image() {
        let output = openai_responses_tool_result_output(&sample_image_json());
        let parts = output.as_array().expect("array");
        assert!(parts.iter().any(|p| p["type"] == "input_image"));
    }

    #[test]
    fn responses_output_keeps_plain_text() {
        let output = openai_responses_tool_result_output("line 1\nline 2");
        assert_eq!(output, serde_json::json!("line 1\nline 2"));
    }
}
