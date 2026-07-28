//! OpenAI-compatible wire request construction.
//!
//! This module is intentionally unaware of HTTP transport and stream parsing. Its job is to encode
//! AstrCode's internal messages/tools into the exact JSON contracts required by Chat Completions
//! and Responses.

use astrcode_core::{
    config::OpenAiApiMode,
    llm::{LlmMessage, LlmRole, PromptCacheRetention},
    thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping},
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
        obj.remove("reasoning");
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
        body["tools"] = tools_to_json(tools, config.supports_strict_tool_use);
        body["tool_choice"] = serde_json::json!("auto");
    }
    apply_common_chat_thinking(config, &mut body);
    apply_prompt_cache_fields(config, &mut body, messages, tools);
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
        body["tools"] = responses_tools_json(tools, config.supports_strict_tool_use);
    }
    apply_prompt_cache_fields(config, &mut body, messages, tools);
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
        tools,
        config.supports_strict_tool_use
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
    supports_strict_tool_use: bool,
) -> String {
    let sys = system_text(messages);
    let tools_json = match api_mode {
        OpenAiApiMode::ChatCompletions => tools_to_json(tools, supports_strict_tool_use),
        OpenAiApiMode::Responses => responses_tools_json(tools, supports_strict_tool_use),
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

    // ─── Thinking wire mapping tests ─────────────────────────────────

    #[test]
    fn responses_thinking_emits_reasoning_effort_when_capability_maps_and_enabled_with_effort() {
        use astrcode_core::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping};
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
            thinking_capability: Some(&ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiResponses,
                allowed_effort: Some(vec!["high".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: false,
            }),
        };
        let body = build_responses_request_body(config, &[], &[]);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_thinking_omitted_when_no_capability() {
        use astrcode_core::thinking::ThinkingConfig;
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
        use astrcode_core::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping};
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
            thinking_capability: Some(&ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiResponses,
                allowed_effort: Some(vec!["high".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: false,
            }),
        };
        let body = build_responses_request_body(config, &[], &[]);
        assert!(
            body.get("reasoning").is_none(),
            "reasoning should be omitted when thinking disabled"
        );
    }

    #[test]
    fn responses_thinking_omitted_when_no_effort_even_if_enabled() {
        use astrcode_core::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping};
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
            thinking_capability: Some(&ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiResponses,
                allowed_effort: Some(vec!["high".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: false,
            }),
        };
        let body = build_responses_request_body(config, &[], &[]);
        assert!(
            body.get("reasoning").is_none(),
            "reasoning should be omitted when effort is not set"
        );
    }

    #[test]
    fn chat_emits_thinking_enabled_for_deepseek_when_capability_maps() {
        use astrcode_core::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping};
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
            thinking_capability: Some(&ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: Some(vec![]),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            }),
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
        use astrcode_core::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping};
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
            thinking_capability: Some(&ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: Some(vec![]),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            }),
        };
        let body = build_chat_request_body(config, &[], &[]);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn chat_omits_thinking_when_no_capability() {
        use astrcode_core::thinking::ThinkingConfig;
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
        use astrcode_core::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping};
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
            thinking_capability: Some(&ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: Some(vec!["low".into(), "medium".into(), "high".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            }),
        };
        let body = build_chat_request_body(config, &[], &[]);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn chat_emits_glm_thinking_toggle() {
        use astrcode_core::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping};
        let config = OpenAiRequestConfig {
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "glm-5.1-flash",
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
            thinking_capability: Some(&ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: Some(vec![]),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            }),
        };
        let body = build_chat_request_body(config, &[], &[]);
        assert_eq!(body["thinking"]["type"], "enabled");
    }
}
