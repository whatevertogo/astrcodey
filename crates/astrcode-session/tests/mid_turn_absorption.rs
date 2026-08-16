//! Mid-turn 输入的 accepted→absorbed 管线回归(写入侧协议合法性)。
//!
//! 事故场景:工具轮次未结算时用户插话,若 `UserMessage` 直接按到达顺序落盘,会把
//! transcript 切成「assistant tool_calls → user → tool results」的非法序列。接受与吸收
//! 分离后,`UserMessage` 只能在 step 边界(工具结果配对落盘之后)由被归属的 turn 提交。

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use astrcode_core::{
    event::DurableEventPayload,
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits},
    tool::{
        ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolOrigin,
        access::ToolPlan,
    },
    types::{TurnId, new_turn_id},
    user_input::UserInput,
};
use astrcode_extension_sdk::{
    extension::ExtensionError,
    runtime_ports::{ToolCatalogProvider, ToolCatalogScope, ToolCatalogSnapshot},
};
use astrcode_session::{Session, SessionCreateParams, SessionExtensionPorts, SessionRuntimeState};
use astrcode_storage::{SessionStore, in_memory::InMemoryEventStore};
use tokio::sync::{Semaphore, mpsc};

mod common;

struct SlowTool {
    release: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl Tool for SlowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "slow_tool".into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            strict: false,
            origin: ToolOrigin::Extension,
            execution_mode: ExecutionMode::Sequential,
            timeout_ms: None,
        }
    }

    async fn plan(
        &self,
        _arguments: &serde_json::Value,
        _ctx: &astrcode_core::tool::ToolPlanningContext,
    ) -> Result<ToolPlan, ToolError> {
        Ok(ToolPlan::default())
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<astrcode_core::tool::ToolExecutionResult, ToolError> {
        self.release.acquire().await.unwrap().forget();
        Ok(astrcode_core::tool::ToolResult::success("slow result").into())
    }
}

struct SlowToolCatalog {
    release: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl ToolCatalogProvider for SlowToolCatalog {
    fn revision(&self) -> u64 {
        1
    }

    async fn tool_catalog(
        &self,
        _scope: &ToolCatalogScope,
    ) -> Result<ToolCatalogSnapshot, ExtensionError> {
        Ok(ToolCatalogSnapshot::complete(
            self.revision(),
            vec![Arc::new(SlowTool {
                release: Arc::clone(&self.release),
            })],
        ))
    }
}

/// 第一轮请求一个慢工具;后续轮记录 provider 可见请求消息后正常收尾。
struct ToolThenRecordLlm {
    calls: AtomicUsize,
    recorded_requests: Mutex<Vec<Vec<LlmMessage>>>,
}

#[async_trait::async_trait]
impl LlmProvider for ToolThenRecordLlm {
    async fn generate_request(
        &self,
        request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        self.recorded_requests.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| (**message).clone())
                .collect(),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        if round == 0 {
            let _ = tx.send(LlmEvent::ToolCallStart {
                call_id: "call-slow".into(),
                name: "slow_tool".into(),
                arguments: "{}".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "tool_calls".into(),
            });
        } else {
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "final answer".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 4096,
        }
    }
}

/// 首次调用挂起直到放行,用于让 turn 在注入窗口内保持活跃。
struct GatedStopLlm {
    calls: AtomicUsize,
    release: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl LlmProvider for GatedStopLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.release.acquire().await.unwrap().forget();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: "done".into(),
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 4096,
        }
    }
}

async fn spawn_session_with_services(
    llm: Arc<dyn LlmProvider>,
    tool_catalog: Arc<dyn ToolCatalogProvider>,
) -> (Session, Arc<dyn SessionStore>) {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    // 禁用 auto-compact:本测试断言精确的 provider 请求轮次,压缩会重写 transcript。
    let context = astrcode_core::config::ContextSettings {
        auto_compact_enabled: false,
        ..Default::default()
    };
    let caps = common::test_runtime_services_with_context_and_extensions(
        llm,
        context,
        SessionExtensionPorts::default(),
        Some(tool_catalog),
    );
    let sid = astrcode_core::types::new_session_id();
    let runtime = Arc::new(SessionRuntimeState::new(sid.clone(), store.clone()));
    let working_dir = std::env::temp_dir().join(sid.as_str());
    std::fs::create_dir_all(&working_dir).unwrap();
    let session = Session::create_with_params(SessionCreateParams {
        working_dir: working_dir.to_string_lossy().into_owned(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime,
        runtime_services: caps,
    })
    .await
    .unwrap();
    (session, store)
}

async fn wait_for_event(
    store: &Arc<dyn SessionStore>,
    sid: &astrcode_core::types::SessionId,
    matches: impl Fn(&DurableEventPayload) -> bool,
) {
    for _ in 0..200 {
        let events = store.replay_events(sid).await.unwrap();
        if events.iter().any(|event| matches(&event.payload)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for durable event");
}

#[tokio::test]
async fn mid_turn_input_is_absorbed_after_tool_round_settles() {
    let tool_release = Arc::new(Semaphore::new(0));
    let llm = Arc::new(ToolThenRecordLlm {
        calls: AtomicUsize::new(0),
        recorded_requests: Mutex::new(Vec::new()),
    });
    let provider: Arc<dyn LlmProvider> = llm.clone();
    let (session, store) = spawn_session_with_services(
        provider,
        Arc::new(SlowToolCatalog {
            release: Arc::clone(&tool_release),
        }),
    )
    .await;
    let sid = session.id().clone();

    let turn_id = new_turn_id();
    let handle = session
        .submit("run the slow tool".into(), turn_id.clone(), None)
        .await
        .unwrap();

    // 工具执行中(轮次未结算)注入:只落 UserInputAccepted,不进 transcript。
    wait_for_event(&store, &sid, |payload| {
        matches!(payload, DurableEventPayload::ToolCallRequested { .. })
    })
    .await;
    let accepted = session
        .emit_durable(
            Some(&turn_id),
            DurableEventPayload::UserInputAccepted {
                input: UserInput::text_only("steer note"),
            },
        )
        .await
        .unwrap();
    let model = session.read_model().await.unwrap();
    assert!(
        !model.model_context.messages.iter().any(|message| {
            message.message.role == LlmRole::User
                && message.message.joined_display_text("\n") == "steer note"
        }),
        "accepted input must not enter the transcript before absorption"
    );
    let pending = &model.execution.pending_inputs;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].turn_id.as_ref(), Some(&turn_id));
    assert_eq!(pending[0].accepted_seq, accepted.seq);

    tool_release.add_permits(1);
    let result = handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);

    let events = store.replay_events(&sid).await.unwrap();
    let seq_of = |matches: &dyn Fn(&DurableEventPayload) -> bool| {
        events
            .iter()
            .find(|event| matches(&event.payload))
            .map(|event| event.seq)
            .unwrap()
    };
    let tool_completed_seq =
        seq_of(&|payload| matches!(payload, DurableEventPayload::ToolCallCompleted { .. }));
    let absorbed = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                DurableEventPayload::UserMessage { text, .. } if text == "steer note"
            )
        })
        .expect("absorbed UserMessage must be durable");
    assert!(
        absorbed.seq > tool_completed_seq,
        "UserMessage must land after the tool round settled"
    );
    assert_eq!(absorbed.turn_id.as_ref(), Some(&turn_id));
    let DurableEventPayload::UserMessage { accepted_seq, .. } = &absorbed.payload else {
        unreachable!()
    };
    assert_eq!(
        *accepted_seq,
        Some(accepted.seq),
        "absorption must link back to the accepted seq"
    );
    assert!(
        session
            .read_model()
            .await
            .unwrap()
            .execution
            .pending_inputs
            .is_empty(),
        "absorption must consume the pending input"
    );

    // 第二轮 provider 请求必须看到完整且协议合法的上下文:
    // assistant tool_calls 与其 tool results 之间不得插入 user 消息。
    let requests = llm.recorded_requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "tool round then final answer");
    let messages = &requests[1];
    let assistant_pos = messages
        .iter()
        .position(|message| {
            message.role == LlmRole::Assistant
                && message.content.iter().any(|content| {
                    matches!(content, LlmContent::ToolCall { call_id, .. } if call_id == "call-slow")
                })
        })
        .expect("assistant tool call must be visible to the provider");
    assert!(
        matches!(
            messages.get(assistant_pos + 1),
            Some(message) if message.role == LlmRole::Tool
        ),
        "tool result must immediately follow the assistant tool call: {messages:?}"
    );
    let note_pos = messages
        .iter()
        .position(|message| {
            message.role == LlmRole::User && message.joined_display_text("\n") == "steer note"
        })
        .expect("absorbed input must be visible to the provider");
    assert!(
        note_pos > assistant_pos + 1,
        "user message must appear after the tool result, never inside the tool round"
    );
}

#[tokio::test]
async fn accepted_input_attributed_to_another_turn_is_not_absorbed() {
    let llm_release = Arc::new(Semaphore::new(0));
    let llm = Arc::new(GatedStopLlm {
        calls: AtomicUsize::new(0),
        release: Arc::clone(&llm_release),
    });
    let provider: Arc<dyn LlmProvider> = llm.clone();
    let (session, store) = spawn_session_with_services(
        provider,
        Arc::new(SlowToolCatalog {
            release: Arc::new(Semaphore::new(0)),
        }),
    )
    .await;
    let sid = session.id().clone();

    let turn_id = new_turn_id();
    let handle = session
        .submit("start turn a".into(), turn_id.clone(), None)
        .await
        .unwrap();
    for _ in 0..200 {
        if llm.calls.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        llm.calls.load(Ordering::SeqCst) > 0,
        "turn a must be active"
    );

    // 归属给其它(已结束)turn 的 accepted 输入:turn A 不得吸收。
    let foreign_turn: TurnId = new_turn_id();
    session
        .emit_durable(
            Some(&foreign_turn),
            DurableEventPayload::UserInputAccepted {
                input: UserInput::text_only("attributed to a finished turn"),
            },
        )
        .await
        .unwrap();

    llm_release.add_permits(1);
    let result = handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);

    let events = store.replay_events(&sid).await.unwrap();
    assert!(
        !events.iter().any(|event| matches!(
            &event.payload,
            DurableEventPayload::UserMessage { text, .. }
                if text == "attributed to a finished turn"
        )),
        "turn A must not absorb an input attributed to another turn"
    );
    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        1,
        "no extra step should run for foreign-attributed input"
    );
    let pending = &session.read_model().await.unwrap().execution.pending_inputs;
    assert_eq!(
        pending.len(),
        1,
        "foreign input stays queued for FIFO start"
    );
    assert_eq!(pending[0].turn_id.as_ref(), Some(&foreign_turn));
}
