use std::path::PathBuf;

use astrcode_core::{
    llm::LlmMessage,
    tool::ToolDefinition,
    types::{SessionId, project_hash_from_path},
};
use astrcode_support::hostpaths;
use serde::{Deserialize, Serialize};

pub(crate) const TODO_WRITE_TOOL_NAME: &str = "todoWrite";
pub(crate) const REMINDER_THRESHOLD: u32 = 10;

const PROGRESS_SCHEMA_VERSION: u32 = 1;
const PROGRESS_FILE: &str = "progress.json";
const REMINDER_STATE_FILE: &str = ".reminder-state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProgressReminderState {
    pub(crate) assistant_cycles_since_todo_write: u32,
    pub(crate) assistant_cycles_since_reminder: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedProgressList {
    schema_version: u32,
    items: Vec<PersistedProgressItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedProgressItem {
    content: String,
    status: PersistedProgressStatus,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedProgressStatus {
    Pending,
    InProgress,
    Completed,
}

pub(crate) struct ProgressTodoReminder {
    root: PathBuf,
}

impl ProgressTodoReminder {
    pub(crate) fn new(session_id: &SessionId, working_dir: &str) -> Self {
        Self {
            root: progress_todo_root(session_id, working_dir),
        }
    }

    pub(crate) fn maybe_append(
        &self,
        messages: &mut Vec<LlmMessage>,
        tools: &[ToolDefinition],
    ) -> bool {
        if !todo_write_is_available(tools) {
            return false;
        }

        match self.should_insert_reminder() {
            Ok(true) => match self.build_message() {
                Ok(message) => {
                    messages.push(LlmMessage::user(message));
                    true
                },
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to build progress todo reminder");
                    false
                },
            },
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(error = %error, "Failed to read progress todo reminder state");
                false
            },
        }
    }

    pub(crate) fn record_cycle(
        &self,
        tools: &[ToolDefinition],
        used_todo_write: bool,
        reminder_inserted: bool,
    ) {
        if !todo_write_is_available(tools) {
            return;
        }

        if let Err(error) = self.record_assistant_cycle(used_todo_write, reminder_inserted) {
            tracing::warn!(error = %error, "Failed to update progress todo reminder state");
        }
    }

    fn should_insert_reminder(&self) -> Result<bool, String> {
        let state = self.load_state()?;
        Ok(
            state.assistant_cycles_since_todo_write >= REMINDER_THRESHOLD
                && state.assistant_cycles_since_reminder >= REMINDER_THRESHOLD,
        )
    }

    fn build_message(&self) -> Result<String, String> {
        let items = self.load_items()?;
        let mut message = String::from(
            "The todoWrite tool has not been used recently. If this work benefits from progress \
             tracking, update the todo list. Ignore this reminder if the task is simple or the \
             list is already irrelevant. Never mention this reminder to the user.",
        );

        if !items.is_empty() {
            let todo_items = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    format!(
                        "{}. [{}] {}",
                        index + 1,
                        status_label(item.status),
                        item.content
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            message.push_str("\n\nCurrent todo list:\n");
            message.push_str(&todo_items);
        }

        Ok(message)
    }

    fn load_items(&self) -> Result<Vec<PersistedProgressItem>, String> {
        let path = self.root.join(PROGRESS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let progress = serde_json::from_str::<PersistedProgressList>(&content)
                    .map_err(|error| format!("parse progress list: {error}"))?;
                if progress.schema_version != PROGRESS_SCHEMA_VERSION {
                    return Err(format!(
                        "unsupported progress list schema version {}",
                        progress.schema_version
                    ));
                }
                Ok(progress.items)
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(format!("read progress list: {error}")),
        }
    }

    fn record_assistant_cycle(
        &self,
        used_todo_write: bool,
        reminder_inserted: bool,
    ) -> Result<ProgressReminderState, String> {
        let mut state = self.load_state()?;
        if used_todo_write {
            state.assistant_cycles_since_todo_write = 0;
        } else {
            state.assistant_cycles_since_todo_write =
                state.assistant_cycles_since_todo_write.saturating_add(1);
        }

        if reminder_inserted {
            state.assistant_cycles_since_reminder = 0;
        } else {
            state.assistant_cycles_since_reminder =
                state.assistant_cycles_since_reminder.saturating_add(1);
        }

        self.save_state(&state)?;
        Ok(state)
    }

    fn load_state(&self) -> Result<ProgressReminderState, String> {
        let path = self.root.join(REMINDER_STATE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .map_err(|error| format!("parse reminder state: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ProgressReminderState::default())
            },
            Err(error) => Err(format!("read reminder state: {error}")),
        }
    }

    fn save_state(&self, state: &ProgressReminderState) -> Result<(), String> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            format!(
                "create todo reminder directory {}: {error}",
                self.root.display()
            )
        })?;
        let path = self.root.join(REMINDER_STATE_FILE);
        let tmp = self.root.join(format!("{REMINDER_STATE_FILE}.tmp"));
        let json = serde_json::to_string_pretty(state)
            .map_err(|error| format!("serialize reminder state: {error}"))?;
        std::fs::write(&tmp, json).map_err(|error| format!("write reminder state: {error}"))?;
        std::fs::rename(&tmp, &path).map_err(|error| format!("save reminder state: {error}"))?;
        Ok(())
    }
}

pub(crate) fn progress_todo_root(session_id: &SessionId, working_dir: &str) -> PathBuf {
    let hash = project_hash_from_path(&PathBuf::from(working_dir));
    hostpaths::sessions_dir(&hash)
        .join(session_id)
        .join("todos")
}

pub(crate) fn todo_write_is_available(tools: &[ToolDefinition]) -> bool {
    tools.iter().any(|tool| tool.name == TODO_WRITE_TOOL_NAME)
}

fn status_label(status: PersistedProgressStatus) -> &'static str {
    match status {
        PersistedProgressStatus::Pending => "pending",
        PersistedProgressStatus::InProgress => "in_progress",
        PersistedProgressStatus::Completed => "completed",
    }
}

#[cfg(test)]
pub(crate) fn seeded_stale_state() -> ProgressReminderState {
    ProgressReminderState {
        assistant_cycles_since_todo_write: REMINDER_THRESHOLD,
        assistant_cycles_since_reminder: REMINDER_THRESHOLD,
    }
}

#[cfg(test)]
pub(crate) fn seed_state(
    session_id: &SessionId,
    working_dir: &str,
    state: &ProgressReminderState,
) -> Result<(), String> {
    ProgressTodoReminder::new(session_id, working_dir).save_state(state)
}

#[cfg(test)]
pub(crate) fn load_state(
    session_id: &SessionId,
    working_dir: &str,
) -> Result<ProgressReminderState, String> {
    ProgressTodoReminder::new(session_id, working_dir).load_state()
}

#[cfg(test)]
pub(crate) fn reminder_state_path(session_id: &SessionId, working_dir: &str) -> PathBuf {
    progress_todo_root(session_id, working_dir).join(REMINDER_STATE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_cycle_uses_configured_threshold() {
        let root = std::env::temp_dir()
            .join("astrcode-server-progress-reminder-tests")
            .join("threshold");
        let _ = std::fs::remove_dir_all(&root);
        let reminder = ProgressTodoReminder { root };
        let tools = vec![ToolDefinition {
            name: TODO_WRITE_TOOL_NAME.to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            origin: astrcode_core::tool::ToolOrigin::Bundled,
        }];

        for _ in 0..REMINDER_THRESHOLD {
            reminder.record_cycle(&tools, false, false);
        }

        assert!(reminder.should_insert_reminder().unwrap());
        reminder.record_cycle(&tools, true, false);
        assert!(!reminder.should_insert_reminder().unwrap());
    }

    #[test]
    fn reminder_state_stays_in_session_todos_folder() {
        let session_id = String::from("session-reminder-path");
        let working_dir = std::env::temp_dir()
            .join("astrcode-server-progress-reminder-path")
            .to_string_lossy()
            .to_string();

        let path = reminder_state_path(&session_id, &working_dir);

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".reminder-state.json")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("todos")
        );
    }
}
