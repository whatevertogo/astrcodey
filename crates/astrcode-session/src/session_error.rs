//! Session error type.

use astrcode_core::types::SessionId;
use astrcode_extension_sdk::extension::ExtensionError;
use astrcode_storage::StorageError;

use crate::session_event_sink::SessionEventPublishError;

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Event publish error: {0}")]
    EventPublish(#[from] SessionEventPublishError),
    #[error("Extension error: {0}")]
    Extension(#[from] ExtensionError),
    #[error("extension runtime changed during session preparation after {attempts} attempts")]
    RuntimeUnstable { attempts: usize },
    #[error("session parent chain contains a cycle at {session_id}")]
    ParentCycle { session_id: SessionId },
    #[error("session creation task failed: {0}")]
    CreationTask(String),
}

impl SessionError {
    /// 错误是否属于临时性故障，调用方可重试。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::EventPublish(error) if error.is_retryable())
    }

    pub(crate) fn uncertain_through_seq(&self) -> Option<u64> {
        match self {
            Self::Storage(error) => error.uncertain_through_seq(),
            Self::EventPublish(error) => error.uncertain_through_seq(),
            _ => None,
        }
    }
}
