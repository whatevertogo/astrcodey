use super::*;

mod sanitize;
mod xml_parsing;
use sanitize as sanitize_impl;
use xml_parsing as xml_parsing_impl;

pub(super) fn render_compact_system_prompt(
    compact_prompt_context: Option<&str>,
    mode: CompactPromptMode,
    effective_max_output_tokens: usize,
    recent_user_context_messages: &[RecentUserContextMessage],
    custom_instructions: Option<&str>,
    contract_repair_feedback: Option<&str>,
) -> String {
    let incremental_block = match mode {
        CompactPromptMode::Fresh => String::new(),
        CompactPromptMode::Incremental { previous_summary } => INCREMENTAL_COMPACT_PROMPT_TEMPLATE
            .replace("{{PREVIOUS_SUMMARY}}", previous_summary.trim()),
    };
    let runtime_context = compact_prompt_context
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("\nCurrent runtime system prompt for context:\n{value}"))
        .unwrap_or_default();
    let custom_instruction_block = custom_instructions
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            format!(
                "\n## Manual Compact Instructions\nFollow these extra requirements for this \
                 compact only:\n{value}"
            )
        })
        .unwrap_or_default();
    let contract_repair_block = contract_repair_feedback
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            format!(
                "\n## Contract Repair\nThe previous compact response violated the required XML \
                 contract.\nReturn all three XML blocks exactly as specified and do not add any \
                 preamble, explanation, or Markdown fence.\nViolation details:\n{value}"
            )
        })
        .unwrap_or_default();
    let recent_user_context_block =
        render_recent_user_context_candidates(recent_user_context_messages);

    BASE_COMPACT_PROMPT_TEMPLATE
        .replace("{{INCREMENTAL_MODE}}", incremental_block.trim())
        .replace("{{CUSTOM_INSTRUCTIONS}}", custom_instruction_block.trim())
        .replace("{{CONTRACT_REPAIR}}", contract_repair_block.trim())
        .replace(
            "{{COMPACT_OUTPUT_TOKEN_CAP}}",
            &effective_max_output_tokens.to_string(),
        )
        .replace(
            "{{RECENT_USER_CONTEXT_MESSAGES}}",
            recent_user_context_block.trim_end(),
        )
        .replace("{{RUNTIME_CONTEXT}}", runtime_context.trim_end())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecentUserContextMessage {
    pub(super) index: usize,
    pub(super) content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedCompactOutput {
    pub(super) summary: String,
    pub(super) recent_user_context_digest: Option<String>,
    pub(super) has_analysis: bool,
    pub(super) has_recent_user_context_digest_block: bool,
    pub(super) used_fallback: bool,
}

fn render_recent_user_context_candidates(messages: &[RecentUserContextMessage]) -> String {
    if messages.is_empty() {
        return "(none)".to_string();
    }

    messages
        .iter()
        .enumerate()
        .map(|(position, message)| format!("Message {}:\n{}", position + 1, message.content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(super) fn collect_recent_user_context_messages(
    messages: &[LlmMessage],
    keep_recent_user_messages: usize,
) -> Vec<RecentUserContextMessage> {
    if keep_recent_user_messages == 0 {
        return Vec::new();
    }

    let mut collected = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            LlmMessage::User {
                content,
                origin: UserMessageOrigin::User,
            } => Some(RecentUserContextMessage {
                index,
                content: content.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    let keep_start = collected
        .len()
        .saturating_sub(keep_recent_user_messages.max(1));
    collected.drain(..keep_start);
    collected
}

pub(super) fn prepare_compact_input(messages: &[LlmMessage]) -> PreparedCompactInput {
    let prompt_mode = latest_previous_summary(messages)
        .map(|previous_summary| CompactPromptMode::Incremental { previous_summary })
        .unwrap_or(CompactPromptMode::Fresh);
    let messages = messages
        .iter()
        .filter_map(normalize_compaction_message)
        .collect::<Vec<_>>();
    let input_units = compaction_units(&messages).len().max(1);
    PreparedCompactInput {
        messages,
        prompt_mode,
        input_units,
    }
}

fn latest_previous_summary(messages: &[LlmMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        LlmMessage::User {
            content,
            origin: UserMessageOrigin::CompactSummary,
        } => parse_compact_summary_message(content)
            .map(|envelope| sanitize_impl::sanitize_compact_summary(&envelope.summary)),
        _ => None,
    })
}

fn normalize_compaction_message(message: &LlmMessage) -> Option<LlmMessage> {
    match message {
        LlmMessage::User {
            content,
            origin: UserMessageOrigin::User,
        } => Some(LlmMessage::User {
            content: content.trim().to_string(),
            origin: UserMessageOrigin::User,
        }),
        LlmMessage::User { .. } => None,
        LlmMessage::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let mut lines = Vec::new();
            let visible = collapse_compaction_whitespace(content);
            if !visible.is_empty() {
                lines.push(visible);
            }
            if !tool_calls.is_empty() {
                let names = tool_calls
                    .iter()
                    .map(|call| call.name.trim())
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>();
                if !names.is_empty() {
                    lines.push(format!("Requested tools: {}", names.join(", ")));
                }
            }
            let normalized = lines.join("\n");
            if normalized.trim().is_empty() {
                None
            } else {
                Some(LlmMessage::Assistant {
                    content: normalized,
                    tool_calls: Vec::new(),
                    reasoning: None,
                })
            }
        },
        LlmMessage::Tool {
            tool_call_id,
            content,
        } => {
            let normalized = normalize_compaction_tool_content(content);
            if normalized.is_empty() {
                None
            } else {
                Some(LlmMessage::Tool {
                    tool_call_id: tool_call_id.clone(),
                    content: normalized,
                })
            }
        },
        LlmMessage::System { .. } => None,
    }
}

fn collapse_compaction_whitespace(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .split("\n\n\n")
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string()
}

pub(super) fn normalize_compaction_tool_content(content: &str) -> String {
    let stripped_child_ref = sanitize_impl::strip_child_agent_reference_hint(content);
    let collapsed = collapse_compaction_whitespace(&stripped_child_ref);
    if collapsed.is_empty() {
        return String::new();
    }
    if astrcode_core::is_persisted_output(&collapsed) {
        return summarize_persisted_tool_output(&collapsed);
    }
    collapsed
}

pub(super) fn sanitize_compact_summary(summary: &str) -> String {
    sanitize_impl::sanitize_compact_summary(summary)
}

pub(super) fn sanitize_recent_user_context_digest(digest: &str) -> String {
    sanitize_impl::sanitize_recent_user_context_digest(digest)
}

pub(super) fn parse_compact_output(content: &str) -> Result<ParsedCompactOutput> {
    xml_parsing_impl::parse_compact_output(content)
}

pub(super) fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    xml_parsing_impl::contains_ascii_case_insensitive(haystack, needle)
}

fn summarize_persisted_tool_output(content: &str) -> String {
    let persisted_path =
        astrcode_core::tool_result_persist::persisted_output_absolute_path(content)
            .unwrap_or_else(|| "unknown persisted path".to_string());
    format!(
        "Large tool output was persisted instead of inlined.\nPersisted path: \
         {persisted_path}\nPreserve only the conclusion, referenced path, and any error."
    )
}
