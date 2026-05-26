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
        let cursor = self.latest_cursor().await?.unwrap_or_else(|| "0".into());
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
