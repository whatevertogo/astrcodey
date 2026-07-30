//! 工具调用执行实现。

use std::{sync::Arc, time::Instant};

use astrcode_core::tool::{
    FileObservation, FileObservationStore, LlmModelIds, ToolCapabilities, ToolDefinition,
    ToolError, ToolExecutionContext, ToolResultArtifactReader,
};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::{
    deferred_tools::suggest_tool_alias,
    session::Session,
    tool_types::{ExecutableToolInvocation, ToolExecutionOutcome, ToolResultCommit},
};
use crate::ToolRegistry;

// ─── Runtime context types ──────────────────────────────────────────────

/// Turn 级工具上下文：hook 共享字段 + session 基础设施能力。
#[derive(Clone)]
pub(crate) struct TurnToolContext {
    pub shared: crate::turn_context::SharedTurnContext,
    pub capabilities: ToolRuntimeCapabilities,
}

impl TurnToolContext {
    pub(crate) fn for_turn(
        session: &Session,
        session_state: &astrcode_session_projection::SessionReadModel,
        tool_selection: astrcode_core::tool::SessionToolSelection,
        session_store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let runtime_services = session.runtime_services();
        let effective = runtime_services.read_effective();
        let approval_history = session.runtime().approval_history();
        if let Some(dir) = session_store_dir.as_deref() {
            let path = crate::permission::approval_history_path(dir);
            if path.exists() {
                approval_history
                    .replace_from(&crate::permission::ApprovalHistoryStore::load_from(&path));
            }
        }
        let permission_chain =
            crate::permission::build_default_chain(&effective, Arc::clone(&approval_history));
        let shared = crate::turn_context::SharedTurnContext {
            session_id: session.id().clone(),
            working_dir: session_state.identity.working_dir.clone(),
            model_id: session_state.identity.model_id.clone(),
            session_store_dir,
            turn_event_sender: None,
            approval_mode: effective.agent.approval_mode,
            tool_selection: Some(tool_selection),
            permission_chain,
            approval_history,
        };
        let capabilities = ToolRuntimeCapabilities::from_session(session, &shared);
        Self {
            shared,
            capabilities,
        }
    }
}

/// 会话级工具运行时能力，从 [`TurnToolContext`] 透传到 [`ToolExecutionContext`]。
#[derive(Clone)]
pub(crate) struct ToolRuntimeCapabilities {
    /// 文件观察存储，用于 read/edit 协作的 read-before-edit 守卫。
    pub file_observation_store: Option<Arc<dyn FileObservationStore>>,
    /// 会话原子操作能力，供 agent 工具使用。
    pub session_ops: Option<Arc<dyn astrcode_core::tool::SessionOperations>>,
    /// 主模型 ID，供声明 `main_model` 的插件使用。
    pub main_model_id: Option<String>,
    /// 小模型 ID，供子 agent / 声明 `small_model` 的插件使用。
    pub small_model_id: Option<String>,
    /// 分档模型 id（注入 ToolCapabilities 前由 runner 按能力裁剪）。
    pub llm_models: LlmModelIds,
    /// session 在存储层的真实目录路径。
    pub session_store_dir: Option<std::path::PathBuf>,
}

impl ToolRuntimeCapabilities {
    fn from_session(session: &Session, shared: &crate::turn_context::SharedTurnContext) -> Self {
        let runtime = Arc::clone(&session.runtime);
        let runtime_services = session.runtime_services();
        let effective = runtime_services.read_effective();
        let main_model_id = shared.model_id.clone();
        let small_model_id = effective.small_llm.model_id.clone();
        Self {
            file_observation_store: Some(runtime.file_observation_store()),
            session_ops: runtime_services.session_ops(),
            small_model_id: Some(small_model_id.clone()),
            session_store_dir: shared.session_store_dir.clone(),
            main_model_id: Some(main_model_id.clone()),
            llm_models: LlmModelIds {
                main: Some(main_model_id),
                small: Some(small_model_id),
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolCallRuntimeContext {
    pub turn: TurnToolContext,
    pub tools: Arc<[ToolDefinition]>,
    pub tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    pub cancellation_token: CancellationToken,
}

fn tool_failure_outcome(
    tool_name: &str,
    err: ToolError,
    duration: std::time::Duration,
) -> ToolExecutionOutcome {
    use astrcode_core::tool::tool_metadata;

    let (message, suggestion): (String, String) = match &err {
        ToolError::NotFound(name) => {
            if let Some(alias) = suggest_tool_alias(name) {
                (
                    format!("Tool `{name}` not found."),
                    format!("Use `{alias}` instead (exact name from the provider tool list)."),
                )
            } else if name.starts_with("mcp__") {
                (
                    format!("Tool `{name}` not found."),
                    "Call `tool_search_tool` first to load the MCP tool schema, then retry with \
                     the exact `mcp__...` name from the search result."
                        .to_string(),
                )
            } else {
                (
                    format!("Tool `{name}` not found."),
                    "Use an exact tool name from the provider tool list. Match file paths with \
                     `glob` (`pattern` arg) and search contents with `grep`. For external MCP \
                     tools, call `tool_search_tool` first."
                        .to_string(),
                )
            }
        },
        ToolError::InvalidArguments(detail) => (
            format!("Invalid arguments for `{tool_name}`: {detail}"),
            "Re-read the parameter schema and retry with corrected arguments. Do not retry with \
             the same arguments."
                .to_string(),
        ),
        ToolError::Execution(detail) => (
            format!("`{tool_name}` failed: {detail}"),
            "Inspect the error above. Adjust arguments or pick a different approach. Do not retry \
             the identical call."
                .to_string(),
        ),
        ToolError::Blocked { reason } => (
            format!("`{tool_name}` was blocked: {reason}"),
            "A hook policy prevented this. Read the reason and adjust your approach instead of \
             retrying."
                .to_string(),
        ),
        ToolError::Timeout(ms) => (
            format!("`{tool_name}` timed out after {ms}ms."),
            "Retry with a smaller scope or a longer timeout if the command legitimately needs \
             more time."
                .to_string(),
        ),
    };

    // suggestion 拼接进 content,LLM 才能看到——单独放进 metadata 不会进 prompt。
    let llm_visible = format!("{message}\nSuggestion: {suggestion}");

    let mut metadata = tool_metadata([
        ("toolName", serde_json::json!(tool_name)),
        ("suggestion", serde_json::json!(suggestion)),
    ]);
    if let ToolError::Timeout(ms) = &err {
        metadata.insert("timeoutMs".into(), serde_json::json!(ms));
    }

    ToolExecutionOutcome::Failed {
        error: llm_visible,
        metadata,
        duration_ms: Some(duration.as_millis() as u64),
    }
}

/// 执行单个工具调用并保留完成、失败、取消三种终态。
pub(crate) async fn execute_tool_call(
    tool_registry: Arc<ToolRegistry>,
    runtime: ToolCallRuntimeContext,
    call: ExecutableToolInvocation,
) -> (usize, ToolExecutionOutcome) {
    if runtime.cancellation_token.is_cancelled() {
        return (
            call.index,
            ToolExecutionOutcome::cancelled("tool execution cancelled", Some(0)),
        );
    }
    execute_tool_call_blocking(tool_registry, runtime, call).await
}

fn tool_capabilities_from_runtime(
    turn: &TurnToolContext,
    tools: Arc<[astrcode_core::tool::ToolDefinition]>,
    tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
) -> ToolCapabilities {
    use astrcode_core::tool::{
        ToolFileServices, ToolHostServices, ToolModelAccess, ToolSessionControl, ToolSessionPaths,
    };

    let capabilities = &turn.capabilities;
    ToolCapabilities {
        models: ToolModelAccess {
            main: capabilities.main_model_id.clone(),
            small: capabilities.small_model_id.clone(),
            tiers: capabilities.llm_models.clone(),
        },
        paths: ToolSessionPaths {
            store_dir: capabilities.session_store_dir.clone(),
        },
        session: ToolSessionControl {
            ops: capabilities.session_ops.clone(),
        },
        files: ToolFileServices {
            observation_store: capabilities.file_observation_store.clone(),
        },
        host: ToolHostServices {
            result_reader: tool_result_reader,
            available_tools: Some(tools.as_ref().to_vec()),
        },
    }
}

/// 普通的阻塞式工具执行（原有逻辑）。
async fn execute_tool_call_blocking(
    tool_registry: Arc<ToolRegistry>,
    runtime: ToolCallRuntimeContext,
    call: ExecutableToolInvocation,
) -> (usize, ToolExecutionOutcome) {
    let started_at = Instant::now();
    let tool_name = call.name;
    let call_id = call.call_id;
    let ToolCallRuntimeContext {
        turn,
        tools,
        tool_result_reader,
        cancellation_token,
        ..
    } = runtime;
    let capabilities = tool_capabilities_from_runtime(&turn, tools, tool_result_reader);
    let tool_ctx = ToolExecutionContext::new(
        turn.shared.session_id.clone(),
        turn.shared.working_dir.clone(),
        Some(call_id.clone()),
        turn.shared.turn_event_tx(),
        capabilities,
    );

    let outcome = tokio::select! {
        _ = cancellation_token.cancelled() => ToolExecutionOutcome::cancelled(
            "tool execution cancelled",
            Some(started_at.elapsed().as_millis() as u64),
        ),
        result = tool_registry.execute(&tool_name, call.tool_input, &tool_ctx) => {
            match result {
                Ok(mut result) => {
                    result.result_mut().duration_ms =
                        Some(started_at.elapsed().as_millis() as u64);
                    ToolExecutionOutcome::Completed(ToolResultCommit::from_execution_result(result))
                },
                Err(error) => tool_failure_outcome(&tool_name, error, started_at.elapsed()),
            }
        },
    };
    drop(tool_ctx);
    if let Some(sender) = turn.shared.turn_event_sender.as_ref() {
        sender.flush().await;
    }

    match &outcome {
        ToolExecutionOutcome::Completed(result) if result.is_error => {
            tracing::warn!(
                tool_name,
                call_id,
                duration_ms = result.duration_ms.unwrap_or_default(),
                error = result.error.as_deref().unwrap_or("unknown error"),
                "tool execution completed with error result"
            );
        },
        ToolExecutionOutcome::Completed(result) => {
            tracing::debug!(
                tool_name,
                call_id,
                duration_ms = result.duration_ms.unwrap_or_default(),
                "tool execution completed"
            );
        },
        ToolExecutionOutcome::Failed {
            error, duration_ms, ..
        } => {
            tracing::warn!(
                tool_name,
                call_id,
                duration_ms = duration_ms.unwrap_or_default(),
                error,
                "tool execution failed"
            );
        },
        ToolExecutionOutcome::Cancelled {
            reason,
            duration_ms,
        } => {
            tracing::debug!(
                tool_name,
                call_id,
                duration_ms = duration_ms.unwrap_or_default(),
                reason,
                "tool execution cancelled"
            );
        },
    }

    (call.index, outcome)
}

// ─── File observation store ──────────────────────────────────────────────────

/// 进程内文件观察存储，用于 read/edit 工具的 read-before-edit 守卫。
///
/// 以规范化路径为 key 记录最近一次 `read` 或成功 `edit` 后的文件快照。
/// 生命周期与 session 一致（由 `TurnRunner` 创建，随 `TurnRunner` 销毁）。
#[derive(Default)]
pub(crate) struct InMemoryFileObservationStore {
    observations: Mutex<std::collections::HashMap<String, FileObservation>>,
}

impl FileObservationStore for InMemoryFileObservationStore {
    fn remember(&self, observation: FileObservation) {
        let mut map = self.observations.lock();
        map.insert(observation.path.clone(), observation);
    }

    fn load(&self, path: &str) -> Option<FileObservation> {
        let map = self.observations.lock();
        map.get(path).cloned()
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::tool::ToolError;

    use super::*;

    #[test]
    fn tool_failures_preserve_guidance_and_metadata() {
        let cases: [(&str, ToolError, &[&str], Option<u64>); 4] = [
            (
                "my_tool",
                ToolError::NotFound("missing".into()),
                &["missing", "Suggestion"],
                None,
            ),
            ("find", ToolError::NotFound("find".into()), &["glob"], None),
            ("shell", ToolError::Timeout(5000), &["5000ms"], Some(5000)),
            (
                "shell",
                ToolError::Blocked {
                    reason: "policy reason".into(),
                },
                &["blocked", "policy reason"],
                None,
            ),
        ];

        for (tool_name, tool_error, expected_fragments, expected_timeout_ms) in cases {
            let outcome =
                tool_failure_outcome(tool_name, tool_error, std::time::Duration::from_millis(50));
            let ToolExecutionOutcome::Failed {
                error, metadata, ..
            } = outcome
            else {
                panic!("tool errors must produce a failed outcome");
            };
            for fragment in expected_fragments {
                assert!(
                    error.contains(fragment),
                    "expected {error:?} to contain {fragment:?}"
                );
            }
            assert_eq!(
                metadata
                    .get("timeoutMs")
                    .and_then(serde_json::Value::as_u64),
                expected_timeout_ms
            );
        }
    }
}
