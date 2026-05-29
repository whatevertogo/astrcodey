//! Session cursor 解析 — compaction 基线 event seq。

use astrcode_core::types::Cursor;

use super::SessionError;

/// 将持久化 cursor 解析为 compaction 基线 event seq。
///
/// 无 cursor 时返回 0（新 session）。cursor 存在但非 u64 时返回 [`SessionError::InvalidCursor`]。
pub(crate) fn parse_base_event_seq(cursor: Option<Cursor>) -> Result<u64, SessionError> {
    match cursor {
        None => Ok(0),
        Some(cursor) => cursor
            .parse::<u64>()
            .map_err(|_| SessionError::InvalidCursor(cursor)),
    }
}
