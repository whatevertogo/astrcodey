//! 工具调用执行实现。
//!
//! 包含阻塞式执行、带后台化能力的执行、以及后台 watcher 逻辑。

use std::{sync::Arc, time::Instant};

use astrcode_core::{
    event::EventPayload,
    storage::ToolResultArtifactReader,
    tool::{
        BackgroundPolicy, BackgroundTaskReader, FileObservation, FileObservationStore, LlmModelIds,
        ToolCapabilities, ToolDefinition, ToolError, ToolExecutionContext, ToolResult,
    },
    types::*,
};
use astrcode_tools::registry::ToolRegistry;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::{
    background::{
        BackgroundTaskCompletion, BackgroundTaskManager, backgrounded_placeholder_result,
    },
    session::Session,
    tool_types::ExecutableToolCall,
    turn_publish::TurnPublisher,
};

// ─── Runtime context types ──────────────────────────────────────────────

/// 会话级工具运行时能力，从 ToolPipeline 透传到 ToolExecutionContext。
///
/// 整合了后台任务、文件观察等按 session 生命周期存在的能力。
#[derive(Clone)]
pub(crate) struct ToolRuntimeCapabilities {
    /// 后台任务完成后的通知通道。
    pub background_result_tx: Option<mpsc::UnboundedSender<BackgroundTaskCompletion>>,
    /// 后台任务管理器，用于注册 watcher handle 以支持取消。
    pub background_tasks: Arc<parking_lot::Mutex<BackgroundTaskManager>>,
    /// 后台任务只读接口，注入到 ToolExecutionContext 供 TaskTool 使用。
    pub background_task_reader: Option<Arc<dyn BackgroundTaskReader>>,
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
    pub(crate) fn for_turn(
        session: &Session,
        _shared: &crate::turn_context::SharedTurnContext,
        background_result_tx: Option<mpsc::UnboundedSender<BackgroundTaskCompletion>>,
        session_store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let runtime = session.runtime();
        let caps = session.caps();
        let background_task_reader: Option<Arc<dyn BackgroundTaskReader>> = Some(Arc::new(
            crate::background::BackgroundTaskReaderImpl::new(runtime.background_tasks()),
        ));
        let effective = caps.read_effective();
        let main_model_id = effective.llm.model_id.clone();
        let small_model_id = effective.small_llm.model_id.clone();
        Self {
            background_result_tx,
            background_tasks: runtime.background_tasks(),
            background_task_reader,
            file_observation_store: Some(runtime.file_observation_store()),
            session_ops: caps.session_ops(),
            small_model_id: Some(small_model_id.clone()),
            session_store_dir,
            main_model_id: Some(main_model_id.clone()),
            llm_models: LlmModelIds {
                main: Some(main_model_id),
                small: Some(small_model_id),
            },
        }
    }
}

pub(crate) struct ToolCallRuntimeContext {
    pub session_id: SessionId,
    pub working_dir: String,
    pub tools: Vec<ToolDefinition>,
    pub tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    pub publisher: Arc<TurnPublisher>,
    pub capabilities: ToolRuntimeCapabilities,
}

fn error_tool_result(
    call_id: String,
    tool_name: &str,
    err: ToolError,
    duration: std::time::Duration,
) -> ToolResult {
    use astrcode_core::tool::tool_metadata;

    let (message, suggestion): (String, String) = match &err {
        ToolError::NotFound(name) => (
            format!("Tool `{name}` not found."),
            "Use `tool_search_tool` to discover available tools, or proceed without it."
                .to_string(),
        ),
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
            "The process may still be running in the background. Use `task` to inspect or cancel \
             it, or retry with a smaller scope."
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

/// 执行单个工具调用，并把异常统一转成工具错误结果。
///
/// 当工具声明了 [`BackgroundPolicy::AutoAfter`] 且执行超过阈值时，
/// 自动将任务转入后台执行，并返回一个占位结果让 LLM 继续推理。
///
/// 工具参数中的 `run_in_background` 字段可以覆盖策略：
/// - `true` → 立即后台化（阈值降为 0）
/// - `false` → 禁止自动后台化（视为 `Never`）
/// - 未设置 → 使用工具声明的默认策略
pub async fn execute_tool_call(
    tool_registry: Arc<ToolRegistry>,
    runtime: ToolCallRuntimeContext,
    mut call: ExecutableToolCall,
) -> (usize, ToolResult) {
    let policy = tool_registry.background_policy(&call.name);
    let effective_policy = resolve_effective_policy(policy, &call.tool_input);

    // run_in_background 是执行层元参数，不属于工具本身的入参。
    if let Some(obj) = call.tool_input.as_object_mut() {
        obj.remove("runInBackground");
    }

    match effective_policy {
        BackgroundPolicy::Never => execute_tool_call_blocking(tool_registry, runtime, call).await,
        BackgroundPolicy::AutoAfter { threshold_secs } => {
            execute_tool_call_with_background(tool_registry, runtime, call, threshold_secs).await
        },
    }
}

/// 根据工具声明的策略和每次调用的参数，决定实际的后台化策略。
fn resolve_effective_policy(
    declared: BackgroundPolicy,
    tool_input: &serde_json::Value,
) -> BackgroundPolicy {
    match tool_input.get("runInBackground").and_then(|v| v.as_bool()) {
        // 显式请求后台化：立即转入后台（阈值 0）
        Some(true) => BackgroundPolicy::AutoAfter { threshold_secs: 0 },
        // 显式禁止后台化：视为 Never
        Some(false) => BackgroundPolicy::Never,
        // 未设置：使用工具声明的策略
        None => declared,
    }
}

use crate::turn_publish::spawn_event_bridge;

fn tool_capabilities_from_runtime(runtime: &ToolCallRuntimeContext) -> ToolCapabilities {
    ToolCapabilities {
        main_model_id: runtime.capabilities.main_model_id.clone(),
        small_model_id: runtime.capabilities.small_model_id.clone(),
        llm_models: runtime.capabilities.llm_models.clone(),
        session_store_dir: runtime.capabilities.session_store_dir.clone(),
        available_tools: Some(runtime.tools.clone()),
        tool_result_reader: runtime.tool_result_reader.clone(),
        background_task_reader: runtime.capabilities.background_task_reader.clone(),
        file_observation_store: runtime.capabilities.file_observation_store.clone(),
        session_ops: runtime.capabilities.session_ops.clone(),
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
        session_id: runtime.session_id,
        working_dir: runtime.working_dir,
        tool_call_id: Some(call.call_id.clone()),
        event_tx: tool_event_tx,
        capabilities,
    };

    let result = match tool_registry
        .execute(&tool_name, call.tool_input, &tool_ctx)
        .await
    {
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

/// 带后台化能力的工具执行。
///
/// spawn 工具执行 task 后，用共享结果槽等待结果或阈值超时。
/// 超时则保留 task 继续在后台执行，watcher 从共享槽读取最终结果并通知 handler。
async fn execute_tool_call_with_background(
    tool_registry: Arc<ToolRegistry>,
    runtime: ToolCallRuntimeContext,
    call: ExecutableToolCall,
    threshold_secs: u64,
) -> (usize, ToolResult) {
    let started_at = Instant::now();
    let tool_name = call.name.clone();
    let call_id = call.call_id.clone();
    let call_index = call.index;

    // 构造工具执行所需的上下文
    let tool_event_tx = {
        let (tool_tx, _bridge_handle) = spawn_event_bridge(Arc::clone(&runtime.publisher));
        Some(tool_tx)
    };

    let tool_ctx = ToolExecutionContext {
        session_id: runtime.session_id.clone(),
        working_dir: runtime.working_dir.clone(),
        tool_call_id: Some(call.call_id.clone()),
        event_tx: tool_event_tx,
        capabilities: tool_capabilities_from_runtime(&runtime),
    };

    // 共享结果槽：exec task 写入，主线程或 watcher 读取
    let result_slot = Arc::new(Mutex::new(
        None::<Result<ToolResult, astrcode_core::tool::ToolError>>,
    ));
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let (exec_complete_tx, exec_complete_rx) = tokio::sync::oneshot::channel::<()>();

    let name = call.name.clone();
    let tool_input = call.tool_input.clone();
    let slot_writer = Arc::clone(&result_slot);
    let exec_handle = tokio::spawn(async move {
        let result = tool_registry.execute(&name, tool_input, &tool_ctx).await;
        *slot_writer.lock() = Some(result);
        let _ = done_tx.send(());
        let _ = exec_complete_tx.send(());
    });

    // 用 timeout 等待完成通知或超时
    match tokio::time::timeout(std::time::Duration::from_secs(threshold_secs), done_rx).await {
        Ok(Ok(())) => {
            // 在阈值内完成
            match result_slot.lock().take() {
                Some(Ok(mut r)) => {
                    r.call_id = call_id.clone();
                    r.duration_ms = Some(started_at.elapsed().as_millis() as u64);
                    tracing::debug!(
                        tool_name,
                        call_id,
                        duration_ms = r.duration_ms.unwrap_or_default(),
                        "tool execution completed (before background threshold)"
                    );
                    (call_index, r)
                },
                Some(Err(e)) => (
                    call_index,
                    error_tool_result(call_id.clone(), &tool_name, e, started_at.elapsed()),
                ),
                None => {
                    // done_tx 发送成功但 result_slot 为空 — 任务在写入结果前异常终止
                    tracing::error!(
                        tool_name,
                        call_id,
                        "done_tx sent but no result available in slot"
                    );
                    (
                        call_index,
                        error_tool_result(
                            call_id.clone(),
                            &tool_name,
                            ToolError::Execution(
                                "tool task completed but no result available".into(),
                            ),
                            started_at.elapsed(),
                        ),
                    )
                },
            }
        },
        Ok(Err(_)) => {
            // done_tx dropped — task panicked or was cancelled
            if let Err(join_err) = exec_handle.await {
                tracing::error!(
                    tool_name,
                    call_id,
                    panic = %join_err,
                    "tool execution task panicked"
                );
            }
            (
                call_index,
                error_tool_result(
                    call_id.clone(),
                    &tool_name,
                    ToolError::Execution("tool task panicked before completion".into()),
                    started_at.elapsed(),
                ),
            )
        },
        Err(_) => {
            // 超时，转入后台。exec_handle 继续运行。
            background_tool_call(
                exec_handle,
                exec_complete_rx,
                result_slot,
                runtime,
                call,
                threshold_secs,
                started_at,
            )
            .await
        },
    }
}

/// 将已超时的工具执行转为后台运行，返回占位结果。
async fn background_tool_call(
    exec_handle: tokio::task::JoinHandle<()>,
    exec_complete_rx: tokio::sync::oneshot::Receiver<()>,
    result_slot: Arc<Mutex<Option<Result<ToolResult, astrcode_core::tool::ToolError>>>>,
    runtime: ToolCallRuntimeContext,
    call: ExecutableToolCall,
    threshold_secs: u64,
    started_at: Instant,
) -> (usize, ToolResult) {
    let tool_name = call.name;
    let call_id = call.call_id;
    let call_index = call.index;
    let task_id = new_background_task_id();

    tracing::info!(
        tool_name,
        call_id,
        task_id = %task_id,
        threshold_secs,
        "tool execution moved to background"
    );

    let bg_reason: String = match threshold_secs {
        0 => "explicit".into(),
        _ => "auto_threshold".into(),
    };
    runtime
        .publisher
        .live(EventPayload::ToolCallBackgrounded {
            call_id: ToolCallId::from(call_id.as_str()),
            tool_name: tool_name.clone(),
            task_id: task_id.clone(),
            reason: bg_reason,
        })
        .await;

    // 闭包专用的变量，之后由 watcher move 消费
    let bg_call_id = call_id.clone();
    let bg_tool_name = tool_name.clone();
    let bg_task_id = task_id.clone();
    let bg_session_id = runtime.session_id.clone();
    let bg_result_tx = runtime.capabilities.background_result_tx.clone();
    let bg_manager = runtime.capabilities.background_tasks.clone();
    let register_task_id = task_id.clone();
    let bg_arguments = call.tool_input.to_string();
    let bg_arguments_json = Some(call.tool_input.clone());

    let watcher_handle = tokio::spawn(async move {
        // 等待 exec 完成（或被 cancel abort 导致 oneshot 断开）
        let _ = exec_complete_rx.await;

        let raw = result_slot.lock().take();
        let mut result = match raw {
            Some(Ok(mut r)) => {
                r.call_id = bg_call_id.clone();
                r.duration_ms = Some(started_at.elapsed().as_millis() as u64);
                r
            },
            Some(Err(e)) => {
                error_tool_result(bg_call_id.clone(), &bg_tool_name, e, started_at.elapsed())
            },
            None => error_tool_result(
                bg_call_id.clone(),
                &bg_tool_name,
                ToolError::Execution("background task completed but no result available".into()),
                started_at.elapsed(),
            ),
        };

        // 在结果元数据中标记后台来源，快照重建时可据此恢复 task_id
        result
            .metadata
            .insert("task_id".into(), serde_json::json!(bg_task_id.to_string()));

        tracing::info!(
            tool_name = bg_tool_name,
            call_id = bg_call_id,
            task_id = %bg_task_id,
            is_error = result.is_error,
            "background task completed"
        );

        // 通过 background_result_tx 通知 handler 进行持久化和广播
        if let Some(tx) = bg_result_tx {
            let _ = tx.send(BackgroundTaskCompletion {
                session_id: bg_session_id,
                task_id: bg_task_id.clone(),
                tool_name: bg_tool_name,
                result,
                arguments: bg_arguments,
                arguments_json: bg_arguments_json,
            });
        }

        // 完成后从管理器移除
        crate::background::complete_background_task(&bg_manager, &bg_task_id);
    });

    // 注册到后台任务管理器，支持中途取消（exec_handle + watcher_handle 都可 abort）
    let mut mgr = runtime.capabilities.background_tasks.lock();
    mgr.register(
        register_task_id,
        runtime.session_id,
        exec_handle,
        watcher_handle,
    );

    // 返回占位结果
    let command = call
        .tool_input
        .get("command")
        .and_then(|v| v.as_str())
        .map(String::from);
    let placeholder = backgrounded_placeholder_result(&call_id, &task_id, command.as_deref());
    (call_index, placeholder)
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
    use astrcode_core::tool::{BackgroundPolicy, ToolError};

    use super::*;

    #[test]
    fn resolve_effective_policy_explicit_true() {
        let input = serde_json::json!({ "runInBackground": true });
        let result =
            resolve_effective_policy(BackgroundPolicy::AutoAfter { threshold_secs: 60 }, &input);
        assert_eq!(result, BackgroundPolicy::AutoAfter { threshold_secs: 0 });
    }

    #[test]
    fn resolve_effective_policy_explicit_false() {
        let input = serde_json::json!({ "runInBackground": false });
        let result =
            resolve_effective_policy(BackgroundPolicy::AutoAfter { threshold_secs: 60 }, &input);
        assert_eq!(result, BackgroundPolicy::Never);
    }

    #[test]
    fn resolve_effective_policy_missing_field_returns_declared() {
        let input = serde_json::json!({ "command": "echo hi" });
        let result =
            resolve_effective_policy(BackgroundPolicy::AutoAfter { threshold_secs: 60 }, &input);
        assert_eq!(result, BackgroundPolicy::AutoAfter { threshold_secs: 60 });
    }

    #[test]
    fn resolve_effective_policy_declared_never_with_override() {
        let input = serde_json::json!({ "runInBackground": true });
        let result = resolve_effective_policy(BackgroundPolicy::Never, &input);
        assert_eq!(result, BackgroundPolicy::AutoAfter { threshold_secs: 0 });
    }

    #[test]
    fn resolve_effective_policy_non_bool_is_none() {
        let input = serde_json::json!({ "runInBackground": "yes" });
        let result =
            resolve_effective_policy(BackgroundPolicy::AutoAfter { threshold_secs: 30 }, &input);
        assert_eq!(result, BackgroundPolicy::AutoAfter { threshold_secs: 30 });
    }

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
