use std::sync::Arc;

use astrcode_context::compaction::{
    CompactError, CompactResult, CompactSkipReason, CompactSummaryRenderOptions,
};
use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{CompactTrigger, ExtensionEvent},
    storage::CompactSnapshotInput,
    types::{SessionId, TurnId},
};
use astrcode_extensions::context::ServerExtensionContext;
use astrcode_protocol::events::ClientNotification;
use astrcode_tools::registry::ToolRegistry;

use super::{CommandHandler, session_snapshot};
use crate::{
    agent::{
        compact::{
            CompactHookContext, collect_compact_instructions, compact_trigger_name,
            compact_with_forked_provider, dispatch_post_compact,
        },
        post_compact::enrich_post_compact_context,
    },
    bootstrap::prompt_fingerprint,
    session::{
        CompactContinuationAppendInput, CompactContinuationCreateInput,
        append_compact_continuation_events, create_compact_continuation_session,
    },
};

struct PendingCompactContinuation {
    parent_session_id: SessionId,
    working_dir: String,
    model_id: String,
    system_prompt: String,
    tool_registry: Arc<ToolRegistry>,
    trigger: CompactTrigger,
    compaction: CompactResult,
    switch_active: bool,
}

impl CommandHandler {
    pub(super) async fn compact_active_session(&mut self) -> Result<(), String> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return Ok(());
        };
        self.compact_session(&sid).await.map(|_| ())
    }

    /// 手动压缩指定会话。
    pub async fn compact_session(&mut self, sid: &SessionId) -> Result<Option<SessionId>, String> {
        if self.active_turns.contains_key(sid) {
            self.send_error(40900, "Cannot compact while a turn is running");
            return Err("Cannot compact while a turn is running".into());
        }

        let state = self
            .runtime
            .session_manager
            .read_model(sid)
            .await
            .map_err(|e| format!("read session {sid}: {e}"))?;
        let tool_registry = self.ensure_tool_registry(sid, &state.working_dir).await;
        let provider_messages = state.provider_messages();
        let tools = tool_registry.list_definitions();
        let compact_instructions = match collect_compact_instructions(
            &self.runtime.extension_runner,
            CompactHookContext {
                session_id: sid,
                working_dir: &state.working_dir,
                model_id: &state.model_id,
                tools: &tools,
                trigger: CompactTrigger::ManualCommand,
                message_count: provider_messages.len(),
            },
        )
        .await
        {
            Ok(instructions) => instructions,
            Err(error) => {
                self.send_error(-32603, &format!("Compaction failed: {error}"));
                return Ok(None);
            },
        };
        let snapshot_path = match self
            .runtime
            .session_manager
            .write_compact_snapshot(
                sid,
                CompactSnapshotInput {
                    trigger: compact_trigger_name(CompactTrigger::ManualCommand).into(),
                    model_id: state.model_id.clone(),
                    working_dir: state.working_dir.clone(),
                    system_prompt: state.system_prompt.clone(),
                    provider_messages: provider_messages.clone(),
                },
            )
            .await
        {
            Ok(path) => path,
            Err(error) => {
                self.send_error(
                    -32603,
                    &format!("Compaction failed: could not write transcript snapshot: {error}"),
                );
                return Ok(None);
            },
        };
        let render_options = CompactSummaryRenderOptions {
            transcript_path: snapshot_path,
        };
        let mut compaction = match compact_with_forked_provider(
            Arc::clone(&self.runtime.llm_provider),
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
                self.send_error(40000, "Nothing to compact");
                return Ok(None);
            },
            Err(error) => {
                self.send_error(-32603, &format!("Compaction failed: {error}"));
                return Ok(None);
            },
        };
        enrich_post_compact_context(
            &mut compaction,
            sid,
            &provider_messages,
            &state.working_dir,
            state.system_prompt.as_deref(),
            &tools,
        )
        .await;

        if let Err(error) = dispatch_post_compact(
            &self.runtime.extension_runner,
            CompactHookContext {
                session_id: sid,
                working_dir: &state.working_dir,
                model_id: &state.model_id,
                tools: &tools,
                trigger: CompactTrigger::ManualCommand,
                message_count: provider_messages.len(),
            },
            &compaction,
        )
        .await
        {
            self.send_error(-32603, &format!("Compaction failed: {error}"));
            return Ok(None);
        }

        let system_prompt = match &state.system_prompt {
            Some(system_prompt) => system_prompt.clone(),
            None => {
                self.configure_session_prompt(sid, &state.working_dir, &tool_registry, None)
                    .await?
            },
        };
        let child_session_id = self
            .create_compact_continuation_child(PendingCompactContinuation {
                parent_session_id: sid.clone(),
                working_dir: state.working_dir.clone(),
                model_id: state.model_id.clone(),
                system_prompt,
                tool_registry,
                trigger: CompactTrigger::ManualCommand,
                compaction,
                switch_active: true,
            })
            .await?;
        Ok(Some(child_session_id))
    }


    async fn create_compact_continuation_child(
        &mut self,
        input: PendingCompactContinuation,
    ) -> Result<SessionId, String> {
        let working_dir = input.working_dir.clone();
        let model_id = input.model_id.clone();
        let system_prompt = input.system_prompt.clone();
        let parent_session_id = input.parent_session_id.clone();
        let is_manual_compact = input.trigger == CompactTrigger::ManualCommand;
        let continuation = create_compact_continuation_session(
            &self.runtime.session_manager,
            CompactContinuationCreateInput {
                parent_session_id: input.parent_session_id,
                working_dir: input.working_dir,
                model_id: input.model_id,
            },
        )
        .await?;
        let child_session_id = continuation.child_session_id.clone();
        self.session_tool_registries
            .insert(child_session_id.clone(), Arc::clone(&input.tool_registry));
        let _ = self.event_tx.send(ClientNotification::Event(
            continuation.child_started.clone(),
        ));

        let ext_ctx = ServerExtensionContext::new(
            child_session_id.clone(),
            working_dir,
            ModelSelection {
                profile_name: String::new(),
                model: model_id,
                provider_kind: String::new(),
            },
        );
        self.runtime
            .extension_runner
            .dispatch(ExtensionEvent::SessionStart, &ext_ctx)
            .await
            .map_err(|e| e.to_string())?;

        let events = append_compact_continuation_events(
            &self.runtime.session_manager,
            CompactContinuationAppendInput {
                session: continuation,
                system_prompt_fingerprint: prompt_fingerprint(&system_prompt),
                system_prompt,
                trigger_name: compact_trigger_name(input.trigger).into(),
                compaction: input.compaction,
            },
        )
        .await?;
        if is_manual_compact {
            // Auto compact emits this from the agent loop at the real compact
            // point. Manual compact has no agent loop, so emit it here after
            // failure/skip paths are behind us and before the boundary event.
            self.record_and_broadcast(&parent_session_id, None, EventPayload::CompactionStarted)
                .await?;
        }
        for event in events.appended_events {
            let _ = self.event_tx.send(ClientNotification::Event(event));
        }
        if input.trigger == CompactTrigger::AutoThreshold {
            self.runtime
                .auto_compact_failures
                .transfer_session(&parent_session_id, &child_session_id);
        }
        if input.switch_active {
            self.active_session_id = Some(child_session_id.clone());
        }
        let child_state = self
            .runtime
            .session_manager
            .read_model(&child_session_id)
            .await
            .map_err(|e| format!("read session {child_session_id}: {e}"))?;
        let _ = self.event_tx.send(ClientNotification::SessionResumed {
            session_id: child_session_id.clone(),
            snapshot: session_snapshot(&child_state),
        });
        Ok(child_session_id)
    }

    pub(super) async fn continue_active_turn_from_compaction(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        trigger: CompactTrigger,
        compaction: CompactResult,
    ) -> Result<SessionId, String> {
        let Some(mut active_turn) = self.active_turns.remove(&session_id) else {
            return Err("stale auto compact transition".into());
        };
        if active_turn.turn_id != turn_id {
            self.active_turns.insert(session_id, active_turn);
            return Err("stale auto compact transition".into());
        }

        let input = PendingCompactContinuation {
            parent_session_id: session_id.clone(),
            working_dir: active_turn.working_dir.clone(),
            model_id: active_turn.model_id.clone(),
            system_prompt: active_turn.system_prompt.clone(),
            tool_registry: Arc::clone(&active_turn.tool_registry),
            trigger,
            compaction,
            switch_active: active_turn.switch_active_on_continuation,
        };

        match self.create_compact_continuation_child(input).await {
            Ok(child_session_id) => {
                active_turn.session_id = child_session_id.clone();
                self.active_turns
                    .insert(child_session_id.clone(), active_turn);
                Ok(child_session_id)
            },
            Err(error) => {
                self.active_turns.insert(session_id, active_turn);
                Err(error)
            },
        }
    }
}
