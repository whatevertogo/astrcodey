use std::sync::Arc;

use astrcode_core::{
    event::{Event, EventPayload},
    extension::ChildToolPolicy,
    types::*,
};

use super::{Session, SessionCreateParams};
use crate::session_runtime::SessionRuntimeState;

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_child(
        &self,
        working_dir: &str,
        model_id: &str,
        agent_name: String,
        task: String,
        extra_system_prompt: Option<String>,
        tool_policy: Option<ChildToolPolicy>,
        source_extension: Option<&str>,
        tool_call_id: ToolCallId,
    ) -> Result<Self, super::SessionError> {
        let primary_llm = primary_llm_for_model_id(&self.caps, model_id);
        let child_runtime = Arc::new(SessionRuntimeState::new(
            primary_llm,
            self.caps.small_llm(),
            model_id.to_string(),
        ));
        if extra_system_prompt.is_some() {
            child_runtime.set_extra_system_prompt(extra_system_prompt);
        }
        let parent_working_dir = self.read_model().await?.working_dir;
        let parent_registry = self.runtime.tool_registry();
        if parent_working_dir == working_dir && !parent_registry.list_definitions().is_empty() {
            let child_registry = parent_registry.clone_with_child_policy(tool_policy.as_ref());
            child_runtime.set_tool_registry(Arc::new(child_registry));
        }
        let child_sid = new_session_id();
        let child = Session::create_with_params(SessionCreateParams {
            store: Arc::clone(&self.store),
            sid: child_sid.clone(),
            working_dir: working_dir.to_string(),
            model_id: model_id.to_string(),
            parent: Some(self.id.clone()),
            tool_policy: tool_policy.clone(),
            source_extension: source_extension.map(str::to_string),
            runtime: child_runtime,
            caps: Arc::clone(&self.caps),
        })
        .await?;

        self.append_event(Event::new(
            self.id.clone(),
            None,
            EventPayload::AgentSessionSpawned {
                child_session_id: child_sid,
                agent_name,
                task,
                tool_policy,
                tool_call_id,
            },
        ))
        .await?;
        Ok(child)
    }

    /// 消费已完成子 turn 的信号并返回已完成的 guards。
    ///
    /// 终态事件已由 `ChildTurnGuard` 后台任务写入；本方法先 drain runtime 上的
    /// 完成通知 channel（丢弃积压 signal，避免重复处理），再收集已完成的 guard。
    pub fn drain_completed_guards(&self) -> Vec<Arc<crate::child_turn::ChildTurnGuard>> {
        self.runtime.drain_completed()
    }
}

/// 子 session 的 turn 使用 `SessionModelBinding.llm`；当目标 model_id 为小模型时选用 small
/// provider。
fn primary_llm_for_model_id(
    caps: &crate::session_runtime_services::SessionRuntimeServices,
    model_id: &str,
) -> std::sync::Arc<dyn astrcode_core::llm::LlmProvider> {
    let effective = caps.read_effective();
    if model_id == effective.small_llm.model_id && model_id != effective.llm.model_id {
        caps.small_llm()
    } else {
        caps.llm()
    }
}
