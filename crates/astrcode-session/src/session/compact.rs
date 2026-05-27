use astrcode_core::event::Event;

use super::Session;
use crate::payload::{
    compact_boundary_payload, session_continued_from_compaction_payload,
    system_prompt_configured_payload,
};

pub(crate) fn normalize_extra_system_prompt(extra_system_prompt: Option<&str>) -> Option<String> {
    extra_system_prompt.and_then(|prompt| {
        let trimmed = prompt.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

impl Session {
    /// 在同一条 session log 上追加 compact 边界（**不**分配新 `session_id`）。
    ///
    /// `continued_session_id` 与 `SessionContinuedFromCompaction.parent_session_id` 均为
    /// `self.id`。子 agent 与主 session 共用此路径；勿假设 compact 会产生 leaf session。
    #[allow(clippy::too_many_arguments)]
    pub async fn append_compact_boundary(
        &self,
        system_prompt: String,
        fingerprint: String,
        extra_system_prompt: Option<String>,
        trigger_name: String,
        compaction: astrcode_context::compaction::CompactResult,
        base_event_seq: u64,
        strategy: astrcode_core::extension::CompactStrategy,
    ) -> Result<Vec<Event>, super::SessionError> {
        // compact 语义：冻结 base_event_seq 之前的历史前缀。
        // 即使 compact 计算期间有新事件写入，也必须以 base_event_seq 作为边界标记，
        // 后续 replay 会将这些新事件归类为 tail delta 追加，不覆盖它们。
        let cursor = base_event_seq.to_string();
        let extra_system_prompt = normalize_extra_system_prompt(extra_system_prompt.as_deref());
        let mut events = Vec::with_capacity(3);
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                compact_boundary_payload(
                    trigger_name,
                    &compaction,
                    self.id.clone(),
                    base_event_seq,
                    strategy,
                ),
            ))
            .await?,
        );
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                system_prompt_configured_payload(system_prompt, fingerprint, extra_system_prompt),
            ))
            .await?,
        );
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                session_continued_from_compaction_payload(self.id.clone(), cursor, &compaction),
            ))
            .await?,
        );
        self.runtime.invalidate_stable_prefix_cache();
        if let Some(cursor) = self.latest_cursor().await? {
            self.checkpoint(&cursor).await?;
        }
        Ok(events)
    }
}
