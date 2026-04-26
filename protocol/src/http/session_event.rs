//! 会话目录事件 DTO。
//!
//! 目录事件载荷由 host-session 拥有；协议层保留同构 wire DTO，
//! 避免 protocol 反向依赖 runtime owner crate。

use serde::{Deserialize, Serialize};

use crate::http::PROTOCOL_VERSION;

/// 会话目录事件 wire DTO。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "event", content = "data")]
pub enum SessionCatalogEventPayload {
    SessionCreated {
        session_id: String,
    },
    SessionDeleted {
        session_id: String,
    },
    ProjectDeleted {
        working_dir: String,
    },
    SessionBranched {
        session_id: String,
        source_session_id: String,
    },
}

/// 会话目录事件信封。
///
/// 为事件载荷添加协议版本号，确保前端可以验证兼容性。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCatalogEventEnvelope {
    /// 协议版本号
    pub protocol_version: u32,
    /// 事件载荷，序列化后扁平化到信封层级
    #[serde(flatten)]
    pub event: SessionCatalogEventPayload,
}

impl SessionCatalogEventEnvelope {
    /// 创建新的事件信封，自动设置协议版本。
    pub fn new(event: SessionCatalogEventPayload) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            event,
        }
    }
}
