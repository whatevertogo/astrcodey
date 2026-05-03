//! Compact prompt 渲染。
//!
//! 模板文件定义九段 summary contract；本模块只负责把模式、修复反馈、
//! hook/custom 指令和运行时 system prompt 填入模板。

use super::plan::CompactPromptMode;
use crate::settings::ContextWindowSettings;

const BASE_COMPACT_PROMPT_TEMPLATE: &str = include_str!("../templates/compact/base.md");
const INCREMENTAL_COMPACT_PROMPT_TEMPLATE: &str =
    include_str!("../templates/compact/incremental.md");

pub(crate) fn render_compact_contract(
    system_prompt: Option<&str>,
    mode: &CompactPromptMode,
    settings: &ContextWindowSettings,
    contract_repair_feedback: Option<&str>,
    custom_instructions: &[String],
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
                 contract.\nReturn one <analysis> scratchpad block followed by the <summary> \
                 block exactly as specified, and do not add any preamble, explanation, or \
                 Markdown fence.\nViolation details:\n{value}"
            )
        })
        .unwrap_or_default();
    let custom_instructions_block = render_custom_instructions(custom_instructions);

    BASE_COMPACT_PROMPT_TEMPLATE
        .replace("{{INCREMENTAL_MODE}}", incremental_block.trim())
        .replace("{{CUSTOM_INSTRUCTIONS}}", custom_instructions_block.trim())
        .replace("{{CONTRACT_REPAIR}}", contract_repair_block.trim())
        .replace(
            "{{COMPACT_OUTPUT_TOKEN_CAP}}",
            &settings.compact_max_output_tokens.max(1).to_string(),
        )
        .replace("{{RECENT_USER_CONTEXT_MESSAGES}}", "(none)")
        .replace("{{RUNTIME_CONTEXT}}", runtime_context.trim_end())
}

/// 为 forked compact 渲染追加在对话尾部的 user request。
///
/// forked 模式保留主 system prompt 和 tools 以复用 provider prompt cache，
/// 因此 compact 指令必须作为最后一条 user message 注入。
pub(crate) fn render_compact_request(
    mode: &CompactPromptMode,
    settings: &ContextWindowSettings,
    contract_repair_feedback: Option<&str>,
    custom_instructions: &[String],
) -> String {
    let compact_contract = render_compact_contract(
        None,
        mode,
        settings,
        contract_repair_feedback,
        custom_instructions,
    );
    format!(
        "You are compacting the conversation above for a continuing coding-agent session.\nUse \
         only the conversation messages above and the previous compact summary if one is included \
         in this request.\nDo not call tools, functions, external systems, or any provider \
         tool-call interface. Tool calls are forbidden for this compact request.\nReturn exactly \
         one <analysis> scratchpad block followed by one <summary> block, with no markdown \
         fences, preamble, or extra text. The <analysis> block is private drafting context and \
         will be stripped before the continued session sees the result.\n\n{compact_contract}"
    )
}

/// 将 hook/调用方提供的额外要求变成模板中的可读 bullet list。
fn render_custom_instructions(instructions: &[String]) -> String {
    let instructions = instructions
        .iter()
        .map(|instruction| instruction.trim())
        .filter(|instruction| !instruction.is_empty())
        .collect::<Vec<_>>();
    if instructions.is_empty() {
        return String::new();
    }

    let mut block = String::from("## Custom Compact Instructions\n");
    for instruction in instructions {
        block.push_str("- ");
        block.push_str(instruction);
        block.push('\n');
    }
    block
}
