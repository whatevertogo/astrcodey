// Agent loop 测试模块 — 通过 #[path] 注入 agent_loop.rs 作为子模块。
// 使用 use super::* 访问 agent_loop 模块的所有类型。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, HookEffect, HookMode, HookSubscription,
    },
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmRole, ModelLimits},
    tool::{
        ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolOrigin,
        ToolResult,
    },
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::{
    sync::{Barrier, mpsc},
    time::{sleep, timeout},
};

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

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![HookSubscription {
            event: ExtensionEvent::PreToolUse,
            mode: HookMode::Blocking,
            priority: 0,
        }]
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

struct ProviderMessageExtension {
    id: &'static str,
    text: &'static str,
    required_tool: Option<&'static str>,
}

#[async_trait::async_trait]
impl Extension for ProviderMessageExtension {
    fn id(&self) -> &str {
        self.id
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![HookSubscription {
            event: ExtensionEvent::BeforeProviderRequest,
            mode: HookMode::Blocking,
            priority: 0,
        }]
    }

    async fn on_event(
        &self,
        _event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        if self
            .required_tool
            .is_some_and(|tool| ctx.find_tool(tool).is_none())
        {
            return Ok(HookEffect::Allow);
        }

        let messages = ctx
            .provider_messages()
            .expect("BeforeProviderRequest should include provider messages");
        assert!(message_text_contains(&messages, "hello"));
        Ok(HookEffect::AppendMessages {
            messages: vec![LlmMessage::user(self.text)],
        })
    }
}

struct CapturingLlm {
    messages: Arc<Mutex<Vec<LlmMessage>>>,
}

#[async_trait::async_trait]
impl LlmProvider for CapturingLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        *self.messages.lock().unwrap() = messages;
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta { delta: "ok".into() });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

struct OverflowThenOkLlm {
    call_count: AtomicUsize,
    captured_messages: Arc<Mutex<Vec<LlmMessage>>>,
}

#[async_trait::async_trait]
impl LlmProvider for OverflowThenOkLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let call_count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        match call_count {
            0 => {
                let _ = tx.send(LlmEvent::Error {
                    message: "maximum context length exceeded".into(),
                });
            },
            1 => {
                let _ = tx.send(LlmEvent::ContentDelta {
                    delta: r#"<summary>
1. Primary Request and Intent:
   Compacted

2. Key Technical Concepts:
   - compact

3. Files and Code Sections:
   - (none)

4. Errors and fixes:
   - prompt too long

5. Problem Solving:
   compacted and retrying

6. All user messages:
   - current

7. Pending Tasks:
   - (none)

8. Current Work:
   retry request

9. Optional Next Step:
   - (none)
</summary>"#
                        .into(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            },
            _ => {
                *self.captured_messages.lock().unwrap() = messages;
                let _ = tx.send(LlmEvent::ContentDelta {
                    delta: "recovered".into(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            },
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
            origin: ToolOrigin::Builtin,
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

struct ToolCallsThenFinalLlm {
    call_count: AtomicUsize,
    calls: Vec<(&'static str, &'static str)>,
    captured_messages: Arc<Mutex<Vec<LlmMessage>>>,
}

#[async_trait::async_trait]
impl LlmProvider for ToolCallsThenFinalLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let call_count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        if call_count == 0 {
            for (call_id, tool_name) in &self.calls {
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: (*call_id).into(),
                    name: (*tool_name).into(),
                    arguments: serde_json::json!({}).to_string(),
                });
            }
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "tool_calls".into(),
            });
        } else {
            *self.captured_messages.lock().unwrap() = messages;
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "done".into(),
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

struct BarrierTool {
    name: &'static str,
    barrier: Arc<Barrier>,
}

#[async_trait::async_trait]
impl Tool for BarrierTool {
    fn definition(&self) -> ToolDefinition {
        test_tool_definition(self.name)
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        self.barrier.wait().await;
        Ok(success_tool_result(self.name))
    }
}

struct DelayTool {
    name: &'static str,
    mode: ExecutionMode,
    delay_ms: u64,
}

#[async_trait::async_trait]
impl Tool for DelayTool {
    fn definition(&self) -> ToolDefinition {
        test_tool_definition(self.name)
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if self.delay_ms > 0 {
            sleep(Duration::from_millis(self.delay_ms)).await;
        }
        Ok(success_tool_result(self.name))
    }
}

struct MarkerTool {
    name: &'static str,
    mode: ExecutionMode,
    marker: Arc<AtomicBool>,
    violation: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Tool for MarkerTool {
    fn definition(&self) -> ToolDefinition {
        test_tool_definition(self.name)
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        match self.name {
            "seq" => {
                sleep(Duration::from_millis(50)).await;
                self.marker.store(true, Ordering::SeqCst);
            },
            "after" if !self.marker.load(Ordering::SeqCst) => {
                self.violation.store(true, Ordering::SeqCst);
            },
            _ => {},
        }
        Ok(success_tool_result(self.name))
    }
}

struct FailingTool {
    name: &'static str,
}

#[async_trait::async_trait]
impl Tool for FailingTool {
    fn definition(&self) -> ToolDefinition {
        test_tool_definition(self.name)
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::Execution(format!("{} failed", self.name)))
    }
}

fn test_tool_definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("test tool {name}"),
        parameters: serde_json::json!({"type": "object"}),
        origin: ToolOrigin::Builtin,
    }
}

fn success_tool_result(content: &str) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        error: None,
        metadata: Default::default(),
        duration_ms: None,
    }
}

fn test_registry(tools: Vec<Arc<dyn Tool>>) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    Arc::new(registry)
}

fn test_context_assembler() -> Arc<astrcode_context::manager::LlmContextAssembler> {
    Arc::new(astrcode_context::manager::LlmContextAssembler::new(
        astrcode_context::settings::ContextWindowSettings::default(),
    ))
}

fn test_services<L>(
    llm: Arc<L>,
    tool_registry: Arc<ToolRegistry>,
    extension_runner: Arc<ExtensionRunner>,
) -> AgentServices
where
    L: LlmProvider + 'static,
{
    AgentServices {
        llm,
        tool_registry,
        extension_runner,
        context_assembler: test_context_assembler(),
    }
}

fn tool_result_contents(messages: &[LlmMessage]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            LlmContent::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

fn message_text_contains(messages: &[LlmMessage], needle: &str) -> bool {
    messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|content| matches!(content, LlmContent::Text { text } if text.contains(needle)))
    })
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
            max_input_tokens: 200000,
            max_output_tokens: 200000,
        }
    }
}

#[tokio::test]
async fn parallel_tools_in_same_batch_overlap() {
    let barrier = Arc::new(Barrier::new(2));
    let tool_registry = test_registry(vec![
        Arc::new(BarrierTool {
            name: "first",
            barrier: Arc::clone(&barrier),
        }),
        Arc::new(BarrierTool {
            name: "second",
            barrier,
        }),
    ]);

    let agent = Agent::new(
        "session-1".into(),
        ".".into(),
        String::new(),
        "mock".into(),
        test_services(
            Arc::new(ToolCallsThenFinalLlm {
                call_count: AtomicUsize::new(0),
                calls: vec![("call-1", "first"), ("call-2", "second")],
                captured_messages: Arc::new(Mutex::new(Vec::new())),
            }),
            tool_registry,
            Arc::new(ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
            )),
        ),
    );

    timeout(
        Duration::from_secs(2),
        agent.process_prompt("run tools", vec![], None),
    )
    .await
    .expect("parallel tools should not deadlock")
    .unwrap();
}

#[tokio::test]
async fn sequential_tool_splits_parallel_batches() {
    let marker = Arc::new(AtomicBool::new(false));
    let violation = Arc::new(AtomicBool::new(false));
    let tool_registry = test_registry(vec![
        Arc::new(DelayTool {
            name: "before",
            mode: ExecutionMode::Parallel,
            delay_ms: 0,
        }),
        Arc::new(MarkerTool {
            name: "seq",
            mode: ExecutionMode::Sequential,
            marker: Arc::clone(&marker),
            violation: Arc::clone(&violation),
        }),
        Arc::new(MarkerTool {
            name: "after",
            mode: ExecutionMode::Parallel,
            marker,
            violation: Arc::clone(&violation),
        }),
    ]);

    let agent = Agent::new(
        "session-1".into(),
        ".".into(),
        String::new(),
        "mock".into(),
        test_services(
            Arc::new(ToolCallsThenFinalLlm {
                call_count: AtomicUsize::new(0),
                calls: vec![("call-1", "before"), ("call-2", "seq"), ("call-3", "after")],
                captured_messages: Arc::new(Mutex::new(Vec::new())),
            }),
            tool_registry,
            Arc::new(ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
            )),
        ),
    );

    agent
        .process_prompt("run tools", vec![], None)
        .await
        .unwrap();

    assert!(
        !violation.load(Ordering::SeqCst),
        "parallel tool after a sequential tool must not start before the sequential barrier"
    );
}

#[tokio::test]
async fn parallel_results_are_committed_in_model_order() {
    let tool_registry = test_registry(vec![
        Arc::new(DelayTool {
            name: "slow",
            mode: ExecutionMode::Parallel,
            delay_ms: 80,
        }),
        Arc::new(DelayTool {
            name: "fast",
            mode: ExecutionMode::Parallel,
            delay_ms: 0,
        }),
    ]);
    let captured_messages = Arc::new(Mutex::new(Vec::new()));

    let agent = Agent::new(
        "session-1".into(),
        ".".into(),
        String::new(),
        "mock".into(),
        test_services(
            Arc::new(ToolCallsThenFinalLlm {
                call_count: AtomicUsize::new(0),
                calls: vec![("call-1", "slow"), ("call-2", "fast")],
                captured_messages: Arc::clone(&captured_messages),
            }),
            tool_registry,
            Arc::new(ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
            )),
        ),
    );

    agent
        .process_prompt("run tools", vec![], None)
        .await
        .unwrap();

    let messages = captured_messages.lock().unwrap();
    assert_eq!(
        tool_result_contents(&messages),
        vec![String::from("slow"), String::from("fast")]
    );
}

#[tokio::test]
async fn parallel_failure_does_not_drop_sibling_result() {
    let tool_registry = test_registry(vec![
        Arc::new(FailingTool { name: "fail" }),
        Arc::new(DelayTool {
            name: "ok",
            mode: ExecutionMode::Parallel,
            delay_ms: 0,
        }),
    ]);
    let captured_messages = Arc::new(Mutex::new(Vec::new()));

    let agent = Agent::new(
        "session-1".into(),
        ".".into(),
        String::new(),
        "mock".into(),
        test_services(
            Arc::new(ToolCallsThenFinalLlm {
                call_count: AtomicUsize::new(0),
                calls: vec![("call-1", "fail"), ("call-2", "ok")],
                captured_messages: Arc::clone(&captured_messages),
            }),
            tool_registry,
            Arc::new(ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
            )),
        ),
    );

    agent
        .process_prompt("run tools", vec![], None)
        .await
        .unwrap();

    let messages = captured_messages.lock().unwrap();
    let contents = tool_result_contents(&messages);
    assert_eq!(contents.len(), 2);
    assert!(contents[0].contains("fail failed"));
    assert_eq!(contents[1], "ok");
}

#[tokio::test]
async fn blocked_pre_tool_use_emits_completed_event_and_preserves_message_order() {
    let executed = Arc::new(AtomicBool::new(false));
    let tool_registry = test_registry(vec![Arc::new(PanicIfExecutedTool {
        executed: Arc::clone(&executed),
    })]);

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
        String::new(),
        "mock".into(),
        test_services(
            Arc::new(ToolThenFinalLlm {
                call_count: AtomicUsize::new(0),
            }),
            tool_registry,
            extension_runner,
        ),
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

#[tokio::test]
async fn session_system_prompt_is_sent_to_llm() {
    let captured_messages = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        "session-1".into(),
        ".".into(),
        "test system prompt".to_string(),
        "mock".into(),
        test_services(
            Arc::new(CapturingLlm {
                messages: Arc::clone(&captured_messages),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
            )),
        ),
    );

    let output = agent.process_prompt("hello", vec![], None).await.unwrap();

    assert_eq!(output.text, "ok");
    let messages = captured_messages.lock().unwrap();
    assert_eq!(
        messages.first().map(|message| &message.role),
        Some(&LlmRole::System)
    );
    assert!(messages.first().is_some_and(|message| {
        message.content.iter().any(
            |content| matches!(content, LlmContent::Text { text } if text == "test system prompt"),
        )
    }));
    assert!(messages.iter().any(|message| message.role == LlmRole::User));
}

#[tokio::test]
async fn provider_hooks_receive_tools_and_chain_message_updates() {
    let captured_messages = Arc::new(Mutex::new(Vec::new()));
    let extension_runner = Arc::new(ExtensionRunner::new(
        Duration::from_secs(1),
        Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
    ));
    extension_runner
        .register(Arc::new(ProviderMessageExtension {
            id: "provider-first",
            text: "first provider note",
            required_tool: Some("visible"),
        }))
        .await;
    extension_runner
        .register(Arc::new(ProviderMessageExtension {
            id: "provider-second",
            text: "second provider note",
            required_tool: None,
        }))
        .await;

    let agent = Agent::new(
        "provider-hook-session".into(),
        std::env::temp_dir()
            .join("astrcode-provider-hook-chain")
            .to_string_lossy()
            .to_string(),
        String::new(),
        "mock".into(),
        test_services(
            Arc::new(CapturingLlm {
                messages: Arc::clone(&captured_messages),
            }),
            test_registry(vec![Arc::new(DelayTool {
                name: "visible",
                mode: ExecutionMode::Sequential,
                delay_ms: 0,
            })]),
            extension_runner,
        ),
    );

    let output = agent.process_prompt("hello", vec![], None).await.unwrap();
    let messages = captured_messages.lock().unwrap();

    assert_eq!(output.text, "ok");
    assert!(message_text_contains(&messages, "first provider note"));
    assert!(message_text_contains(&messages, "second provider note"));
}

#[tokio::test]
async fn prompt_too_long_compacts_and_retries_once() {
    let captured_messages = Arc::new(Mutex::new(Vec::new()));
    let llm = Arc::new(OverflowThenOkLlm {
        call_count: AtomicUsize::new(0),
        captured_messages: Arc::clone(&captured_messages),
    });
    let agent = Agent::new(
        "overflow-session".into(),
        ".".into(),
        String::new(),
        "mock".into(),
        test_services(
            llm.clone(),
            Arc::new(ToolRegistry::new()),
            Arc::new(ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
            )),
        ),
    );
    let mut history = Vec::new();
    for index in 0..6 {
        history.push(LlmMessage::user(format!("old user {index}")));
        history.push(LlmMessage::assistant(format!("old answer {index}")));
    }
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    let output = agent
        .process_prompt("current", history, Some(event_tx))
        .await
        .unwrap();

    assert_eq!(output.text, "recovered");
    assert_eq!(llm.call_count.load(Ordering::SeqCst), 3);
    assert!(
        captured_messages
            .lock()
            .unwrap()
            .iter()
            .any(|message| message_text_contains(
                std::slice::from_ref(message),
                "<compact_summary>"
            ))
    );
    let mut saw_compaction = false;
    let mut saw_error = false;
    while let Ok(payload) = event_rx.try_recv() {
        match payload {
            EventPayload::CompactionCompleted { .. } => saw_compaction = true,
            EventPayload::ErrorOccurred { .. } => saw_error = true,
            _ => {},
        }
    }
    assert!(saw_compaction);
    assert!(!saw_error);
}
