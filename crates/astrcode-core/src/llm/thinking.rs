//! Model thinking/reasoning domain contracts.
//!
//! This module defines the normalized thinking configuration model used throughout
//! the runtime and the capability description consumed by configuration and provider
//! boundaries. Built-in provider policy lives in [`crate::config`].

use serde::{Deserialize, Serialize};

// ── Normalized Thinking Config ──────────────────────────────────────────

/// Normalized thinking/reasoning configuration consumed by the runtime.
///
/// This is the single canonical representation after capability validation has
/// been applied at the config resolution boundary.
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
    #[cfg(test)]
    fn is_active(&self) -> bool {
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

/// Validate a [`ThinkingConfig`] against a [`ThinkingCapability`].
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
    if let Some(ref effort) = config.effort
        && let Some(ref allowed) = capability.allowed_effort
    {
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

    // Validate budget_tokens
    if let Some(budget) = config.budget_tokens {
        if capability.budget_min.is_none() && capability.budget_max.is_none() {
            issues.push("budget_tokens is not supported by this model".into());
        }
        if let Some(min) = capability.budget_min
            && budget < min
        {
            issues.push(format!("budget_tokens {} below minimum {}", budget, min));
        }
        if let Some(max) = capability.budget_max
            && budget > max
        {
            issues.push(format!("budget_tokens {} exceeds maximum {}", budget, max));
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

#[cfg(test)]
mod tests {
    use super::*;

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
