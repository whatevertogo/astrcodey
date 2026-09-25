//! Fork preparation and compensation before publishing the new session.

use std::{panic::AssertUnwindSafe, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, PersistedSystemPrompt},
    llm::TranscriptMessage,
    types::{Cursor, SessionId},
};
use astrcode_extension_sdk::extension::LifecycleEvent;
use astrcode_session::{Session, SessionCreateParams, SessionError};
use futures_util::FutureExt;

use super::{SessionManager, SessionManagerError};

struct ForkCreationInput {
    source_id: SessionId,
    session_id: SessionId,
    working_dir: String,
    model_id: String,
    initial_system_prompt: PersistedSystemPrompt,
    source_cursor: Cursor,
    first_user_message: Option<String>,
    messages: Vec<TranscriptMessage>,
    source_extension: Option<String>,
}

#[derive(Clone, Copy)]
enum FailedForkCreationStage {
    Persisted,
    LifecycleStartFailed,
}

impl SessionManager {
    /// Fork 一个已有会话，创建新 session 并复制 fork 点之前的消息前缀。
    ///
    /// fork 保证新 session 发送给 LLM 的 system prompt + 消息前缀与源 session 完全一致，
    /// 从而让 provider 侧的 KV 缓存（prompt cache）自动命中。
    ///
    /// - `source_id`: 源会话 ID
    /// - `at_cursor`: 可选 fork 点 cursor（event seq 的十进制字符串），为 None 则从末尾 fork
    ///
    /// 返回新 session 及其初始事件。
    pub(crate) async fn fork(
        &self,
        source_id: &SessionId,
        at_cursor: Option<&Cursor>,
        source_extension: Option<&str>,
    ) -> Result<Session, SessionManagerError> {
        let source_model = self.event_store.session_read_model(source_id).await?;

        let fork_cursor = at_cursor.cloned().unwrap_or_else(|| source_model.cursor());

        let (transcript_messages, first_user_message) = if at_cursor.is_some() {
            let events = self.event_store.replay_events(source_id).await?;
            let truncated_seq: u64 = fork_cursor
                .parse()
                .map_err(|_| SessionManagerError::InvalidCursor(fork_cursor.clone()))?;
            let truncated_events: Vec<_> = events
                .into_iter()
                .filter(|event| event.seq <= truncated_seq)
                .collect();
            let truncated_model =
                astrcode_session_projection::replay(source_id.clone(), &truncated_events)?;
            let first_user_message = truncated_model.first_user_message().map(str::to_owned);
            (truncated_model.model_context.messages, first_user_message)
        } else {
            (
                source_model.model_context.messages.clone(),
                source_model.first_user_message().map(str::to_owned),
            )
        };

        let input = ForkCreationInput {
            source_id: source_id.clone(),
            session_id: astrcode_core::types::new_session_id(),
            working_dir: source_model.identity.working_dir.clone(),
            model_id: source_model.identity.model_id.clone(),
            initial_system_prompt: PersistedSystemPrompt {
                text: source_model.system_prompt.text.clone(),
                fingerprint: source_model.system_prompt.fingerprint.clone(),
                extra_system_prompt: source_model.system_prompt.extra.clone(),
                source: astrcode_core::event::SystemPromptSource::Inherited,
            },
            source_cursor: fork_cursor,
            first_user_message,
            messages: transcript_messages
                .into_iter()
                .map(|entry| TranscriptMessage {
                    message: Arc::unwrap_or_clone(entry.message),
                    origin: entry.origin,
                })
                .collect(),
            source_extension: source_extension.map(str::to_owned),
        };
        let new_sid = input.session_id.clone();
        let manager = self.clone();
        let task = self.spawn_creation_task(async move {
            match AssertUnwindSafe(manager.create_fork_transaction(input))
                .catch_unwind()
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    manager
                        .compensate_panicked_creation(&new_sid, "fork session")
                        .await;
                    Err(SessionManagerError::CreationTask(
                        "fork session creation transaction panicked".into(),
                    ))
                },
            }
        })?;
        task.await.map_err(|error| {
            SessionManagerError::CreationTask(format!(
                "fork session creation transaction stopped: {error}"
            ))
        })?
    }

    async fn create_fork_transaction(
        &self,
        input: ForkCreationInput,
    ) -> Result<Session, SessionManagerError> {
        let ForkCreationInput {
            source_id,
            session_id: new_sid,
            working_dir,
            model_id,
            initial_system_prompt,
            source_cursor,
            first_user_message,
            messages,
            source_extension,
        } = input;
        let runtime = self.runtime_for(&new_sid);
        let creation = runtime.begin_creation();
        let publication = self
            .event_sink
            .defer_publication(new_sid.clone())
            .map_err(SessionError::from)?;
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir,
            model_id,
            parent_session_id: None,
            tool_selection: None,
            source_extension,
            extra_system_prompt: None,
            initial_system_prompt: Some(initial_system_prompt),
            runtime,
            runtime_services: Arc::clone(&self.runtime_services),
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                if matches!(&error, SessionError::EventPublish(_)) {
                    if let Err(compensation_error) = self.discard_failed_creation(&new_sid).await {
                        tracing::warn!(
                            source_session_id = %source_id,
                            fork_session_id = %new_sid,
                            error = %error,
                            compensation_error = %compensation_error,
                            "failed to fully compensate fork session creation"
                        );
                    }
                } else {
                    self.runtime_services.session_resources().cleanup(&new_sid);
                }
                return Err(error.into());
            },
        };

        if let Err(error) = session
            .emit_durable(
                None,
                DurableEventPayload::SessionForked {
                    source_session_id: source_id.clone(),
                    source_cursor,
                    first_user_message,
                    messages,
                },
            )
            .await
        {
            self.compensate_failed_fork_creation(
                &source_id,
                &session,
                &error,
                FailedForkCreationStage::Persisted,
            )
            .await;
            return Err(error.into());
        }

        if let Err(error) = session
            .ensure_lifecycle_initialized(LifecycleEvent::SessionStart)
            .await
        {
            self.compensate_failed_fork_creation(
                &source_id,
                &session,
                &error,
                FailedForkCreationStage::LifecycleStartFailed,
            )
            .await;
            return Err(error.into());
        }

        if let Err(error) = self.sync_durable_events_required(&new_sid).await {
            if let Err(compensation_error) = self.discard_failed_lifecycle_start(&session).await {
                tracing::warn!(
                    source_session_id = %source_id,
                    fork_session_id = %new_sid,
                    error = %error,
                    compensation_error = %compensation_error,
                    "failed to fully compensate fork session creation"
                );
            }
            return Err(error);
        }

        creation.commit();
        publication.commit();
        Ok(session)
    }

    async fn compensate_failed_fork_creation(
        &self,
        source_session_id: &SessionId,
        fork: &Session,
        cause: &SessionError,
        stage: FailedForkCreationStage,
    ) {
        let compensation_result = match stage {
            FailedForkCreationStage::Persisted => self
                .discard_failed_creation(fork.id())
                .await
                .map_err(|error| format!("discard fork session: {error}")),
            FailedForkCreationStage::LifecycleStartFailed => {
                self.discard_failed_lifecycle_start(fork).await
            },
        };
        if let Err(compensation_error) = compensation_result {
            tracing::warn!(
                source_session_id = %source_session_id,
                fork_session_id = %fork.id(),
                error = %cause,
                compensation_error = %compensation_error,
                "failed to fully compensate fork session creation"
            );
        }
    }
}
