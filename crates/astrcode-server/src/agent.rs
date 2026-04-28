//! Agent — the ephemeral turn processor created from session events.

use std::{collections::HashSet, sync::Arc, time::Instant};

use astrcode_context::pruning::PruneState;
use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionEvent, PostToolUseInput, PreToolUseInput},
    llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole},
    prompt::{PromptContext, PromptProvider},
    tool::{ToolExecutionContext, ToolResult},
    types::*,
};
use astrcode_extensions::{
    context::ServerExtensionContext,
    runner::{ExtensionRunner, ProviderHookOutcome, ToolHookOutcome},
};
use astrcode_support::shell::resolve_shell;
use tokio::sync::mpsc;

use crate::capability::CapabilityRouter;

/// Agent — a transient turn processor.
///
/// Created from a session projection, processes one turn, emits event payloads,
/// and is discarded. Session identity and persistence stay in the handler.
pub struct Agent {
    session_id: SessionId,
    working_dir: String,
    model_id: String,
    llm: Arc<dyn LlmProvider>,
    prompt: Arc<dyn PromptProvider>,
    capability: Arc<CapabilityRouter>,
    extension_runner: Arc<ExtensionRunner>,
    pruner: PruneState,
    tool_allowlist: Option<HashSet<String>>,
    system_prompt_suffix: Option<String>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionId,
        working_dir: String,
        llm: Arc<dyn LlmProvider>,
        prompt: Arc<dyn PromptProvider>,
        capability: Arc<CapabilityRouter>,
        extension_runner: Arc<ExtensionRunner>,
        model_id: String,
        max_tool_result_bytes: usize,
    ) -> Self {
        Self {
            session_id,
            working_dir,
            model_id,
            llm,
            prompt,
            capability,
            extension_runner,
            pruner: PruneState::new(max_tool_result_bytes),
            tool_allowlist: None,
            system_prompt_suffix: None,
        }
    }

    pub fn with_tool_allowlist(mut self, allowed_tools: Vec<String>) -> Self {
        self.tool_allowlist = Some(allowed_tools.into_iter().collect());
        self
    }

    pub fn with_system_prompt_suffix(mut self, suffix: String) -> Self {
        if !suffix.trim().is_empty() {
            self.system_prompt_suffix = Some(suffix);
        }
        self
    }

    fn tool_is_allowed(&self, name: &str) -> bool {
        self.tool_allowlist
            .as_ref()
            .is_none_or(|allowed| tool_name_matches_allowlist(allowed, name))
    }

    /// Build extension context for the current turn.
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

    /// Process a user prompt through the full agent loop.
    ///
    /// When `event_tx` is Some, real-time event payloads are streamed. The
    /// handler wraps them with session/turn metadata and decides durability.
    pub async fn process_prompt(
        &self,
        user_text: &str,
        history: Vec<LlmMessage>,
        event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    ) -> Result<AgentTurnOutput, AgentError> {
        // Build context with tool definitions filled for extension visibility
        let mut ext_ctx = self.build_ext_ctx();
        let mut tools = self.capability.list_definitions().await;
        if let Some(allowed) = &self.tool_allowlist {
            tools.retain(|tool| tool_name_matches_allowlist(allowed, &tool.name));
        }
        let tool_map: std::collections::HashMap<_, _> =
            tools.iter().map(|t| (t.name.clone(), t.clone())).collect();
        ext_ctx.set_tools(tool_map);

        // Dispatch TurnStart
        self.extension_runner
            .dispatch(ExtensionEvent::TurnStart, &ext_ctx)
            .await?;

        // Dispatch UserPromptSubmit before building messages.
        // If a Blocking hook rejects the prompt, fire TurnEnd before returning.
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

        let mut messages = history;
        messages.push(LlmMessage::user(user_text));

        let prompt_ctx = PromptContext {
            working_dir: self.working_dir.clone(),
            os: std::env::consts::OS.into(),
            shell: resolve_shell().name,
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            available_tools: tools
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
            custom: Default::default(),
        };

        let plan = self.prompt.assemble(prompt_ctx).await;
        if !plan.system_blocks.is_empty() || self.system_prompt_suffix.is_some() {
            let mut system_parts: Vec<String> = plan
                .system_blocks
                .iter()
                .map(|b| b.content.clone())
                .collect();
            if let Some(suffix) = &self.system_prompt_suffix {
                system_parts.push(suffix.clone());
            }
            let system_text = system_parts.join("\n\n");
            // Replace existing system message if present, otherwise insert at front
            if let Some(pos) = messages.iter().position(|m| m.role == LlmRole::System) {
                messages[pos] = LlmMessage::system(system_text);
            } else {
                messages.insert(0, LlmMessage::system(system_text));
            }
        }

        let mut final_text = String::new();
        let mut all_tool_results: Vec<ToolResult> = Vec::new();

        loop {
            // --- BeforeProviderRequest hook ---
            let mut send_messages = messages.clone();
            {
                let mut ext_ctx = self.build_ext_ctx();
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
            let message_id = new_message_id();
            let mut message_started = false;
            let mut current_text = String::new();
            let mut tool_calls: Vec<PendingToolCall> = Vec::new();

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
                            if let Some(tx) = &event_tx {
                                if message_started {
                                    let _ = tx.send(EventPayload::AssistantMessageCompleted {
                                        message_id: message_id.clone(),
                                        text: current_text.clone(),
                                    });
                                }
                            }
                            messages.push(LlmMessage::assistant(&current_text));
                            final_text.push_str(&current_text);
                        }

                        if tool_calls.is_empty() {
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

            // --- AfterProviderResponse hook ---
            let _ = self
                .extension_runner
                .dispatch(ExtensionEvent::AfterProviderResponse, &ext_ctx)
                .await;

            let tool_ctx = ToolExecutionContext {
                session_id: self.session_id.clone(),
                working_dir: self.working_dir.clone(),
                model_id: self.model_id.clone(),
                available_tools: tools.clone(),
            };

            for tc in &tool_calls {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
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
                    if let Some(tx) = &event_tx {
                        let _ = tx.send(EventPayload::ToolCallRequested {
                            call_id: tc.call_id.clone(),
                            tool_name: tc.name.clone(),
                            arguments: args.clone(),
                        });
                        let _ = tx.send(EventPayload::ToolCallCompleted {
                            call_id: tc.call_id.clone(),
                            tool_name: tc.name.clone(),
                            result: blocked_result.clone(),
                        });
                    }
                    messages.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: vec![LlmContent::ToolCall {
                            call_id: tc.call_id.clone(),
                            name: tc.name.clone(),
                            arguments: args,
                        }],
                        name: None,
                    });
                    messages.push(LlmMessage {
                        role: LlmRole::Tool,
                        content: vec![LlmContent::ToolResult {
                            tool_call_id: tc.call_id.clone(),
                            content: blocked_result.content.clone(),
                            is_error: true,
                        }],
                        name: Some(tc.name.clone()),
                    });
                    all_tool_results.push(blocked_result);
                    continue;
                }

                let mut pre_ctx = self.build_ext_ctx();
                pre_ctx.set_pre_tool_use_input(PreToolUseInput {
                    tool_name: tc.name.clone(),
                    tool_input: args.clone(),
                });

                let pre_hook_outcome = self
                    .extension_runner
                    .dispatch_tool_hook(ExtensionEvent::PreToolUse, &pre_ctx)
                    .await?;

                let tool_args = match &pre_hook_outcome {
                    ToolHookOutcome::ModifiedInput { tool_input } => tool_input.clone(),
                    _ => args.clone(),
                };

                if let Some(tx) = &event_tx {
                    let _ = tx.send(EventPayload::ToolCallRequested {
                        call_id: tc.call_id.clone(),
                        tool_name: tc.name.clone(),
                        arguments: tool_args.clone(),
                    });
                }

                messages.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: tc.call_id.clone(),
                        name: tc.name.clone(),
                        arguments: tool_args.clone(),
                    }],
                    name: None,
                });

                if let ToolHookOutcome::Blocked { reason } = pre_hook_outcome {
                    let blocked_result = ToolResult {
                        call_id: tc.call_id.clone(),
                        content: format!("Tool execution blocked by hook: {reason}"),
                        is_error: true,
                        error: Some(reason),
                        metadata: Default::default(),
                        duration_ms: None,
                    };
                    if let Some(tx) = &event_tx {
                        let _ = tx.send(EventPayload::ToolCallCompleted {
                            call_id: tc.call_id.clone(),
                            tool_name: tc.name.clone(),
                            result: blocked_result.clone(),
                        });
                    }
                    messages.push(LlmMessage {
                        role: LlmRole::Tool,
                        content: vec![LlmContent::ToolResult {
                            tool_call_id: tc.call_id.clone(),
                            content: blocked_result.content.clone(),
                            is_error: true,
                        }],
                        name: Some(tc.name.clone()),
                    });
                    all_tool_results.push(blocked_result);
                    continue;
                }

                let execution_input = tool_args.clone();
                let started_at = Instant::now();
                let mut result = match self
                    .capability
                    .execute(&tc.name, tool_args, &tool_ctx)
                    .await
                {
                    Ok(mut result) => {
                        result.call_id = tc.call_id.clone();
                        result.duration_ms = Some(started_at.elapsed().as_millis() as u64);
                        self.pruner.prune_result(&mut result);
                        if result.is_error && result.error.is_none() {
                            result.error = Some(result.content.clone());
                        }
                        result
                    },
                    Err(e) => {
                        let err_msg = format!("Error: {}", e);
                        ToolResult {
                            call_id: tc.call_id.clone(),
                            content: err_msg.clone(),
                            is_error: true,
                            error: Some(err_msg.clone()),
                            metadata: Default::default(),
                            duration_ms: Some(started_at.elapsed().as_millis() as u64),
                        }
                    },
                };

                let mut post_ctx = self.build_ext_ctx();
                post_ctx.set_post_tool_use_input(PostToolUseInput {
                    tool_name: tc.name.clone(),
                    tool_input: execution_input,
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

                if let Some(tx) = &event_tx {
                    let _ = tx.send(EventPayload::ToolCallCompleted {
                        call_id: tc.call_id.clone(),
                        tool_name: tc.name.clone(),
                        result: result.clone(),
                    });
                }
                messages.push(LlmMessage {
                    role: LlmRole::Tool,
                    content: vec![LlmContent::ToolResult {
                        tool_call_id: tc.call_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    }],
                    name: Some(tc.name.clone()),
                });
                all_tool_results.push(result);
            }
        }
    }
}

/// Output from an agent turn.
pub struct AgentTurnOutput {
    pub text: String,
    pub finish_reason: String,
    pub tool_results: Vec<ToolResult>,
}

struct PendingToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

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

fn tool_name_matches_allowlist(allowed: &HashSet<String>, tool_name: &str) -> bool {
    allowed.iter().any(|allowed_name| {
        allowed_name == tool_name
            || claude_tool_alias(allowed_name)
                .is_some_and(|alias| alias.eq_ignore_ascii_case(tool_name))
    })
}

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

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use astrcode_core::{
        extension::{Extension, ExtensionContext, ExtensionError, HookEffect, HookMode},
        llm::{LlmError, ModelLimits},
        prompt::{PromptPlan, PromptProvider},
        tool::{ExecutionMode, Tool, ToolDefinition, ToolError},
    };
    use astrcode_extensions::runner::ExtensionRunner;
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn claude_tool_aliases_match_local_tool_names() {
        let allowed = HashSet::from([
            String::from("Read"),
            String::from("Grep"),
            String::from("Bash"),
        ]);

        assert!(tool_name_matches_allowlist(&allowed, "readFile"));
        assert!(tool_name_matches_allowlist(&allowed, "grep"));
        assert!(tool_name_matches_allowlist(&allowed, "shell"));
        assert!(!tool_name_matches_allowlist(&allowed, "writeFile"));
    }

    struct BlockingPreToolExtension;

    #[async_trait::async_trait]
    impl Extension for BlockingPreToolExtension {
        fn id(&self) -> &str {
            "blocking-pre-tool"
        }

        fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
            vec![(ExtensionEvent::PreToolUse, HookMode::Blocking)]
        }

        async fn on_event(
            &self,
            event: ExtensionEvent,
            ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            if event == ExtensionEvent::PreToolUse {
                let input = ctx
                    .pre_tool_use_input()
                    .expect("PreToolUse should include tool payload");
                if input.tool_name == "shell"
                    && input
                        .tool_input
                        .get("command")
                        .and_then(|value| value.as_str())
                        .is_some_and(|command| command.contains("rm -rf"))
                {
                    return Ok(HookEffect::Block {
                        reason: "dangerous command".into(),
                    });
                }
            }
            Ok(HookEffect::Allow)
        }
    }

    struct EmptyPrompt;

    #[async_trait::async_trait]
    impl PromptProvider for EmptyPrompt {
        async fn assemble(&self, _context: PromptContext) -> PromptPlan {
            PromptPlan {
                system_blocks: vec![],
                prepend_messages: vec![],
                append_messages: vec![],
                extra_tools: vec![],
            }
        }
    }

    struct PanicIfExecutedTool {
        executed: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Tool for PanicIfExecutedTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "shell".into(),
                description: "test shell".into(),
                parameters: serde_json::json!({"type": "object"}),
                is_builtin: true,
            }
        }

        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Sequential
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolResult, ToolError> {
            self.executed.store(true, Ordering::SeqCst);
            Ok(ToolResult {
                call_id: String::new(),
                content: "should not run".into(),
                is_error: false,
                error: None,
                metadata: Default::default(),
                duration_ms: None,
            })
        }
    }

    struct ToolThenFinalLlm {
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolThenFinalLlm {
        async fn generate(
            &self,
            messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::unbounded_channel();
            if call == 0 {
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: "call-1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "rm -rf /"}).to_string(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "tool_calls".into(),
                });
            } else {
                assert!(
                    messages
                        .iter()
                        .any(|message| message.content.iter().any(|content| {
                            matches!(
                                content,
                                LlmContent::ToolResult {
                                    content,
                                    is_error: true,
                                    ..
                                } if content.contains("Tool execution blocked by hook")
                            )
                        })),
                    "blocked tool result should be sent back to the LLM"
                );
                let _ = tx.send(LlmEvent::ContentDelta {
                    delta: "handled".into(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            }
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    #[tokio::test]
    async fn blocked_pre_tool_use_emits_completed_event_and_preserves_message_order() {
        let capability = Arc::new(CapabilityRouter::new());
        let executed = Arc::new(AtomicBool::new(false));
        capability
            .register_stable(Arc::new(PanicIfExecutedTool {
                executed: Arc::clone(&executed),
            }))
            .await;

        let extension_runner = Arc::new(ExtensionRunner::new(
            Duration::from_secs(1),
            Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
        ));
        extension_runner
            .register(Arc::new(BlockingPreToolExtension))
            .await;

        let agent = Agent::new(
            "session-1".into(),
            ".".into(),
            Arc::new(ToolThenFinalLlm {
                call_count: AtomicUsize::new(0),
            }),
            Arc::new(EmptyPrompt),
            capability,
            extension_runner,
            "mock".into(),
            8192,
        );

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let output = agent
            .process_prompt("run dangerous command", vec![], Some(event_tx))
            .await
            .unwrap();

        assert_eq!(output.finish_reason, "stop");
        assert!(!executed.load(Ordering::SeqCst));

        let mut saw_requested = false;
        let mut saw_completed_after_requested = false;
        while let Ok(payload) = event_rx.try_recv() {
            match payload {
                EventPayload::ToolCallRequested { .. } => {
                    saw_requested = true;
                },
                EventPayload::ToolCallCompleted { result, .. } => {
                    assert!(result.is_error);
                    assert!(result.content.contains("Tool execution blocked by hook"));
                    saw_completed_after_requested = saw_requested;
                },
                _ => {},
            }
        }

        assert!(saw_requested);
        assert!(saw_completed_after_requested);
    }
}
