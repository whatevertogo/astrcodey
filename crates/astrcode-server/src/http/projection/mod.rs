//! Event → DTO 投影。
//!
//! 子模块按职责切分：
//! - `args`: 工具参数 → 折叠摘要文本。
//! - `blocks`: payload/message → ConversationBlockDto。
//! - `live`: 实时 event → ConversationDeltaDto。
//! - `replay`: 历史 event → ConversationDeltaDto。
//! - `snapshot`: session read model → ConversationSnapshotResponseDto。

pub(in crate::http) mod args;
pub(in crate::http) mod blocks;
pub(in crate::http) mod live;
pub(in crate::http) mod replay;
pub(in crate::http) mod snapshot;
