//! 工具调用执行实现。

use std::{sync::Arc, time::Instant};

use astrcode_core::{
    tool::{
        FileObservation, FileObservationStore, LlmModelIds, ToolCapabilities, ToolDefinition,
        ToolError, ToolExecutionContext, ToolResultArtifactReader, access::ResourceLease,
    },
    types::TurnId,
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
        turn_id: TurnId,
        tool_selection: astrcode_core::tool::SessionToolSelection,
        session_store_dir: Option<std::path::PathBuf>,
        cancellation_token: CancellationToken,
    ) -> Self {
        let runtime_services = session.runtime_services();
        let effective = runtime_services.read_effective();
        let approval_history = session.runtime().approval_history();
        let permission_chain = crate::permission::build_default_chain(&effective);
        let shared = crate::turn_context::SharedTurnContext {
            session_id: session.id().clone(),
            turn_id: Some(turn_id),
            working_dir: session_state.identity.working_dir.clone(),
            model_id: session_state.identity.model_id.clone(),
            session_store_dir,
            turn_event_sender: None,
            approval_mode: effective.agent.approval_mode,
            tool_selection: Some(tool_selection),
            permission_chain,
            approval_history,
            cancellation_token,
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
        Self {
            file_observation_store: Some(runtime.file_observation_store()),
            session_ops: runtime_services.session_ops(),
            session_store_dir: shared.session_store_dir.clone(),
            llm_models: LlmModelIds {
                main: Some(shared.model_id.clone()),
                small: Some(effective.small_llm.model_id.clone()),
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
    let ExecutableToolInvocation {
        index,
        call_id,
        name: tool_name,
        tool_input,
        plan,
    } = call;
    let ToolCallRuntimeContext {
        turn,
        tools,
        tool_result_reader,
        cancellation_token,
        ..
    } = runtime;
    let capabilities = tool_capabilities_from_runtime(&turn, tools, tool_result_reader);
    let mut tool_ctx = ToolExecutionContext::new(
        turn.shared.session_id.clone(),
        turn.shared.working_dir.clone(),
        Some(call_id.clone()),
        turn.shared.turn_event_tx(),
        capabilities,
    )
    .with_resource_lease(ResourceLease::from_plan(&plan))
    .with_cancellation(cancellation_token.clone());
    if let Some(turn_id) = &turn.shared.turn_id {
        tool_ctx = tool_ctx.with_turn_id(turn_id.clone());
    }

    let outcome = tokio::select! {
        _ = cancellation_token.cancelled() => ToolExecutionOutcome::cancelled(
            "tool execution cancelled",
            Some(started_at.elapsed().as_millis() as u64),
        ),
        result = tool_registry.execute(&tool_name, tool_input, &tool_ctx) => {
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
    if let Some(sender) = turn.shared.turn_event_sender.as_ref()
        && let Err(error) = sender.flush().await
    {
        // flush 只确认本调用入队的 live 事件已处理完毕，失败不代表工具结果丢失；
        // 用 ingress 错误覆盖 outcome 会把成功的工具执行伪装成失败。
        tracing::warn!(
            tool_name,
            call_id,
            error = %error,
            "failed to flush tool events after execution; keeping tool outcome"
        );
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

    (index, outcome)
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
    use astrcode_core::{
        permission::ApprovalMode,
        tool::{ExecutionMode, SessionToolSelection, Tool, ToolError, ToolOrigin, ToolResult},
        types::SessionId,
    };

    use super::*;
    use crate::{
        permission::{ApprovalHistoryStore, PermissionChain},
        turn_context::SharedTurnContext,
    };

    #[derive(Debug, PartialEq, Eq)]
    struct CapturedContext {
        session_id: String,
        turn_id: String,
        tool_call_id: String,
        working_dir: String,
    }

    struct ContextCaptureTool(Arc<Mutex<Option<CapturedContext>>>);

    #[async_trait::async_trait]
    impl Tool for ContextCaptureTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "capture_context".into(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
                strict: false,
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            }
        }

        async fn plan(
            &self,
            _arguments: &serde_json::Value,
            _ctx: &astrcode_core::tool::ToolPlanningContext,
        ) -> Result<astrcode_core::tool::access::ToolPlan, ToolError> {
            Ok(astrcode_core::tool::access::ToolPlan::default())
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            ctx: &ToolExecutionContext,
        ) -> Result<astrcode_core::tool::ToolExecutionResult, ToolError> {
            *self.0.lock() = Some(CapturedContext {
                session_id: ctx.scope.session_id.to_string(),
                turn_id: ctx.turn_id().expect("turn attribution").to_string(),
                tool_call_id: ctx
                    .scope
                    .tool_call_id
                    .clone()
                    .expect("tool call attribution"),
                working_dir: ctx.scope.working_dir.clone(),
            });
            Ok(ToolResult::success("captured").into())
        }
    }

    #[tokio::test]
    async fn turn_tool_context_reaches_tool_execution_context() {
        let captured = Arc::new(Mutex::new(None));
        let tool = Arc::new(ContextCaptureTool(Arc::clone(&captured)));
        let definition = tool.definition();
        let mut registry = ToolRegistry::new();
        registry.register(tool).expect("register capture tool");
        let runtime = ToolCallRuntimeContext {
            turn: TurnToolContext {
                shared: SharedTurnContext {
                    session_id: SessionId::new("session-source"),
                    turn_id: Some(TurnId::new("turn-source")),
                    working_dir: "/workspace".into(),
                    model_id: "model".into(),
                    session_store_dir: None,
                    turn_event_sender: None,
                    approval_mode: ApprovalMode::default(),
                    tool_selection: Some(SessionToolSelection::default()),
                    permission_chain: Arc::new(PermissionChain::new(Vec::new())),
                    approval_history: Arc::new(ApprovalHistoryStore::default()),
                    cancellation_token: CancellationToken::new(),
                },
                capabilities: ToolRuntimeCapabilities {
                    file_observation_store: None,
                    session_ops: None,
                    llm_models: LlmModelIds::default(),
                    session_store_dir: None,
                },
            },
            tools: Arc::from([definition]),
            tool_result_reader: None,
            cancellation_token: CancellationToken::new(),
        };

        let (index, outcome) = execute_tool_call(
            Arc::new(registry),
            runtime,
            ExecutableToolInvocation {
                index: 7,
                call_id: "call-source".into(),
                name: "capture_context".into(),
                tool_input: serde_json::json!({}),
                plan: astrcode_core::tool::access::ToolPlan::default(),
            },
        )
        .await;

        assert_eq!(index, 7);
        assert!(matches!(outcome, ToolExecutionOutcome::Completed(_)));
        assert_eq!(
            captured.lock().as_ref(),
            Some(&CapturedContext {
                session_id: "session-source".into(),
                turn_id: "turn-source".into(),
                tool_call_id: "call-source".into(),
                working_dir: "/workspace".into(),
            })
        );
    }

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
