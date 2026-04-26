use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use astrcode_core::{
    AgentEventContext, LlmMessage, PersistedToolOutput, Result, StorageEvent, StorageEventPayload,
    is_persisted_output,
    project::project_dir_name,
    tool_result_persist::{PersistedToolResult, TOOL_RESULT_PREVIEW_LIMIT, TOOL_RESULTS_DIR},
};
use astrcode_support::hostpaths::resolve_home_dir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultReplacementRecord {
    pub tool_call_id: String,
    pub persisted_output: PersistedToolOutput,
    pub replacement: String,
    pub original_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ToolResultReplacementState {
    replacements: HashMap<String, ToolResultReplacementRecord>,
    frozen: HashSet<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolResultBudgetStats {
    pub replacement_count: usize,
    pub reapply_count: usize,
    pub bytes_saved: usize,
    pub over_budget_message_count: usize,
}

#[derive(Debug, Clone)]
pub struct ToolResultBudgetOutcome {
    pub messages: Vec<LlmMessage>,
    pub events: Vec<StorageEvent>,
    pub stats: ToolResultBudgetStats,
}

pub struct ApplyToolResultBudgetRequest<'a> {
    pub messages: &'a [LlmMessage],
    pub session_id: &'a str,
    pub working_dir: &'a Path,
    pub replacement_state: &'a mut ToolResultReplacementState,
    pub aggregate_budget_bytes: usize,
    pub turn_id: &'a str,
    pub agent: &'a AgentEventContext,
}

impl ToolResultReplacementState {
    pub fn seed(records: impl IntoIterator<Item = ToolResultReplacementRecord>) -> Self {
        let mut state = Self::default();
        for record in records {
            state
                .replacements
                .insert(record.tool_call_id.clone(), record);
        }
        state
    }

    fn replacement_for(&self, tool_call_id: &str) -> Option<&ToolResultReplacementRecord> {
        self.replacements.get(tool_call_id)
    }

    fn is_frozen(&self, tool_call_id: &str) -> bool {
        self.frozen.contains(tool_call_id)
    }

    fn freeze(&mut self, tool_call_id: String) {
        self.frozen.insert(tool_call_id);
    }

    fn record_replacement(&mut self, tool_call_id: String, record: ToolResultReplacementRecord) {
        self.replacements.insert(tool_call_id.clone(), record);
        self.frozen.remove(&tool_call_id);
    }
}

pub fn apply_tool_result_budget(
    request: ApplyToolResultBudgetRequest<'_>,
) -> Result<ToolResultBudgetOutcome> {
    let mut messages = request.messages.to_vec();
    let mut events = Vec::new();
    let mut stats = ToolResultBudgetStats::default();
    let Some(batch_start) = trailing_tool_batch_start(&messages) else {
        return Ok(ToolResultBudgetOutcome {
            messages,
            events,
            stats,
        });
    };

    let mut total_bytes = 0usize;
    for message in &messages[batch_start..] {
        if let LlmMessage::Tool { content, .. } = message {
            total_bytes = total_bytes.saturating_add(content.len());
        }
    }

    for message in &mut messages[batch_start..] {
        let LlmMessage::Tool {
            tool_call_id,
            content,
        } = message
        else {
            continue;
        };
        if let Some(record) = request.replacement_state.replacement_for(tool_call_id) {
            if content != &record.replacement {
                total_bytes = total_bytes
                    .saturating_sub(content.len())
                    .saturating_add(record.replacement.len());
                *content = record.replacement.clone();
                stats.reapply_count = stats.reapply_count.saturating_add(1);
            }
        }
    }

    if total_bytes <= request.aggregate_budget_bytes {
        return Ok(ToolResultBudgetOutcome {
            messages,
            events,
            stats,
        });
    }
    stats.over_budget_message_count = 1;

    let session_dir = resolve_session_dir(request.working_dir, request.session_id)?;
    let mut fresh_candidates = messages[batch_start..]
        .iter()
        .enumerate()
        .filter_map(|(offset, message)| match message {
            LlmMessage::Tool {
                tool_call_id,
                content,
            } if request
                .replacement_state
                .replacement_for(tool_call_id)
                .is_none()
                && !request.replacement_state.is_frozen(tool_call_id)
                && !is_persisted_output(content) =>
            {
                Some((batch_start + offset, tool_call_id.clone(), content.len()))
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    fresh_candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.2));

    let mut replaced = HashSet::new();
    for (index, tool_call_id, original_len) in fresh_candidates {
        if total_bytes <= request.aggregate_budget_bytes {
            break;
        }
        let LlmMessage::Tool { content, .. } = &messages[index] else {
            continue;
        };
        let replacement = persist_tool_result(&session_dir, &tool_call_id, content);
        let Some(persisted_output) = replacement.persisted.clone() else {
            continue;
        };
        let saved_bytes = original_len.saturating_sub(replacement.output.len());
        let record = ToolResultReplacementRecord {
            tool_call_id: tool_call_id.clone(),
            persisted_output: persisted_output.clone(),
            replacement: replacement.output.clone(),
            original_bytes: original_len as u64,
        };
        request
            .replacement_state
            .record_replacement(tool_call_id.clone(), record.clone());
        messages[index] = LlmMessage::Tool {
            tool_call_id: tool_call_id.clone(),
            content: replacement.output.clone(),
        };
        events.push(tool_result_reference_applied_event(
            request.turn_id,
            request.agent,
            &tool_call_id,
            &record.persisted_output,
            &record.replacement,
            record.original_bytes,
        ));
        total_bytes = total_bytes
            .saturating_sub(original_len)
            .saturating_add(replacement.output.len());
        stats.replacement_count = stats.replacement_count.saturating_add(1);
        stats.bytes_saved = stats.bytes_saved.saturating_add(saved_bytes);
        replaced.insert(tool_call_id);
    }

    for message in &messages[batch_start..] {
        if let LlmMessage::Tool {
            tool_call_id,
            content,
        } = message
        {
            if request
                .replacement_state
                .replacement_for(tool_call_id)
                .is_none()
                && !is_persisted_output(content)
                && !replaced.contains(tool_call_id)
            {
                request.replacement_state.freeze(tool_call_id.clone());
            }
        }
    }

    Ok(ToolResultBudgetOutcome {
        messages,
        events,
        stats,
    })
}

fn trailing_tool_batch_start(messages: &[LlmMessage]) -> Option<usize> {
    let trailing_tools = messages
        .iter()
        .rev()
        .take_while(|message| matches!(message, LlmMessage::Tool { .. }))
        .count();
    if trailing_tools == 0 {
        None
    } else {
        Some(messages.len().saturating_sub(trailing_tools))
    }
}

fn resolve_session_dir(working_dir: &Path, session_id: &str) -> Result<PathBuf> {
    Ok(project_dir(working_dir)?.join("sessions").join(session_id))
}

pub fn project_dir(working_dir: &Path) -> Result<PathBuf> {
    Ok(projects_dir()?.join(project_dir_name(working_dir)))
}

fn projects_dir() -> Result<PathBuf> {
    Ok(astrcode_dir()?.join("projects"))
}

fn astrcode_dir() -> Result<PathBuf> {
    Ok(resolve_home_dir()?.join(".astrcode"))
}

fn persist_tool_result(
    session_dir: &Path,
    tool_call_id: &str,
    content: &str,
) -> PersistedToolResult {
    let content_bytes = content.len();
    let results_dir = session_dir.join(TOOL_RESULTS_DIR);

    if std::fs::create_dir_all(&results_dir).is_err() {
        log::warn!(
            "tool-result: failed to create dir '{}', falling back to truncation",
            results_dir.display()
        );
        return PersistedToolResult {
            output: truncate_with_notice(content),
            persisted: None,
        };
    }

    let safe_id: String = tool_call_id
        .chars()
        .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
        .take(64)
        .collect();
    let path = results_dir.join(format!("{safe_id}.txt"));

    if std::fs::write(&path, content).is_err() {
        log::warn!(
            "tool-result: failed to write '{}', falling back to truncation",
            path.display()
        );
        return PersistedToolResult {
            output: truncate_with_notice(content),
            persisted: None,
        };
    }

    let relative_path = path
        .strip_prefix(session_dir)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/");
    let persisted = PersistedToolOutput {
        storage_kind: "toolResult".to_string(),
        absolute_path: normalize_absolute_path(&path),
        relative_path,
        total_bytes: content_bytes as u64,
        preview_text: build_preview_text(content),
        preview_bytes: TOOL_RESULT_PREVIEW_LIMIT.min(content.len()) as u64,
    };

    PersistedToolResult {
        output: format_persisted_output(&persisted),
        persisted: Some(persisted),
    }
}

fn format_persisted_output(persisted: &PersistedToolOutput) -> String {
    format!(
        "<persisted-output>\nLarge tool output was saved to a file instead of being \
         inlined.\nPath: {}\nBytes: {}\nRead the file with `readFile`.\nIf you only need a \
         section, read a smaller chunk instead of the whole file.\nStart from the first chunk \
         when you do not yet know the right section.\nSuggested first read: {{ path: {:?}, \
         charOffset: 0, maxChars: 20000 }}\n</persisted-output>",
        persisted.absolute_path, persisted.total_bytes, persisted.absolute_path
    )
}

fn build_preview_text(content: &str) -> String {
    let preview_limit = TOOL_RESULT_PREVIEW_LIMIT.min(content.len());
    let truncated_at = content.floor_char_boundary(preview_limit);
    content[..truncated_at].to_string()
}

fn normalize_absolute_path(path: &Path) -> String {
    normalize_verbatim_path(path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn normalize_verbatim_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rendered) = path.to_str() {
            if let Some(stripped) = rendered.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{}", stripped));
            }
            if let Some(stripped) = rendered.strip_prefix(r"\\?\") {
                return PathBuf::from(stripped);
            }
        }
    }

    path
}

fn truncate_with_notice(content: &str) -> String {
    let limit = TOOL_RESULT_PREVIEW_LIMIT.min(content.len());
    let truncated_at = content.floor_char_boundary(limit);
    let prefix = &content[..truncated_at];
    format!(
        "{prefix}\n\n... [output truncated to {limit} bytes because persisted storage is \
         unavailable; use offset/limit parameters or rerun with a narrower scope for full content]"
    )
}

fn tool_result_reference_applied_event(
    turn_id: &str,
    agent: &AgentEventContext,
    tool_call_id: &str,
    persisted_output: &PersistedToolOutput,
    replacement: &str,
    original_bytes: u64,
) -> StorageEvent {
    StorageEvent {
        turn_id: Some(turn_id.to_string()),
        agent: agent.clone(),
        payload: StorageEventPayload::ToolResultReferenceApplied {
            tool_call_id: tool_call_id.to_string(),
            persisted_output: persisted_output.clone(),
            replacement: replacement.to_string(),
            original_bytes,
        },
    }
}
