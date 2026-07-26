//! 会话原子操作 API re-export。
//!
//! `SessionOperations` trait 定义在 `astrcode-core/src/tool.rs`，此处 re-export
//! 方便插件侧使用。

use serde::{Deserialize, Serialize};

pub use crate::{
    extension::SessionToolSelection,
    tool::{
        CreateSessionRequest, SessionAccess, SessionAccessPair, SessionApiError, SessionHandle,
        SessionOperations, SessionStatus, SubmitTurnRequest, SubmitTurnResult,
    },
};

/// 插件 session API 共用的工具选择线缆契约。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionToolSelectionDto {
    All {
        #[serde(default)]
        except: Vec<String>,
    },
    Only {
        #[serde(default)]
        names: Vec<String>,
    },
}

impl From<SessionToolSelection> for SessionToolSelectionDto {
    fn from(selection: SessionToolSelection) -> Self {
        match selection {
            SessionToolSelection::All { except } => Self::All { except },
            SessionToolSelection::Only { names } => Self::Only { names },
        }
    }
}
