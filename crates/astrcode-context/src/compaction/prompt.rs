use super::plan::CompactPromptMode;
use crate::settings::ContextWindowSettings;

const BASE_COMPACT_PROMPT_TEMPLATE: &str = include_str!("../templates/compact/base.md");
const INCREMENTAL_COMPACT_PROMPT_TEMPLATE: &str =
    include_str!("../templates/compact/incremental.md");

pub(crate) fn render_compact_system_prompt(
    system_prompt: Option<&str>,
    mode: &CompactPromptMode,
    settings: &ContextWindowSettings,
    contract_repair_feedback: Option<&str>,
) -> String {
    let incremental_block = match mode {
        CompactPromptMode::Fresh => String::new(),
        CompactPromptMode::Incremental { previous_summary } => INCREMENTAL_COMPACT_PROMPT_TEMPLATE
            .replace("{{PREVIOUS_SUMMARY}}", previous_summary.trim()),
    };
    let runtime_context = system_prompt
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("\nCurrent runtime system prompt for context:\n{value}"))
        .unwrap_or_default();
    let contract_repair_block = contract_repair_feedback
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            format!(
                "\n## Contract Repair\nThe previous compact response violated the required XML \
                 contract.\nReturn the <summary> block exactly as specified and do not add any \
                 preamble, explanation, or Markdown fence.\nViolation details:\n{value}"
            )
        })
        .unwrap_or_default();

    BASE_COMPACT_PROMPT_TEMPLATE
        .replace("{{INCREMENTAL_MODE}}", incremental_block.trim())
        .replace("{{CUSTOM_INSTRUCTIONS}}", "")
        .replace("{{CONTRACT_REPAIR}}", contract_repair_block.trim())
        .replace(
            "{{COMPACT_OUTPUT_TOKEN_CAP}}",
            &settings.compact_max_output_tokens.max(1).to_string(),
        )
        .replace("{{RECENT_USER_CONTEXT_MESSAGES}}", "(none)")
        .replace("{{RUNTIME_CONTEXT}}", runtime_context.trim_end())
}
