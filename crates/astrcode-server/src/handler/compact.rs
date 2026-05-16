use astrcode_context::compaction::{
    CompactSkipReason, CompactSummaryRenderOptions, compact_messages_with_render_options,
};
use astrcode_core::{
    event::EventPayload, extension::CompactTrigger, storage::CompactSnapshotInput, types::SessionId,
};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::{
    compact::{
        CompactHookContext, collect_compact_instructions, compact_trigger_name,
        dispatch_post_compact,
    },
    post_compact::enrich_post_compact_context,
};

use super::{CommandHandler, HandlerError, session_snapshot};
use crate::bootstrap::prompt_fingerprint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualCompactOutcome {
    Compacted { session_id: SessionId },
    Skipped { message: String },
}

impl CommandHandler {
    pub(super) async fn compact_active_session(&mut self) -> Result<(), HandlerError> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return Ok(());
        };
        match self.compact_session(&sid).await {
            Ok(ManualCompactOutcome::Compacted { .. }) => Ok(()),
            Ok(ManualCompactOutcome::Skipped { message }) => {
                self.send_error(40000, &message);
                Ok(())
            },
            Err(error) => {
                self.send_error(-32603, &error.to_string());
                Err(error)
            },
        }
    }

    /// 手动压缩指定会话。
    pub async fn compact_session(
        &mut self,
        sid: &SessionId,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        if self.active_turns.contains_key(sid) {
            self.send_error(40900, "Cannot compact while a turn is running");
            return Err(HandlerError::CompactBlocked);
        }

        let session = self
            .runtime
            .session_manager
            .open(sid.clone())
            .await
            .map_err(|e| HandlerError::Other(format!("open session {sid}: {e}")))?;

        let state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        let tool_registry = self.ensure_tool_registry(sid, &state.working_dir).await;
        let provider_messages = state.provider_messages();
        let tools = tool_registry.list_definitions();

        let hook_ctx = CompactHookContext {
            session_id: sid.as_str(),
            working_dir: &state.working_dir,
            model_id: &state.model_id,
            trigger: CompactTrigger::ManualCommand,
            message_count: provider_messages.len(),
        };
        let custom_instructions =
            match collect_compact_instructions(&self.runtime.extension_runner, hook_ctx).await {
                Ok(instructions) => instructions,
                Err(error) => {
                    return Err(HandlerError::Other(format!("Compaction failed: {error}")));
                },
            };

        let snapshot_path = match session
            .write_compact_snapshot(CompactSnapshotInput {
                trigger: compact_trigger_name(CompactTrigger::ManualCommand).into(),
                model_id: state.model_id.clone(),
                working_dir: state.working_dir.clone(),
                system_prompt: state.system_prompt.clone(),
                provider_messages: provider_messages.clone(),
            })
            .await
        {
            Ok(path) => path,
            Err(error) => {
                return Err(HandlerError::Other(format!(
                    "Compaction failed: could not write transcript snapshot: {error}"
                )));
            },
        };
        let render_options = CompactSummaryRenderOptions {
            transcript_path: snapshot_path,
            custom_instructions,
        };
        let mut compaction = match compact_messages_with_render_options(
            &provider_messages,
            state.system_prompt.as_deref(),
            &render_options,
        ) {
            Ok(compaction) => compaction,
            Err(CompactSkipReason::Empty | CompactSkipReason::NothingToCompact) => {
                return Ok(ManualCompactOutcome::Skipped {
                    message: "Nothing to compact".into(),
                });
            },
        };
        enrich_post_compact_context(
            &mut compaction,
            sid.as_str(),
            &provider_messages,
            &state.working_dir,
            state.system_prompt.as_deref(),
            &tools,
            self.runtime.context_assembler.settings(),
        )
        .await;

        if let Err(error) =
            dispatch_post_compact(&self.runtime.extension_runner, hook_ctx, &compaction).await
        {
            return Err(HandlerError::Other(format!("Compaction failed: {error}")));
        }

        let system_prompt = state.system_prompt.clone().ok_or_else(|| {
            HandlerError::Other("Cannot compact session without system prompt".into())
        })?;

        // Manual compact has no agent loop, so emit CompactionStarted here.
        self.record_and_broadcast(sid, None, EventPayload::CompactionStarted)
            .await
            .map_err(HandlerError::Other)?;

        let fp = prompt_fingerprint(&system_prompt);
        let trigger = compact_trigger_name(CompactTrigger::ManualCommand).into();
        let events = session
            .append_compact_boundary(system_prompt, fp, trigger, compaction)
            .await
            .map_err(|e| HandlerError::Other(e.to_string()))?;

        for event in &events {
            let _ = self.event_tx.send(ClientNotification::Event(event.clone()));
        }

        let state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        let _ = self.event_tx.send(ClientNotification::SessionResumed {
            session_id: sid.clone().into_string(),
            snapshot: session_snapshot(&state),
        });

        Ok(ManualCompactOutcome::Compacted {
            session_id: sid.clone(),
        })
    }
}
