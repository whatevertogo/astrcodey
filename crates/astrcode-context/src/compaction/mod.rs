//! LLM 驱动的上下文压缩模块。
//!
//! 当上下文窗口接近容量上限时，通过 LLM 对历史对话进行摘要压缩，
//! 保留关键信息的同时释放 token 空间。

use astrcode_core::llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole};

use crate::{
    settings::ContextWindowSettings,
    token_usage::{estimate_request_tokens, estimate_text_tokens},
};

const COMPACT_SUMMARY_MARKER: &str = "<compact_summary>";
const COMPACT_SUMMARY_END: &str = "</compact_summary>";

mod assemble;
mod parse;
mod plan;
mod prompt;

pub use assemble::{CompactSummaryEnvelope, format_compact_summary};
pub use parse::{CompactParseError, ParsedCompactOutput, parse_compact_output};
use plan::visible_message_text;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactSkipReason {
    Empty,
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

pub fn compact_messages(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
) -> Result<CompactResult, CompactSkipReason> {
    if messages.is_empty() {
        return Err(CompactSkipReason::Empty);
    }
    let keep_start = split_compact_start(messages).ok_or(CompactSkipReason::NothingToCompact)?;
    if keep_start == 0 {
        return Err(CompactSkipReason::NothingToCompact);
    }

    let prefix = &messages[..keep_start];
    let retained_messages = messages[keep_start..].to_vec();
    let pre_tokens = estimate_request_tokens(messages, system_prompt);
    let summary = summarize_prefix(prefix);
    let context_messages = vec![LlmMessage::user(assemble::compact_summary_message_text(
        &summary,
    ))];
    let post_tokens = estimate_request_tokens(
        &[context_messages.clone(), retained_messages.clone()].concat(),
        system_prompt,
    );

    Ok(CompactResult {
        pre_tokens,
        post_tokens,
        summary,
        messages_removed: removed_visible_messages(prefix),
        context_messages,
        retained_messages,
    })
}

pub async fn compact_messages_with_provider(
    provider: &dyn LlmProvider,
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    settings: &ContextWindowSettings,
) -> Result<CompactResult, CompactError> {
    if messages.is_empty() {
        return Err(CompactSkipReason::Empty.into());
    }

    let keep_start = split_compact_start(messages).ok_or(CompactSkipReason::NothingToCompact)?;
    if keep_start == 0 {
        return Err(CompactSkipReason::NothingToCompact.into());
    }

    let prefix = &messages[..keep_start];
    let retained_messages = messages[keep_start..].to_vec();
    let pre_tokens = estimate_request_tokens(messages, system_prompt);
    let mut repair_feedback: Option<String> = None;
    let max_attempts = settings.compact_max_retry_attempts.max(1);
    let mut parsed = None;
    let mut last_parse_error = None;

    for _ in 0..max_attempts {
        let prepared_input = plan::prepare_compact_input(prefix);
        if prepared_input.messages.is_empty() {
            return Err(CompactSkipReason::NothingToCompact.into());
        }
        let compact_system_prompt = prompt::render_compact_system_prompt(
            system_prompt,
            &prepared_input.prompt_mode,
            settings,
            repair_feedback.as_deref(),
        );
        let mut compact_messages = Vec::with_capacity(prepared_input.messages.len() + 1);
        compact_messages.push(LlmMessage::system(compact_system_prompt));
        compact_messages.extend(prepared_input.messages);

        let output = collect_compact_output(provider, compact_messages).await?;
        match parse_compact_output(&output) {
            Ok(candidate) => {
                if let Some(violation) = parse::compact_contract_violation(&candidate) {
                    repair_feedback = Some(violation);
                    parsed = Some(candidate);
                    continue;
                }
                parsed = Some(candidate);
                break;
            },
            Err(error) => {
                repair_feedback = Some(error.to_string());
                last_parse_error = Some(error);
            },
        }
    }

    let parsed = parsed.ok_or_else(|| {
        last_parse_error
            .unwrap_or_else(|| CompactParseError::new("compact response did not contain a summary"))
    })?;
    let summary = assemble::sanitize_compact_summary(&parsed.summary);
    let context_messages = vec![LlmMessage::user(assemble::compact_summary_message_text(
        &summary,
    ))];
    let post_tokens = estimate_request_tokens(
        &[context_messages.clone(), retained_messages.clone()].concat(),
        system_prompt,
    );

    Ok(CompactResult {
        pre_tokens,
        post_tokens,
        summary,
        messages_removed: removed_visible_messages(prefix),
        context_messages,
        retained_messages,
    })
}

pub fn parse_compact_summary_message(content: &str) -> Option<CompactSummaryEnvelope> {
    assemble::parse_compact_summary_message(content)
}

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

pub fn is_synthetic_context_message(message: &LlmMessage) -> bool {
    is_compact_summary_message(message)
}

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

async fn collect_compact_output(
    provider: &dyn LlmProvider,
    messages: Vec<LlmMessage>,
) -> Result<String, LlmError> {
    let mut rx = provider.generate(messages, Vec::new()).await?;
    let mut output = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => output.push_str(&delta),
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => return Err(LlmError::StreamParse(message)),
            LlmEvent::ToolCallStart { .. } | LlmEvent::ToolCallDelta { .. } => {},
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use astrcode_core::llm::ModelLimits;
    use tokio::sync::mpsc;

    use super::*;

    struct CapturingCompactProvider {
        messages: Arc<Mutex<Vec<LlmMessage>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CapturingCompactProvider {
        async fn generate(
            &self,
            messages: Vec<LlmMessage>,
            _tools: Vec<astrcode_core::tool::ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            *self.messages.lock().unwrap() = messages;
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: valid_compact_summary().into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    fn valid_compact_summary() -> &'static str {
        r#"<summary>
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
        let result = compact_messages(&messages, None).unwrap();

        assert_eq!(result.messages_removed, 3);
        assert_eq!(result.retained_messages.len(), 2);
        assert!(is_compact_summary_message(&result.context_messages[0]));
        assert!(visible_message_text(&result.context_messages[0]).contains("Summary:\n"));
    }

    #[test]
    fn compact_turn_split_ignores_synthetic_context_messages() {
        let messages = vec![
            LlmMessage::user(assemble::compact_summary_message_text("old compacted work")),
            LlmMessage::user("old real"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("recent real"),
        ];
        let result = compact_messages(&messages, None).unwrap();

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
    fn parse_compact_output_accepts_required_nine_section_summary() {
        let raw = r#"
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
        let prompt = prompt::render_compact_system_prompt(
            Some("system prompt"),
            &plan::CompactPromptMode::Fresh,
            &settings,
            None,
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
        assert!(!prompt.contains("<analysis>"));
        assert!(!prompt.contains("<recent_user_context_digest>"));
    }

    #[tokio::test]
    async fn provider_compact_uses_incremental_prompt_when_previous_summary_exists() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingCompactProvider {
            messages: Arc::clone(&captured),
        };
        let messages = vec![
            LlmMessage::user(assemble::compact_summary_message_text(
                "prior compact summary",
            )),
            LlmMessage::user("old real"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("recent real"),
        ];

        let result = compact_messages_with_provider(
            &provider,
            &messages,
            None,
            &ContextWindowSettings::default(),
        )
        .await
        .unwrap();

        assert_eq!(result.messages_removed, 1);
        let sent = captured.lock().unwrap();
        let compact_system_prompt = visible_message_text(&sent[0]);
        assert!(compact_system_prompt.contains("## Incremental Mode"));
        assert!(compact_system_prompt.contains("prior compact summary"));
    }
}
