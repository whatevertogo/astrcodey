use std::sync::LazyLock;

use super::{ThinkingCapability, ThinkingWireMapping};
use crate::config::raw::ProviderWireFormat;

static BUILTIN_THINKING_SPECS: LazyLock<Vec<BuiltinThinkingSpec>> = LazyLock::new(|| {
    vec![
        BuiltinThinkingSpec {
            provider_kinds: &["openai"],
            wire_format: ProviderWireFormat::OpenAiResponses,
            model_family_prefixes: &["o1", "o3"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiResponses,
                allowed_effort: Some(vec!["low".into(), "medium".into(), "high".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: false,
            },
        },
        BuiltinThinkingSpec {
            provider_kinds: &["anthropic"],
            wire_format: ProviderWireFormat::AnthropicMessages,
            model_family_prefixes: &["claude-opus-4-6", "claude-sonnet-4-6"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::AnthropicAdaptive,
                allowed_effort: Some(vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "max".into(),
                ]),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            },
        },
        BuiltinThinkingSpec {
            provider_kinds: &["anthropic"],
            wire_format: ProviderWireFormat::AnthropicMessages,
            model_family_prefixes: &["claude"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::AnthropicBudget,
                allowed_effort: Some(Vec::new()),
                budget_min: Some(1024),
                budget_max: Some(64_000),
                can_disable: true,
            },
        },
        BuiltinThinkingSpec {
            provider_kinds: &["deepseek"],
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            model_family_prefixes: &["deepseek"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: Some(Vec::new()),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            },
        },
        BuiltinThinkingSpec {
            provider_kinds: &["zhipu"],
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            model_family_prefixes: &["glm"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: Some(Vec::new()),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            },
        },
    ]
});

struct BuiltinThinkingSpec {
    provider_kinds: &'static [&'static str],
    wire_format: ProviderWireFormat,
    model_family_prefixes: &'static [&'static str],
    capability: ThinkingCapability,
}

/// Resolve the built-in [`ThinkingCapability`] for a provider, wire format, and model family.
///
/// Unknown combinations, including generic OpenAI-compatible profiles, require
/// an explicit capability declaration in configuration.
pub fn resolve_thinking_capability(
    provider_kind: &str,
    wire_format: ProviderWireFormat,
    model_id: &str,
) -> Option<ThinkingCapability> {
    BUILTIN_THINKING_SPECS
        .iter()
        .find(|spec| {
            spec.provider_kinds.contains(&provider_kind)
                && spec.wire_format == wire_format
                && (spec.model_family_prefixes.is_empty()
                    || spec
                        .model_family_prefixes
                        .iter()
                        .any(|prefix| model_id.starts_with(prefix)))
        })
        .map(|spec| spec.capability.clone())
}
