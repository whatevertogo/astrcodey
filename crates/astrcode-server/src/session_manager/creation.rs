//! Root session creation and initial model selection.

use std::{panic::AssertUnwindSafe, sync::Arc};

use astrcode_core::{
    config::EffectiveConfig,
    tool::{CreateRootSessionRequest, SessionToolSelection},
    types::SessionId,
};
use astrcode_extension_sdk::extension::LifecycleEvent;
use astrcode_session::{Session, SessionCreateParams, SessionError};
use futures_util::FutureExt;

use super::{SessionManager, SessionManagerError};

/// 校验扩展提供的 root 模型偏好。
///
/// `"inherit"`/空串视为未指定(与子会话路径 `spawn_child` 的过滤一致);
/// 其余值必须命中运行时实际可切换的模型集合。运行时只有主/小两个
/// provider 实例,`llm_for_model_id` 对任何其他值都静默回退主 provider——
/// 后台无人值守的 root 会把 typo 变成静默错模型,因此在创建边界显式拒绝。
fn validated_root_model_preference(
    preference: Option<String>,
    effective: &EffectiveConfig,
) -> Result<Option<String>, SessionManagerError> {
    let Some(model_id) = preference.filter(|id| !id.is_empty() && id != "inherit") else {
        return Ok(None);
    };
    let candidates = [
        effective.llm.model_id.as_str(),
        effective.small_llm.model_id.as_str(),
    ];
    if candidates.contains(&model_id.as_str()) {
        return Ok(Some(model_id));
    }
    Err(SessionManagerError::InvalidRequest(format!(
        "unknown model_preference {model_id:?}; available models: {}",
        candidates
            .into_iter()
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

impl SessionManager {
    #[cfg(test)]
    pub(crate) async fn create(&self, working_dir: &str) -> Result<Session, SessionManagerError> {
        self.create_with_tool_selection(working_dir, None).await
    }

    pub(crate) async fn create_with_tool_selection(
        &self,
        working_dir: &str,
        tool_selection: Option<&SessionToolSelection>,
    ) -> Result<Session, SessionManagerError> {
        self.create_root_with_options(working_dir, tool_selection, None, None, None)
            .await
    }

    /// 创建扩展持有的顶层会话(宿主入口:`SessionOperations::create_root_session`)。
    pub(crate) async fn create_for_extension(
        &self,
        request: CreateRootSessionRequest,
    ) -> Result<Session, SessionManagerError> {
        let model_preference = validated_root_model_preference(
            request.model_preference,
            &self.runtime_services.read_effective(),
        )?;
        self.create_root_with_options(
            &request.working_dir,
            request.tool_selection.as_ref(),
            request.source_extension,
            model_preference,
            request.system_prompt,
        )
        .await
    }

    async fn create_root_with_options(
        &self,
        working_dir: &str,
        tool_selection: Option<&SessionToolSelection>,
        source_extension: Option<String>,
        model_preference: Option<String>,
        extra_system_prompt: Option<String>,
    ) -> Result<Session, SessionManagerError> {
        let manager = self.clone();
        let working_dir = working_dir.to_owned();
        let tool_selection = tool_selection.cloned();
        let task = self.spawn_creation_task(async move {
            let sid = astrcode_core::types::new_session_id();
            match AssertUnwindSafe(manager.create_root_transaction(
                sid.clone(),
                working_dir,
                tool_selection,
                source_extension,
                model_preference,
                extra_system_prompt,
            ))
            .catch_unwind()
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    manager
                        .compensate_panicked_creation(&sid, "root session")
                        .await;
                    Err(SessionManagerError::CreationTask(
                        "root session creation transaction panicked".into(),
                    ))
                },
            }
        })?;
        task.await.map_err(|error| {
            SessionManagerError::CreationTask(format!(
                "root session creation transaction stopped: {error}"
            ))
        })?
    }

    async fn create_root_transaction(
        &self,
        sid: SessionId,
        working_dir: String,
        tool_selection: Option<SessionToolSelection>,
        source_extension: Option<String>,
        model_preference: Option<String>,
        extra_system_prompt: Option<String>,
    ) -> Result<Session, SessionManagerError> {
        let runtime = self.runtime_for(&sid);
        let creation = runtime.begin_creation();
        let publication = self
            .event_sink
            .defer_publication(sid.clone())
            .map_err(SessionError::from)?;
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir,
            model_id: model_preference
                .unwrap_or_else(|| self.runtime_services.read_effective().llm.model_id.clone()),
            parent_session_id: None,
            tool_selection,
            source_extension,
            extra_system_prompt,
            initial_system_prompt: None,
            runtime,
            runtime_services: Arc::clone(&self.runtime_services),
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                if matches!(&error, SessionError::EventPublish(_)) {
                    if let Err(compensation_error) = self.discard_failed_creation(&sid).await {
                        tracing::warn!(
                            session_id = %sid,
                            error = %error,
                            compensation_error = %compensation_error,
                            "failed to fully compensate root session creation"
                        );
                    }
                } else {
                    self.runtime_services.session_resources().cleanup(&sid);
                }
                return Err(error.into());
            },
        };

        if let Err(error) = session
            .ensure_lifecycle_initialized(LifecycleEvent::SessionStart)
            .await
        {
            if let Err(compensation_error) = self.discard_failed_lifecycle_start(&session).await {
                tracing::warn!(
                    session_id = %sid,
                    error = %error,
                    compensation_error = %compensation_error,
                    "failed to fully compensate root session creation"
                );
            }
            return Err(error.into());
        }

        if let Err(error) = self.sync_durable_events_required(&sid).await {
            if let Err(compensation_error) = self.discard_failed_lifecycle_start(&session).await {
                tracing::warn!(
                    session_id = %sid,
                    error = %error,
                    compensation_error = %compensation_error,
                    "failed to fully compensate root session creation"
                );
            }
            return Err(error);
        }

        creation.commit();
        publication.commit();
        Ok(session)
    }
}
