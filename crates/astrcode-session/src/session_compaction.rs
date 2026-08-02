//! Session compaction service — transcript rewrite and checkpoint.

use astrcode_core::{compaction::CompactStrategy, event::StoredEvent};

use crate::{payload::transcript_rewritten_payload, session::Session, session_error::SessionError};

impl Session {
    /// 记录 compact 并重写同一 session 的 provider transcript 前缀。
    pub async fn rewrite_transcript_for_compaction(
        &self,
        trigger_name: String,
        compaction: astrcode_context::CompactResult,
        source_seq: u64,
        strategy: CompactStrategy,
    ) -> Result<StoredEvent, SessionError> {
        let event = self
            .emit_durable(
                None,
                transcript_rewritten_payload(trigger_name, &compaction, source_seq, strategy),
            )
            .await?;
        if let Err(error) = self.checkpoint(&event.seq.to_string()).await {
            tracing::warn!(
                session_id = %self.id(),
                seq = event.seq,
                error = %error,
                "transcript rewrite committed but checkpoint was skipped"
            );
        }
        Ok(event)
    }
}
