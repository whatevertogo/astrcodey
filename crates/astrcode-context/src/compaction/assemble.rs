//! Compact summary 的上下文消息封装。
//!
//! Parser 负责校验模型输出；assembler 负责把摘要变成后续 provider request
//! 能稳定识别的 synthetic user message。

use super::{COMPACT_SUMMARY_END, COMPACT_SUMMARY_MARKER, parse::extract_summary_for_context};

pub const COMPACT_CONTINUATION_PREAMBLE: &str =
    "This session is being continued from a previous conversation that ran out of context. The \
     summary below covers the earlier portion of the conversation.";
pub const COMPACT_TRANSCRIPT_HINT_PREFIX: &str =
    "If you need specific details from before compaction (like exact code snippets, error \
     messages, or content you generated), read the full transcript at ";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactSummaryRenderOptions {
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummaryEnvelope {
    /// 已去掉 `<compact_summary>` 包装和 `Summary:` 前缀的正文。
    pub summary: String,
}

/// 将任意摘要文本标准化为 `Summary:\n...` 形态。
pub fn format_compact_summary(summary: &str) -> String {
    let summary = extract_summary_for_context(summary);
    if summary
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("summary:")
    {
        summary.trim().to_string()
    } else {
        format!("Summary:\n{}", summary.trim())
    }
}

/// 构造压缩后重新注入 provider history 的 synthetic user message 文本。
pub(crate) fn compact_summary_message_text(
    summary: &str,
    options: &CompactSummaryRenderOptions,
) -> String {
    let mut body = vec![
        COMPACT_CONTINUATION_PREAMBLE.to_string(),
        String::new(),
        format_compact_summary(summary),
    ];

    if let Some(path) = options
        .transcript_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        body.extend([
            String::new(),
            format!("{COMPACT_TRANSCRIPT_HINT_PREFIX}{path}"),
        ]);
    }

    format!(
        "{COMPACT_SUMMARY_MARKER}\n{}\n{COMPACT_SUMMARY_END}",
        body.join("\n")
    )
}

/// 从 synthetic compact message 中取回摘要正文。
pub(crate) fn parse_compact_summary_message(content: &str) -> Option<CompactSummaryEnvelope> {
    let trimmed = content.trim();
    let body = trimmed
        .strip_prefix(COMPACT_SUMMARY_MARKER)
        .and_then(|value| value.trim().strip_suffix(COMPACT_SUMMARY_END))
        .map(str::trim)
        .unwrap_or(trimmed);
    let body = strip_compact_preamble(body);
    let body = strip_compact_transcript_hint(body);
    let summary = body
        .trim_start()
        .strip_prefix("Summary:")
        .unwrap_or(body)
        .trim();
    (!summary.is_empty()).then(|| CompactSummaryEnvelope {
        summary: summary.to_string(),
    })
}

fn strip_compact_preamble(body: &str) -> &str {
    body.trim_start()
        .strip_prefix(COMPACT_CONTINUATION_PREAMBLE)
        .map(str::trim)
        .unwrap_or(body)
}

fn strip_compact_transcript_hint(body: &str) -> &str {
    let trimmed = body.trim_end();
    let Some((prefix, last_line)) = trimmed.rsplit_once('\n') else {
        return trimmed;
    };
    if last_line
        .trim_start()
        .starts_with(COMPACT_TRANSCRIPT_HINT_PREFIX)
    {
        prefix.trim_end()
    } else {
        trimmed
    }
}

/// 摘要进入长期上下文前的最后清理。
pub(crate) fn sanitize_compact_summary(summary: &str) -> String {
    let collapsed = collapse_compaction_whitespace(summary);
    redact_route_sensitive_tokens(&collapsed)
}

/// 合并多余空行和行尾空白，避免 compact summary 自身继续膨胀。
pub(crate) fn collapse_compaction_whitespace(content: &str) -> String {
    let mut output = String::new();
    let mut blank_seen = false;
    for line in content.lines().map(str::trim_end) {
        if line.trim().is_empty() {
            if !blank_seen && !output.trim().is_empty() {
                output.push('\n');
                output.push('\n');
            }
            blank_seen = true;
        } else {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(line);
            blank_seen = false;
        }
    }
    output.trim().to_string()
}

/// 避免把运行时路由 ID 写进长期摘要后继续传播。
fn redact_route_sensitive_tokens(content: &str) -> String {
    let mut redacted = String::with_capacity(content.len());
    let mut token = String::new();
    for ch in content.chars() {
        if ch.is_whitespace() {
            if !token.is_empty() {
                redacted.push_str(&redact_route_token(&token));
                token.clear();
            }
            redacted.push(ch);
        } else {
            token.push(ch);
        }
    }
    if !token.is_empty() {
        redacted.push_str(&redact_route_token(&token));
    }
    redacted
}

fn redact_route_token(token: &str) -> String {
    let trimmed =
        token.trim_matches(|ch: char| matches!(ch, '`' | '"' | '\'' | ',' | ';' | ')' | ']' | '}'));
    if trimmed.starts_with("root-agent:") || trimmed.starts_with("agent-") {
        token.replace(trimmed, "<agent-id>")
    } else if trimmed.starts_with("subrun-") {
        token.replace(trimmed, "<subrun-id>")
    } else if trimmed.starts_with("session-") {
        token.replace(trimmed, "<session-id>")
    } else {
        token.to_string()
    }
}
