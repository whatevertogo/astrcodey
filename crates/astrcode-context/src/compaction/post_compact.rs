//! Post-compact context helpers.

use std::collections::{HashMap, HashSet};

use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};

use super::assemble::collapse_compaction_whitespace;
use crate::token_usage::{estimate_text_tokens, truncate_text_to_tokens};

const POST_COMPACT_CONTEXT_MARKER: &str = "<post_compact_context>";
const POST_COMPACT_CONTEXT_END: &str = "</post_compact_context>";
const MAX_FILES_TO_RESTORE: usize = 5;
const FILE_TOKEN_BUDGET: usize = 50_000;
const MAX_TOKENS_PER_FILE: usize = 5_000;
const TRUNCATION_MARKER: &str = "\n\n[... file content truncated after compaction; use read on \
                                 this path if more detail is needed]";

#[derive(Debug, Clone)]
pub struct PostCompactFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct PostCompactNote {
    pub title: String,
    pub body: String,
}

pub(crate) fn is_post_compact_context_message(message: &LlmMessage) -> bool {
    message.role == LlmRole::User
        && message.content.iter().any(|content| {
            matches!(
                content,
                LlmContent::Text { text }
                    if text.trim_start().starts_with(POST_COMPACT_CONTEXT_MARKER)
            )
        })
}

pub fn recent_read_paths(
    source_messages: &[LlmMessage],
    retained_messages: &[LlmMessage],
) -> Vec<String> {
    let retained_paths = read_paths(retained_messages);
    let mut seen_paths = HashSet::new();
    let mut paths = Vec::new();
    for path in read_paths_in_order(source_messages).into_iter().rev() {
        if retained_paths.contains(&path) || !seen_paths.insert(path.clone()) {
            continue;
        }
        paths.push(path);
        if paths.len() >= MAX_FILES_TO_RESTORE {
            break;
        }
    }
    paths.reverse();
    paths
}

pub fn post_compact_context_message(
    files: Vec<PostCompactFile>,
    notes: Vec<PostCompactNote>,
) -> Option<LlmMessage> {
    let files = budget_files(files);
    if files.is_empty() && notes.is_empty() {
        return None;
    }
    Some(LlmMessage::user(render_post_compact_context(
        &files, &notes,
    )))
}

fn budget_files(files: Vec<PostCompactFile>) -> Vec<PostCompactFile> {
    let mut used_tokens = 0usize;
    let mut kept = Vec::new();
    for file in files.into_iter().take(MAX_FILES_TO_RESTORE) {
        let content = truncate_to_tokens(&file.content, MAX_TOKENS_PER_FILE);
        let tokens = estimate_text_tokens(&file.path) + estimate_text_tokens(&content);
        if used_tokens.saturating_add(tokens) > FILE_TOKEN_BUDGET {
            continue;
        }
        used_tokens += tokens;
        kept.push(PostCompactFile {
            path: file.path,
            content,
        });
    }
    kept
}

fn read_paths(messages: &[LlmMessage]) -> HashSet<String> {
    read_call_paths(messages).into_values().collect()
}

fn read_paths_in_order(messages: &[LlmMessage]) -> Vec<String> {
    let call_paths = read_call_paths(messages);
    let mut paths = Vec::new();

    for message in messages {
        if message.role != LlmRole::Tool || !message.name.as_deref().is_some_and(is_read_tool) {
            continue;
        }
        for content in &message.content {
            let LlmContent::ToolResult {
                tool_call_id,
                content: _,
                is_error,
            } = content
            else {
                continue;
            };
            if *is_error {
                continue;
            }
            if let Some(path) = call_paths.get(tool_call_id) {
                paths.push(path.clone());
            }
        }
    }

    paths
}

fn read_call_paths(messages: &[LlmMessage]) -> HashMap<String, String> {
    let mut paths = HashMap::new();
    for message in messages {
        if message.role != LlmRole::Assistant {
            continue;
        }
        for content in &message.content {
            let LlmContent::ToolCall {
                call_id,
                name,
                arguments,
            } = content
            else {
                continue;
            };
            if !is_read_tool(name) {
                continue;
            }
            if let Some(path) = arguments.get("path").and_then(|value| value.as_str()) {
                paths.insert(call_id.clone(), path.to_string());
            }
        }
    }
    paths
}

fn is_read_tool(name: &str) -> bool {
    name.eq_ignore_ascii_case("read")
}

fn truncate_to_tokens(content: &str, max_tokens: usize) -> String {
    truncate_text_to_tokens(content, max_tokens, TRUNCATION_MARKER)
}

fn render_post_compact_context(files: &[PostCompactFile], notes: &[PostCompactNote]) -> String {
    let mut lines = vec![
        POST_COMPACT_CONTEXT_MARKER.to_string(),
        "The compact summary removed some operational context. The entries below were restored \
         after compaction for continuity."
            .to_string(),
    ];

    if !files.is_empty() {
        lines.extend([String::new(), "## Recent Read Files".to_string()]);
        for file in files {
            lines.extend([
                String::new(),
                format!("### {}", file.path),
                "```text".to_string(),
                collapse_compaction_whitespace(&file.content),
                "```".to_string(),
            ]);
        }
    }

    if !notes.is_empty() {
        lines.extend([String::new(), "## Runtime Notes".to_string()]);
        for note in notes {
            lines.extend([
                String::new(),
                format!("### {}", note.title),
                collapse_compaction_whitespace(&note.body),
            ]);
        }
    }

    lines.push(POST_COMPACT_CONTEXT_END.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn read_call(call_id: &str, path: &str) -> LlmMessage {
        LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: call_id.into(),
                name: "read".into(),
                arguments: json!({ "path": path }),
            }],
            name: None,
        }
    }

    fn read_result(call_id: &str) -> LlmMessage {
        LlmMessage::tool("read", call_id, "read content", false)
    }

    fn file(path: &str, content: &str) -> PostCompactFile {
        PostCompactFile {
            path: path.into(),
            content: content.into(),
        }
    }

    fn render(files: Vec<PostCompactFile>) -> String {
        let message = post_compact_context_message(files, Vec::new()).unwrap();
        message_text(&message)
    }

    fn read_result_with_content(call_id: &str, content: &str) -> LlmMessage {
        LlmMessage::tool("read", call_id, content, false)
    }

    #[test]
    fn extracts_recent_read_paths_excluded_from_retained_tail() {
        let source = vec![
            read_call("old", "src/old.rs"),
            read_result("old"),
            LlmMessage::assistant("answer"),
            read_call("recent", "src/recent.rs"),
            read_result("recent"),
        ];
        let retained = vec![LlmMessage::assistant("answer")];

        let paths = recent_read_paths(&source, &retained);

        assert_eq!(paths, ["src/old.rs", "src/recent.rs"]);
    }

    #[test]
    fn skips_reads_already_visible_in_retained_tail() {
        let source = vec![
            read_call("old", "src/old.rs"),
            read_result("old"),
            read_call("recent", "src/recent.rs"),
            read_result("recent"),
        ];
        let retained = vec![read_call("recent", "src/recent.rs"), read_result("recent")];

        let paths = recent_read_paths(&source, &retained);

        assert_eq!(paths, ["src/old.rs"]);
    }

    #[test]
    fn keeps_most_recent_unique_reads_under_count_limit() {
        let mut source = Vec::new();
        for index in 0..7 {
            let call_id = format!("call-{index}");
            source.push(read_call(&call_id, &format!("src/{index}.rs")));
            source.push(read_result(&call_id));
        }

        let paths = recent_read_paths(&source, &[]);

        assert_eq!(
            paths,
            ["src/2.rs", "src/3.rs", "src/4.rs", "src/5.rs", "src/6.rs"]
        );
    }

    #[test]
    fn renders_restored_files_and_runtime_notes() {
        let message = post_compact_context_message(
            vec![file("src/lib.rs", "fresh content")],
            vec![PostCompactNote {
                title: "Plan File".into(),
                body: "plan body".into(),
            }],
        )
        .unwrap();

        let text = message_text(&message);
        assert!(text.contains("<post_compact_context>"));
        assert!(text.contains("src/lib.rs"));
        assert!(text.contains("fresh content"));
        assert!(text.contains("Plan File"));
        assert!(text.contains("plan body"));
    }

    #[test]
    fn render_truncates_large_file_content() {
        let text = render(vec![file("huge.rs", &"x".repeat(MAX_TOKENS_PER_FILE * 5))]);

        assert!(text.contains("huge.rs"));
        assert!(text.contains("file content truncated after compaction"));
        assert!(estimate_text_tokens(&text) < MAX_TOKENS_PER_FILE + 200);
    }

    #[test]
    fn ignores_failed_read_results() {
        let source = vec![
            read_call("ok", "src/ok.rs"),
            read_result("ok"),
            read_call("err", "src/err.rs"),
            LlmMessage::tool("read", "err", "failed", true),
            read_call("manual", "src/manual.rs"),
            read_result_with_content("manual", "manual content"),
        ];

        let paths = recent_read_paths(&source, &[]);

        assert_eq!(paths, ["src/ok.rs", "src/manual.rs"]);
    }

    fn message_text(message: &LlmMessage) -> String {
        message
            .content
            .iter()
            .map(|content| match content {
                LlmContent::Text { text } => text.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
