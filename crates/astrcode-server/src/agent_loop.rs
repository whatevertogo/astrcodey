//! Agent loop — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：组装提示词、调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。
//! `drive_agent` 负责在回合执行时转发事件流并等待最终输出。

use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    sync::Arc,
    time::Instant,
};

use astrcode_context::{
    compaction::{CompactError, CompactSkipReason, is_prompt_too_long_message},
    manager::{ContextPrepareInput, LlmContextAssembler},
};
use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionEvent, PostToolUseInput, PreToolUseInput},
    llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole},
    tool::{ExecutionMode, ToolDefinition, ToolExecutionContext, ToolResult},
    types::*,
};
use astrcode_extensions::{
    context::ServerExtensionContext,
    runner::{ExtensionRunner, ProviderHookOutcome, ToolHookOutcome},
};
use astrcode_tools::registry::ToolRegistry;
use tokio::{sync::mpsc, task::JoinSet};

use crate::session::compaction_applied_payload;

/// 并行执行工具调用时的最大并发数。
const MAX_PARALLEL_TOOL_CALLS: usize = 5;

/// 运行 agent 的一次 process_prompt，通过 select! + drain 实时处理事件。
///
/// `on_event` 在每个事件到达时被调用（包含 select 阶段和 drain 阶段）。
/// 返回 `(output, emitted_error)`。
pub(crate) async fn drive_agent<F, Fut>(
    agent: &Agent,
    user_text: &str,
    history: Vec<LlmMessage>,
    mut on_event: F,
) -> (Result<AgentTurnOutput, AgentError>, bool)
where
    F: FnMut(EventPayload) -> Fut,
    Fut: Future<Output = ()>,
{
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let agent_future = agent.process_prompt(user_text, history, Some(event_tx));
    tokio::pin!(agent_future);

    let mut emitted_error = false;
    let mut events_closed = false;
    let output = loop {
        tokio::select! {
            result = &mut agent_future => break result,
            payload = event_rx.recv(), if !events_closed => {
                match payload {
                    Some(payload) => {
                        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                            emitted_error = true;
                        }
                        on_event(payload).await;
                    },
                    None => events_closed = true,
                }
            },
        }
    };

    while let Some(payload) = event_rx.recv().await {
        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
            emitted_error = true;
        }
        on_event(payload).await;
    }

    (output, emitted_error)
}

/// Agent — a transient turn processor.
///
/// Created from a session projection, processes one turn, emits event payloads,
/// and is discarded. Session identity and persistence stay in the handler.
pub struct Agent {
    /// 所属会话的唯一标识。
    session_id: SessionId,
    /// 当前工作目录，用于工具执行时的相对路径解析。
    working_dir: String,
    /// 当前使用的模型标识（如 "gpt-4o"）。
    model_id: String,
    /// LLM 提供者，负责与语言模型通信。
    llm: Arc<dyn LlmProvider>,
    /// 会话初始化时固定下来的完整 system prompt。
    system_prompt: String,
    /// 工具注册表，包含当前会话可用的所有工具定义。
    tool_registry: Arc<ToolRegistry>,
    /// 扩展运行器，用于分发 PreToolUse / PostToolUse 等钩子事件。
    extension_runner: Arc<ExtensionRunner>,
    /// 上下文组装器，负责窗口估算与摘要压缩。
    context_assembler: Arc<LlmContextAssembler>,
    /// 工具白名单。设置后仅允许调用列表中的工具，用于子会话等受限场景。
    tool_allowlist: Option<HashSet<String>>,
}

#[derive(Clone)]
pub struct AgentServices {
    pub llm: Arc<dyn LlmProvider>,
    pub tool_registry: Arc<ToolRegistry>,
    pub extension_runner: Arc<ExtensionRunner>,
    pub context_assembler: Arc<LlmContextAssembler>,
}

impl Agent {
    /// 创建一个新的 Agent 实例。
    ///
    /// # 参数
    /// - `session_id`: 所属会话的唯一标识
    /// - `working_dir`: 当前工作目录
    /// - `system_prompt`: 会话初始化时固定下来的完整 system prompt
    /// - `model_id`: 使用的模型标识
    /// - `services`: 当前回合需要的共享运行时服务
    pub fn new(
        session_id: SessionId,
        working_dir: String,
        system_prompt: String,
        model_id: String,
        services: AgentServices,
    ) -> Self {
        Self {
            session_id,
            working_dir,
            model_id,
            llm: services.llm,
            system_prompt,
            tool_registry: services.tool_registry,
            extension_runner: services.extension_runner,
            context_assembler: services.context_assembler,
            tool_allowlist: None,
        }
    }

    /// 设置工具白名单，仅允许调用指定名称的工具。
    /// 用于子会话等需要限制可用工具的场景。
    pub fn with_tool_allowlist(mut self, allowed_tools: Vec<String>) -> Self {
        self.tool_allowlist = Some(allowed_tools.into_iter().collect());
        self
    }

    /// 检查指定工具名是否在白名单中。
    /// 如果未设置白名单，则允许所有工具。
    fn tool_is_allowed(&self, name: &str) -> bool {
        self.tool_allowlist
            .as_ref()
            .is_none_or(|allowed| tool_name_matches_allowlist(allowed, name))
    }

    /// 构建当前回合的扩展上下文，包含会话 ID、工作目录和模型信息。
    fn build_ext_ctx(&self) -> ServerExtensionContext {
        ServerExtensionContext::new(
            self.session_id.clone(),
            self.working_dir.clone(),
            ModelSelection {
                profile_name: String::new(),
                model: self.model_id.clone(),
                provider_kind: String::new(),
            },
        )
    }

    fn build_ext_ctx_with_tools(&self, tools: &[ToolDefinition]) -> ServerExtensionContext {
        let mut ctx = self.build_ext_ctx();
        ctx.set_tools(
            tools
                .iter()
                .cloned()
                .map(|tool| (tool.name.clone(), tool))
                .collect(),
        );
        ctx
    }

    /// 预处理工具调用列表。
    ///
    /// 对每个待执行的工具调用依次执行：
    /// 1. 解析 JSON 参数（解析失败时使用空对象并记录警告）。
    /// 2. 检查工具白名单，不在白名单中的工具直接标记为 `Blocked`。
    /// 3. 分发 `PreToolUse` 扩展钩子，允许扩展修改输入或阻止执行。
    /// 4. 根据工具注册表确定执行模式（并行 / 串行）。
    async fn prepare_tool_calls(
        &self,
        tool_calls: &[PendingToolCall],
        tools: &[ToolDefinition],
        event_tx: &Option<mpsc::UnboundedSender<EventPayload>>,
    ) -> Result<Vec<PreparedToolCall>, AgentError> {
        let mut prepared = Vec::with_capacity(tool_calls.len());

        for (index, tc) in tool_calls.iter().enumerate() {
            let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                tracing::warn!(
                    tool = %tc.name,
                    error = %e,
                    "Malformed tool call arguments, using empty object"
                );
                serde_json::json!({})
            });

            if !self.tool_is_allowed(&tc.name) {
                let blocked_result = ToolResult {
                    call_id: tc.call_id.clone(),
                    content: format!("Tool '{}' is not available to this agent", tc.name),
                    is_error: true,
                    error: Some(format!("tool '{}' is not allowed", tc.name)),
                    metadata: Default::default(),
                    duration_ms: None,
                };
                send_tool_requested(event_tx, tc, &args);
                prepared.push(PreparedToolCall {
                    index,
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    tool_input: args,
                    mode: ExecutionMode::Sequential,
                    outcome: PreparedToolOutcome::Blocked(blocked_result),
                });
                continue;
            }

            let mut pre_ctx = self.build_ext_ctx_with_tools(tools);
            pre_ctx.set_pre_tool_use_input(PreToolUseInput {
                tool_name: tc.name.clone(),
                tool_input: args.clone(),
            });

            let pre_hook_outcome = self
                .extension_runner
                .dispatch_tool_hook(ExtensionEvent::PreToolUse, &pre_ctx)
                .await?;

            let tool_input = match &pre_hook_outcome {
                ToolHookOutcome::ModifiedInput { tool_input } => tool_input.clone(),
                _ => args.clone(),
            };

            send_tool_requested(event_tx, tc, &tool_input);

            let outcome = if let ToolHookOutcome::Blocked { reason } = pre_hook_outcome {
                PreparedToolOutcome::Blocked(ToolResult {
                    call_id: tc.call_id.clone(),
                    content: format!("Tool execution blocked by hook: {reason}"),
                    is_error: true,
                    error: Some(reason),
                    metadata: Default::default(),
                    duration_ms: None,
                })
            } else {
                PreparedToolOutcome::Ready
            };

            let mode = match &outcome {
                PreparedToolOutcome::Ready => self.tool_registry.execution_mode(&tc.name),
                PreparedToolOutcome::Blocked(_) => ExecutionMode::Sequential,
            };

            prepared.push(PreparedToolCall {
                index,
                call_id: tc.call_id.clone(),
                name: tc.name.clone(),
                tool_input,
                mode,
                outcome,
            });
        }

        Ok(prepared)
    }

    /// 执行已预处理的工具调用。
    ///
    /// 按顺序遍历预处理结果，根据执行模式决定调度方式：
    /// - **Blocked**：直接使用预处理阶段的阻止结果，并刷新并行批次。
    /// - **Parallel**：加入并行批次，由 `flush_parallel_batch` 统一调度。
    /// - **Sequential**：先刷新并行批次，再单独执行当前调用。
    ///
    /// 最终返回按索引排序的工具结果映射。
    async fn execute_prepared_tool_calls(
        &self,
        prepared: &[PreparedToolCall],
        tools: &[ToolDefinition],
        event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    ) -> Result<BTreeMap<usize, ToolResult>, AgentError> {
        let mut results = BTreeMap::new();
        let mut parallel_batch = Vec::new();

        for call in prepared {
            match &call.outcome {
                PreparedToolOutcome::Blocked(result) => {
                    self.flush_parallel_batch(
                        &mut parallel_batch,
                        tools,
                        event_tx.clone(),
                        &mut results,
                    )
                    .await?;
                    results.insert(call.index, result.clone());
                },
                PreparedToolOutcome::Ready if call.mode == ExecutionMode::Parallel => {
                    parallel_batch.push(call.to_executable());
                },
                PreparedToolOutcome::Ready => {
                    self.flush_parallel_batch(
                        &mut parallel_batch,
                        tools,
                        event_tx.clone(),
                        &mut results,
                    )
                    .await?;
                    let (index, result) = execute_tool_call(
                        Arc::clone(&self.tool_registry),
                        self.session_id.clone(),
                        self.working_dir.clone(),
                        self.model_id.clone(),
                        tools.to_vec(),
                        event_tx.clone(),
                        call.to_executable(),
                    )
                    .await;
                    results.insert(index, result);
                },
            }
        }

        self.flush_parallel_batch(&mut parallel_batch, tools, event_tx, &mut results)
            .await?;

        Ok(results)
    }

    /// 刷新并行工具调用批次。
    ///
    /// 使用 `JoinSet` 同时启动最多 `MAX_PARALLEL_TOOL_CALLS` 个工具调用任务，
    /// 每当一个任务完成后立即补充下一个待执行调用，保持并发水位不变。
    /// 所有结果按索引写入 `results` 映射。
    async fn flush_parallel_batch(
        &self,
        batch: &mut Vec<ExecutableToolCall>,
        tools: &[ToolDefinition],
        event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
        results: &mut BTreeMap<usize, ToolResult>,
    ) -> Result<(), AgentError> {
        if batch.is_empty() {
            return Ok(());
        }

        let mut pending = std::mem::take(batch).into_iter();
        let mut join_set = JoinSet::new();

        for _ in 0..MAX_PARALLEL_TOOL_CALLS {
            let Some(call) = pending.next() else { break };
            self.spawn_tool_call(&mut join_set, call, tools, event_tx.clone());
        }

        while let Some(joined) = join_set.join_next().await {
            let (index, result) =
                joined.map_err(|err| AgentError::Llm(format!("tool task failed: {err}")))?;
            results.insert(index, result);

            if let Some(call) = pending.next() {
                self.spawn_tool_call(&mut join_set, call, tools, event_tx.clone());
            }
        }

        Ok(())
    }

    /// 将单个工具调用封装为异步任务并加入 `JoinSet`。
    ///
    /// 克隆必要的上下文（工具注册表、会话 ID、工作目录等），
    /// 使任务可以在独立线程中安全执行。
    fn spawn_tool_call(
        &self,
        join_set: &mut JoinSet<(usize, ToolResult)>,
        call: ExecutableToolCall,
        tools: &[ToolDefinition],
        event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    ) {
        let tool_registry = Arc::clone(&self.tool_registry);
        let session_id = self.session_id.clone();
        let working_dir = self.working_dir.clone();
        let model_id = self.model_id.clone();
        let tools = tools.to_vec();

        join_set.spawn(async move {
            execute_tool_call(
                tool_registry,
                session_id,
                working_dir,
                model_id,
                tools,
                event_tx,
                call,
            )
            .await
        });
    }

    /// 提交工具执行结果。
    ///
    /// 对每个已执行的工具调用依次处理：
    /// 1. 分发 `PostToolUse` 扩展钩子，允许扩展修改结果内容或阻止。
    /// 2. 通过 `event_tx` 发送 `ToolCallCompleted` 事件通知客户端。
    /// 3. 将工具结果消息追加到 LLM 对话历史，供下一轮调用使用。
    async fn commit_tool_results(
        &self,
        mut input: CommitToolResults<'_>,
    ) -> Result<(), AgentError> {
        for call in input.prepared {
            let mut result = input
                .results
                .remove(&call.index)
                .unwrap_or_else(|| missing_tool_result(call));

            if matches!(&call.outcome, PreparedToolOutcome::Ready) {
                if result.is_error && result.error.is_none() {
                    result.error = Some(result.content.clone());
                }

                let mut post_ctx = self.build_ext_ctx_with_tools(input.tools);
                post_ctx.set_post_tool_use_input(PostToolUseInput {
                    tool_name: call.name.clone(),
                    tool_input: call.tool_input.clone(),
                    tool_result: result.clone(),
                });

                match self
                    .extension_runner
                    .dispatch_tool_hook(ExtensionEvent::PostToolUse, &post_ctx)
                    .await?
                {
                    ToolHookOutcome::ModifiedResult { content } => {
                        result.content = content;
                        if result.is_error {
                            result.error = Some(result.content.clone());
                        }
                    },
                    ToolHookOutcome::Blocked { reason } => {
                        result.content = format!("Tool result blocked by hook: {reason}");
                        result.is_error = true;
                        result.error = Some(reason);
                    },
                    ToolHookOutcome::Allow | ToolHookOutcome::ModifiedInput { .. } => {},
                }
            }

            if let Some(tx) = input.event_tx {
                let _ = tx.send(EventPayload::ToolCallCompleted {
                    call_id: call.call_id.clone(),
                    tool_name: call.name.clone(),
                    result: result.clone(),
                });
            }
            input.messages.push(LlmMessage {
                role: LlmRole::Tool,
                content: vec![LlmContent::ToolResult {
                    tool_call_id: call.call_id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                }],
                name: Some(call.name.clone()),
            });
            input.all_tool_results.push(result);
        }

        Ok(())
    }

    /// 处理用户输入的完整 Agent 循环。
    ///
    /// 整体流程：
    /// 1. 分发 `TurnStart` 和 `UserPromptSubmit` 扩展事件。
    /// 2. 使用 session 初始化时固定下来的 system prompt 构造消息前缀。
    /// 3. 进入 Agent 循环（LLM 调用 → 工具执行 → 再次调用 LLM），直到 LLM 不再请求工具调用为止。
    ///
    /// 当 `event_tx` 为 `Some` 时，会实时流式发送事件载荷，
    /// 由 handler 层包装会话/回合元数据后决定持久化策略。
    pub async fn process_prompt(
        &self,
        user_text: &str,
        history: Vec<LlmMessage>,
        event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    ) -> Result<AgentTurnOutput, AgentError> {
        // 构建扩展上下文，填充工具定义供扩展钩子查询
        let mut ext_ctx = self.build_ext_ctx();
        let mut tools = self.tool_registry.list_definitions();
        if let Some(allowed) = &self.tool_allowlist {
            tools.retain(|tool| tool_name_matches_allowlist(allowed, &tool.name));
        }
        let tool_map: std::collections::HashMap<_, _> =
            tools.iter().map(|t| (t.name.clone(), t.clone())).collect();
        ext_ctx.set_tools(tool_map);

        // 分发 TurnStart 事件，通知扩展新回合开始
        self.extension_runner
            .dispatch(ExtensionEvent::TurnStart, &ext_ctx)
            .await?;

        // 分发 UserPromptSubmit 事件。
        // 如果扩展通过 Blocking 钩子拒绝了提示词，先触发 TurnEnd 再返回错误。
        if let Err(e) = self
            .extension_runner
            .dispatch(ExtensionEvent::UserPromptSubmit, &ext_ctx)
            .await
        {
            let _ = self
                .extension_runner
                .dispatch(ExtensionEvent::TurnEnd, &ext_ctx)
                .await;
            return Err(e.into());
        }

        // 每轮都以同一份 session system prompt 开头；历史来自 eventlog 投影。
        let mut messages = Vec::with_capacity(history.len() + 2);
        if !self.system_prompt.trim().is_empty() {
            messages.push(LlmMessage::system(self.system_prompt.clone()));
        }
        messages.extend(
            history
                .into_iter()
                .filter(|message| message.role != LlmRole::System),
        );
        messages.push(LlmMessage::user(user_text));

        // 累积本轮 Agent 的最终文本输出和所有工具执行结果
        let mut final_text = String::new();
        let mut all_tool_results: Vec<ToolResult> = Vec::new();
        let mut did_overflow_retry_this_turn = false;

        // ── Agent 主循环 ──
        // 每轮迭代：调用 LLM → 处理响应 → 执行工具 → 将结果追加到消息历史 → 下一轮
        loop {
            let (system_messages, visible_messages): (Vec<_>, Vec<_>) = messages
                .iter()
                .cloned()
                .partition(|message| message.role == LlmRole::System);
            let prepared_context = match self
                .context_assembler
                .prepare_provider_messages_with_provider(
                    ContextPrepareInput {
                        messages: visible_messages,
                        system_prompt: Some(&self.system_prompt),
                        model_limits: self.llm.model_limits(),
                    },
                    self.llm.as_ref(),
                )
                .await
            {
                Ok(prepared_context) => prepared_context,
                Err(CompactError::Skip(
                    CompactSkipReason::Empty | CompactSkipReason::NothingToCompact,
                )) => self
                    .context_assembler
                    .prepare_provider_messages(ContextPrepareInput {
                        messages: messages
                            .iter()
                            .filter(|message| message.role != LlmRole::System)
                            .cloned()
                            .collect(),
                        system_prompt: Some(&self.system_prompt),
                        model_limits: self.llm.model_limits(),
                    }),
                Err(error) => return Err(AgentError::Llm(error.to_string())),
            };
            if let Some(compaction) = prepared_context.compaction {
                if let Some(tx) = &event_tx {
                    let _ = tx.send(EventPayload::CompactionStarted);
                    let _ = tx.send(compaction_applied_payload(&compaction));
                    let _ = tx.send(EventPayload::CompactionCompleted {
                        pre_tokens: compaction.pre_tokens,
                        post_tokens: compaction.post_tokens,
                        summary: compaction.summary,
                    });
                }
                messages = [system_messages.clone(), prepared_context.messages.clone()].concat();
            }

            // 分发 BeforeProviderRequest 钩子，允许扩展修改发送给 LLM 的消息或阻止请求
            let mut send_messages = [system_messages, prepared_context.messages].concat();
            {
                let mut ext_ctx = self.build_ext_ctx_with_tools(&tools);
                ext_ctx.set_provider_messages(send_messages.clone());
                match self
                    .extension_runner
                    .dispatch_provider_hook(ExtensionEvent::BeforeProviderRequest, &ext_ctx)
                    .await?
                {
                    ProviderHookOutcome::Blocked { reason } => {
                        self.extension_runner
                            .dispatch(ExtensionEvent::TurnEnd, &ext_ctx)
                            .await?;
                        return Err(AgentError::Llm(reason));
                    },
                    ProviderHookOutcome::ModifiedMessages { messages } => {
                        send_messages = messages;
                    },
                    ProviderHookOutcome::Allow => {},
                }
            }
            let mut rx = match self.llm.generate(send_messages, tools.clone()).await {
                Ok(rx) => rx,
                Err(e) => {
                    // LLM 调用级别失败（网络/认证等），需要通知客户端，
                    // 否则外部消费者无法感知此错误（流中错误通过 LlmEvent::Error 已有处理）
                    if let Some(tx) = &event_tx {
                        let _ = tx.send(EventPayload::ErrorOccurred {
                            code: -32603,
                            message: e.to_string(),
                            recoverable: false,
                        });
                    }
                    let _ = self
                        .extension_runner
                        .dispatch(ExtensionEvent::TurnEnd, &ext_ctx)
                        .await;
                    return Err(e.into());
                },
            };
            // 每轮 LLM 调用的局部状态
            let message_id = new_message_id(); // 本轮消息的唯一 ID
            let mut message_started = false; // 是否已发送 AssistantMessageStarted
            let mut current_text = String::new(); // 本轮累积的文本增量
            let mut tool_calls: Vec<PendingToolCall> = Vec::new(); // LLM 请求的工具调用
            // 延迟到工具调用执行完毕后才发送 AssistantMessageCompleted，
            // 避免消息在工具执行前就被标记为已完成。
            let mut completed_text: Option<String> = None;
            let mut retry_provider_request = false;

            // 消费 LLM 事件流，处理文本增量和工具调用增量
            while let Some(event) = rx.recv().await {
                match event {
                    LlmEvent::ContentDelta { delta } => {
                        if let Some(tx) = &event_tx {
                            if !message_started {
                                let _ = tx.send(EventPayload::AssistantMessageStarted {
                                    message_id: message_id.clone(),
                                });
                                message_started = true;
                            }
                            let _ = tx.send(EventPayload::AssistantTextDelta {
                                message_id: message_id.clone(),
                                delta: delta.clone(),
                            });
                        }
                        current_text.push_str(&delta);
                    },
                    LlmEvent::ToolCallStart {
                        call_id,
                        name,
                        arguments,
                    } => {
                        if let Some(tx) = &event_tx {
                            let _ = tx.send(EventPayload::ToolCallStarted {
                                call_id: call_id.clone(),
                                tool_name: name.clone(),
                            });
                            if !arguments.is_empty() {
                                let _ = tx.send(EventPayload::ToolCallArgumentsDelta {
                                    call_id: call_id.clone(),
                                    delta: arguments.clone(),
                                });
                            }
                        }
                        tool_calls.push(PendingToolCall {
                            call_id,
                            name,
                            arguments,
                        });
                    },
                    LlmEvent::ToolCallDelta { call_id, delta } => {
                        if let Some(tc) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                            tc.arguments.push_str(&delta);
                        }
                        if let Some(tx) = &event_tx {
                            let _ =
                                tx.send(EventPayload::ToolCallArgumentsDelta { call_id, delta });
                        }
                    },
                    LlmEvent::Done { finish_reason } => {
                        if !current_text.is_empty() {
                            let text = std::mem::take(&mut current_text);
                            messages.push(LlmMessage::assistant(&text));
                            final_text.push_str(&text);
                            completed_text = Some(text);
                        }

                        if tool_calls.is_empty() {
                            // 无工具调用：消息在此处完成并直接返回
                            if let (Some(text), true) = (completed_text.take(), message_started) {
                                if let Some(tx) = &event_tx {
                                    let _ = tx.send(EventPayload::AssistantMessageCompleted {
                                        message_id: message_id.clone(),
                                        text,
                                    });
                                }
                            }
                            self.extension_runner
                                .dispatch(ExtensionEvent::TurnEnd, &ext_ctx)
                                .await?;
                            return Ok(AgentTurnOutput {
                                text: final_text,
                                finish_reason,
                                tool_results: all_tool_results,
                            });
                        }
                        break;
                    },
                    LlmEvent::Error { message } => {
                        if is_prompt_too_long_message(&message) && !did_overflow_retry_this_turn {
                            did_overflow_retry_this_turn = true;
                            let (system_messages, visible_messages): (Vec<_>, Vec<_>) = messages
                                .iter()
                                .cloned()
                                .partition(|message| message.role == LlmRole::System);
                            if let Ok(prepared) = self
                                .context_assembler
                                .compact_provider_messages_with_provider(
                                    self.llm.as_ref(),
                                    visible_messages,
                                    Some(&self.system_prompt),
                                )
                                .await
                            {
                                let compaction = prepared
                                    .compaction
                                    .expect("compact_provider_messages should include compaction");
                                if let Some(tx) = &event_tx {
                                    let _ = tx.send(EventPayload::CompactionStarted);
                                    let _ = tx.send(compaction_applied_payload(&compaction));
                                    let _ = tx.send(EventPayload::CompactionCompleted {
                                        pre_tokens: compaction.pre_tokens,
                                        post_tokens: compaction.post_tokens,
                                        summary: compaction.summary,
                                    });
                                }
                                messages = [system_messages, prepared.messages].concat();
                                retry_provider_request = true;
                                break;
                            }
                        }
                        if let Some(tx) = &event_tx {
                            let _ = tx.send(EventPayload::ErrorOccurred {
                                code: -32603,
                                message: message.clone(),
                                recoverable: false,
                            });
                        }
                        self.extension_runner
                            .dispatch(ExtensionEvent::TurnEnd, &ext_ctx)
                            .await?;
                        return Err(AgentError::Llm(message));
                    },
                }
            }
            if retry_provider_request {
                continue;
            }

            // 分发 AfterProviderResponse 钩子，允许扩展在收到 LLM 响应后执行操作
            if let Err(e) = self
                .extension_runner
                .dispatch(ExtensionEvent::AfterProviderResponse, &ext_ctx)
                .await
            {
                let _ = self
                    .extension_runner
                    .dispatch(ExtensionEvent::TurnEnd, &ext_ctx)
                    .await;
                return Err(e.into());
            }

            // ── 工具调用执行管线 ──
            // 1. 预处理：白名单检查 + PreToolUse 钩子
            let prepared_tool_calls = self
                .prepare_tool_calls(&tool_calls, &tools, &event_tx)
                .await?;
            // 将助手侧的工具调用消息追加到对话历史（包含工具名和参数）
            messages.push(assistant_tool_call_message(&prepared_tool_calls));
            // 2. 执行：按并行/串行模式调度工具
            let tool_results = self
                .execute_prepared_tool_calls(&prepared_tool_calls, &tools, event_tx.clone())
                .await?;
            // 3. 提交：裁剪结果 + PostToolUse 钩子 + 追加到对话历史
            self.commit_tool_results(CommitToolResults {
                prepared: &prepared_tool_calls,
                results: tool_results,
                tools: &tools,
                messages: &mut messages,
                all_tool_results: &mut all_tool_results,
                event_tx: &event_tx,
            })
            .await?;

            // 工具调用全部执行完毕后才发送消息完成事件，
            // 保证 completed 事件在所有 tool_call 事件之后。
            if let Some(text) = completed_text.take() {
                if message_started {
                    if let Some(tx) = &event_tx {
                        let _ = tx.send(EventPayload::AssistantMessageCompleted {
                            message_id: message_id.clone(),
                            text,
                        });
                    }
                }
            }
        }
    }
}

/// Agent 回合的输出结果。
pub struct AgentTurnOutput {
    /// 助手回复的文本内容
    pub text: String,
    /// 结束原因（如 "stop"、"tool_calls"）
    pub finish_reason: String,
    /// 本回合中所有工具调用的结果
    pub tool_results: Vec<ToolResult>,
}

/// 等待执行的工具调用，在 LLM 流式响应中逐步积累参数。
pub(crate) struct PendingToolCall {
    /// 工具调用的唯一标识
    pub(crate) call_id: String,
    /// 工具名称
    pub(crate) name: String,
    /// 工具调用的 JSON 参数（可能跨多个 delta 事件拼接）
    pub(crate) arguments: String,
}

pub(crate) struct PreparedToolCall {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
    pub(crate) mode: ExecutionMode,
    pub(crate) outcome: PreparedToolOutcome,
}

struct CommitToolResults<'a> {
    prepared: &'a [PreparedToolCall],
    results: BTreeMap<usize, ToolResult>,
    tools: &'a [ToolDefinition],
    messages: &'a mut Vec<LlmMessage>,
    all_tool_results: &'a mut Vec<ToolResult>,
    event_tx: &'a Option<mpsc::UnboundedSender<EventPayload>>,
}

pub(crate) enum PreparedToolOutcome {
    Ready,
    Blocked(ToolResult),
}

#[derive(Clone)]
pub(crate) struct ExecutableToolCall {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
}

impl PreparedToolCall {
    pub(crate) fn to_executable(&self) -> ExecutableToolCall {
        ExecutableToolCall {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            tool_input: self.tool_input.clone(),
        }
    }
}

pub(crate) fn send_tool_requested(
    event_tx: &Option<mpsc::UnboundedSender<EventPayload>>,
    tc: &PendingToolCall,
    arguments: &serde_json::Value,
) {
    if let Some(tx) = event_tx {
        let _ = tx.send(EventPayload::ToolCallRequested {
            call_id: tc.call_id.clone(),
            tool_name: tc.name.clone(),
            arguments: arguments.clone(),
        });
    }
}

pub(crate) fn assistant_tool_call_message(prepared: &[PreparedToolCall]) -> LlmMessage {
    LlmMessage {
        role: LlmRole::Assistant,
        content: prepared
            .iter()
            .map(|call| LlmContent::ToolCall {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.tool_input.clone(),
            })
            .collect(),
        name: None,
    }
}

pub(crate) async fn execute_tool_call(
    tool_registry: Arc<ToolRegistry>,
    session_id: String,
    working_dir: String,
    model_id: String,
    tools: Vec<ToolDefinition>,
    event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    call: ExecutableToolCall,
) -> (usize, ToolResult) {
    let started_at = Instant::now();
    let tool_ctx = ToolExecutionContext {
        session_id,
        working_dir,
        model_id,
        available_tools: tools,
        tool_call_id: Some(call.call_id.clone()),
        event_tx,
    };

    let mut result = match tool_registry
        .execute(&call.name, call.tool_input.clone(), &tool_ctx)
        .await
    {
        Ok(mut result) => {
            result.call_id = call.call_id.clone();
            result.duration_ms = Some(started_at.elapsed().as_millis() as u64);
            result
        },
        Err(e) => {
            let err_msg = format!("Error: {}", e);
            ToolResult {
                call_id: call.call_id.clone(),
                content: err_msg.clone(),
                is_error: true,
                error: Some(err_msg),
                metadata: Default::default(),
                duration_ms: Some(started_at.elapsed().as_millis() as u64),
            }
        },
    };

    if result.call_id.is_empty() {
        result.call_id = call.call_id.clone();
    }

    (call.index, result)
}

pub(crate) fn missing_tool_result(call: &PreparedToolCall) -> ToolResult {
    let message = format!("Tool '{}' did not produce a result", call.name);
    ToolResult {
        call_id: call.call_id.clone(),
        content: message.clone(),
        is_error: true,
        error: Some(message),
        metadata: Default::default(),
        duration_ms: None,
    }
}

/// Agent 处理过程中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Tool error: {0}")]
    Tool(#[from] astrcode_core::tool::ToolError),
    #[error("Extension error: {0}")]
    Extension(#[from] astrcode_core::extension::ExtensionError),
}

impl From<astrcode_core::llm::LlmError> for AgentError {
    fn from(e: astrcode_core::llm::LlmError) -> Self {
        AgentError::Llm(e.to_string())
    }
}

/// 检查工具名是否匹配白名单，支持 Claude 风格的别名映射。
/// 例如白名单中有 "Read"，则 "readFile" 也能匹配。
pub(crate) fn tool_name_matches_allowlist(allowed: &HashSet<String>, tool_name: &str) -> bool {
    allowed.iter().any(|allowed_name| {
        allowed_name == tool_name
            || claude_tool_alias(allowed_name)
                .is_some_and(|alias| alias.eq_ignore_ascii_case(tool_name))
    })
}

/// 将简短的工具名映射为 Claude 风格的实际工具名。
/// 例如 "read" → "readFile"，"bash" → "shell"。
fn claude_tool_alias(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "read" => Some("readFile"),
        "write" => Some("writeFile"),
        "edit" | "multiedit" => Some("editFile"),
        "grep" => Some("grep"),
        "glob" => Some("findFiles"),
        "bash" => Some("shell"),
        _ => None,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "agent_loop_tests.rs"]
mod tests;
