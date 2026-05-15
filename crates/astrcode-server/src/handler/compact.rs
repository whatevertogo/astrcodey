use astrcode_context::compaction::{
    CompactError, CompactResult, CompactSkipReason, CompactSummaryRenderOptions,
};
use astrcode_core::{
    event::EventPayload,
    extension::CompactTrigger,
    storage::CompactSnapshotInput,
    types::{SessionId, TurnId},
};
use astrcode_protocol::events::ClientNotification;

use super::{CommandHandler, HandlerError, session_snapshot};
use crate::{
    agent::{
        compact::{
            CompactHookContext, collect_compact_instructions, compact_trigger_name,
            compact_with_forked_provider, dispatch_post_compact,
        },
        post_compact::enrich_post_compact_context,
    },
    bootstrap::prompt_fingerprint,
    session::{SameSessionCompactionInput, Session, append_same_session_compaction},
};

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

        let session = Session::open(self.runtime.event_store.clone(), sid.clone())
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
        let compact_instructions =
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
        };
        let mut compaction = match compact_with_forked_provider(
            self.runtime.read_llm_provider(),
            tools.clone(),
            &provider_messages,
            state.system_prompt.as_deref(),
            self.runtime.context_assembler.settings(),
            &compact_instructions,
            &render_options,
        )
        .await
        {
            Ok(compaction) => compaction,
            Err(CompactError::Skip(
                CompactSkipReason::Empty | CompactSkipReason::NothingToCompact,
            )) => {
                return Ok(ManualCompactOutcome::Skipped {
                    message: "Nothing to compact".into(),
                });
            },
            Err(error) => {
                return Err(HandlerError::Other(format!("Compaction failed: {error}")));
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

        let events = append_same_session_compaction(
            &session,
            SameSessionCompactionInput {
                session_id: sid.clone(),
                system_prompt_fingerprint: prompt_fingerprint(&system_prompt),
                system_prompt,
                trigger_name: compact_trigger_name(CompactTrigger::ManualCommand).into(),
                compaction,
            },
        )
        .await
        .map_err(|e| HandlerError::Other(e.to_string()))?;

        for event in events {
            let _ = self.event_tx.send(ClientNotification::Event(event));
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

    pub(super) async fn continue_active_turn_from_compaction(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        _trigger: CompactTrigger,
        compaction: CompactResult,
    ) -> Result<SessionId, HandlerError> {
        // 校验 active turn 存在且 turn_id 匹配，但不 remove/reinsert。
        let Some(active_turn) = self.active_turns.get(&session_id) else {
            return Err(HandlerError::Other("stale auto compact transition".into()));
        };
        if active_turn.turn_id != turn_id {
            return Err(HandlerError::Other("stale auto compact transition".into()));
        }
        let system_prompt = active_turn.system_prompt.clone();

        let session = Session::open(self.runtime.event_store.clone(), session_id.clone())
            .await
            .map_err(|e| HandlerError::Other(format!("open session: {e}")))?;

        // Auto compact 的 CompactionStarted 已由 agent loop 发出，不再重复。
        let events = append_same_session_compaction(
            &session,
            SameSessionCompactionInput {
                session_id: session_id.clone(),
                system_prompt_fingerprint: prompt_fingerprint(&system_prompt),
                system_prompt,
                trigger_name: compact_trigger_name(CompactTrigger::AutoThreshold).into(),
                compaction,
            },
        )
        .await
        .map_err(|e| HandlerError::Other(e.to_string()))?;

        for event in events {
            let _ = self.event_tx.send(ClientNotification::Event(event));
        }

        let state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session {session_id}: {e}")))?;
        let _ = self.event_tx.send(ClientNotification::SessionResumed {
            session_id: session_id.clone().into_string(),
            snapshot: session_snapshot(&state),
        });

        Ok(session_id)
    }
}
