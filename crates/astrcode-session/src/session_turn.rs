//! Session turn submission and finalization service.

use std::{path::PathBuf, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, LiveEventPayload, Phase},
    llm::{LlmRole, TranscriptMessageOrigin},
    message_attachment::MessageAttachment,
    tool::SessionToolSelection,
    types::*,
    user_input::UserInput,
};
use astrcode_extension_sdk::{
    extension::{
        UserMessageEnvelopeResult,
        internal::{RuntimeHookCallContext, runtime_user_message_envelope_context},
    },
    runtime_ports::TurnExtensionView,
};
use astrcode_session_projection::SessionReadModel;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    payload::{
        JSON_RPC_INTERNAL_ERROR, TURN_FINISH_ABORTED, TURN_FINISH_ERROR,
        agent_run_completed_payload, turn_completed_payload,
    },
    runtime_stability::RuntimeStabilityBudget,
    session::Session,
    session_error::SessionError,
    session_runtime_services::RuntimeGenerationView,
    tool_exec::TurnToolContext,
    turn_context::{TurnError, hook_call_context_for_read_model},
    turn_handle::{SharedTurnFinalization, TurnHandle},
    turn_runner::{RunTurnResult, TurnFinalization, TurnLoop, run_turn},
};

impl Session {
    fn turn_hook_call_context(
        &self,
        state: &SessionReadModel,
        session_store_dir: Option<PathBuf>,
        turn_id: &TurnId,
        cancellation: &CancellationToken,
        runtime_generation: &RuntimeGenerationView,
    ) -> RuntimeHookCallContext {
        hook_call_context_for_read_model(self.id(), state, session_store_dir)
            .with_turn_id(turn_id.to_string())
            .with_llm_providers(
                runtime_generation.llm_bindings_for_model_id(&state.identity.model_id),
            )
            .with_cancellation(cancellation.clone())
    }

    async fn emit_turn_start_events(
        &self,
        text: &str,
        attachments: &[MessageAttachment],
        turn_id: &TurnId,
        accepted_seq: Option<u64>,
    ) -> Result<(), TurnError> {
        self.emit_durable(Some(turn_id), DurableEventPayload::TurnStarted)
            .await?;
        self.emit_durable(
            Some(turn_id),
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.to_string(),
                attachments: attachments.to_vec(),
                accepted_seq,
            },
        )
        .await?;
        self.emit_live(Some(turn_id), LiveEventPayload::AgentRunStarted);
        Ok(())
    }

    async fn apply_user_message_envelope(
        &self,
        runtime_view: &TurnExtensionView,
        text: String,
        attachments: &[MessageAttachment],
        call: RuntimeHookCallContext,
    ) -> Result<String, TurnError> {
        let original_text = text.clone();
        let ctx = runtime_user_message_envelope_context(call, text, attachments.to_vec());
        match runtime_view
            .turn_hooks()
            .emit_user_message_envelope(ctx)
            .await?
        {
            UserMessageEnvelopeResult::Allow => Ok(original_text),
            UserMessageEnvelopeResult::ReplaceText { text } => Ok(text),
            UserMessageEnvelopeResult::AppendText { text } => {
                let mut combined = original_text;
                if !combined.is_empty() && !text.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(&text);
                Ok(combined)
            },
            UserMessageEnvelopeResult::Block { reason } => Err(TurnError::InputBlocked { reason }),
        }
    }

    async fn prepare_turn_runner(
        &self,
        runtime_view: &TurnExtensionView,
        runtime_generation: RuntimeGenerationView,
        turn_id: &TurnId,
        cancellation_token: CancellationToken,
    ) -> Result<TurnLoop, TurnError> {
        let session_store_dir = self
            .runtime
            .store()
            .session_store_dir(self.id())
            .await
            .map_err(SessionError::from)?;
        let approval_history = self.runtime.approval_history();
        let approval_history_path = session_store_dir
            .as_deref()
            .map(crate::permission::approval_history_path);
        approval_history
            .ensure_loaded(approval_history_path.as_deref())
            .await
            .map_err(|error| TurnError::ApprovalHistory(error.to_string()))?;

        let mut pre_state = self.read_model().await?;
        let model_id = if pre_state.identity.parent.is_some() {
            pre_state.identity.model_id.clone()
        } else {
            runtime_generation.effective().llm.model_id.clone()
        };
        if pre_state.identity.model_id != model_id {
            self.emit_durable(
                None,
                DurableEventPayload::ModelIdChanged {
                    model_id: model_id.clone(),
                },
            )
            .await?;
            pre_state = self.read_model().await?;
        }
        let llm = runtime_generation.llm_for_model_id(&model_id);
        let working_dir = pre_state.identity.working_dir.clone();
        let (registry, tool_selection, prompt_changed) = if pre_state.system_prompt.source
            == astrcode_core::event::SystemPromptSource::Inherited
        {
            let tool_selection = self.effective_tool_selection(self.id(), &pre_state).await?;
            let tool_selection_ref: Option<&SessionToolSelection> = tool_selection.as_ref();
            let mut stability = RuntimeStabilityBudget::new();
            let tool_snapshot = self
                .resolve_tool_registry_snapshot(
                    runtime_view,
                    &working_dir,
                    tool_selection_ref,
                    &mut stability,
                )
                .await?;
            (tool_snapshot.registry, tool_selection, false)
        } else {
            let stored_fingerprint = pre_state.system_prompt.fingerprint.clone();
            let hook_call = self.turn_hook_call_context(
                &pre_state,
                session_store_dir.clone(),
                turn_id,
                &cancellation_token,
                &runtime_generation,
            );
            let prepared = self
                .prepare_runtime_snapshot(runtime_view, &pre_state, hook_call)
                .await?;
            let prompt_changed = self
                .persist_system_prompt(prepared.prompt, Some(&stored_fingerprint))
                .await?;
            (prepared.registry, prepared.tool_selection, prompt_changed)
        };

        let session_state = if prompt_changed {
            // Prompt 刷新可能写入 durable event，需重读 projection。
            self.read_model().await?
        } else {
            pre_state
        };
        let turn = TurnToolContext::for_turn(
            self,
            &runtime_generation,
            &session_state,
            turn_id.clone(),
            tool_selection.unwrap_or_default(),
            session_store_dir,
            cancellation_token,
        );
        TurnLoop::new_with_llm(
            self.clone(),
            llm,
            turn,
            registry,
            runtime_view.turn_hooks_arc(),
            runtime_generation,
        )
    }

    async fn run_and_finalize_turn(
        session: Session,
        mut agent: TurnLoop,
        text: String,
        turn_id: TurnId,
        cancellation_token: CancellationToken,
        completion_tx: oneshot::Sender<RunTurnResult>,
        finalization_state: SharedTurnFinalization,
    ) {
        let mut result = run_turn(&mut agent, &text, &turn_id).await;
        match finalize_turn(&session, &turn_id, &result.finalization).await {
            Ok(()) => result.finalization.mark_persisted(),
            Err(error) => {
                tracing::error!(
                    session_id = %session.id(),
                    turn_id = %turn_id,
                    %error,
                    finish_reason = %result.finalization.finish_reason,
                    "failed to persist turn finalization; registry must retain ownership for retry"
                );
            },
        }
        cancellation_token.cancel();
        *finalization_state.lock() = Some(result.finalization.clone());
        let _ = completion_tx.send(result);
    }

    fn spawn_prepared_turn(&self, agent: TurnLoop, text: String, turn_id: TurnId) -> TurnHandle {
        let cancellation_token = agent.cancellation_token();
        let (completion_tx, completion_rx) = oneshot::channel();
        let turn_id_for_task = turn_id.clone();
        let session_for_completion = self.clone();
        let cancellation_for_task = cancellation_token.clone();
        let finalization_state = Arc::new(Mutex::new(None));
        let finalization_for_task = Arc::clone(&finalization_state);
        let join = tokio::spawn(async move {
            Self::run_and_finalize_turn(
                session_for_completion,
                agent,
                text,
                turn_id_for_task,
                cancellation_for_task,
                completion_tx,
                finalization_for_task,
            )
            .await;
        });

        TurnHandle::new(
            turn_id,
            join,
            cancellation_token,
            completion_rx,
            finalization_state,
        )
    }

    pub async fn submit(
        &self,
        input: UserInput,
        turn_id: TurnId,
        accepted_seq: Option<u64>,
    ) -> Result<TurnHandle, TurnError> {
        let cancellation_token = CancellationToken::new();
        let setup_cancellation = cancellation_token.clone().drop_guard();
        let (runtime_generation, runtime_view) =
            self.runtime_services.pin_turn_generation().await?;
        let UserInput { text, attachments } = input;
        let envelope_state = self.read_model().await?;
        let envelope_call = self.turn_hook_call_context(
            &envelope_state,
            self.session_store_dir().await,
            &turn_id,
            &cancellation_token,
            &runtime_generation,
        );
        let text = self
            .apply_user_message_envelope(&runtime_view, text, &attachments, envelope_call)
            .await?;
        self.emit_turn_start_events(&text, &attachments, &turn_id, accepted_seq)
            .await?;
        let agent = match self
            .prepare_turn_runner(
                &runtime_view,
                runtime_generation,
                &turn_id,
                cancellation_token.clone(),
            )
            .await
        {
            Ok(agent) => agent,
            Err(error) => {
                self.settle_failed_turn_setup(&turn_id, &error).await;
                return Err(error);
            },
        };
        setup_cancellation.disarm();
        Ok(self.spawn_prepared_turn(agent, text, turn_id))
    }

    /// 重新驱动事件日志中仍处于 active step 的原 turn，不追加新的用户消息。
    pub async fn resume(&self, turn_id: TurnId) -> Result<TurnHandle, TurnError> {
        let cancellation_token = CancellationToken::new();
        let setup_cancellation = cancellation_token.clone().drop_guard();
        let state = self.read_model().await?;
        let text = state
            .model_context
            .messages
            .iter()
            .rev()
            .find(|message| message.origin.is_none() && message.message.role == LlmRole::User)
            .map(|message| message.message.joined_display_text("\n"))
            .unwrap_or_default();
        let (runtime_generation, runtime_view) =
            self.runtime_services.pin_turn_generation().await?;
        let agent = self
            .prepare_turn_runner(
                &runtime_view,
                runtime_generation,
                &turn_id,
                cancellation_token,
            )
            .await?;
        setup_cancellation.disarm();
        self.emit_live(Some(&turn_id), LiveEventPayload::AgentRunStarted);
        Ok(self.spawn_prepared_turn(agent, text, turn_id))
    }

    async fn settle_failed_turn_setup(&self, turn_id: &TurnId, error: &TurnError) {
        if let Err(persist_error) = self
            .emit_durable(
                Some(turn_id),
                DurableEventPayload::ErrorOccurred {
                    code: JSON_RPC_INTERNAL_ERROR,
                    message: error.to_string(),
                    recoverable: false,
                },
            )
            .await
        {
            tracing::error!(
                session_id = %self.id(),
                %turn_id,
                error = %persist_error,
                "failed to persist turn setup error"
            );
        }
        if let Err(persist_error) = self
            .emit_durable(Some(turn_id), turn_completed_payload(TURN_FINISH_ERROR))
            .await
        {
            tracing::error!(
                session_id = %self.id(),
                %turn_id,
                error = %persist_error,
                "failed to complete turn after setup error"
            );
        }
        self.emit_live(
            Some(turn_id),
            agent_run_completed_payload(TURN_FINISH_ERROR),
        );
    }
}

pub async fn finalize_aborted_turn(
    session: &Session,
    turn_id: &TurnId,
) -> Result<(), SessionError> {
    let state = session.read_model().await?;
    if state.execution.phase == Phase::Idle {
        return Ok(());
    }
    emit_interrupted_tool_results(
        session,
        &state,
        Some(turn_id),
        InterruptedToolOutcome::Cancelled,
    )
    .await?;
    let has_aborted_context = state
        .model_context
        .messages
        .last()
        .is_some_and(|message| message.origin == Some(TranscriptMessageOrigin::TurnAborted));
    if !has_aborted_context {
        emit_turn_aborted_context(session, Some(turn_id)).await?;
    }
    emit_turn_completed(session, turn_id, TURN_FINISH_ABORTED).await
}

pub async fn finalize_turn(
    session: &Session,
    turn_id: &TurnId,
    finalization: &TurnFinalization,
) -> Result<(), SessionError> {
    if finalization.aborted {
        return finalize_aborted_turn(session, turn_id).await;
    }

    let state = session.read_model().await?;
    if state.execution.phase == Phase::Idle {
        return Ok(());
    }
    if let Some(message) = &finalization.pending_error
        && state.execution.phase != Phase::Error
    {
        session
            .emit_durable(
                Some(turn_id),
                DurableEventPayload::ErrorOccurred {
                    code: JSON_RPC_INTERNAL_ERROR,
                    message: message.clone(),
                    recoverable: false,
                },
            )
            .await?;
    }
    emit_turn_completed(session, turn_id, &finalization.finish_reason).await
}

/// 持久化 turn 完成事件并发送对应 live 事件。
async fn emit_turn_completed(
    session: &Session,
    turn_id: &TurnId,
    finish_reason: &str,
) -> Result<(), SessionError> {
    session
        .emit_durable(Some(turn_id), turn_completed_payload(finish_reason))
        .await?;
    session.emit_live(Some(turn_id), agent_run_completed_payload(finish_reason));
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedToolOutcome {
    Failed,
    Cancelled,
}

pub async fn emit_interrupted_tool_results(
    session: &Session,
    state: &SessionReadModel,
    turn_id: Option<&TurnId>,
    outcome: InterruptedToolOutcome,
) -> Result<usize, SessionError> {
    let mut emitted = 0;
    for pending in state.tool_calls_needing_interruption() {
        let payload = match outcome {
            InterruptedToolOutcome::Failed => DurableEventPayload::ToolCallFailed {
                call_id: pending.call_id.into(),
                tool_name: pending.tool_name,
                error: "tool execution interrupted before completion".into(),
                metadata: Default::default(),
                duration_ms: None,
                arguments: String::new(),
                arguments_json: None,
            },
            InterruptedToolOutcome::Cancelled => DurableEventPayload::ToolCallCancelled {
                call_id: pending.call_id.into(),
                tool_name: pending.tool_name,
                reason: "turn aborted".into(),
                duration_ms: None,
                arguments: String::new(),
                arguments_json: None,
            },
        };
        session.emit_durable(turn_id, payload).await?;
        emitted += 1;
    }
    Ok(emitted)
}

pub async fn emit_turn_aborted_context(
    session: &Session,
    turn_id: Option<&TurnId>,
) -> Result<(), SessionError> {
    session
        .emit_durable(turn_id, DurableEventPayload::TurnAbortedContext)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use astrcode_extension_sdk::{
        extension::{
            ExtensionError, LifecycleEvent, PromptContributions, UserMessageEnvelopeResult,
            internal::{
                RuntimeLifecycleContext, RuntimePromptBuildContext,
                RuntimeUserMessageEnvelopeContext,
            },
        },
        runtime_ports::{
            PromptContributor, RuntimeSnapshotProvider, RuntimeSnapshotState,
            SessionOperationsProvider, ToolCatalogProvider, ToolCatalogScope, ToolCatalogSnapshot,
            TurnExtensionView, TurnExtensionViewProvider, TurnHooks,
        },
    };
    use astrcode_storage::in_memory::InMemoryEventStore;

    use super::*;
    use crate::{
        SessionExtensionPorts, SessionRuntimeServices,
        session::SessionCreateParams,
        session_runtime::SessionRuntimeState,
        test_support::{NoopContextAssembler, test_effective_config},
    };

    struct SwitchingRuntime {
        state: Arc<SwitchingState>,
    }

    struct TaggedLlm {
        max_input_tokens: usize,
    }

    #[async_trait::async_trait]
    impl astrcode_core::llm::LlmProvider for TaggedLlm {
        async fn generate_request(
            &self,
            _request: astrcode_core::llm::LlmRequest,
        ) -> Result<
            tokio::sync::mpsc::UnboundedReceiver<astrcode_core::llm::LlmEvent>,
            astrcode_core::llm::LlmError,
        > {
            unreachable!("the lifecycle probe stops before the provider stage")
        }

        fn model_limits(&self) -> astrcode_core::llm::ModelLimits {
            astrcode_core::llm::ModelLimits {
                max_input_tokens: self.max_input_tokens,
                max_output_tokens: 1_024,
            }
        }
    }

    struct SwitchingState {
        current_generation: AtomicU64,
        runtime_services: OnceLock<Arc<SessionRuntimeServices>>,
        runtime_published: AtomicBool,
        calls: Mutex<Vec<String>>,
        hook_calls: Mutex<Vec<RecordedHookCall>>,
        model_calls: Mutex<Vec<String>>,
    }

    struct RecordedHookCall {
        operation: String,
        turn_id: Option<String>,
        cancellation: CancellationToken,
    }

    struct TaggedRuntimeView {
        generation: u64,
        state: Arc<SwitchingState>,
    }

    impl TaggedRuntimeView {
        fn record(&self, operation: &str) {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("{}:{operation}", self.generation));
        }

        fn record_hook(&self, operation: &str, call: &RuntimeHookCallContext) {
            self.record(operation);
            match (call.turn_id(), call.llm_providers()) {
                (Some(_), Some(providers)) => {
                    self.state.model_calls.lock().unwrap().push(format!(
                        "{operation}:{}:{}",
                        providers.main().model_limits().max_input_tokens,
                        providers.small().model_limits().max_input_tokens,
                    ));
                },
                (None, None) => {},
                _ => panic!("model providers and turn identity must be scoped together"),
            }
            self.state
                .hook_calls
                .lock()
                .unwrap()
                .push(RecordedHookCall {
                    operation: operation.to_owned(),
                    turn_id: call.turn_id().map(str::to_owned),
                    cancellation: call.cancellation().clone(),
                });
        }
    }

    #[async_trait::async_trait]
    impl ToolCatalogProvider for TaggedRuntimeView {
        fn revision(&self) -> u64 {
            self.generation
        }

        async fn tool_catalog(
            &self,
            _scope: &ToolCatalogScope,
        ) -> Result<ToolCatalogSnapshot, ExtensionError> {
            self.record("tool_catalog");
            Ok(ToolCatalogSnapshot::complete(self.generation, Vec::new()))
        }
    }

    #[async_trait::async_trait]
    impl PromptContributor for TaggedRuntimeView {
        async fn collect_prompt_contributions(
            &self,
            ctx: RuntimePromptBuildContext,
        ) -> Result<PromptContributions, ExtensionError> {
            self.record_hook("prompt", ctx.call());
            Ok(PromptContributions::default())
        }
    }

    #[async_trait::async_trait]
    impl TurnHooks for TaggedRuntimeView {
        async fn emit_user_message_envelope(
            &self,
            ctx: RuntimeUserMessageEnvelopeContext,
        ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
            self.record_hook("envelope", ctx.call());
            self.state.current_generation.store(2, Ordering::Release);
            if !self.state.runtime_published.swap(true, Ordering::AcqRel) {
                let runtime_services = self.state.runtime_services.get().unwrap();
                runtime_services.publish_runtime_generation(
                    runtime_services.read_effective().as_ref().clone(),
                    Arc::new(TaggedLlm {
                        max_input_tokens: 3,
                    }),
                    Arc::new(TaggedLlm {
                        max_input_tokens: 4,
                    }),
                );
            }
            Ok(UserMessageEnvelopeResult::Allow)
        }

        async fn emit_lifecycle(
            &self,
            event: LifecycleEvent,
            ctx: RuntimeLifecycleContext,
        ) -> Result<(), ExtensionError> {
            self.record_hook(
                match event {
                    LifecycleEvent::TurnStart => "turn_start",
                    LifecycleEvent::UserPromptSubmit => "prompt_submit",
                    LifecycleEvent::TurnEnd => "turn_end",
                    _ => "other_lifecycle",
                },
                ctx.call(),
            );
            if event == LifecycleEvent::TurnStart {
                Err(ExtensionError::Internal("stop before provider call".into()))
            } else {
                Ok(())
            }
        }
    }

    impl TurnExtensionViewProvider for SwitchingRuntime {
        fn turn_extension_view(&self) -> TurnExtensionView {
            let generation = self.state.current_generation.load(Ordering::Acquire);
            let view = Arc::new(TaggedRuntimeView {
                generation,
                state: Arc::clone(&self.state),
            });
            let tool_catalog: Arc<dyn ToolCatalogProvider> = view.clone();
            let prompt_contributor: Arc<dyn PromptContributor> = view.clone();
            let turn_hooks: Arc<dyn TurnHooks> = view;
            TurnExtensionView::new(generation, tool_catalog, prompt_contributor, turn_hooks)
        }
    }

    impl RuntimeSnapshotProvider for SwitchingRuntime {
        fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
            RuntimeSnapshotState::Stable(self.state.current_generation.load(Ordering::Acquire))
        }
    }

    impl SessionOperationsProvider for SwitchingRuntime {}

    #[tokio::test]
    async fn turn_pins_extension_and_model_generations_before_first_hook() {
        let state = Arc::new(SwitchingState {
            current_generation: AtomicU64::new(9),
            runtime_services: OnceLock::new(),
            runtime_published: AtomicBool::new(false),
            calls: Mutex::new(Vec::new()),
            hook_calls: Mutex::new(Vec::new()),
            model_calls: Mutex::new(Vec::new()),
        });
        let extension_runtime = Arc::new(SwitchingRuntime {
            state: Arc::clone(&state),
        });
        let main_llm: Arc<dyn astrcode_core::llm::LlmProvider> = Arc::new(TaggedLlm {
            max_input_tokens: 1,
        });
        let small_llm: Arc<dyn astrcode_core::llm::LlmProvider> = Arc::new(TaggedLlm {
            max_input_tokens: 2,
        });
        let runtime_services = Arc::new(SessionRuntimeServices::new_with_context_assembler(
            main_llm,
            small_llm,
            test_effective_config(astrcode_core::config::ContextSettings::default()),
            SessionExtensionPorts::from_adapter(extension_runtime),
            Arc::new(NoopContextAssembler::new(
                astrcode_core::config::ContextSettings::default(),
            )),
            Arc::new(astrcode_context::NoopPostCompactEnricher),
        ));
        state
            .runtime_services
            .set(Arc::clone(&runtime_services))
            .unwrap_or_else(|_| panic!("runtime services already set"));
        let session_id = new_session_id();
        let store: Arc<dyn astrcode_storage::SessionStore> = Arc::new(InMemoryEventStore::new());
        let runtime = Arc::new(SessionRuntimeState::new(session_id, store));
        let session = Session::create_with_params(SessionCreateParams {
            working_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            model_id: "mock-model".into(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            extra_system_prompt: None,
            initial_system_prompt: None,
            runtime,
            runtime_services,
        })
        .await
        .unwrap();

        state.calls.lock().unwrap().clear();
        state.hook_calls.lock().unwrap().clear();
        state.model_calls.lock().unwrap().clear();
        state.current_generation.store(1, Ordering::Release);
        let turn_id = new_turn_id();
        let handle = session
            .submit(
                UserInput::text_only("pin generation"),
                turn_id.clone(),
                None,
            )
            .await
            .unwrap();
        let result = handle.wait().await.unwrap();
        assert!(result.output.is_err());
        assert_eq!(state.current_generation.load(Ordering::Acquire), 2);

        let calls = state.calls.lock().unwrap().clone();
        for expected in [
            "1:envelope",
            "1:tool_catalog",
            "1:prompt",
            "1:turn_start",
            "1:prompt_submit",
            "1:turn_end",
        ] {
            assert!(
                calls.iter().any(|call| call == expected),
                "calls: {calls:?}"
            );
        }
        assert!(
            calls.iter().all(|call| call.starts_with("1:")),
            "turn mixed extension generations: {calls:?}"
        );
        let model_calls = state.model_calls.lock().unwrap().clone();
        assert!(
            model_calls.iter().all(|call| call.ends_with(":1:2")),
            "turn mixed core model generations: {model_calls:?}"
        );

        {
            let hook_calls = state.hook_calls.lock().unwrap();
            for operation in [
                "envelope",
                "prompt",
                "turn_start",
                "prompt_submit",
                "turn_end",
            ] {
                let call = hook_calls
                    .iter()
                    .find(|call| call.operation == operation)
                    .unwrap_or_else(|| panic!("missing {operation} hook context"));
                assert_eq!(call.turn_id.as_deref(), Some(turn_id.as_str()));
                assert!(
                    call.cancellation.is_cancelled(),
                    "{operation} did not observe turn cancellation"
                );
            }
        }

        state.calls.lock().unwrap().clear();
        state.hook_calls.lock().unwrap().clear();
        state.model_calls.lock().unwrap().clear();
        let next_turn_id = new_turn_id();
        let next = session
            .submit(
                UserInput::text_only("use published generation"),
                next_turn_id,
                None,
            )
            .await
            .unwrap();
        assert!(next.wait().await.unwrap().output.is_err());

        let calls = state.calls.lock().unwrap().clone();
        assert!(
            calls.iter().all(|call| call.starts_with("2:")),
            "new turn did not use the published extension generation: {calls:?}"
        );
        let model_calls = state.model_calls.lock().unwrap().clone();
        assert!(
            model_calls.iter().all(|call| call.ends_with(":3:4")),
            "new turn did not use the published model generation: {model_calls:?}"
        );
    }
}
