//! LLM 驱动的上下文压缩模块。
//!
//! 当上下文窗口接近容量上限时，通过 LLM 对历史对话进行摘要压缩，
//! 保留关键信息的同时释放 token 空间。
//!
//! 这里定义 compact 的语义边界：如何选择要压缩的消息、如何渲染摘要
//! request、如何校验模型返回的 `<summary>`，以及如何把摘要重新组装成
//! provider 可见的 synthetic user message。真正的工具权限、hook 和 provider
//! 调用细节由调用方通过闭包承担。

use std::future::Future;

use astrcode_core::llm::{LlmContent, LlmError, LlmMessage, LlmRole};
use astrcode_support::text::compact_inline;

use crate::{ContextSettings, token_budget::estimate_request_tokens};

const COMPACT_SUMMARY_END: &str = "</compact_summary>";
const MAX_PTL_RETRIES: usize = 3;
const MAX_SUMMARY_LINE_CHARS: usize = 320;

mod assemble;
mod parse;
mod plan;
mod post_compact;
mod prompt;

pub use assemble::{CompactSummaryEnvelope, format_compact_summary};
pub use astrcode_core::context::{
    COMPACT_SUMMARY_MARKER, CompactError, CompactResult, CompactSkipReason,
    CompactSummaryRenderOptions, is_compact_summary_message, is_compact_summary_text,
    is_prompt_too_long_message, is_synthetic_context_message,
};
pub use parse::{CompactParseError, ParsedCompactOutput, parse_compact_output};
use plan::{PreparedCompactInput, visible_message_text};
pub use post_compact::{
    PostCompactFile, PostCompactNote, agent_status_note, append_post_compact_context,
    recent_read_paths,
};

pub struct CompactExecution {
    pub result: CompactResult,
    pub llm_api_failed: bool,
}

struct PreparedCompactParts {
    prefix: Vec<LlmMessage>,
    prepared_input: PreparedCompactInput,
    retained_messages: Vec<LlmMessage>,
    pre_tokens: usize,
    messages_removed: usize,
}

impl From<CompactParseError> for CompactError {
    fn from(value: CompactParseError) -> Self {
        Self::Parse(value.to_string())
    }
}

/// 不调用 LLM 的 compact fallback，并使用指定的 summary 渲染选项。
#[cfg(test)]
fn compact_messages_with_render_options(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    render_options: &CompactSummaryRenderOptions,
) -> Result<CompactResult, CompactSkipReason> {
    compact_messages_with_render_options_and_keep(messages, system_prompt, render_options, None)
}

fn compact_messages_with_render_options_and_keep(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    render_options: &CompactSummaryRenderOptions,
    keep_recent_turns: Option<usize>,
) -> Result<CompactResult, CompactSkipReason> {
    let parts = prepare_compact_parts(messages, system_prompt, keep_recent_turns)?;
    let summary = summarize_prefix(&parts.prefix);
    Ok(finish_compact_summary(
        summary,
        parts.retained_messages,
        parts.pre_tokens,
        parts.messages_removed,
        system_prompt,
        render_options,
    ))
}

/// 使用调用方提供的文本请求函数生成 compact summary。
pub async fn compact_messages_with_request<F, Fut>(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    settings: &ContextSettings,
    custom_instructions: &[String],
    render_options: &CompactSummaryRenderOptions,
    keep_recent_turns: Option<usize>,
    mut request_text: F,
) -> Result<CompactResult, CompactError>
where
    F: FnMut(Vec<LlmMessage>) -> Fut,
    Fut: Future<Output = Result<String, CompactError>>,
{
    let parts = prepare_compact_parts(messages, system_prompt, keep_recent_turns)?;
    let round_starts = api_round_starts(&parts.prepared_input.messages);
    let mut repair_feedback: Option<String> = None;
    let mut ptl_rounds_dropped = 0usize;
    let mut repair_attempts = 0u8;
    let max_attempts = settings.compact_max_retry_attempts.max(1);
    let mut last_error: Option<CompactError> = None;

    while repair_attempts < max_attempts {
        let Some(message_start) = round_starts.get(ptl_rounds_dropped).copied() else {
            break;
        };
        let compact_messages = request_messages(
            &parts.prepared_input,
            message_start,
            system_prompt,
            settings,
            repair_feedback.as_deref(),
            custom_instructions,
        );
        let output = match request_text(compact_messages).await {
            Ok(output) => output,
            Err(error) if should_retry_prompt_too_long(&error) => {
                let next_drop = ptl_rounds_dropped + 1;
                if next_drop > MAX_PTL_RETRIES || next_drop >= round_starts.len() {
                    last_error = Some(error);
                    break;
                }
                ptl_rounds_dropped = next_drop;
                continue;
            },
            Err(error) => {
                last_error = Some(error);
                break;
            },
        };
        repair_attempts += 1;
        match parse_compact_output(&output) {
            Ok(parsed) => {
                return Ok(finish_compact_summary(
                    assemble::sanitize_compact_summary(&parsed.summary),
                    parts.retained_messages,
                    parts.pre_tokens,
                    parts.messages_removed,
                    system_prompt,
                    render_options,
                ));
            },
            Err(error) => {
                repair_feedback = Some(error.to_string());
                last_error = Some(error.into());
            },
        }
    }

    Err(last_error.unwrap_or_else(|| {
        CompactParseError::new("compact response did not contain a summary").into()
    }))
}

/// LLM compact + deterministic fallback 的统一入口。
///
/// 先尝试调用 LLM 生成摘要，失败时降级到确定性模板。
/// 用于 auto-compact 和 manual compact 两条路径。
pub async fn compact_messages_with_fallback<F, Fut>(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    settings: &ContextSettings,
    custom_instructions: &[String],
    render_options: &CompactSummaryRenderOptions,
    keep_recent_turns: Option<usize>,
    request_text: F,
) -> Result<CompactExecution, CompactSkipReason>
where
    F: FnMut(Vec<LlmMessage>) -> Fut,
    Fut: Future<Output = Result<String, CompactError>>,
{
    match compact_messages_with_request(
        messages,
        system_prompt,
        settings,
        custom_instructions,
        render_options,
        keep_recent_turns,
        request_text,
    )
    .await
    {
        Ok(result) => Ok(CompactExecution {
            result,
            llm_api_failed: false,
        }),
        Err(CompactError::Skip(reason)) => Err(reason),
        Err(error) => {
            let llm_api_failed = matches!(error, CompactError::Llm(_));
            tracing::warn!(%error, "LLM compact failed, falling back to deterministic");
            compact_messages_with_render_options_and_keep(
                messages,
                system_prompt,
                render_options,
                keep_recent_turns,
            )
            .map(|result| CompactExecution {
                result,
                llm_api_failed,
            })
        },
    }
}

/// 仅使用确定性模板压缩，不调用 LLM。
pub fn compact_messages_deterministic(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    render_options: &CompactSummaryRenderOptions,
    keep_recent_turns: Option<usize>,
) -> Result<CompactExecution, CompactSkipReason> {
    compact_messages_with_render_options_and_keep(
        messages,
        system_prompt,
        render_options,
        keep_recent_turns,
    )
    .map(|result| CompactExecution {
        result,
        llm_api_failed: true,
    })
}

fn should_retry_prompt_too_long(error: &CompactError) -> bool {
    matches!(error, CompactError::Llm(LlmError::PromptTooLong(_)))
        || is_prompt_too_long_message(&error.to_string())
}

pub fn parse_compact_summary_message(content: &str) -> Option<CompactSummaryEnvelope> {
    assemble::parse_compact_summary_message(content)
}

/// 是否可在 `split_after` 所指的 message 之后切分压缩边界（Kimi `canSplitAfter` 语义）。
///
/// `keep_start` 为保留区首条消息下标时，应对 `split_after = keep_start - 1` 调用本函数。
pub fn can_split_after(messages: &[LlmMessage], split_after: usize) -> bool {
    let Some(message) = messages.get(split_after) else {
        return true;
    };
    if message.role == LlmRole::User && !is_synthetic_context_message(message) {
        return false;
    }
    if message.role == LlmRole::Assistant {
        let has_tool_calls = message
            .content
            .iter()
            .any(|content| matches!(content, LlmContent::ToolCall { .. }));
        if has_tool_calls {
            return false;
        }
    }
    if messages
        .get(split_after + 1)
        .is_some_and(|next| next.role == LlmRole::Tool)
    {
        return false;
    }
    true
}

fn can_compact_before(messages: &[LlmMessage], keep_start: usize) -> bool {
    if keep_start == 0 {
        return false;
    }
    can_split_after(messages, keep_start - 1)
}

fn adjust_keep_start_to_safe_boundary(
    messages: &[LlmMessage],
    turn_starts: &[usize],
    mut keep_start: usize,
) -> Option<usize> {
    while keep_start > 0 && !can_compact_before(messages, keep_start) {
        let previous = turn_starts
            .iter()
            .rev()
            .copied()
            .find(|index| *index < keep_start)?;
        keep_start = previous;
    }
    can_compact_before(messages, keep_start).then_some(keep_start)
}

fn split_compact_start(messages: &[LlmMessage], keep_recent_turns: Option<usize>) -> Option<usize> {
    let has_compressible = messages
        .iter()
        .any(|m| m.role == LlmRole::Assistant && !is_synthetic_context_message(m));
    if !has_compressible {
        return None;
    }

    let turn_starts = user_turn_starts(messages);
    // 将 `keep_recent_turns` 兜底为1，确保默认保留最近一轮llm消息，避免压缩掉所有消息导致信息丢失
    let keep_turns = keep_recent_turns.unwrap_or(1);
    if keep_turns >= turn_starts.len() {
        return None;
    }

    if keep_turns == 0 {
        return Some(messages.len());
    }

    let candidate = turn_starts
        .get(turn_starts.len().saturating_sub(keep_turns))
        .copied()?;
    adjust_keep_start_to_safe_boundary(messages, &turn_starts, candidate)
}

fn removed_visible_messages(messages: &[LlmMessage]) -> usize {
    messages
        .iter()
        .filter(|message| !is_synthetic_context_message(message))
        .count()
}

fn prepare_compact_parts(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    keep_recent_turns: Option<usize>,
) -> Result<PreparedCompactParts, CompactSkipReason> {
    if messages.is_empty() {
        return Err(CompactSkipReason::Empty);
    }
    let keep_start = split_compact_start(messages, keep_recent_turns)
        .ok_or(CompactSkipReason::NothingToCompact)?;

    let prefix = messages[..keep_start].to_vec();
    let prepared_input = plan::prepare_compact_input(&prefix);
    if prepared_input.messages.is_empty() {
        return Err(CompactSkipReason::NothingToCompact);
    }

    let retained_messages = messages[keep_start..].to_vec();
    let pre_tokens = estimate_request_tokens(messages, system_prompt);
    let messages_removed = removed_visible_messages(&prefix);
    Ok(PreparedCompactParts {
        prefix,
        prepared_input,
        retained_messages,
        pre_tokens,
        messages_removed,
    })
}

fn user_turn_starts(messages: &[LlmMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (message.role == LlmRole::User && !is_synthetic_context_message(message))
                .then_some(index)
        })
        .collect()
}

fn request_messages(
    prepared_input: &PreparedCompactInput,
    message_start: usize,
    system_prompt: Option<&str>,
    settings: &ContextSettings,
    repair_feedback: Option<&str>,
    custom_instructions: &[String],
) -> Vec<LlmMessage> {
    let input_messages = &prepared_input.messages[message_start..];
    let mut messages = Vec::with_capacity(input_messages.len() + 2);
    if let Some(system_prompt) = system_prompt.filter(|value| !value.trim().is_empty()) {
        messages.push(LlmMessage::system(system_prompt.to_string()));
    }
    messages.extend_from_slice(input_messages);
    messages.push(LlmMessage::user(prompt::render_compact_request(
        &prepared_input.prompt_mode,
        settings,
        repair_feedback,
        custom_instructions,
    )));
    messages
}

fn api_round_starts(messages: &[LlmMessage]) -> Vec<usize> {
    if messages.is_empty() {
        return Vec::new();
    }
    std::iter::once(0)
        .chain(
            messages
                .iter()
                .enumerate()
                .skip(1)
                .filter_map(|(index, message)| (message.role == LlmRole::User).then_some(index)),
        )
        .collect()
}

fn finish_compact_summary(
    summary: String,
    retained_messages: Vec<LlmMessage>,
    pre_tokens: usize,
    messages_removed: usize,
    system_prompt: Option<&str>,
    render_options: &CompactSummaryRenderOptions,
) -> CompactResult {
    let context_messages = vec![LlmMessage::user(assemble::compact_summary_message_text(
        &summary,
        render_options,
    ))];
    let post_tokens = estimate_request_tokens(
        &[context_messages.clone(), retained_messages.clone()].concat(),
        system_prompt,
    );

    CompactResult {
        pre_tokens,
        post_tokens,
        summary,
        messages_removed,
        context_messages,
        retained_messages,
        transcript_path: render_options.transcript_path.clone(),
    }
}

fn summarize_prefix(messages: &[LlmMessage]) -> String {
    let mut lines = vec![
        "1. Primary Request and Intent:".to_string(),
        format!("   - Compacted {} earlier messages.", messages.len()),
        String::new(),
        "2. Key Technical Concepts:".to_string(),
        "   - (unknown from deterministic fallback)".to_string(),
        String::new(),
        "3. Files and Code Sections:".to_string(),
        "   - (none)".to_string(),
        String::new(),
        "4. Errors and fixes:".to_string(),
        "   - (none)".to_string(),
        String::new(),
        "5. Problem Solving:".to_string(),
        "   - Deterministic fallback summary was used because provider-backed compact was \
         unavailable."
            .to_string(),
        String::new(),
        "6. All user messages:".to_string(),
    ];

    for message in messages.iter().rev().take(12).rev() {
        let role = message.role.as_str();
        let text = visible_message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        let text = compact_inline(&text, MAX_SUMMARY_LINE_CHARS);
        if message.role == LlmRole::User {
            lines.push(format!("   - {text}"));
        } else {
            lines.push(format!("   - {role}: {text}"));
        }
    }
    lines.extend([
        String::new(),
        "7. Pending Tasks:".to_string(),
        "   - (unknown)".to_string(),
        String::new(),
        "8. Current Work:".to_string(),
        "   - (unknown)".to_string(),
        String::new(),
        "9. Optional Next Step:".to_string(),
        "   - (none)".to_string(),
    ]);

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    fn assistant_tool_call(call_id: &str, name: &str, arguments: serde_json::Value) -> LlmMessage {
        LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: call_id.into(),
                name: name.into(),
                arguments,
            }],
            name: None,
            reasoning_content: None,
        }
    }

    fn valid_compact_summary() -> &'static str {
        r#"<analysis>
The summary should preserve the compact contract and omit this scratchpad later.
</analysis>

<summary>
1. Primary Request and Intent:
   preserve structure

2. Key Technical Concepts:
   - compact

3. Files and Code Sections:
   - crates/astrcode-context/src/compaction/mod.rs

4. Errors and fixes:
   - (none)

5. Problem Solving:
   compacted

6. All user messages:
   - user asked for compact

7. Pending Tasks:
   - (none)

8. Current Work:
   compact parser

9. Optional Next Step:
   - (none)
</summary>"#
    }

    #[test]
    fn compact_keeps_recent_user_turns_and_builds_context_message() {
        let messages = vec![
            LlmMessage::user("old one"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("old two"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("recent"),
        ];
        let result =
            compact_messages_with_render_options(&messages, None, &Default::default()).unwrap();

        assert_eq!(result.messages_removed, 4);
        assert_eq!(result.retained_messages.len(), 1);
        assert_eq!(visible_message_text(&result.retained_messages[0]), "recent");
        assert!(is_compact_summary_message(&result.context_messages[0]));
        assert!(visible_message_text(&result.context_messages[0]).contains("Summary:\n"));
    }

    #[test]
    fn compact_turn_split_ignores_synthetic_context_messages() {
        let messages = vec![
            LlmMessage::user(assemble::compact_summary_message_text(
                "old compacted work",
                &CompactSummaryRenderOptions::default(),
            )),
            LlmMessage::user("old real"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("recent real"),
        ];
        let result =
            compact_messages_with_render_options(&messages, None, &Default::default()).unwrap();

        assert_eq!(result.retained_messages.len(), 1);
        assert_eq!(
            visible_message_text(&result.retained_messages[0]),
            "recent real"
        );
        assert_eq!(result.messages_removed, 2);
    }

    #[test]
    fn can_split_after_rejects_unsafe_boundaries() {
        let messages = vec![
            LlmMessage::user("u1"),
            LlmMessage::assistant("a1"),
            LlmMessage::user("u2"),
            assistant_tool_call("call-1", "tool", json!({})),
            LlmMessage::tool("tool", "call-1", "ok", false),
            LlmMessage::user("u3"),
        ];
        assert!(!can_split_after(&messages, 0));
        assert!(!can_split_after(&messages, 2));
        assert!(!can_split_after(&messages, 3));
        assert!(can_split_after(&messages, 4));
    }

    #[test]
    fn split_compact_start_skips_unsafe_user_turn_boundary() {
        let messages = vec![
            LlmMessage::user("old"),
            assistant_tool_call("c1", "read", json!({"path": "a"})),
            LlmMessage::tool("read", "c1", "done", false),
            LlmMessage::user("recent"),
            LlmMessage::assistant("done"),
        ];
        let keep_start = split_compact_start(&messages, Some(1)).unwrap();
        assert_eq!(keep_start, 3);
        assert_eq!(visible_message_text(&messages[keep_start]), "recent");
    }

    #[test]
    fn compact_keep_recent_turns_supports_zero_and_exact_tail_count() {
        let messages = vec![
            LlmMessage::user("u1"),
            LlmMessage::assistant("a1"),
            LlmMessage::user("u2"),
            LlmMessage::assistant("a2"),
            LlmMessage::user("u3"),
            LlmMessage::assistant("a3"),
        ];

        let full = compact_messages_with_render_options_and_keep(
            &messages,
            None,
            &Default::default(),
            Some(0),
        )
        .unwrap();
        assert!(full.retained_messages.is_empty());
        assert_eq!(full.messages_removed, 6);

        let keep_two = compact_messages_with_render_options_and_keep(
            &messages,
            None,
            &Default::default(),
            Some(2),
        )
        .unwrap();
        assert_eq!(keep_two.retained_messages.len(), 4);
        assert_eq!(visible_message_text(&keep_two.retained_messages[0]), "u2");
        assert_eq!(keep_two.messages_removed, 2);

        let nothing = compact_messages_with_render_options_and_keep(
            &messages,
            None,
            &Default::default(),
            Some(3),
        );
        assert!(matches!(nothing, Err(CompactSkipReason::NothingToCompact)));
    }

    #[test]
    fn prompt_too_long_classifier_ignores_rate_limits() {
        assert!(is_prompt_too_long_message(
            "maximum context length exceeded"
        ));
        assert!(!is_prompt_too_long_message(
            "rate limit: too many tokens per minute"
        ));
    }

    #[test]
    fn format_compact_summary_strips_analysis_and_summary_xml() {
        let raw = r#"
<analysis>
scratchpad that should not survive
</analysis>

<summary>
1. Primary Request and Intent:
   migrate context-window
</summary>
"#;

        let formatted = format_compact_summary(raw);

        assert_eq!(
            formatted,
            "Summary:\n1. Primary Request and Intent:\n   migrate context-window"
        );
        assert!(!formatted.contains("<analysis>"));
        assert!(!formatted.contains("<summary>"));
    }

    #[test]
    fn compact_summary_message_adds_fixed_context_and_parses_model_summary() {
        let message = assemble::compact_summary_message_text(
            "1. Primary Request and Intent:\n   keep user intent",
            &CompactSummaryRenderOptions {
                transcript_path: Some("C:\\Users\\18794\\.astrcode\\compact.jsonl".into()),
                custom_instructions: Vec::new(),
            },
        );

        assert!(message.starts_with("<compact_summary>\nThis session is being continued"));
        assert!(message.contains("Resume directly: do not acknowledge this summary"));
        assert!(message.contains("Summary:\n1. Primary Request and Intent:"));
        assert!(message.contains("read the full transcript at C:\\Users\\18794"));

        let parsed = parse_compact_summary_message(&message).unwrap();
        assert_eq!(
            parsed.summary,
            "1. Primary Request and Intent:\n   keep user intent"
        );
    }

    #[test]
    fn parse_compact_output_accepts_required_nine_section_summary() {
        let parsed = parse_compact_output(valid_compact_summary()).unwrap();

        assert!(parsed.summary.contains("Primary Request and Intent"));
        assert!(!parsed.summary.contains("scratchpad"));
        assert!(!parsed.summary.contains("<analysis>"));
    }

    #[test]
    fn parse_compact_output_rejects_missing_required_section() {
        let raw = r#"
<summary>
1. Primary Request and Intent:
   preserve structure
</summary>
"#;

        let error = parse_compact_output(raw).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("compact summary missing required section title")
        );
    }

    #[test]
    fn compact_template_contains_required_nine_section_contract() {
        let settings = ContextSettings::default();
        let prompt = prompt::render_compact_contract(
            Some("system prompt"),
            &plan::CompactPromptMode::Fresh,
            &settings,
            None,
            &[],
        );

        for section in parse::REQUIRED_SUMMARY_SECTIONS {
            assert!(prompt.contains(section), "missing {section}");
        }
        assert!(prompt.contains("<summary>"));
        assert!(prompt.contains("<analysis>"));
        assert!(prompt.contains("scratchpad"));
        assert!(!prompt.contains("<recent_user_context_digest>"));
    }

    #[test]
    fn compact_repair_prompt_preserves_analysis_then_summary_contract() {
        let settings = ContextSettings::default();
        let prompt = prompt::render_compact_contract(
            None,
            &plan::CompactPromptMode::Fresh,
            &settings,
            Some("missing section"),
            &[],
        );

        assert!(
            prompt.contains("Return one <analysis> scratchpad block followed by the <summary>")
        );
        assert!(!prompt.contains("Return the <summary> block exactly"));
    }

    #[tokio::test]
    async fn compact_request_closure_receives_forked_prompt() {
        let settings = ContextSettings::default();
        let messages = vec![
            LlmMessage::user("old user"),
            LlmMessage::assistant("old answer"),
            LlmMessage::user("recent user"),
        ];
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_for_request = Arc::clone(&captured);

        let result = compact_messages_with_request(
            &messages,
            Some("main system prompt"),
            &settings,
            &[String::from("preserve compact instruction")],
            &CompactSummaryRenderOptions::default(),
            None,
            move |request| {
                *captured_for_request.lock().unwrap() = request;
                async { Ok(valid_compact_summary().to_string()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(result.messages_removed, 2);
        let request = captured.lock().unwrap();

        assert_eq!(request[0].role, LlmRole::System);
        assert_eq!(visible_message_text(&request[0]), "main system prompt");
        assert_eq!(request.last().unwrap().role, LlmRole::User);
        let summary_request = visible_message_text(request.last().unwrap());
        assert!(summary_request.contains("Do not call tools"));
        assert!(summary_request.contains("<analysis>"));
        assert!(summary_request.contains("1. Primary Request and Intent:"));
        assert!(summary_request.contains("<summary>"));
        assert!(summary_request.contains("preserve compact instruction"));
        assert!(!summary_request.contains("Current runtime system prompt"));
    }

    #[tokio::test]
    async fn compact_request_renders_tool_results_as_transcript_text() {
        let settings = ContextSettings::default();
        let messages = vec![
            LlmMessage::user("read a file"),
            LlmMessage {
                role: LlmRole::Assistant,
                content: vec![LlmContent::ToolCall {
                    call_id: "call-read".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "src/lib.rs" }),
                }],
                name: None,
                reasoning_content: None,
            },
            LlmMessage::tool("read", "call-read", "pub fn compact_fixture() {}", false),
            LlmMessage::assistant("The file defines compact_fixture."),
            LlmMessage::user("current request"),
        ];
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_for_request = Arc::clone(&captured);

        compact_messages_with_request(
            &messages,
            None,
            &settings,
            &[],
            &CompactSummaryRenderOptions::default(),
            None,
            move |request| {
                *captured_for_request.lock().unwrap() = request;
                async { Ok(valid_compact_summary().to_string()) }
            },
        )
        .await
        .unwrap();

        let request = captured.lock().unwrap();
        assert!(
            request.iter().all(|message| message.role != LlmRole::Tool),
            "compact request should be plain transcript text, not provider tool protocol"
        );
        assert!(request.iter().any(|message| {
            visible_message_text(message).contains("tool read result")
                && visible_message_text(message).contains("pub fn compact_fixture()")
        }));
    }

    #[tokio::test]
    async fn compact_prompt_too_long_drops_oldest_api_round_and_retries() {
        let settings = ContextSettings::default();
        let messages = vec![
            LlmMessage::user("round one user"),
            LlmMessage::assistant("round one assistant"),
            LlmMessage::user("round two user"),
            LlmMessage::assistant("round two assistant"),
            LlmMessage::user("round three user"),
            LlmMessage::assistant("round three assistant"),
            LlmMessage::user("current user"),
        ];
        let attempts = Arc::new(Mutex::new(0usize));
        let requests = Arc::new(Mutex::new(Vec::<Vec<LlmMessage>>::new()));
        let attempts_for_request = Arc::clone(&attempts);
        let requests_for_request = Arc::clone(&requests);

        let result = compact_messages_with_request(
            &messages,
            None,
            &settings,
            &[],
            &CompactSummaryRenderOptions::default(),
            None,
            move |request| {
                requests_for_request.lock().unwrap().push(request);
                let attempts = Arc::clone(&attempts_for_request);
                async move {
                    let mut attempts = attempts.lock().unwrap();
                    *attempts += 1;
                    if *attempts == 1 {
                        Err(
                            LlmError::PromptTooLong("compact request exceeded context".into())
                                .into(),
                        )
                    } else {
                        Ok(valid_compact_summary().to_string())
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(result.messages_removed, 6);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .iter()
                .any(|message| { visible_message_text(message).contains("round one user") })
        );
        assert!(!requests[1].iter().any(|message| {
            visible_message_text(message).contains("round one user")
                || visible_message_text(message).contains("round one assistant")
        }));
        assert!(
            requests[1]
                .iter()
                .any(|message| { visible_message_text(message).contains("round two user") })
        );
    }
}
