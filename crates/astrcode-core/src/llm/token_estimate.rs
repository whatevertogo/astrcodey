//! Provider 请求的轻量 token 估算。
//!
//! 这里刻意使用稳定的字符启发式，而不是 provider 专用 tokenizer；调用方应优先采用
//! provider 返回的真实用量。

use super::{LlmContent, LlmMessage};

const MESSAGE_BASE_TOKENS: usize = 6;
const TOOL_CALL_BASE_TOKENS: usize = 12;
const CHARS_PER_TOKEN: usize = 4;
const REQUEST_PADDING_NUMERATOR: usize = 4;
const REQUEST_PADDING_DENOMINATOR: usize = 3;

/// 估算 provider 请求的输入 token，并加入保守 padding。
pub fn estimate_request_tokens(messages: &[LlmMessage], system_prompt: Option<&str>) -> usize {
    let system_tokens = system_prompt.map_or(0, estimate_text_tokens);
    let raw_total = system_tokens + messages.iter().map(estimate_message_tokens).sum::<usize>();
    raw_total
        .saturating_mul(REQUEST_PADDING_NUMERATOR)
        .div_ceil(REQUEST_PADDING_DENOMINATOR)
}

/// 估算单条消息的 token，包含固定消息开销。
pub fn estimate_message_tokens(message: &LlmMessage) -> usize {
    MESSAGE_BASE_TOKENS
        + message
            .content
            .iter()
            .map(estimate_content_tokens)
            .sum::<usize>()
}

/// 按四个字符约等于一个 token 的稳定启发式估算文本。
pub fn estimate_text_tokens(text: &str) -> usize {
    estimate_char_tokens(text.chars().count()).max(1)
}

/// 按四个字符约等于一个 token 估算字符数量；零个字符返回零。
pub fn estimate_char_tokens(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// 将 token 预算换算为同一启发式下的最大字符预算。
pub fn estimate_char_budget(tokens: usize) -> usize {
    tokens.saturating_mul(CHARS_PER_TOKEN)
}

fn estimate_content_tokens(content: &LlmContent) -> usize {
    match content {
        LlmContent::Text { text } => estimate_text_tokens(text),
        LlmContent::Image { base64, .. } => {
            // Base64 是 ASCII，字节长度等于字符数，无需扫描整段图片数据。
            estimate_char_tokens(base64.len()).max(1)
        },
        LlmContent::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            TOOL_CALL_BASE_TOKENS
                + estimate_text_tokens(call_id)
                + estimate_text_tokens(name)
                + estimate_text_tokens(&arguments.to_string())
        },
        LlmContent::ToolResult {
            tool_call_id,
            content,
            ..
        } => estimate_text_tokens(tool_call_id) + estimate_text_tokens(content),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    #[test]
    fn mixed_request_estimate_preserves_content_overhead_and_padding() {
        let mut message = LlmMessage::user("1234");
        message.content.extend([
            LlmContent::Image {
                base64: "1234".into(),
                media_type: "image/png".into(),
                filename: None,
            },
            LlmContent::ToolCall {
                call_id: "1".into(),
                name: String::new(),
                arguments: json!({}),
            },
            LlmContent::ToolResult {
                tool_call_id: String::new(),
                content: "12345".into(),
                is_error: false,
            },
        ]);

        assert_eq!(estimate_text_tokens(""), 1);
        assert_eq!(estimate_char_tokens(0), 0);
        assert_eq!(estimate_char_budget(2), 8);
        assert_eq!(estimate_char_budget(usize::MAX), usize::MAX);
        assert_eq!(estimate_message_tokens(&message), 26);
        assert_eq!(estimate_request_tokens(&[message], Some("12345")), 38);
    }
}
