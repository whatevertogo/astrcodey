//! Post-compact context helpers.

use std::collections::{HashMap, HashSet};

use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};

use super::assemble::collapse_compaction_whitespace;
use crate::{
    ContextSettings,
    token_budget::{estimate_text_tokens, truncate_text_to_tokens},
};

const POST_COMPACT_CONTEXT_MARKER: &str = "<post_compact_context>";
const POST_COMPACT_CONTEXT_END: &str = "</post_compact_context>";
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
    settings: &ContextSettings,
) -> Vec<String> {
    let retained_index = scan_tool_messages(retained_messages);
    let retained_paths = read_paths(&retained_index);
    let source_index = scan_tool_messages(source_messages);
    let mut seen_paths = HashSet::new();
    let mut paths = Vec::new();
    for path in read_paths_in_order(&source_index).into_iter().rev() {
        if retained_paths.contains(&path) || !seen_paths.insert(path.clone()) {
            continue;
        }
        paths.push(path);
        if paths.len() >= settings.post_compact_max_files {
            break;
        }
    }
    paths.reverse();
    paths
}

pub fn agent_status_note(
    messages: &[LlmMessage],
    max_entries: usize,
    max_chars: usize,
) -> Option<PostCompactNote> {
    let index = scan_tool_messages(messages);
    let mut entries = Vec::new();
    for result in &index.results {
        if !is_agent_tool(result.tool_name.as_deref()) {
            continue;
        }
        let Some(call_id) = result.tool_call_id.as_deref() else {
            continue;
        };
        let description = index
            .calls
            .get(call_id)
            .and_then(|call| call.description.as_deref())
            .unwrap_or("agent task");
        let status = if result.is_error {
            "failed"
        } else {
            "completed"
        };
        entries.push(format!(
            "- {description}: {status}\n{}",
            truncate_chars(&result.content, 1200)
        ));
    }
    if entries.is_empty() {
        return None;
    }
    let start = entries.len().saturating_sub(max_entries);
    Some(PostCompactNote {
        title: "Agent Task Status".into(),
        body: truncate_chars(&entries[start..].join("\n\n"), max_chars),
    })
}

pub fn post_compact_context_message(
    files: Vec<PostCompactFile>,
    notes: Vec<PostCompactNote>,
    settings: &ContextSettings,
) -> Option<LlmMessage> {
    let files = budget_files(files, settings);
    if files.is_empty() && notes.is_empty() {
        return None;
    }
    Some(LlmMessage::user(render_post_compact_context(
        &files, &notes,
    )))
}

fn budget_files(files: Vec<PostCompactFile>, settings: &ContextSettings) -> Vec<PostCompactFile> {
    let mut used_tokens = 0usize;
    let mut kept = Vec::new();
    for file in files.into_iter().take(settings.post_compact_max_files) {
        let content = truncate_to_tokens(&file.content, settings.post_compact_max_tokens_per_file);
        let tokens = estimate_text_tokens(&file.path) + estimate_text_tokens(&content);
        if used_tokens.saturating_add(tokens) > settings.post_compact_token_budget {
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

fn read_paths(index: &ToolMessageIndex) -> HashSet<String> {
    index
        .calls
        .values()
        .filter(|call| is_read_tool(&call.name))
        .filter_map(|call| call.path.clone())
        .collect()
}

fn read_paths_in_order(index: &ToolMessageIndex) -> Vec<String> {
    let mut paths = Vec::new();
    for result in &index.results {
        if result.is_error || !result.tool_name.as_deref().is_some_and(is_read_tool) {
            continue;
        }
        let Some(call_id) = result.tool_call_id.as_deref() else {
            continue;
        };
        if let Some(path) = index.calls.get(call_id).and_then(|call| call.path.as_ref()) {
            paths.push(path.clone());
        }
    }
    paths
}

#[derive(Debug, Default)]
struct ToolMessageIndex {
    calls: HashMap<String, ToolCallInfo>,
    results: Vec<ToolResultEntry>,
}

#[derive(Debug)]
struct ToolCallInfo {
    name: String,
    path: Option<String>,
    description: Option<String>,
}

#[derive(Debug)]
struct ToolResultEntry {
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    content: String,
    is_error: bool,
}

fn scan_tool_messages(messages: &[LlmMessage]) -> ToolMessageIndex {
    let mut index = ToolMessageIndex::default();
    for message in messages {
        match message.role {
            LlmRole::Assistant => {
                for content in &message.content {
                    let LlmContent::ToolCall {
                        call_id,
                        name,
                        arguments,
                    } = content
                    else {
                        continue;
                    };
                    let description = arguments
                        .get("description")
                        .and_then(|value| value.as_str())
                        .or_else(|| {
                            arguments
                                .get("subagent_type")
                                .and_then(|value| value.as_str())
                        })
                        .map(str::to_string);
                    index.calls.insert(
                        call_id.clone(),
                        ToolCallInfo {
                            name: name.clone(),
                            path: arguments
                                .get("path")
                                .and_then(|value| value.as_str())
                                .map(str::to_string),
                            description,
                        },
                    );
                }
            },
            LlmRole::Tool => {
                for content in &message.content {
                    let LlmContent::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = content
                    else {
                        continue;
                    };
                    index.results.push(ToolResultEntry {
                        tool_name: message.name.clone(),
                        tool_call_id: Some(tool_call_id.clone()),
                        content: content.clone(),
                        is_error: *is_error,
                    });
                }
            },
            _ => {},
        }
    }
    index
}

fn is_agent_tool(name: Option<&str>) -> bool {
    name.is_some_and(|tool_name| tool_name.eq_ignore_ascii_case("agent"))
}

fn truncate_chars(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let mut truncated = content.chars().take(max_chars).collect::<String>();
    truncated.push_str("\n\n[... post-compact context truncated]");
    truncated
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
            reasoning_content: None,
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
        let settings = ContextSettings::default();
        let message = post_compact_context_message(files, Vec::new(), &settings).unwrap();
        message.joined_display_text("\n")
    }

    fn read_result_with_content(call_id: &str, content: &str) -> LlmMessage {
        LlmMessage::tool("read", call_id, content, false)
    }

    fn default_settings() -> ContextSettings {
        ContextSettings::default()
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

        let paths = recent_read_paths(&source, &retained, &default_settings());

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

        let paths = recent_read_paths(&source, &retained, &default_settings());

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

        let paths = recent_read_paths(&source, &[], &default_settings());

        assert_eq!(
            paths,
            ["src/2.rs", "src/3.rs", "src/4.rs", "src/5.rs", "src/6.rs"]
        );
    }

    #[test]
    fn renders_restored_files_and_runtime_notes() {
        let settings = default_settings();
        let message = post_compact_context_message(
            vec![file("src/lib.rs", "fresh content")],
            vec![PostCompactNote {
                title: "Plan File".into(),
                body: "plan body".into(),
            }],
            &settings,
        )
        .unwrap();

        let text = message.joined_display_text("\n");
        assert!(text.contains("<post_compact_context>"));
        assert!(text.contains("src/lib.rs"));
        assert!(text.contains("fresh content"));
        assert!(text.contains("Plan File"));
        assert!(text.contains("plan body"));
    }

    #[test]
    fn render_truncates_large_file_content() {
        let settings = default_settings();
        let text = render(vec![file(
            "huge.rs",
            &"x".repeat(settings.post_compact_max_tokens_per_file * 5),
        )]);

        assert!(text.contains("huge.rs"));
        assert!(text.contains("file content truncated after compaction"));
        assert!(estimate_text_tokens(&text) < settings.post_compact_max_tokens_per_file + 200);
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

        let paths = recent_read_paths(&source, &[], &default_settings());

        assert_eq!(paths, ["src/ok.rs", "src/manual.rs"]);
    }
}
