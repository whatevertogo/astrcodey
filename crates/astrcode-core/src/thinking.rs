//! Model thinking/reasoning domain types and built-in capability resolution.
//!
//! This module defines the normalized thinking configuration model used throughout
//! the runtime, the capability description that declares what thinking features a
//! provider/wire/model combination supports, and the built-in capability lookup
//! by provider kind + wire format + model family.

use serde::{Deserialize, Serialize};

// ── Normalized Thinking Config ──────────────────────────────────────────

/// Normalized thinking/reasoning configuration consumed by the runtime.
///
/// This is the single canonical representation after legacy conversion and
/// capability validation have been applied at the config resolution boundary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThinkingConfig {
    /// Whether thinking/reasoning is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Arbitrary effort string (e.g. `"low"`, `"medium"`, `"high"`, provider-specific).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Maximum thinking tokens (provider-dependent; Anthropic `budget_tokens`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

impl ThinkingConfig {
    /// Removes effort and budget fields that cannot affect a disabled request.
    pub fn normalized(mut self) -> Self {
        if !self.enabled {
            self.effort = None;
            self.budget_tokens = None;
        }
        self
    }

    /// Returns `true` when thinking is meaningfully active (enabled with either
    /// an effort level or budget constraint).
    pub fn is_active(&self) -> bool {
        self.enabled && (self.effort.is_some() || self.budget_tokens.is_some())
    }
}

// ── Wire Mapping ───────────────────────────────────────────────────────

/// How thinking maps to the wire protocol for a given provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingWireMapping {
    /// OpenAI Responses API `reasoning.effort` (o-series models).
    OpenAiResponses,
    /// Anthropic adaptive thinking (`thinking.type: "adaptive"` + output effort).
    AnthropicAdaptive,
    /// Anthropic extended thinking with required `budget_tokens`.
    AnthropicBudget,
    /// OpenAI Chat Completions thinking (legacy `reasoning` effort field).
    OpenAiChat,
}

// ── Thinking Capability ────────────────────────────────────────────────

/// Describes what thinking features a provider/wire/model combination supports.
///
/// Capabilities are composable: an `allowed_effort` list paired with
/// `budget_min`/`budget_max` boundaries. `None` accepts provider-specific effort
/// strings; an empty list means effort is unsupported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThinkingCapability {
    /// How thinking maps to the wire protocol.
    pub wire_mapping: ThinkingWireMapping,
    /// Allowed effort values (e.g. `["low", "medium", "high"]`).
    /// `None` means arbitrary strings are accepted; `Some(vec![])` means effort is unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_effort: Option<Vec<String>>,
    /// Minimum `budget_tokens` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_min: Option<u32>,
    /// Maximum `budget_tokens` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_max: Option<u32>,
    /// Whether this wire mapping can explicitly disable thinking.
    #[serde(default = "default_can_disable")]
    pub can_disable: bool,
}

const fn default_can_disable() -> bool {
    true
}

// ── Built-in Thinking Capability Catalog ───────────────────────────────

/// Convenience helper to create a `Some(vec![])` for the toggle-only case.
fn empty_effort() -> Option<Vec<String>> {
    Some(vec![])
}

/// Create the static built-in thinking capability catalog.
fn builtin_thinking_specs() -> Vec<BuiltinThinkingSpec> {
    vec![
        // OpenAI reasoning models with the stable low/medium/high effort set.
        BuiltinThinkingSpec {
            provider_kinds: &["openai"],
            wire_format: super::config::raw::ProviderWireFormat::OpenAiResponses,
            model_family_prefixes: &["o1", "o3"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiResponses,
                allowed_effort: Some(vec!["low".into(), "medium".into(), "high".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: false,
            },
        },
        // New Anthropic models expose adaptive thinking plus output effort.
        BuiltinThinkingSpec {
            provider_kinds: &["anthropic"],
            wire_format: super::config::raw::ProviderWireFormat::AnthropicMessages,
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
        // Older Claude models use explicit extended-thinking token budgets.
        BuiltinThinkingSpec {
            provider_kinds: &["anthropic"],
            wire_format: super::config::raw::ProviderWireFormat::AnthropicMessages,
            model_family_prefixes: &["claude"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::AnthropicBudget,
                allowed_effort: empty_effort(),
                budget_min: Some(1024),
                budget_max: Some(64_000),
                can_disable: true,
            },
        },
        // DeepSeek (OpenAI Chat Completions reasoning via `reasoning` field)
        BuiltinThinkingSpec {
            provider_kinds: &["deepseek"],
            wire_format: super::config::raw::ProviderWireFormat::OpenAiChatCompletions,
            model_family_prefixes: &["deepseek"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: empty_effort(),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            },
        },
        // Zhipu models
        BuiltinThinkingSpec {
            provider_kinds: &["zhipu"],
            wire_format: super::config::raw::ProviderWireFormat::OpenAiChatCompletions,
            model_family_prefixes: &["glm"],
            capability: ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiChat,
                allowed_effort: empty_effort(),
                budget_min: None,
                budget_max: None,
                can_disable: true,
            },
        },
    ]
}

/// A static entry in the built-in thinking capability catalog.
struct BuiltinThinkingSpec {
    provider_kinds: &'static [&'static str],
    wire_format: super::config::raw::ProviderWireFormat,
    /// Glob-like model ID prefix patterns. Empty means match all models for the provider.
    model_family_prefixes: &'static [&'static str],
    capability: ThinkingCapability,
}

/// Resolve the built-in [`ThinkingCapability`] for a given provider + wire + model.
///
/// Returns `None` for unknown `provider_kind` / `wire_format` combinations (including
/// generic `"openai-compatible"` providers, which are not matched by the catalog).
///
/// Matching uses:
/// - Exact `provider_kind` match
/// - Exact `wire_format` match
/// - Model ID starts with one of the family prefixes (empty prefix list matches all models)
pub fn resolve_thinking_capability(
    provider_kind: &str,
    wire_format: super::config::raw::ProviderWireFormat,
    model_id: &str,
) -> Option<ThinkingCapability> {
    for spec in builtin_thinking_specs() {
        if !spec.provider_kinds.contains(&provider_kind) {
            continue;
        }
        if spec.wire_format != wire_format {
            continue;
        }
        if !spec.model_family_prefixes.is_empty()
            && !spec
                .model_family_prefixes
                .iter()
                .any(|p| model_id.starts_with(p))
        {
            continue;
        }
        return Some(spec.capability);
    }
    None
}

/// Validate and normalize a [`ThinkingConfig`] against a [`ThinkingCapability`].
///
/// Returns a list of validation issues (empty = valid). The `ThinkingConfig` is
/// not mutated — the caller should decide whether to clamp/fallback based on the
/// issues reported.
///
/// ## Rules
/// - If `enabled` is `false`, an issue is reported when the capability cannot explicitly disable
///   thinking.
/// - If effort is set but the capability disallows it (`allowed_effort == Some(vec![])`), an issue
///   is reported.
/// - If effort is set and the capability has a non-empty allowed list but the value isn't in it, an
///   issue is reported.
/// - If `budget_tokens` is outside `budget_min`..`budget_max`, an issue is reported.
pub fn validate_thinking(config: &ThinkingConfig, capability: &ThinkingCapability) -> Vec<String> {
    let mut issues = Vec::new();
    if !config.enabled {
        if !capability.can_disable {
            issues.push("thinking cannot be disabled for this model".into());
        }
        return issues;
    }

    // Validate effort
    if let Some(ref effort) = config.effort {
        if let Some(ref allowed) = capability.allowed_effort {
            if allowed.is_empty() {
                issues.push(format!(
                    "effort '{}' not supported by this provider (thinking is toggle-only)",
                    effort
                ));
            } else if !allowed.contains(effort) {
                issues.push(format!(
                    "effort '{}' not in allowed values {:?}",
                    effort, allowed
                ));
            }
        }
    }

    // Validate budget_tokens
    if let Some(budget) = config.budget_tokens {
        if capability.budget_min.is_none() && capability.budget_max.is_none() {
            issues.push("budget_tokens is not supported by this model".into());
        }
        if let Some(min) = capability.budget_min {
            if budget < min {
                issues.push(format!("budget_tokens {} below minimum {}", budget, min));
            }
        }
        if let Some(max) = capability.budget_max {
            if budget > max {
                issues.push(format!("budget_tokens {} exceeds maximum {}", budget, max));
            }
        }
    }

    if config.effort.is_none()
        && capability
            .allowed_effort
            .as_ref()
            .is_some_and(|allowed| !allowed.is_empty())
    {
        issues.push("an effort value is required when thinking is enabled".into());
    }
    if config.budget_tokens.is_none() && capability.budget_min.is_some() {
        issues.push("budget_tokens is required when thinking is enabled".into());
    }

    issues
}

/// Convert legacy [`crate::llm::ThinkingLevel`] effort to an effort string.
pub fn thinking_level_to_effort(level: crate::llm::ThinkingLevel) -> &'static str {
    level.as_wire_value()
}

/// Convert a legacy `(reasoning: bool, thinking_level: Option<ThinkingLevel>)` pair
/// into a [`ThinkingConfig`].
pub fn legacy_to_thinking_config(
    reasoning: bool,
    thinking_level: Option<crate::llm::ThinkingLevel>,
) -> ThinkingConfig {
    if !reasoning && thinking_level.is_none() {
        return ThinkingConfig::default();
    }
    ThinkingConfig {
        enabled: true,
        effort: thinking_level.map(|l| l.as_wire_value().to_string()),
        budget_tokens: None,
    }
}

/// Convert a [`ThinkingConfig`] effort string back to a [`ThinkingLevel`] if it
/// matches one of the standard levels.
pub fn effort_to_thinking_level(effort: &str) -> Option<crate::llm::ThinkingLevel> {
    match effort {
        "low" => Some(crate::llm::ThinkingLevel::Low),
        "medium" => Some(crate::llm::ThinkingLevel::Medium),
        "high" => Some(crate::llm::ThinkingLevel::High),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ThinkingLevel;

    #[test]
    fn resolve_openai_responses_has_effort_enum() {
        let cap = resolve_thinking_capability(
            "openai",
            super::super::config::raw::ProviderWireFormat::OpenAiResponses,
            "o3-mini",
        )
        .expect("o3 family should resolve");
        assert_eq!(cap.wire_mapping, ThinkingWireMapping::OpenAiResponses);
        let allowed = cap
            .allowed_effort
            .expect("openai responses has allowed effort");
        assert!(allowed.contains(&"low".to_string()));
        assert!(allowed.contains(&"medium".to_string()));
        assert!(allowed.contains(&"high".to_string()));
    }

    #[test]
    fn resolve_openai_responses_gpt_4_1_is_not_assumed_to_reason() {
        assert!(
            resolve_thinking_capability(
                "openai",
                super::super::config::raw::ProviderWireFormat::OpenAiResponses,
                "gpt-4.1-nano",
            )
            .is_none()
        );
    }

    #[test]
    fn resolve_modern_anthropic_uses_adaptive_effort() {
        let cap = resolve_thinking_capability(
            "anthropic",
            super::super::config::raw::ProviderWireFormat::AnthropicMessages,
            "claude-sonnet-4-6",
        )
        .expect("anthropic should resolve");
        assert_eq!(cap.wire_mapping, ThinkingWireMapping::AnthropicAdaptive);
        assert!(
            cap.allowed_effort
                .is_some_and(|values| values.contains(&"max".into()))
        );
        assert_eq!(cap.budget_min, None);
    }

    #[test]
    fn resolve_legacy_anthropic_has_budget_constraints() {
        let cap = resolve_thinking_capability(
            "anthropic",
            super::super::config::raw::ProviderWireFormat::AnthropicMessages,
            "claude-3-7-sonnet-latest",
        )
        .expect("legacy anthropic should resolve");
        assert_eq!(cap.wire_mapping, ThinkingWireMapping::AnthropicBudget);
        assert_eq!(cap.budget_min, Some(1024));
        assert_eq!(cap.budget_max, Some(64_000));
        assert_eq!(cap.allowed_effort, Some(vec![]));
    }

    #[test]
    fn resolve_deepseek_uses_openai_chat_mapping() {
        let cap = resolve_thinking_capability(
            "deepseek",
            super::super::config::raw::ProviderWireFormat::OpenAiChatCompletions,
            "deepseek-v4-flash",
        )
        .expect("deepseek should resolve");
        assert_eq!(cap.wire_mapping, ThinkingWireMapping::OpenAiChat);
        // Effort not allowed (toggle-only)
        assert_eq!(cap.allowed_effort, Some(vec![]));
    }

    #[test]
    fn resolve_unknown_openai_compatible_returns_none() {
        // Generic openai-compatible provider_kind should NOT match any built-in spec
        let cap = resolve_thinking_capability(
            "openai-compatible",
            super::super::config::raw::ProviderWireFormat::OpenAiChatCompletions,
            "gpt-4.1",
        );
        assert!(cap.is_none(), "openai-compatible should not resolve");
    }

    #[test]
    fn resolve_unknown_provider_returns_none() {
        let cap = resolve_thinking_capability(
            "unknown-provider",
            super::super::config::raw::ProviderWireFormat::OpenAiChatCompletions,
            "some-model",
        );
        assert!(cap.is_none());
    }

    #[test]
    fn validate_thinking_valid_config() {
        let cap = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiResponses,
            allowed_effort: Some(vec!["low".into(), "medium".into(), "high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: false,
        };
        let config = ThinkingConfig {
            enabled: true,
            effort: Some("high".into()),
            budget_tokens: None,
        };
        let issues = validate_thinking(&config, &cap);
        assert!(issues.is_empty());
    }

    #[test]
    fn validate_thinking_respects_disable_capability() {
        let mut cap = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiChat,
            allowed_effort: Some(vec![]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = ThinkingConfig {
            enabled: false,
            effort: Some("high".into()),
            budget_tokens: None,
        };
        let issues = validate_thinking(&config, &cap);
        assert!(issues.is_empty());

        cap.can_disable = false;
        let issues = validate_thinking(&config, &cap);
        assert_eq!(issues, vec!["thinking cannot be disabled for this model"]);
    }

    #[test]
    fn validate_thinking_reports_disallowed_effort() {
        let cap = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::OpenAiChat,
            allowed_effort: Some(vec![]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = ThinkingConfig {
            enabled: true,
            effort: Some("high".into()),
            budget_tokens: None,
        };
        let issues = validate_thinking(&config, &cap);
        assert!(!issues.is_empty());
        assert!(issues[0].contains("not supported"));
    }

    #[test]
    fn validate_thinking_reports_budget_out_of_range() {
        let cap = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::AnthropicBudget,
            allowed_effort: None,
            budget_min: Some(1024),
            budget_max: Some(64_000),
            can_disable: true,
        };
        let config = ThinkingConfig {
            enabled: true,
            effort: None,
            budget_tokens: Some(100),
        };
        let issues = validate_thinking(&config, &cap);
        assert!(!issues.is_empty());
        assert!(issues.iter().any(|i| i.contains("below minimum")));
    }

    #[test]
    fn legacy_conversion_reasoning_only() {
        let tc = legacy_to_thinking_config(true, None);
        assert!(tc.enabled);
        assert_eq!(tc.effort, None);
        assert_eq!(tc.budget_tokens, None);
    }

    #[test]
    fn legacy_conversion_reasoning_with_level() {
        let tc = legacy_to_thinking_config(true, Some(ThinkingLevel::High));
        assert!(tc.enabled);
        assert_eq!(tc.effort, Some("high".into()));
        assert_eq!(tc.budget_tokens, None);
    }

    #[test]
    fn legacy_conversion_neither() {
        let tc = legacy_to_thinking_config(false, None);
        assert!(!tc.enabled);
        assert_eq!(tc.effort, None);
    }

    #[test]
    fn effort_round_trip() {
        for level in [
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ] {
            let effort = thinking_level_to_effort(level);
            assert_eq!(effort_to_thinking_level(effort), Some(level));
        }
    }

    #[test]
    fn is_active_requires_enabled_plus_effort_or_budget() {
        assert!(!ThinkingConfig::default().is_active());
        assert!(
            !ThinkingConfig {
                enabled: true,
                effort: None,
                budget_tokens: None,
            }
            .is_active()
        );
        assert!(
            ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            }
            .is_active()
        );
        assert!(
            ThinkingConfig {
                enabled: true,
                effort: None,
                budget_tokens: Some(4096),
            }
            .is_active()
        );
    }

    #[test]
    fn normalized_removes_inactive_options() {
        assert_eq!(
            ThinkingConfig {
                enabled: false,
                effort: Some("high".into()),
                budget_tokens: Some(4096),
            }
            .normalized(),
            ThinkingConfig::default()
        );
    }
}
