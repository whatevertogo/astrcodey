//! Coding-owned context retained across transcript compaction.

use std::collections::{HashMap, HashSet};

use astrcode_extension_sdk::{
    extension::{
        CompactContributions, CompactRetainedContext, ExtensionCall, ExtensionError,
        PreCompactContext, PreCompactHandler, PreCompactResult, Registrar,
    },
    host::{HostWorkspaceReadOutput, HostWorkspaceReadRequest},
    llm::{LlmContent, LlmMessage, LlmRole},
};

const READ_TOOL_NAME: &str = "read";
const COMPACT_PRIORITY: i32 = 50;

pub(super) fn register(registrar: &mut Registrar) {
    registrar.on_pre_compact(
        COMPACT_PRIORITY,
        std::sync::Arc::new(CodingPreCompactHandler),
    );
}

struct CodingPreCompactHandler;

#[async_trait::async_trait]
impl PreCompactHandler for CodingPreCompactHandler {
    async fn handle(&self, ctx: PreCompactContext) -> Result<PreCompactResult, ExtensionError> {
        let paths = recent_successful_read_paths(ctx.source_messages(), ctx.retained_file_limit());
        if paths.is_empty() {
            return Ok(PreCompactResult::Allow);
        }

        let workspace = ctx.host().workspace()?;
        let mut retained_context = Vec::with_capacity(paths.len());
        for path in paths {
            let output = workspace.read(HostWorkspaceReadRequest::new(&path)).await;
            let Ok(HostWorkspaceReadOutput::Text { content, .. }) = output else {
                continue;
            };
            retained_context.push(CompactRetainedContext::File { path, content });
        }

        if retained_context.is_empty() {
            Ok(PreCompactResult::Allow)
        } else {
            Ok(PreCompactResult::Contributions(CompactContributions {
                instructions: Vec::new(),
                retained_context,
            }))
        }
    }
}

fn recent_successful_read_paths(messages: &[LlmMessage], limit: usize) -> Vec<String> {
    let mut calls = HashMap::new();
    let mut successful_paths = Vec::new();

    for message in messages {
        match message.role {
            LlmRole::Assistant => {
                for content in &message.content {
                    let LlmContent::ToolCall {
                        call_id,
                        name,
                        arguments,
                        ..
                    } = content
                    else {
                        continue;
                    };
                    if name == READ_TOOL_NAME
                        && let Some(path) =
                            arguments.get("path").and_then(serde_json::Value::as_str)
                    {
                        calls.insert(call_id.as_str(), path);
                    }
                }
            },
            LlmRole::Tool => {
                for content in &message.content {
                    let LlmContent::ToolResult {
                        tool_call_id,
                        is_error: false,
                        ..
                    } = content
                    else {
                        continue;
                    };
                    if message.name.as_deref() == Some(READ_TOOL_NAME)
                        && let Some(path) = calls.get(tool_call_id.as_str())
                    {
                        successful_paths.push((*path).to_string());
                    }
                }
            },
            _ => {},
        }
    }

    let mut seen = HashSet::new();
    let mut selected = successful_paths
        .into_iter()
        .rev()
        .filter(|path| seen.insert(path.clone()))
        .take(limit)
        .collect::<Vec<_>>();
    selected.reverse();
    selected
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
                name: READ_TOOL_NAME.into(),
                arguments: json!({ "path": path }),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        }
    }

    #[test]
    fn recent_paths_require_success_and_keep_latest_unique_order() {
        let messages = vec![
            read_call("one", "src/one.rs"),
            LlmMessage::tool(READ_TOOL_NAME, "one", "old", false),
            read_call("bad", "src/bad.rs"),
            LlmMessage::tool(READ_TOOL_NAME, "bad", "error", true),
            read_call("again", "src/one.rs"),
            LlmMessage::tool(READ_TOOL_NAME, "again", "new", false),
            read_call("two", "src/two.rs"),
            LlmMessage::tool(READ_TOOL_NAME, "two", "ok", false),
        ];

        assert_eq!(
            recent_successful_read_paths(&messages, 2),
            ["src/one.rs", "src/two.rs"]
        );
    }
}
