use astrcode_core::types::{SessionId, TurnId};
use astrcode_protocol::events::ExtensionCommandInfoDto;

use crate::{session_manager::SessionManagerError, turn_scheduler::TurnScheduleError};

pub struct CommandList {
    pub commands: Vec<ExtensionCommandInfoDto>,
}

/// 用户输入提交结果：被接受进入 Turn，或被斜杠命令处理。
#[derive(Debug)]
pub enum PromptSubmission {
    Accepted { turn_id: TurnId },
    Handled { message: String },
}

#[derive(Debug)]
pub enum CommandInvocation {
    Display { content: String, is_error: bool },
    Handled { message: String },
    Started { turn_id: TurnId },
}

impl CommandInvocation {
    pub(crate) fn into_prompt_submission(self) -> PromptSubmission {
        match self {
            Self::Display { content, is_error } => PromptSubmission::Handled {
                message: if is_error {
                    format!("Error: {content}")
                } else {
                    content
                },
            },
            Self::Handled { message } => PromptSubmission::Handled { message },
            Self::Started { turn_id } => PromptSubmission::Accepted { turn_id },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualCompactOutcome {
    Compacted { session_id: SessionId },
    Skipped { message: String },
}

/// Session command application error.
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    #[error("A turn is already running")]
    TurnAlreadyRunning,
    #[error("No active turn")]
    NoActiveTurn,
    #[error("No active session")]
    NoActiveSession,
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Unknown command: /{0}")]
    UnknownCommand(String),
    #[error("Cannot compact while a turn is running")]
    CompactBlocked,
    #[error("Compaction skipped: {0}")]
    CompactionSkipped(String),
    #[error(transparent)]
    SessionManager(#[from] SessionManagerError),
    #[error(transparent)]
    Session(astrcode_session::SessionError),
    #[error(transparent)]
    Turn(astrcode_session::TurnError),
    #[error(transparent)]
    Compact(astrcode_context::CompactError),
    #[error("LLM error: {0}")]
    Llm(#[source] astrcode_core::llm::LlmError),
    #[error(transparent)]
    Extension(astrcode_extension_sdk::extension::ExtensionError),
    #[error("Session close failed: {0}")]
    SessionClose(String),
    /// Command actor 通道已关闭，服务不可用。
    #[error("Command actor is unavailable")]
    ActorUnavailable,
    /// 验证失败或状态不满足前置条件。
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
}

impl From<TurnScheduleError> for HandlerError {
    fn from(error: TurnScheduleError) -> Self {
        match error {
            TurnScheduleError::TurnAlreadyRunning => Self::TurnAlreadyRunning,
            TurnScheduleError::NoActiveTurn => Self::NoActiveTurn,
            TurnScheduleError::QueueFull { .. }
            | TurnScheduleError::EmptyInput
            | TurnScheduleError::InputTooLarge { .. } => Self::InvalidRequest(error.to_string()),
            TurnScheduleError::SessionNotFound(message) => Self::SessionNotFound(message),
            TurnScheduleError::SessionManager(error) => Self::SessionManager(error),
            TurnScheduleError::Session(error) | TurnScheduleError::EventEmit(error) => {
                Self::Session(error)
            },
            TurnScheduleError::Turn(error) => Self::Turn(error),
            error @ (TurnScheduleError::RecycleRelationRollbackFailed { .. }
            | TurnScheduleError::DeleteRelationUpdateFailed { .. }
            | TurnScheduleError::ChildRelationConflict { .. }
            | TurnScheduleError::CompletionOwnershipLost { .. }
            | TurnScheduleError::BackgroundTasksClosed) => Self::SessionClose(error.to_string()),
        }
    }
}

/// 解析后的斜杠命令。
pub(crate) struct ParsedSlashCommand {
    pub name: String,
    pub arguments: String,
}

impl ParsedSlashCommand {
    pub(crate) fn has_name(&self) -> bool {
        !self.name.trim().trim_start_matches('/').is_empty()
    }
}

/// 解析斜杠命令，如 "/compact arg1 arg2"。
/// 返回 None 表示不是斜杠命令。
pub(crate) fn parse_slash_command(text: &str) -> Option<ParsedSlashCommand> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix('/')?.trim();
    if body.is_empty() {
        return Some(ParsedSlashCommand {
            name: String::new(),
            arguments: String::new(),
        });
    }

    let (name, arguments) = body
        .split_once(char::is_whitespace)
        .map(|(name, arguments)| (name, arguments.trim()))
        .unwrap_or((body, ""));

    Some(ParsedSlashCommand {
        name: name.to_ascii_lowercase(),
        arguments: arguments.to_string(),
    })
}
