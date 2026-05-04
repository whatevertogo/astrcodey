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

use crate::{
    settings::ContextWindowSettings,
    token_usage::{estimate_request_tokens, estimate_text_tokens},
};

const COMPACT_SUMMARY_MARKER: &str = "<compact_summary>";
const COMPACT_SUMMARY_END: &str = "</compact_summary>";
const MAX_PTL_RETRIES: usize = 3;

mod assemble;
mod parse;
mod plan;
mod post_compact;
mod prompt;

pub use assemble::{CompactSummaryEnvelope, CompactSummaryRenderOptions, format_compact_summary};
pub use parse::{CompactParseError, ParsedCompactOutput, parse_compact_output};
use plan::{PreparedCompactInput, visible_message_text};
pub use post_compact::{PostCompactFile, PostCompactNote, recent_read_paths};

/// 压缩操作的结果。
///
/// 记录压缩前后的 token 数量以及 LLM 生成的摘要文本。
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// 压缩前的 token 数量。
    pub pre_tokens: usize,
    /// 压缩后的 token 数量。
    pub post_tokens: usize,
    /// LLM 生成的对话摘要。
    pub summary: String,
    /// 压缩掉的可见消息数量。
    pub messages_removed: usize,
    /// 供 provider 使用的合成上下文消息。
    pub context_messages: Vec<LlmMessage>,
    /// 保留的可见消息尾部。
    pub retained_messages: Vec<LlmMessage>,
    /// compact 前 transcript snapshot 的可读路径。
    pub transcript_path: Option<String>,
}

impl CompactResult {
    /// 追加 compact 后恢复的运行时上下文。
    ///
    /// 调用方只提供已经收集好的文件和运行时备注；是否生成 message、如何渲染
    /// `<post_compact_context>`、以及如何加入 hidden context 由 compact 模块掌握。
    pub fn append_post_compact_context(
        &mut self,
        files: Vec<PostCompactFile>,
        notes: Vec<PostCompactNote>,
    ) {
        if let Some(message) = post_compact::post_compact_context_message(files, notes) {
            self.context_messages.push(message);
        }
    }
}

struct PreparedCompactParts {
    prefix: Vec<LlmMessage>,
    prepared_input: PreparedCompactInput,
    retained_messages: Vec<LlmMessage>,
    pre_tokens: usize,
    messages_removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactSkipReason {
    /// 没有任何可压缩消息。
    Empty,
    /// 有消息，但根据当前切分策略没有安全的历史前缀可压缩。
    NothingToCompact,
}

#[derive(Debug)]
pub enum CompactError {
    Skip(CompactSkipReason),
    Parse(CompactParseError),
    Llm(LlmError),
}

impl std::fmt::Display for CompactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skip(reason) => write!(f, "compact skipped: {reason:?}"),
            Self::Parse(error) => write!(f, "compact parse error: {error}"),
            Self::Llm(error) => write!(f, "compact llm error: {error}"),
        }
    }
}

impl std::error::Error for CompactError {}

impl From<CompactSkipReason> for CompactError {
    fn from(value: CompactSkipReason) -> Self {
        Self::Skip(value)
    }
}

impl From<CompactParseError> for CompactError {
    fn from(value: CompactParseError) -> Self {
        Self::Parse(value)
    }
}

impl From<LlmError> for CompactError {
    fn from(value: LlmError) -> Self {
        Self::Llm(value)
    }
}

/// 不调用 LLM 的 compact fallback，并使用指定的 summary 渲染选项。
pub fn compact_messages_with_render_options(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    render_options: &CompactSummaryRenderOptions,
) -> Result<CompactResult, CompactSkipReason> {
    let parts = prepare_compact_parts(messages, system_prompt)?;
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
    settings: &ContextWindowSettings,
    custom_instructions: &[String],
    render_options: &CompactSummaryRenderOptions,
    mut request_text: F,
) -> Result<CompactResult, CompactError>
where
    F: FnMut(Vec<LlmMessage>) -> Fut,
    Fut: Future<Output = Result<String, CompactError>>,
{
    let parts = prepare_compact_parts(messages, system_prompt)?;
    let mut repair_feedback: Option<String> = None;
    let mut ptl_rounds_dropped = 0usize;
    let mut repair_attempts = 0u8;
    let max_attempts = settings.compact_max_retry_attempts.max(1);
    let mut last_error: Option<CompactError> = None;

    while repair_attempts < max_attempts {
        let Some(prepared_input) =
            compact_input_for_ptl_retry(&parts.prepared_input, ptl_rounds_dropped)
        else {
            break;
        };
        let compact_messages = request_messages(
            &prepared_input,
            system_prompt,
            settings,
            repair_feedback.as_deref(),
            custom_instructions,
        );
        let output = match request_text(compact_messages).await {
            Ok(output) => output,
            Err(error) if should_retry_prompt_too_long(&error) => {
                let Some(next_drop) =
                    next_ptl_retry_drop(&parts.prepared_input, ptl_rounds_dropped)
                else {
                    last_error = Some(error);
                    break;
                };
                ptl_rounds_dropped = next_drop;
                last_error = Some(error);
                continue;
            },
            Err(error) => {
                last_error = Some(error);
                break;
            },
        };
        repair_attempts += 1;
        match finish_compact_output(
            &output,
            parts.retained_messages.clone(),
            parts.pre_tokens,
            parts.messages_removed,
            system_prompt,
            render_options,
        ) {
            Ok(compaction) => return Ok(compaction),
            Err(CompactError::Parse(error)) => {
                repair_feedback = Some(error.to_string());
                last_error = Some(CompactError::Parse(error));
            },
            Err(error) => {
                last_error = Some(error);
                break;
            },
        }
    }

    Err(last_error.unwrap_or_else(|| {
        CompactParseError::new("compact response did not contain a summary").into()
    }))
}

fn should_retry_prompt_too_long(error: &CompactError) -> bool {
    matches!(error, CompactError::Llm(LlmError::PromptTooLong(_)))
        || is_prompt_too_long_message(&error.to_string())
}

pub fn parse_compact_summary_message(content: &str) -> Option<CompactSummaryEnvelope> {
    assemble::parse_compact_summary_message(content)
}

/// 判断消息是否是 compact 后注入的 synthetic context message。
pub fn is_compact_summary_message(message: &LlmMessage) -> bool {
    message.role == LlmRole::User
        && message.content.iter().any(|content| {
            matches!(
                content,
                LlmContent::Text { text }
                    if text.trim_start().starts_with(COMPACT_SUMMARY_MARKER)
            )
        })
}

/// 预留给更宽泛的 synthetic context 判断。
pub fn is_synthetic_context_message(message: &LlmMessage) -> bool {
    is_compact_summary_message(message) || post_compact::is_post_compact_context_message(message)
}

/// 粗略识别 provider 返回的上下文过长错误。
///
/// 这里故意排除 rate limit / quota 等错误，避免把限流误判为可 compact 重试。
pub fn is_prompt_too_long_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let positive = [
        "prompt too long",
        "context length",
        "maximum context",
        "too many tokens",
        "input is too long",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let negative = ["rate limit", "quota", "throttle", "timeout"]
        .iter()
        .any(|needle| lower.contains(needle));
    positive && !negative
}

fn split_compact_start(messages: &[LlmMessage]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            (index > 0
                && message.role == LlmRole::Assistant
                && !is_synthetic_context_message(message))
            .then_some(index)
        })
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
) -> Result<PreparedCompactParts, CompactSkipReason> {
    if messages.is_empty() {
        return Err(CompactSkipReason::Empty);
    }
    let keep_start = split_compact_start(messages).ok_or(CompactSkipReason::NothingToCompact)?;
    if keep_start == 0 {
        return Err(CompactSkipReason::NothingToCompact);
    }

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

fn request_messages(
    prepared_input: &PreparedCompactInput,
    system_prompt: Option<&str>,
    settings: &ContextWindowSettings,
    repair_feedback: Option<&str>,
    custom_instructions: &[String],
) -> Vec<LlmMessage> {
    let mut messages = Vec::with_capacity(prepared_input.messages.len() + 2);
    if let Some(system_prompt) = system_prompt.filter(|value| !value.trim().is_empty()) {
        messages.push(LlmMessage::system(system_prompt.to_string()));
    }
    messages.extend(prepared_input.messages.clone());
    messages.push(LlmMessage::user(prompt::render_compact_request(
        &prepared_input.prompt_mode,
        settings,
        repair_feedback,
        custom_instructions,
    )));
    messages
}

fn compact_input_for_ptl_retry(
    prepared_input: &PreparedCompactInput,
    rounds_dropped: usize,
) -> Option<PreparedCompactInput> {
    if rounds_dropped == 0 {
        return Some(prepared_input.clone());
    }
    let ranges = api_round_ranges(&prepared_input.messages);
    let start = ranges.get(rounds_dropped)?.start;
    Some(PreparedCompactInput {
        messages: prepared_input.messages[start..].to_vec(),
        prompt_mode: prepared_input.prompt_mode.clone(),
    })
}

fn next_ptl_retry_drop(
    prepared_input: &PreparedCompactInput,
    current_rounds_dropped: usize,
) -> Option<usize> {
    let next = current_rounds_dropped.saturating_add(1);
    let round_count = api_round_ranges(&prepared_input.messages).len();
    (next <= MAX_PTL_RETRIES && next < round_count).then_some(next)
}

fn api_round_ranges(messages: &[LlmMessage]) -> Vec<std::ops::Range<usize>> {
    if messages.is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (index, message) in messages.iter().enumerate().skip(1) {
        if message.role == LlmRole::User {
            ranges.push(start..index);
            start = index;
        }
    }
    ranges.push(start..messages.len());
    ranges
}

fn finish_compact_output(
    raw_output: &str,
    retained_messages: Vec<LlmMessage>,
    pre_tokens: usize,
    messages_removed: usize,
    system_prompt: Option<&str>,
    render_options: &CompactSummaryRenderOptions,
) -> Result<CompactResult, CompactError> {
    let parsed = parse_compact_output(raw_output)?;
    Ok(finish_compact_summary(
        assemble::sanitize_compact_summary(&parsed.summary),
        retained_messages,
        pre_tokens,
        messages_removed,
        system_prompt,
        render_options,
    ))
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
        if message.role == LlmRole::User {
            lines.push(format!("   - {}", truncate_summary_line(&text)));
        } else {
            lines.push(format!("   - {role}: {}", truncate_summary_line(&text)));
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

fn truncate_summary_line(text: &str) -> String {
    let max_chars = 320usize.min(estimate_text_tokens(text).saturating_mul(4).max(1));
    if text.chars().count() <= max_chars {
        return text.trim().to_string();
    }
    let mut end = 0usize;
    for (index, _) in text.char_indices().take(max_chars) {
        end = index;
    }
    format!("{}...", text[..end].trim())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

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

        assert_eq!(result.messages_removed, 3);
        assert_eq!(result.retained_messages.len(), 2);
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

        assert_eq!(result.retained_messages.len(), 2);
        assert_eq!(result.messages_removed, 1);
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
            },
        );

        assert!(message.starts_with("<compact_summary>\nThis session is being continued"));
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
        let raw = r#"
<analysis>
scratchpad that should be ignored
</analysis>

<summary>
1. Primary Request and Intent:
   preserve structure

2. Key Technical Concepts:
   - compact

3. Files and Code Sections:
   - crates/astrcode-context/src/compaction.rs

4. Errors and fixes:
   - (none)

5. Problem Solving:
   done

6. All user messages:
   - user asked for compact

7. Pending Tasks:
   - (none)

8. Current Work:
   compact parser

9. Optional Next Step:
   - (none)
</summary>
"#;

        let parsed = parse_compact_output(raw).unwrap();

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
        let settings = ContextWindowSettings::default();
        let prompt = prompt::render_compact_contract(
            Some("system prompt"),
            &plan::CompactPromptMode::Fresh,
            &settings,
            None,
            &[],
        );

        for section in [
            "1. Primary Request and Intent:",
            "2. Key Technical Concepts:",
            "3. Files and Code Sections:",
            "4. Errors and fixes:",
            "5. Problem Solving:",
            "6. All user messages:",
            "7. Pending Tasks:",
            "8. Current Work:",
            "9. Optional Next Step:",
        ] {
            assert!(prompt.contains(section), "missing {section}");
        }
        assert!(prompt.contains("<summary>"));
        assert!(prompt.contains("<analysis>"));
        assert!(prompt.contains("scratchpad"));
        assert!(!prompt.contains("<recent_user_context_digest>"));
    }

    #[test]
    fn compact_repair_prompt_preserves_analysis_then_summary_contract() {
        let settings = ContextWindowSettings::default();
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
        let settings = ContextWindowSettings::default();
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
            move |request| {
                *captured_for_request.lock().unwrap() = request;
                async { Ok(valid_compact_summary().to_string()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(result.messages_removed, 1);
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
    async fn compact_prompt_too_long_drops_oldest_api_round_and_retries() {
        let settings = ContextWindowSettings::default();
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

        assert_eq!(result.messages_removed, 5);
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
