use super::{COMPACT_SUMMARY_END, COMPACT_SUMMARY_MARKER, parse::extract_summary_for_context};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummaryEnvelope {
    pub summary: String,
}

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

pub(crate) fn compact_summary_message_text(summary: &str) -> String {
    format!(
        "{COMPACT_SUMMARY_MARKER}\n{}\n{COMPACT_SUMMARY_END}",
        format_compact_summary(summary)
    )
}

pub(crate) fn parse_compact_summary_message(content: &str) -> Option<CompactSummaryEnvelope> {
    let trimmed = content.trim();
    let body = trimmed
        .strip_prefix(COMPACT_SUMMARY_MARKER)
        .and_then(|value| value.trim().strip_suffix(COMPACT_SUMMARY_END))
        .map(str::trim)
        .unwrap_or(trimmed);
    let summary = body
        .trim_start()
        .strip_prefix("Summary:")
        .unwrap_or(body)
        .trim();
    (!summary.is_empty()).then(|| CompactSummaryEnvelope {
        summary: summary.to_string(),
    })
}

pub(crate) fn sanitize_compact_summary(summary: &str) -> String {
    let collapsed = collapse_compaction_whitespace(summary);
    redact_route_sensitive_tokens(&collapsed)
}

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
