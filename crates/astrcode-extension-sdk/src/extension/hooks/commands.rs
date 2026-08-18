//! Slash command types: definitions, completions, and execution results.

use serde::{Deserialize, Serialize};

use super::types::StatusItemUpdatePayload;
pub use crate::wire::command::{
    CommandAvailability, CommandExecution, SessionCommandKind, SlashCommand,
};

/// Completion item for slash command arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCompletionItem {
    pub label: String,
    pub insert_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Completion result for slash command arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCompletions {
    #[serde(default)]
    pub items: Vec<CommandCompletionItem>,
    #[serde(default)]
    pub truncated: bool,
}

/// Execution result of an extension slash command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionCommandResult {
    /// Display text only, without starting an agent turn.
    Display {
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_update: Option<StatusItemUpdatePayload>,
    },
    /// Synchronously handled to completion, without starting an agent turn.
    Handled { message: String },
    /// Start an agent turn, merging the additional instructions into the user message.
    StartTurn { instructions: String },
}

impl ExtensionCommandResult {
    pub fn display(content: impl Into<String>, is_error: bool) -> Self {
        Self::Display {
            content: content.into(),
            is_error,
            status_update: None,
        }
    }

    pub fn display_with_status(
        content: impl Into<String>,
        is_error: bool,
        status_update: StatusItemUpdatePayload,
    ) -> Self {
        Self::Display {
            content: content.into(),
            is_error,
            status_update: Some(status_update),
        }
    }

    pub fn handled(message: impl Into<String>) -> Self {
        Self::Handled {
            message: message.into(),
        }
    }

    pub fn start_turn(instructions: impl Into<String>) -> Self {
        Self::StartTurn {
            instructions: instructions.into(),
        }
    }
}
