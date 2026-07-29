use crate::{
    config::ModelOptionsConfig,
    llm::{ThinkingLevel, thinking::ThinkingConfig},
};

/// Resolve the configured thinking value while preserving legacy model options.
pub fn model_thinking_config(options: &ModelOptionsConfig) -> Option<ThinkingConfig> {
    options.thinking.clone().or_else(|| {
        (options.reasoning.is_some() || options.thinking_level.is_some()).then(|| {
            legacy_to_thinking_config(options.reasoning.unwrap_or(false), options.thinking_level)
        })
    })
}

pub(super) fn legacy_to_thinking_config(
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

pub(super) fn effort_to_thinking_level(effort: &str) -> Option<ThinkingLevel> {
    match effort {
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_thinking_config_preserves_current_and_legacy_forms() {
        let current = ModelOptionsConfig {
            thinking: Some(ThinkingConfig {
                enabled: true,
                effort: Some("max".into()),
                budget_tokens: None,
            }),
            reasoning: Some(false),
            thinking_level: Some(ThinkingLevel::Low),
        };
        assert_eq!(model_thinking_config(&current), current.thinking);

        let legacy = ModelOptionsConfig {
            thinking: None,
            reasoning: Some(true),
            thinking_level: Some(ThinkingLevel::High),
        };
        assert_eq!(
            model_thinking_config(&legacy),
            Some(ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            })
        );

        assert_eq!(model_thinking_config(&ModelOptionsConfig::default()), None);
        for level in [
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ] {
            assert_eq!(effort_to_thinking_level(level.as_wire_value()), Some(level));
        }
    }
}
