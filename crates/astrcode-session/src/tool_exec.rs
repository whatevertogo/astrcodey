//! 工具调用执行实现。

use std::{sync::Arc, time::Instant};

use astrcode_core::{
    storage::ToolResultArtifactReader,
    tool::{
        FileObservation, FileObservationStore, LlmModelIds, ToolCapabilities, ToolDefinition,
        ToolError, ToolExecutionContext, ToolResult,
    },
};
use astrcode_tools::registry::ToolRegistry;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::{
    deferred_tools::suggest_tool_alias, session::Session, tool_types::ExecutableToolCall,
    turn_publish::TurnEvents,
};

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
        session_state: &astrcode_core::storage::SessionReadModel,
        session_store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let shared = crate::turn_context::SharedTurnContext {
            session_id: session.id().clone(),
            working_dir: session_state.working_dir.clone(),
            model_id: session_state.model_id.clone(),
            session_store_dir: session_store_dir.clone(),
            turn_event_tx: None,
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
        let caps = session.caps();
        let effective = caps.read_effective();
        let main_model_id = shared.model_id.clone();
        let small_model_id = effective.small_llm.model_id.clone();
        Self {
            file_observation_store: Some(runtime.file_observation_store()),
            session_ops: caps.session_ops(),
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

pub(crate) struct ToolCallRuntimeContext {
    pub turn: TurnToolContext,
    pub tools: Vec<ToolDefinition>,
    pub tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    pub publisher: Arc<TurnEvents>,
    pub cancellation_token: CancellationToken,
}

fn error_tool_result(
    call_id: String,
    tool_name: &str,
    err: ToolError,
    duration: std::time::Duration,
) -> ToolResult {
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

    ToolResult {
        call_id,
        content: llm_visible.clone(),
        is_error: true,
        error: Some(llm_visible),
        metadata,
        duration_ms: Some(duration.as_millis() as u64),
    }
}

/// 工具在执行完成前被中断（取消、abort、协议修复）时的统一错误结果。
pub fn interrupted_tool_result(
    call_id: String,
    tool_name: &str,
    duration: std::time::Duration,
) -> ToolResult {
    error_tool_result(
        call_id,
        tool_name,
        ToolError::Execution("tool execution interrupted before completion".into()),
        duration,
    )
}

/// 执行单个工具调用，并把异常统一转成工具错误结果。
pub async fn execute_tool_call(
    tool_registry: Arc<ToolRegistry>,
    runtime: ToolCallRuntimeContext,
    call: ExecutableToolCall,
) -> (usize, ToolResult) {
    if runtime.cancellation_token.is_cancelled() {
        return (
            call.index,
            interrupted_tool_result(call.call_id.clone(), &call.name, std::time::Duration::ZERO),
        );
    }
    execute_tool_call_blocking(tool_registry, runtime, call).await
}

use crate::turn_publish::spawn_event_bridge;

fn tool_capabilities_from_runtime(runtime: &ToolCallRuntimeContext) -> ToolCapabilities {
    let capabilities = &runtime.turn.capabilities;
    ToolCapabilities {
        model_id: Some(runtime.turn.shared.model_id.clone()),
        main_model_id: capabilities.main_model_id.clone(),
        small_model_id: capabilities.small_model_id.clone(),
        llm_models: capabilities.llm_models.clone(),
        session_store_dir: capabilities.session_store_dir.clone(),
        available_tools: Some(runtime.tools.clone()),
        tool_result_reader: runtime.tool_result_reader.clone(),
        file_observation_store: capabilities.file_observation_store.clone(),
        session_ops: capabilities.session_ops.clone(),
        extension_event_sink: None,
    }
}

/// 普通的阻塞式工具执行（原有逻辑）。
async fn execute_tool_call_blocking(
    tool_registry: Arc<ToolRegistry>,
    runtime: ToolCallRuntimeContext,
    call: ExecutableToolCall,
) -> (usize, ToolResult) {
    let started_at = Instant::now();
    let tool_name = call.name;
    let call_id = call.call_id.clone();
    let capabilities = tool_capabilities_from_runtime(&runtime);
    let tool_event_bridge = Some(spawn_event_bridge(runtime.publisher));
    let tool_event_tx = tool_event_bridge
        .as_ref()
        .map(|(tool_tx, _)| tool_tx.clone());
    let tool_ctx = ToolExecutionContext {
        session_id: runtime.turn.shared.session_id.clone(),
        working_dir: runtime.turn.shared.working_dir.clone(),
        tool_call_id: Some(call.call_id.clone()),
        event_tx: tool_event_tx,
        capabilities,
    };

    let result = match tokio::select! {
        _ = runtime.cancellation_token.cancelled() => {
            Err(ToolError::Execution("tool execution interrupted before completion".into()))
        },
        result = tool_registry.execute(&tool_name, call.tool_input, &tool_ctx) => result,
    } {
        Ok(mut result) => {
            result.call_id = call.call_id.clone();
            result.duration_ms = Some(started_at.elapsed().as_millis() as u64);
            result
        },
        Err(e) => error_tool_result(call.call_id.clone(), &tool_name, e, started_at.elapsed()),
    };
    // Release the tool-side sender before awaiting the bridge; otherwise the
    // bridge keeps waiting for more tool progress events and this call hangs.
    drop(tool_ctx);
    if let Some((tool_tx, bridge)) = tool_event_bridge {
        drop(tool_tx);
        if let Err(e) = bridge.await {
            tracing::error!(tool_name, call_id, panic = %e, "event bridge task panicked");
        }
    }

    if result.is_error {
        tracing::warn!(
            tool_name,
            call_id,
            duration_ms = result.duration_ms.unwrap_or_default(),
            error = result.error.as_deref().unwrap_or("unknown error"),
            "tool execution completed with error"
        );
    } else {
        tracing::debug!(
            tool_name,
            call_id,
            duration_ms = result.duration_ms.unwrap_or_default(),
            "tool execution completed"
        );
    }

    (call.index, result)
}

// ─── File observation store ──────────────────────────────────────────────────

/// 进程内文件观察存储，用于 read/edit 工具的 read-before-edit 守卫。
///
/// 以规范化路径为 key 记录最近一次 `read` 或成功 `edit` 后的文件快照。
/// 生命周期与 session 一致（由 `TurnRunner` 创建，随 `TurnRunner` 销毁）。
#[derive(Default)]
pub struct InMemoryFileObservationStore {
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
    fn error_tool_result_not_found() {
        let result = error_tool_result(
            "call-1".into(),
            "my_tool",
            ToolError::NotFound("missing".into()),
            std::time::Duration::from_millis(50),
        );
        assert_eq!(result.call_id, "call-1");
        assert!(result.is_error);
        assert!(result.content.contains("missing"));
        assert!(result.content.contains("Suggestion"));
    }

    #[test]
    fn error_tool_result_not_found_suggests_glob_for_legacy_find() {
        let result = error_tool_result(
            "call-2".into(),
            "find",
            ToolError::NotFound("find".into()),
            std::time::Duration::from_millis(10),
        );
        assert!(result.content.contains("glob"));
    }

    #[test]
    fn error_tool_result_timeout_includes_ms() {
        let result = error_tool_result(
            "call-2".into(),
            "shell",
            ToolError::Timeout(5000),
            std::time::Duration::from_millis(5000),
        );
        assert!(result.content.contains("5000ms"));
        assert_eq!(result.metadata["timeoutMs"], serde_json::json!(5000));
    }

    #[test]
    fn error_tool_result_blocked() {
        let result = error_tool_result(
            "call-3".into(),
            "shell",
            ToolError::Blocked {
                reason: "policy reason".into(),
            },
            std::time::Duration::from_millis(10),
        );
        assert!(result.content.contains("blocked"));
        assert!(result.content.contains("policy reason"));
    }
}
