use astrcode_context::compaction::{
    CompactSkipReason, CompactSummaryRenderOptions, compact_messages_with_fallback,
};
use astrcode_core::{
    event::EventPayload, extension::CompactTrigger, storage::CompactSnapshotInput, types::SessionId,
};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::{
    compact::{
        CompactHookContext, collect_compact_instructions, compact_trigger_name,
        dispatch_post_compact, make_compact_request_fn,
    },
    post_compact::enrich_post_compact_context,
};
use astrcode_support::hash::hex_fingerprint;

use super::{CommandHandler, HandlerError, session_snapshot};

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

        // 标记 session 为 compacting 状态，让输入自动排队
        self.compacting_sessions.insert(sid.clone());

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
        // Session runtime 已持有当前 session 的工具表快照；空时按需刷新。
        let tool_registry = {
            let current = session.runtime().tool_registry();
            if current.list_definitions().is_empty() {
                session.refresh_tools(&state.working_dir).await
            } else {
                current
            }
        };
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
            custom_instructions: custom_instructions.clone(),
        };
        let llm = self.runtime.config_manager.read_llm_provider();
        let request_fn = make_compact_request_fn(llm);
        let settings = self.runtime.context_assembler.settings().clone();
        let mut compaction = match compact_messages_with_fallback(
            &provider_messages,
            state.system_prompt.as_deref(),
            &settings,
            &custom_instructions,
            &render_options,
            request_fn,
        )
        .await
        {
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
        session
            .emit_live(None, EventPayload::CompactionStarted)
            .await;

        let fp = hex_fingerprint(system_prompt.as_bytes());
        let trigger = compact_trigger_name(CompactTrigger::ManualCommand).into();
        let messages_removed = compaction.messages_removed;
        let events = session
            .append_compact_boundary(
                system_prompt,
                fp,
                state.extra_system_prompt.clone(),
                trigger,
                compaction,
            )
            .await
            .map_err(|e| HandlerError::Other(e.to_string()))?;

        for event in &events {
            self.broadcast_event(event.clone());
        }

        // 发送 CompactionCompleted 事件
        session
            .emit_live(None, EventPayload::CompactionCompleted { messages_removed })
            .await;

        let state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        self.event_bus
            .send_notification(ClientNotification::SessionResumed {
                session_id: sid.clone().into_string(),
                snapshot: session_snapshot(&state),
            });

        Ok(ManualCompactOutcome::Compacted {
            session_id: sid.clone(),
        })
    }
}
