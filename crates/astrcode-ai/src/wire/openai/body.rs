//! OpenAI-compatible wire request construction.
//!
//! This module is intentionally unaware of HTTP transport and stream parsing. Its job is to encode
//! AstrCode's internal messages/tools into the exact JSON contracts required by Chat Completions
//! and Responses.

use astrcode_core::{
    config::OpenAiApiMode,
    llm::{LlmMessage, LlmRole, PromptCacheRetention, ThinkingLevel},
    tool::ToolDefinition,
};

use crate::serialization::{
    chat_message_to_json, prompt_cache_retention_wire_value, responses_input_items,
    responses_tools_json, stable_hash_hex, system_text, tools_to_json,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenAiRequestConfig<'a> {
    pub api_mode: OpenAiApiMode,
    pub model_id: &'a str,
    pub max_output_tokens: usize,
    pub supports_stream_usage: bool,
    pub supports_prompt_cache_key: bool,
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    pub thinking_level: Option<ThinkingLevel>,
}

pub(crate) fn endpoint_url(api_mode: OpenAiApiMode, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match api_mode {
        OpenAiApiMode::ChatCompletions => {
            if base.ends_with("/chat/completions") {
                base.to_string()
            } else {
                format!("{base}/chat/completions")
            }
        },
        OpenAiApiMode::Responses => {
            if base.ends_with("/responses") {
                base.to_string()
            } else {
                format!("{base}/responses")
            }
        },
    }
}

pub(crate) fn input_tokens_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses/input_tokens") {
        base.to_string()
    } else if base.ends_with("/responses") {
        format!("{base}/input_tokens")
    } else {
        format!("{base}/responses/input_tokens")
    }
}

pub(crate) fn build_request_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    match config.api_mode {
        OpenAiApiMode::ChatCompletions => build_chat_request_body(config, messages, tools),
        OpenAiApiMode::Responses => build_responses_request_body(config, messages, tools),
    }
}

pub(crate) fn build_input_token_count_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let config = OpenAiRequestConfig {
        api_mode: OpenAiApiMode::Responses,
        ..config
    };
    let mut body = build_responses_request_body(config, messages, tools);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("max_output_tokens");
        obj.remove("stream");
        obj.remove("parallel_tool_calls");
    }
    body
}

fn build_chat_request_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let messages_json: Vec<serde_json::Value> = messages.iter().map(chat_message_to_json).collect();

    let mut body = serde_json::json!({
        "model": config.model_id,
        "messages": messages_json,
        "max_tokens": config.max_output_tokens,
        "stream": true,
    });
    if config.supports_stream_usage {
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }

    if !tools.is_empty() {
        body["tools"] = tools_to_json(tools);
        body["tool_choice"] = serde_json::json!("auto");
    }
    apply_prompt_cache_fields(config, &mut body, messages, tools);
    body
}

fn build_responses_request_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let input: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| !matches!(m.role, LlmRole::System))
        .flat_map(responses_input_items)
        .collect();

    let mut body = serde_json::json!({
        "model": config.model_id,
        "instructions": system_text(messages),
        "input": input,
        "max_output_tokens": config.max_output_tokens,
        "stream": true,
    });

    if !tools.is_empty() {
        body["parallel_tool_calls"] = serde_json::json!(true);
        body["tools"] = responses_tools_json(tools);
    }
    if let Some(level) = config.thinking_level {
        body["reasoning"] = serde_json::json!({
            "effort": level.as_wire_value()
        });
    }
    apply_prompt_cache_fields(config, &mut body, messages, tools);

    body
}

fn apply_prompt_cache_fields(
    config: OpenAiRequestConfig<'_>,
    body: &mut serde_json::Value,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) {
    if !config.supports_prompt_cache_key {
        return;
    }

    body["prompt_cache_key"] = serde_json::json!(prompt_cache_key(
        config.api_mode,
        config.model_id,
        messages,
        tools
    ));
    if let Some(retention) = config.prompt_cache_retention {
        body["prompt_cache_retention"] = serde_json::json!(prompt_cache_retention_wire_value(
            config.api_mode,
            retention
        ));
    }
}

fn prompt_cache_key(
    api_mode: OpenAiApiMode,
    model_id: &str,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> String {
    let sys = system_text(messages);
    let tools_json = match api_mode {
        OpenAiApiMode::ChatCompletions => tools_to_json(tools),
        OpenAiApiMode::Responses => responses_tools_json(tools),
    };
    let tools_text = serde_json::to_string(&tools_json).unwrap_or_default();
    format!(
        "astrcode-{}",
        stable_hash_hex(&[model_id, sys.as_str(), tools_text.as_str()])
    )
}

#[cfg(test)]
mod tests {
    use astrcode_core::config::OpenAiApiMode;

    use super::*;

    #[test]
    fn resolves_responses_input_tokens_endpoint_from_base_url() {
        assert_eq!(
            input_tokens_endpoint("https://api.test/v1"),
            "https://api.test/v1/responses/input_tokens"
        );
        assert_eq!(
            input_tokens_endpoint("https://api.test/v1/responses"),
            "https://api.test/v1/responses/input_tokens"
        );
    }

    #[test]
    fn resolves_chat_and_responses_endpoint_from_base_url() {
        assert_eq!(
            endpoint_url(OpenAiApiMode::ChatCompletions, "https://api.test/v1"),
            "https://api.test/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(OpenAiApiMode::Responses, "https://api.test/v1"),
            "https://api.test/v1/responses"
        );
    }
}
