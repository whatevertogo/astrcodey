//! Generic rendering and budgeting for extension-contributed compact context.

use astrcode_core::llm::LlmMessage;

use super::assemble::collapse_compaction_whitespace;
use crate::{
    CompactResult, CompactRetainedContext, ContextSettings, POST_COMPACT_CONTEXT_MARKER,
    token_budget::{estimate_text_tokens, truncate_text_to_tokens},
};

const POST_COMPACT_CONTEXT_END: &str = "</post_compact_context>";
const TRUNCATION_MARKER: &str = "\n\n[... retained context truncated after compaction]";

/// Append extension-owned retained context after the compact summary.
///
/// Contributions retain dispatcher order. The context crate knows only how to budget and render
/// the typed values; discovering plans, files, agent state, or any other domain fact remains the
/// contributing extension's responsibility.
pub fn append_compact_retained_context(
    compaction: &mut CompactResult,
    contributions: Vec<CompactRetainedContext>,
    settings: &ContextSettings,
) {
    let contributions = budget_contributions(contributions, settings);
    if contributions.is_empty() {
        return;
    }
    compaction
        .summary_messages
        .push(LlmMessage::user(render_retained_context(&contributions)));
}

fn budget_contributions(
    contributions: Vec<CompactRetainedContext>,
    settings: &ContextSettings,
) -> Vec<CompactRetainedContext> {
    let mut remaining_tokens = settings.post_compact_token_budget;
    let mut retained_files = 0usize;
    let mut kept = Vec::new();

    for contribution in contributions {
        if remaining_tokens == 0 {
            break;
        }

        let contribution = match contribution {
            CompactRetainedContext::File { path, content } => {
                if retained_files >= settings.post_compact_max_files || path.trim().is_empty() {
                    continue;
                }
                let header_tokens = estimate_text_tokens(&path);
                let content_budget = remaining_tokens
                    .saturating_sub(header_tokens)
                    .min(settings.post_compact_max_tokens_per_file);
                let Some(content) = budget_body(&content, content_budget) else {
                    continue;
                };
                retained_files += 1;
                CompactRetainedContext::File { path, content }
            },
            CompactRetainedContext::Note { title, body } => {
                if title.trim().is_empty() {
                    continue;
                }
                let header_tokens = estimate_text_tokens(&title);
                let content_budget = remaining_tokens.saturating_sub(header_tokens);
                let Some(body) = budget_body(&body, content_budget) else {
                    continue;
                };
                CompactRetainedContext::Note { title, body }
            },
        };

        remaining_tokens = remaining_tokens.saturating_sub(contribution.estimated_tokens());
        kept.push(contribution);
    }
    kept
}

fn budget_body(content: &str, max_tokens: usize) -> Option<String> {
    if content.trim().is_empty() {
        return None;
    }
    if estimate_text_tokens(content) <= max_tokens {
        return Some(content.to_string());
    }
    if max_tokens < estimate_text_tokens(TRUNCATION_MARKER) {
        return None;
    }
    Some(truncate_text_to_tokens(
        content,
        max_tokens,
        TRUNCATION_MARKER,
    ))
}

fn render_retained_context(contributions: &[CompactRetainedContext]) -> String {
    let mut lines = vec![
        POST_COMPACT_CONTEXT_MARKER.to_string(),
        "Extensions retained the ordered context below because the compact summary may no longer \
         contain it."
            .to_string(),
    ];

    for contribution in contributions {
        lines.push(String::new());
        match contribution {
            CompactRetainedContext::File { path, content } => lines.extend([
                format!("## File: {path}"),
                "```text".to_string(),
                collapse_compaction_whitespace(content),
                "```".to_string(),
            ]),
            CompactRetainedContext::Note { title, body } => {
                lines.extend([format!("## {title}"), collapse_compaction_whitespace(body)])
            },
        }
    }

    lines.push(POST_COMPACT_CONTEXT_END.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> CompactRetainedContext {
        CompactRetainedContext::File {
            path: path.into(),
            content: content.into(),
        }
    }

    fn note(title: &str, body: &str) -> CompactRetainedContext {
        CompactRetainedContext::Note {
            title: title.into(),
            body: body.into(),
        }
    }

    #[test]
    fn retained_context_preserves_order_and_applies_one_global_budget() {
        assert_eq!(
            budget_body("fits", estimate_text_tokens("fits")),
            Some("fits".into())
        );

        let mut settings = ContextSettings {
            post_compact_max_files: 1,
            post_compact_token_budget: 80,
            post_compact_max_tokens_per_file: 30,
            ..ContextSettings::default()
        };
        let contributions = vec![
            note("Session Plan", "implement the typed compact boundary"),
            file("src/lib.rs", &"x".repeat(1_000)),
            file("src/ignored.rs", "file count limit"),
            note("Agent Status", &"y".repeat(1_000)),
        ];
        let budgeted = budget_contributions(contributions, &settings);

        assert_eq!(budgeted.len(), 3);
        assert!(matches!(
            &budgeted[0],
            CompactRetainedContext::Note { title, .. } if title == "Session Plan"
        ));
        assert!(matches!(
            &budgeted[1],
            CompactRetainedContext::File { path, content }
                if path == "src/lib.rs" && content.contains("retained context truncated")
        ));
        assert!(matches!(
            &budgeted[2],
            CompactRetainedContext::Note { title, body }
                if title == "Agent Status" && body.contains("retained context truncated")
        ));
        assert!(
            budgeted
                .iter()
                .map(CompactRetainedContext::estimated_tokens)
                .sum::<usize>()
                <= settings.post_compact_token_budget
        );

        let mut compaction = CompactResult {
            pre_tokens: 100,
            post_tokens: 10,
            summary: "summary".into(),
            messages_removed: 3,
            summary_messages: vec![LlmMessage::user("summary")],
            retained_messages: Vec::new(),
            transcript_path: None,
        };
        append_compact_retained_context(&mut compaction, budgeted, &settings);
        let rendered = compaction.summary_messages[1].joined_display_text("\n");
        let plan = rendered.find("Session Plan").expect("plan contribution");
        let file = rendered.find("src/lib.rs").expect("file contribution");
        let agent = rendered.find("Agent Status").expect("agent contribution");
        assert!(plan < file && file < agent);
        assert!(!rendered.contains("ignored.rs"));

        settings.post_compact_token_budget = 0;
        append_compact_retained_context(
            &mut compaction,
            vec![note("Not rendered", "no budget")],
            &settings,
        );
        assert_eq!(compaction.summary_messages.len(), 2);
    }
}
