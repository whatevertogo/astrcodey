//! Runtime-owned post-compact context restoration.
//!
//! `astrcode-context` decides how compact summaries are shaped. Runtime facts
//! that require filesystem or tool-registry access stay here.

use std::{
    cmp::Reverse,
    fs,
    path::{Path, PathBuf},
};

use astrcode_context::{
    compaction::{CompactResult, PostCompactFile, PostCompactNote, recent_read_paths},
    token_usage::truncate_text_to_tokens,
};
use astrcode_core::{
    llm::{LlmContent, LlmMessage, LlmRole},
    tool::{ToolDefinition, ToolOrigin},
    types::project_hash_from_path,
};
use astrcode_support::hostpaths::{is_path_within, resolve_path, session_plan_dir};

const PLAN_NOTE_MAX_CHARS: usize = 40_000;
const SKILLS_TOKEN_BUDGET: usize = 25_000;
const AGENT_NOTE_MAX_CHARS: usize = 20_000;
const TOOL_NOTE_MAX_CHARS: usize = 16_000;
const TOKEN_TRUNCATION_MARKER: &str = "\n\n[... post-compact context truncated]";

pub(crate) async fn enrich_post_compact_context(
    compaction: &mut CompactResult,
    session_id: &str,
    source_messages: &[LlmMessage],
    working_dir: &str,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
) {
    let source_messages = source_messages.to_vec();
    let retained_messages = compaction.retained_messages.clone();
    let working_dir = working_dir.to_string();
    let system_prompt = system_prompt.map(str::to_string);
    let tools = tools.to_vec();
    let session_id = session_id.to_string();

    let Ok((files, notes)) = tokio::task::spawn_blocking(move || {
        collect_post_compact_context(
            &source_messages,
            &retained_messages,
            &working_dir,
            &session_id,
            system_prompt.as_deref(),
            &tools,
        )
    })
    .await
    else {
        return;
    };

    compaction.append_post_compact_context(files, notes);
}

fn collect_post_compact_context(
    source_messages: &[LlmMessage],
    retained_messages: &[LlmMessage],
    working_dir: &str,
    session_id: &str,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
) -> (Vec<PostCompactFile>, Vec<PostCompactNote>) {
    let working_dir = PathBuf::from(working_dir);
    let files = fresh_recent_read_files(source_messages, retained_messages, &working_dir);
    let mut notes = Vec::new();

    if let Some(note) = latest_plan_note(&working_dir, session_id) {
        notes.push(note);
    }
    if let Some(note) = skills_note(system_prompt) {
        notes.push(note);
    }
    if let Some(note) = agent_status_note(source_messages) {
        notes.push(note);
    }
    if let Some(note) = tool_delta_note(tools) {
        notes.push(note);
    }

    (files, notes)
}

fn fresh_recent_read_files(
    source_messages: &[LlmMessage],
    retained_messages: &[LlmMessage],
    working_dir: &Path,
) -> Vec<PostCompactFile> {
    recent_read_paths(source_messages, retained_messages)
        .into_iter()
        .filter_map(|path| fresh_read_file(working_dir, &path))
        .collect()
}

fn fresh_read_file(working_dir: &Path, requested_path: &str) -> Option<PostCompactFile> {
    let resolved = resolve_path(working_dir, &PathBuf::from(requested_path));
    if !is_path_within(&resolved, working_dir) || !resolved.is_file() {
        return None;
    }
    let content = fs::read_to_string(&resolved).ok()?;
    Some(PostCompactFile {
        path: requested_path.to_string(),
        content,
    })
}

fn latest_plan_note(working_dir: &Path, session_id: &str) -> Option<PostCompactNote> {
    let project_hash = project_hash_from_path(&working_dir.to_path_buf());
    let plans_dir = session_plan_dir(&project_hash, session_id);
    let mut plans = fs::read_dir(&plans_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|entry| {
            let path = entry.path();
            if !is_path_within(&path, &plans_dir) || !path.is_file() {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect::<Vec<_>>();
    plans.sort_by_key(|entry| Reverse(entry.0));

    let path = plans.into_iter().next()?.1;
    let content = fs::read_to_string(&path).ok()?;
    Some(PostCompactNote {
        title: "Plan File".into(),
        body: format!(
            "Path: {}\n\n{}",
            path.display(),
            truncate_chars(&content, PLAN_NOTE_MAX_CHARS)
        ),
    })
}

fn skills_note(system_prompt: Option<&str>) -> Option<PostCompactNote> {
    let skills = extract_markdown_section(system_prompt?, "Skills")?;
    Some(PostCompactNote {
        title: "Loaded Skill Content".into(),
        body: truncate_to_tokens(skills, SKILLS_TOKEN_BUDGET),
    })
}

fn agent_status_note(messages: &[LlmMessage]) -> Option<PostCompactNote> {
    let agent_calls = agent_call_descriptions(messages);
    let mut entries = Vec::new();
    for message in messages {
        if message.role != LlmRole::Tool || message.name.as_deref() != Some("agent") {
            continue;
        }
        for content in &message.content {
            let LlmContent::ToolResult {
                tool_call_id,
                content,
                is_error,
            } = content
            else {
                continue;
            };
            let description = agent_calls
                .get(tool_call_id)
                .cloned()
                .unwrap_or_else(|| "agent task".to_string());
            let status = if *is_error { "failed" } else { "completed" };
            entries.push(format!(
                "- {description}: {status}\n{}",
                truncate_chars(content, 1200)
            ));
        }
    }

    if entries.is_empty() {
        return None;
    }
    let start = entries.len().saturating_sub(5);
    Some(PostCompactNote {
        title: "Agent Task Status".into(),
        body: truncate_chars(&entries[start..].join("\n\n"), AGENT_NOTE_MAX_CHARS),
    })
}

fn agent_call_descriptions(messages: &[LlmMessage]) -> std::collections::HashMap<String, String> {
    let mut calls = std::collections::HashMap::new();
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
            if name != "agent" {
                continue;
            }
            let description = arguments
                .get("description")
                .and_then(|value| value.as_str())
                .or_else(|| {
                    arguments
                        .get("subagent_type")
                        .and_then(|value| value.as_str())
                })
                .unwrap_or("agent task");
            calls.insert(call_id.clone(), description.to_string());
        }
    }
    calls
}

fn tool_delta_note(tools: &[ToolDefinition]) -> Option<PostCompactNote> {
    if tools.is_empty() {
        return None;
    }

    let mut lines = tools
        .iter()
        .map(|tool| format!("- {} ({})", tool.name, tool_origin_name(tool.origin)))
        .collect::<Vec<_>>();
    lines.sort();

    Some(PostCompactNote {
        title: "Available Tool Delta".into(),
        body: truncate_chars(&lines.join("\n"), TOOL_NOTE_MAX_CHARS),
    })
}

fn tool_origin_name(origin: ToolOrigin) -> &'static str {
    match origin {
        ToolOrigin::Builtin => "builtin",
        ToolOrigin::Bundled => "bundled",
        ToolOrigin::Extension => "extension",
        ToolOrigin::Sdk => "sdk",
        ToolOrigin::Mcp => "mcp",
    }
}

fn extract_markdown_section<'a>(content: &'a str, heading: &str) -> Option<&'a str> {
    let heading_line = format!("# {heading}");
    let lines = markdown_lines(content);
    let start = lines
        .iter()
        .position(|line| line.text.trim() == heading_line)?;
    let byte_start = lines[start].end;
    let byte_end = lines
        .iter()
        .skip(start + 1)
        .find_map(|line| {
            (line.text.starts_with("# ") && line.text.trim() != heading_line).then_some(line.start)
        })
        .unwrap_or(content.len());
    let section = content[byte_start..byte_end].trim();
    (!section.is_empty()).then_some(section)
}

struct MarkdownLine<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn markdown_lines(content: &str) -> Vec<MarkdownLine<'_>> {
    let mut offset = 0usize;
    let mut lines = Vec::new();
    for raw in content.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        lines.push(MarkdownLine {
            start,
            end: offset,
            text: raw.trim_end_matches(['\r', '\n']),
        });
    }
    if content.is_empty() || content.ends_with('\n') {
        return lines;
    }
    lines
}

fn truncate_chars(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let mut truncated = content.chars().take(max_chars).collect::<String>();
    truncated.push_str(TOKEN_TRUNCATION_MARKER);
    truncated
}

fn truncate_to_tokens(content: &str, max_tokens: usize) -> String {
    truncate_text_to_tokens(content, max_tokens, TOKEN_TRUNCATION_MARKER)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Mutex, OnceLock},
        time::Duration,
    };

    use astrcode_context::token_usage::estimate_text_tokens;
    use astrcode_core::tool::ToolOrigin;
    use astrcode_support::hostpaths;
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
        LlmMessage::tool("read", call_id, "old content", false)
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

    #[tokio::test]
    async fn post_compact_rereads_recent_files_from_disk() {
        let temp = tempfile_dir("post-compact-reread");
        fs::write(temp.join("source.rs"), "fresh disk content").unwrap();
        let messages = vec![read_call("read-1", "source.rs"), read_result("read-1")];
        let mut compaction = CompactResult {
            pre_tokens: 100,
            post_tokens: 10,
            summary: "summary".into(),
            messages_removed: 2,
            context_messages: vec![LlmMessage::user("summary")],
            retained_messages: Vec::new(),
            transcript_path: None,
        };

        enrich_post_compact_context(
            &mut compaction,
            "session-post-compact-reread",
            &messages,
            temp.to_str().unwrap(),
            None,
            &[],
        )
        .await;

        let restored = message_text(compaction.context_messages.last().unwrap());
        assert!(restored.contains("source.rs"));
        assert!(restored.contains("fresh disk content"));
        assert!(!restored.contains("old content"));
    }

    #[test]
    fn post_compact_adds_plan_agent_and_tool_notes() {
        let _guard = test_env_lock().lock().unwrap();
        let temp = tempfile_dir("post-compact-notes");
        let home = temp.join("home");
        std::env::set_var("ASTRCODE_TEST_HOME", &home);
        let session_id = "session-post-compact-notes";
        let plans = hostpaths::session_plan_dir(&project_hash_from_path(&temp), session_id);
        fs::create_dir_all(&plans).unwrap();
        fs::write(plans.join("work.md"), "plan body").unwrap();
        let messages = vec![
            LlmMessage {
                role: LlmRole::Assistant,
                content: vec![LlmContent::ToolCall {
                    call_id: "agent-1".into(),
                    name: "agent".into(),
                    arguments: json!({ "description": "inspect compact" }),
                }],
                name: None,
            },
            LlmMessage::tool("agent", "agent-1", "agent output", false),
        ];
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "read files".into(),
            parameters: json!({}),
            origin: ToolOrigin::Builtin,
        }];
        let mut compaction = CompactResult {
            pre_tokens: 100,
            post_tokens: 10,
            summary: "summary".into(),
            messages_removed: 2,
            context_messages: vec![LlmMessage::user("summary")],
            retained_messages: Vec::new(),
            transcript_path: None,
        };

        let (files, notes) = collect_post_compact_context(
            &messages,
            &compaction.retained_messages,
            temp.to_str().unwrap(),
            session_id,
            Some("# Skills\n\nskill body\n\n# Agents\n\nagent list"),
            &tools,
        );
        compaction.append_post_compact_context(files, notes);
        std::env::remove_var("ASTRCODE_TEST_HOME");

        let restored = message_text(compaction.context_messages.last().unwrap());
        assert!(restored.contains("Plan File"));
        assert!(restored.contains("plan body"));
        assert!(restored.contains("Loaded Skill Content"));
        assert!(restored.contains("skill body"));
        assert!(restored.contains("Agent Task Status"));
        assert!(restored.contains("inspect compact"));
        assert!(restored.contains("Available Tool Delta"));
        assert!(restored.contains("read (builtin)"));
    }

    #[test]
    fn skill_section_extraction_handles_crlf_and_multibyte_text() {
        let system_prompt = "# Identity\r\n中文\r\n# Skills\r\n技能内容\r\n# Agents\r\nagent list";

        let note = skills_note(Some(system_prompt)).unwrap();

        assert_eq!(note.body, "技能内容");
    }

    #[test]
    fn latest_plan_note_ignores_project_local_omx_plans() {
        let _guard = test_env_lock().lock().unwrap();
        let temp = tempfile_dir("post-compact-no-omx-plan");
        let home = temp.join("home");
        std::env::set_var("ASTRCODE_TEST_HOME", &home);
        let project_plans = temp.join(".omx").join("plans");
        fs::create_dir_all(&project_plans).unwrap();
        fs::write(project_plans.join("work.md"), "project local plan").unwrap();

        let note = latest_plan_note(&temp, "session-without-plan");

        std::env::remove_var("ASTRCODE_TEST_HOME");
        assert!(note.is_none());
    }

    #[test]
    fn skill_note_uses_claude_sized_token_budget() {
        let large_skill = "x".repeat(SKILLS_TOKEN_BUDGET * 5);
        let system_prompt = format!("# Skills\n\n{large_skill}\n\n# Agents\n\nagent list");

        let note = skills_note(Some(&system_prompt)).unwrap();

        assert!(note.body.contains("post-compact context truncated"));
        assert!(estimate_text_tokens(&note.body) < SKILLS_TOKEN_BUDGET + 200);
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("astrcode-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        std::thread::sleep(Duration::from_millis(1));
        path
    }

    fn test_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
