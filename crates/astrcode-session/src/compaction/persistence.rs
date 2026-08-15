//! Compact rewrite 的 durable 提交边界。

use astrcode_context::{CompactResult, ContextSnapshot};
use astrcode_core::{compaction::CompactStrategy, event::transcript_prefix_fingerprint};
use astrcode_storage::StorageError;

use crate::{payload::transcript_rewritten_payload, session::Session, session_error::SessionError};

/// 追加 rewrite 并确认 EventLog 已 fsync。
pub(crate) async fn persist_compaction(
    session: &Session,
    compaction: &CompactResult,
    snapshot: &ContextSnapshot,
    strategy: CompactStrategy,
) -> Result<(), SessionError> {
    let retained_messages = snapshot
        .retained_transcript_messages(&compaction.retained_messages)
        .ok_or_else(|| {
            StorageError::InvalidEvent(
                "compact retained messages are not the source transcript suffix".into(),
            )
        })?;
    let fingerprint = transcript_prefix_fingerprint(&snapshot.system_prompt, &snapshot.messages)
        .map_err(|error| SessionError::Storage(StorageError::Serialization(error)))?;
    session
        .emit_durable_and_sync(
            None,
            transcript_rewritten_payload(
                compaction,
                &retained_messages,
                snapshot.source_seq,
                fingerprint,
                strategy,
            ),
        )
        .await?;

    Ok(())
}
