//! Compact rewrite 的 durable 提交边界。

use astrcode_context::{CompactResult, ContextSnapshot};
use astrcode_core::{
    compaction::CompactStrategy,
    event::{StoredEvent, transcript_prefix_fingerprint},
};

use crate::{payload::transcript_rewritten_payload, session::Session, session_error::SessionError};

/// 追加 rewrite，确认 EventLog 已 fsync，再以 best-effort 方式创建恢复快照。
pub(crate) async fn persist_compaction(
    session: &Session,
    compaction: &CompactResult,
    trigger_name: &str,
    snapshot: &ContextSnapshot,
    strategy: CompactStrategy,
) -> Result<StoredEvent, SessionError> {
    session
        .rewrite_transcript_for_compaction(
            trigger_name.to_owned(),
            compaction.clone(),
            snapshot.source_seq,
            transcript_prefix_fingerprint(&snapshot.system_prompt, &snapshot.messages),
            strategy,
        )
        .await
}

impl Session {
    /// 记录 compact rewrite，并在返回前确认 EventLog 已 fsync。
    pub async fn rewrite_transcript_for_compaction(
        &self,
        trigger_name: String,
        compaction: CompactResult,
        source_seq: u64,
        source_fingerprint: String,
        strategy: CompactStrategy,
    ) -> Result<StoredEvent, SessionError> {
        let event = self
            .emit_durable(
                None,
                transcript_rewritten_payload(
                    trigger_name,
                    &compaction,
                    source_seq,
                    source_fingerprint,
                    strategy,
                ),
            )
            .await?;

        self.sync_durable_events().await?;

        if let Err(error) = self.checkpoint(&event.seq.to_string()).await {
            tracing::warn!(
                session_id = %self.id(),
                seq = event.seq,
                error = %error,
                "transcript rewrite is durable but checkpoint was skipped"
            );
        }

        Ok(event)
    }
}
