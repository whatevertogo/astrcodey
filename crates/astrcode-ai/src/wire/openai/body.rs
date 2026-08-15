//! OpenAI-compatible wire request construction.
//!
//! This module is intentionally unaware of HTTP transport and stream parsing. Its job is to encode
//! AstrCode's internal messages/tools into the exact JSON contracts required by Chat Completions
//! and Responses.

use std::sync::Arc;

use astrcode_core::{
    config::OpenAiApiMode,
    event::stable_hash_hex,
    llm::{
        LlmMessage, LlmRole, PromptCacheRetention,
        thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping},
    },
    tool::ToolDefinition,
};

use super::serialization::{
    chat_message_to_json, prompt_cache_retention_wire_value, responses_input_items,
    responses_tools_json, system_text, tools_to_json,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenAiRequestConfig<'a> {
    pub api_mode: OpenAiApiMode,
    pub model_id: &'a str,
    pub max_output_tokens: usize,
    pub supports_stream_usage: bool,
    pub supports_prompt_cache_key: bool,
    pub supports_strict_tool_use: bool,
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    pub thinking: &'a ThinkingConfig,
    pub thinking_capability: Option<&'a ThinkingCapability>,
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
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    match config.api_mode {
        OpenAiApiMode::ChatCompletions => build_chat_request_body(config, messages, tools),
        OpenAiApiMode::Responses => build_responses_request_body(config, messages, tools),
    }
}

pub(crate) fn build_input_token_count_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let config = OpenAiRequestConfig {
        api_mode: OpenAiApiMode::Responses,
        ..config
    };
    let system = system_text(messages);
    let mut body = build_responses_body(config, messages, tools, &system);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("max_output_tokens");
        obj.remove("stream");
        obj.remove("parallel_tool_calls");
    }
    body
}

fn build_chat_request_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let messages_json: Vec<serde_json::Value> =
        messages.iter().map(|m| chat_message_to_json(m)).collect();

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
        body["tools"] = tools_to_json(tools, config.supports_strict_tool_use);
        body["tool_choice"] = serde_json::json!("auto");
    }
    apply_common_chat_thinking(config, &mut body);
    let system = system_text(messages);
    apply_prompt_cache_fields(config, &mut body, &system);
    body
}

/// Apply OpenAI Chat Completions thinking fields based on capability mapping.
fn apply_common_chat_thinking(config: OpenAiRequestConfig<'_>, body: &mut serde_json::Value) {
    let Some(cap) = config.thinking_capability else {
        // Unknown/no capability: emit no thinking fields
        return;
    };
    if cap.wire_mapping != ThinkingWireMapping::OpenAiChat {
        return;
    }
    if config.thinking.enabled {
        body["thinking"] = serde_json::json!({"type": "enabled"});
        if let Some(ref effort) = config.thinking.effort {
            // If effort set for an explicitly declared OpenAiChat capability,
            // also emit reasoning_effort using that exact allowed value.
            body["reasoning_effort"] = serde_json::json!(effort);
        }
    } else {
        body["thinking"] = serde_json::json!({"type": "disabled"});
    }
}

/// 构建 Responses 请求体的公共形状（model/instructions/input/tools/parallel_tool_calls）。
///
/// 不含 prompt cache 字段与 reasoning——由具体调用方按需追加。`system` 由调用方算好传入，
/// 避免正式请求与 count_tokens 各自重复计算系统提示；count_tokens 路径也因此完全不触碰缓存键。
fn build_responses_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
    system: &str,
) -> serde_json::Value {
    let input: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| !matches!(m.role, LlmRole::System))
        .flat_map(|m| responses_input_items(m))
        .collect();

    let mut body = serde_json::json!({
        "model": config.model_id,
        "instructions": system,
        "input": input,
        "max_output_tokens": config.max_output_tokens,
        "stream": true,
    });

    if !tools.is_empty() {
        body["parallel_tool_calls"] = serde_json::json!(true);
        body["tools"] = responses_tools_json(tools, config.supports_strict_tool_use);
    }
    body
}

fn build_responses_request_body(
    config: OpenAiRequestConfig<'_>,
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let system = system_text(messages);
    let mut body = build_responses_body(config, messages, tools, &system);
    apply_prompt_cache_fields(config, &mut body, &system);
    apply_responses_thinking(config, &mut body);
    body
}

/// Apply OpenAI Responses thinking fields.
/// Emits reasoning.effort only when capability is OpenAiResponses, thinking enabled, and effort
/// set.
fn apply_responses_thinking(config: OpenAiRequestConfig<'_>, body: &mut serde_json::Value) {
    let Some(cap) = config.thinking_capability else {
        return;
    };
    if cap.wire_mapping != ThinkingWireMapping::OpenAiResponses {
        return;
    }
    if !config.thinking.enabled {
        return;
    }
    if let Some(ref effort) = config.thinking.effort {
        body["reasoning"] = serde_json::json!({
            "effort": effort
        });
    }
}

fn apply_prompt_cache_fields(
    config: OpenAiRequestConfig<'_>,
    body: &mut serde_json::Value,
    system_text: &str,
) {
    if !config.supports_prompt_cache_key {
        return;
    }

    body["prompt_cache_key"] =
        serde_json::json!(prompt_cache_key(config.model_id, system_text, body));
    if let Some(retention) = config.prompt_cache_retention {
        body["prompt_cache_retention"] = serde_json::json!(prompt_cache_retention_wire_value(
            config.api_mode,
            retention
        ));
    }
}

/// 派生 OpenAI prompt cache key：哈希 (model, system 文本, tools 序列化文本)。
///
/// tools 已由请求体构建器按当前 api_mode 组装进 `body["tools"]`，这里直接序列化复用，
/// 不再按原始 `tools` 重建——避免每个请求把全部工具 schema 重建并序列化两次。
fn prompt_cache_key(model_id: &str, system_text: &str, body: &serde_json::Value) -> String {
    let tools_text = body
        .get("tools")
        .map(|tools| serde_json::to_string(tools).unwrap_or_default())
        .unwrap_or_else(|| "[]".to_string());
    format!(
        "astrcode-{}",
        stable_hash_hex(&[model_id, system_text, tools_text.as_str()])
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

    // ─── Thinking wire mapping tests ─────────────────────────────────

    #[test]
    fn responses_thinking_emits_reasoning_effort_when_capability_maps_and_enabled_with_effort() {
        use astrcode_core::llm::thinking::{
            ThinkingCapability, ThinkingConfig, ThinkingWireMapping,
        };
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiResponses,
            allowed_effort: Some(vec!["high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: false,
        };
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::Responses,
            model_id: "o3-mini",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_responses_request_body(config, &[], &[]);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_thinking_omitted_when_no_capability() {
        use astrcode_core::llm::thinking::ThinkingConfig;
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::Responses,
            model_id: "o3-mini",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            },
            thinking_capability: None,
        };
        let body = build_responses_request_body(config, &[], &[]);
        assert!(
            body.get("reasoning").is_none(),
            "reasoning should be omitted when no capability"
        );
    }

    #[test]
    fn responses_thinking_omitted_when_disabled_even_with_capability() {
        use astrcode_core::llm::thinking::{
            ThinkingCapability, ThinkingConfig, ThinkingWireMapping,
        };
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiResponses,
            allowed_effort: Some(vec!["high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: false,
        };
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::Responses,
            model_id: "o3-mini",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: false,
                effort: None,
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_responses_request_body(config, &[], &[]);
        assert!(
            body.get("reasoning").is_none(),
            "reasoning should be omitted when thinking disabled"
        );
    }

    #[test]
    fn responses_thinking_omitted_when_no_effort_even_if_enabled() {
        use astrcode_core::llm::thinking::{
            ThinkingCapability, ThinkingConfig, ThinkingWireMapping,
        };
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiResponses,
            allowed_effort: Some(vec!["high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: false,
        };
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::Responses,
            model_id: "o3-mini",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: None,
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_responses_request_body(config, &[], &[]);
        assert!(
            body.get("reasoning").is_none(),
            "reasoning should be omitted when effort is not set"
        );
    }

    #[test]
    fn chat_emits_thinking_enabled_for_deepseek_when_capability_maps() {
        use astrcode_core::llm::thinking::{
            ThinkingCapability, ThinkingConfig, ThinkingWireMapping,
        };
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiChat,
            allowed_effort: Some(vec![]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "deepseek-chat",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: None,
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_chat_request_body(config, &[], &[]);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(
            body.get("reasoning_effort").is_none(),
            "toggle-only should not emit reasoning_effort"
        );
    }

    #[test]
    fn chat_emits_thinking_disabled_only_when_capability_maps_openai_chat() {
        use astrcode_core::llm::thinking::{
            ThinkingCapability, ThinkingConfig, ThinkingWireMapping,
        };
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiChat,
            allowed_effort: Some(vec![]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "deepseek-chat",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: false,
                effort: None,
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_chat_request_body(config, &[], &[]);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn chat_omits_thinking_when_no_capability() {
        use astrcode_core::llm::thinking::ThinkingConfig;
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "generic-model",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            },
            thinking_capability: None,
        };
        let body = build_chat_request_body(config, &[], &[]);
        assert!(
            body.get("thinking").is_none(),
            "thinking should be omitted for unknown capability"
        );
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort should be omitted for unknown capability"
        );
    }

    #[test]
    fn chat_emits_reasoning_effort_when_openai_chat_capability_with_effort() {
        use astrcode_core::llm::thinking::{
            ThinkingCapability, ThinkingConfig, ThinkingWireMapping,
        };
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiChat,
            allowed_effort: Some(vec!["low".into(), "medium".into(), "high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "deepseek-chat",
            max_output_tokens: 1024,
            supports_stream_usage: false,
            supports_prompt_cache_key: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: Some("low".into()),
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_chat_request_body(config, &[], &[]);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn prompt_cache_key_pinned_for_chat_and_responses() {
        use astrcode_core::{
            llm::{LlmMessage, thinking::ThinkingConfig},
            tool::{ExecutionMode, ToolDefinition, ToolOrigin},
        };
        let thinking = ThinkingConfig {
            enabled: false,
            effort: None,
            budget_tokens: None,
        };
        let sample_tool = || ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type":"object"}),
            strict: false,
            origin: ToolOrigin::Bundled,
            execution_mode: ExecutionMode::Parallel,
        };
        let messages = [
            Arc::new(LlmMessage::system("sys")),
            Arc::new(LlmMessage::user("hi")),
        ];

        let responses_body = build_responses_request_body(
            OpenAiRequestConfig {
                api_mode: OpenAiApiMode::Responses,
                model_id: "pin-model",
                max_output_tokens: 1024,
                supports_stream_usage: false,
                supports_prompt_cache_key: true,
                supports_strict_tool_use: false,
                prompt_cache_retention: None,
                thinking: &thinking,
                thinking_capability: None,
            },
            &messages,
            &[sample_tool()],
        );
        let chat_body = build_chat_request_body(
            OpenAiRequestConfig {
                api_mode: OpenAiApiMode::ChatCompletions,
                model_id: "pin-model",
                max_output_tokens: 1024,
                supports_stream_usage: false,
                supports_prompt_cache_key: true,
                supports_strict_tool_use: false,
                prompt_cache_retention: None,
                thinking: &thinking,
                thinking_capability: None,
            },
            &messages,
            &[sample_tool()],
        );
        // 缓存键是与上游网关的事实线缆契约，逐位钉死：重构（复用 body["tools"]、
        // system_text 只算一次）后必须保持不变。
        assert_eq!(
            responses_body["prompt_cache_key"],
            "astrcode-9c44520b23fa5dcf"
        );
        assert_eq!(chat_body["prompt_cache_key"], "astrcode-08191e4b2a01666b");
    }
}
