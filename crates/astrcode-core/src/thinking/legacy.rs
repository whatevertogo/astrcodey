use super::ThinkingConfig;
use crate::llm::ThinkingLevel;

/// Convert the legacy reasoning fields into the canonical thinking configuration.
pub fn legacy_to_thinking_config(
    reasoning: bool,
    thinking_level: Option<ThinkingLevel>,
) -> ThinkingConfig {
    if !reasoning && thinking_level.is_none() {
        return ThinkingConfig::default();
    }
    ThinkingConfig {
        enabled: true,
        effort: thinking_level.map(|level| level.as_wire_value().to_string()),
        budget_tokens: None,
    }
}

/// Convert a canonical effort value back to a legacy level when representable.
pub fn effort_to_thinking_level(effort: &str) -> Option<ThinkingLevel> {
    match effort {
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        _ => None,
    }
}
