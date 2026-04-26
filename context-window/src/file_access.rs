use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use astrcode_core::{LlmMessage, ToolCallRequest, UserMessageOrigin};
use astrcode_runtime_contract::tool::ToolExecutionResult;
use serde::Deserialize;

use super::token_usage::estimate_text_tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRecoveryConfig {
    pub max_tracked_files: usize,
    pub max_recovered_files: usize,
    pub recovery_token_budget: usize,
}

#[derive(Debug, Clone)]
struct TrackedFileAccess {
    path: PathBuf,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct FileAccessTracker {
    accesses: VecDeque<TrackedFileAccess>,
    max_tracked_files: usize,
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

impl FileAccessTracker {
    pub fn new(max_tracked_files: usize) -> Self {
        Self {
            accesses: VecDeque::new(),
            max_tracked_files: max_tracked_files.max(1),
        }
    }

    pub fn seed_from_messages(
        messages: &[LlmMessage],
        max_tracked_files: usize,
        working_dir: &Path,
    ) -> Self {
        let mut tracker = Self::new(max_tracked_files);
        let mut pending_reads = HashMap::<String, ReadFileArgs>::new();

        for message in messages {
            match message {
                LlmMessage::Assistant { tool_calls, .. } => {
                    for call in tool_calls {
                        if call.name != "readFile" {
                            continue;
                        }
                        if let Ok(args) = serde_json::from_value::<ReadFileArgs>(call.args.clone())
                        {
                            pending_reads.insert(call.id.clone(), args);
                        }
                    }
                },
                LlmMessage::Tool { tool_call_id, .. } => {
                    let Some(args) = pending_reads.remove(tool_call_id) else {
                        continue;
                    };
                    tracker.record_access(access_from_args(&args, None, working_dir));
                },
                _ => {},
            }
        }

        tracker
    }

    pub fn record_tool_result(
        &mut self,
        tool_call: &ToolCallRequest,
        result: &ToolExecutionResult,
        working_dir: &Path,
    ) {
        if tool_call.name != "readFile" || !result.ok {
            return;
        }
        let Ok(args) = serde_json::from_value::<ReadFileArgs>(tool_call.args.clone()) else {
            return;
        };
        self.record_access(access_from_args(
            &args,
            result.metadata.as_ref(),
            working_dir,
        ));
    }

    pub fn build_recovery_messages(&self, config: FileRecoveryConfig) -> Vec<LlmMessage> {
        let mut recovered = Vec::new();
        let mut remaining_tokens = config.recovery_token_budget.max(1);

        for access in self.accesses.iter().rev() {
            if recovered.len() >= config.max_recovered_files.max(1) {
                break;
            }

            let Some(content) = render_recovery_message(access, remaining_tokens) else {
                continue;
            };
            let used_tokens = estimate_text_tokens(&content);
            if used_tokens > remaining_tokens {
                continue;
            }

            remaining_tokens = remaining_tokens.saturating_sub(used_tokens);
            recovered.push(LlmMessage::User {
                content,
                origin: UserMessageOrigin::ReactivationPrompt,
            });
        }

        recovered.reverse();
        recovered
    }

    fn record_access(&mut self, access: TrackedFileAccess) {
        self.accesses.retain(|entry| !same_access(entry, &access));
        self.accesses.push_back(access);

        while self.accesses.len() > self.max_tracked_files {
            self.accesses.pop_front();
        }
    }
}

fn access_from_args(
    args: &ReadFileArgs,
    metadata: Option<&serde_json::Value>,
    working_dir: &Path,
) -> TrackedFileAccess {
    let path = metadata
        .and_then(|value| value.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve_path(working_dir, &args.path));

    TrackedFileAccess {
        path,
        offset: args.offset,
        limit: args.limit,
    }
}

fn resolve_path(working_dir: &Path, raw_path: &str) -> PathBuf {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        path
    } else {
        working_dir.join(path)
    }
}

fn same_access(left: &TrackedFileAccess, right: &TrackedFileAccess) -> bool {
    left.path == right.path && left.offset == right.offset && left.limit == right.limit
}

fn render_recovery_message(access: &TrackedFileAccess, budget_tokens: usize) -> Option<String> {
    let raw_text = match fs::read_to_string(&access.path) {
        Ok(text) => slice_text(text, access.offset, access.limit),
        Err(error) => {
            return Some(format!(
                "Recovered file context after compaction is unavailable.\nPath: {}\nReason: {}",
                access.path.display(),
                error
            ));
        },
    };

    let header = format!(
        "Recovered file context after compaction.\nPath: {}\n{}Content:\n",
        access.path.display(),
        format_range(access.offset, access.limit)
    );
    let available_body_tokens = budget_tokens
        .saturating_sub(estimate_text_tokens(&header))
        .max(32);
    let body = truncate_to_token_budget(&raw_text, available_body_tokens);
    if body.trim().is_empty() {
        return None;
    }

    Some(format!("{header}```text\n{body}\n```"))
}

fn slice_text(text: String, offset: Option<usize>, limit: Option<usize>) -> String {
    if offset.is_none() && limit.is_none() {
        return text;
    }

    let lines = text.lines().collect::<Vec<_>>();
    let start = offset.unwrap_or(0).min(lines.len());
    let end = limit
        .map(|value| start.saturating_add(value).min(lines.len()))
        .unwrap_or(lines.len());
    lines[start..end].join("\n")
}

fn format_range(offset: Option<usize>, limit: Option<usize>) -> String {
    match (offset, limit) {
        (Some(offset), Some(limit)) => format!("Line range: {}-{}\n", offset + 1, offset + limit),
        (Some(offset), None) => format!("Line start: {}\n", offset + 1),
        _ => String::new(),
    }
}

fn truncate_to_token_budget(text: &str, budget_tokens: usize) -> String {
    let target_chars = budget_tokens.saturating_mul(4).max(64);
    if text.chars().count() <= target_chars {
        return text.to_string();
    }

    let mut end = 0usize;
    for (index, _) in text.char_indices().take(target_chars) {
        end = index;
    }
    if end == 0 {
        return text.chars().take(target_chars).collect::<String>();
    }
    format!(
        "{}\n[truncated after compaction recovery budget]",
        &text[..end]
    )
}
