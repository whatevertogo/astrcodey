//! Session 跨实例恢复时 extra_system_prompt 不丢失。

use std::sync::Arc;

use astrcode_core::{
    event::Phase,
    extension::{ExtensionError, PromptBuildContext, PromptContributions, SessionToolSelection},
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    storage::EventStore,
    tool::{
        ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolOrigin,
        ToolResult,
    },
    types::{ToolCallId, new_session_id, new_turn_id},
};
use astrcode_extension_sdk::{
    runtime_ports::{NoopRuntimePorts, PromptContributor},
    tool_pack::{ToolPack, ToolPackScope},
};
use astrcode_session::{
    Session, SessionCreateParams, SessionExtensionPorts, SessionRuntimeServices,
    SessionRuntimeState,
};
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

mod common;

struct UnusedLlm;

struct FailingPromptContributor;

struct ReadWriteToolPack;
struct NamedTool(&'static str);

impl ToolPack for ReadWriteToolPack {
    fn tools(&self, _scope: &ToolPackScope<'_>) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(NamedTool("read")), Arc::new(NamedTool("write"))]
    }
}

#[async_trait::async_trait]
impl Tool for NamedTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            strict: false,
            origin: ToolOrigin::Sdk,
            execution_mode: ExecutionMode::Sequential,
        }
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        unreachable!("selection test does not execute tools")
    }
}

#[async_trait::async_trait]
impl PromptContributor for FailingPromptContributor {
    async fn collect_prompt_contributions(
        &self,
        _ctx: PromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        Err(ExtensionError::Internal(
            "intentional prompt failure".into(),
        ))
    }
}

#[async_trait::async_trait]
impl LlmProvider for UnusedLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        unreachable!("test does not run a turn")
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

fn test_caps() -> Arc<SessionRuntimeServices> {
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    common::test_runtime_services(llm)
}

#[tokio::test]
async fn refresh_prompt_with_none_preserves_existing_extra() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let caps = test_caps();
    let sid = new_session_id();

    // 第一次 — 模拟子会话首次 spawn：runtime 注入 extra，refresh_prompt 显式传入
    let runtime_a = Arc::new(SessionRuntimeState::new(
        caps.llm(),
        caps.small_llm(),
        "mock-model".into(),
    ));
    runtime_a.update_prompt_extra(Some("child agent body".into()));
    let session_a = Session::create_with_params(SessionCreateParams {
        store: Arc::clone(&store),
        session_id: sid.clone(),
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        runtime: Arc::clone(&runtime_a),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();
    let wrote_a = session_a
        .refresh_prompt(".", Some("child agent body"), None)
        .await
        .expect("first refresh_prompt should succeed");
    assert!(wrote_a, "first refresh should write SystemPromptConfigured");

    let state_after_first = session_a.read_model().await.unwrap();
    assert_eq!(
        state_after_first.extra_system_prompt.as_deref(),
        Some("child agent body"),
    );

    // 模拟跨进程重启：丢弃 runtime_a，开新 runtime + Session 实例
    drop(session_a);
    drop(runtime_a);
    let runtime_b = Arc::new(SessionRuntimeState::new(
        caps.llm(),
        caps.small_llm(),
        "mock-model".into(),
    ));
    assert!(runtime_b.prompt_extra().is_none());
    let session_b = Session::open(
        Arc::clone(&store),
        sid.clone(),
        Arc::clone(&runtime_b),
        Arc::clone(&caps),
    )
    .await
    .unwrap();
    // handler 风格的调用 — extra=None，期望「保留」从 projection 恢复
    let stored_fp = state_after_first.system_prompt_fingerprint.clone();
    let wrote_b = session_b
        .refresh_prompt(".", None, stored_fp.as_deref())
        .await
        .expect("second refresh_prompt should succeed");
    assert!(
        !wrote_b,
        "fingerprint hit should skip writing a new SystemPromptConfigured event",
    );

    // 关键断言：projection 仍然带着 extra；runtime 被恢复
    let state_after_second = session_b.read_model().await.unwrap();
    assert_eq!(
        state_after_second.extra_system_prompt.as_deref(),
        Some("child agent body"),
        "extra_system_prompt must survive refresh_prompt(None) on a reopened session",
    );
    assert_eq!(
        runtime_b.prompt_extra().as_deref(),
        Some("child agent body"),
        "runtime_b should be hydrated from projection",
    );
}

#[tokio::test]
async fn child_tool_selection_stays_within_parent_boundary_and_survives_reopen() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    let caps =
        common::test_runtime_services_with_tool_packs(llm, vec![Arc::new(ReadWriteToolPack)]);
    let parent_selection = SessionToolSelection::Only {
        names: vec!["write".into(), "read".into()],
    };
    let parent = Session::create_with_params(SessionCreateParams {
        store: Arc::clone(&store),
        session_id: new_session_id(),
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: Some(parent_selection),
        source_extension: None,
        runtime: Arc::new(SessionRuntimeState::new(
            caps.llm(),
            caps.small_llm(),
            "mock-model".into(),
        )),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    let direct_child = Session::create_with_params(SessionCreateParams {
        store: Arc::clone(&store),
        session_id: new_session_id(),
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: Some(parent.id().clone()),
        tool_selection: None,
        source_extension: None,
        runtime: Arc::new(SessionRuntimeState::new(
            caps.llm(),
            caps.small_llm(),
            "mock-model".into(),
        )),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();
    assert_eq!(
        direct_child.read_model().await.unwrap().tool_selection,
        Some(SessionToolSelection::Only {
            names: vec!["read".into(), "write".into()]
        }),
        "every child creation path must persist the inherited parent boundary",
    );

    let child = parent
        .spawn_child(
            ".",
            "mock-model",
            "worker".into(),
            "test selection boundary".into(),
            None,
            Some(SessionToolSelection::All {
                except: vec!["read".into()],
            }),
            None,
            ToolCallId::new("call-1"),
        )
        .await
        .unwrap();
    assert_eq!(
        child.read_model().await.unwrap().tool_selection,
        Some(SessionToolSelection::Only {
            names: vec!["write".into()]
        })
    );

    let effective = child
        .configure_tools(SessionToolSelection::All {
            except: vec!["write".into()],
        })
        .await
        .unwrap();
    assert_eq!(
        effective,
        SessionToolSelection::Only {
            names: vec!["read".into()]
        }
    );

    let child_id = child.id().clone();
    drop(child);
    let reopened = Session::open(
        Arc::clone(&store),
        child_id,
        Arc::new(SessionRuntimeState::new(
            caps.llm(),
            caps.small_llm(),
            "mock-model".into(),
        )),
        caps,
    )
    .await
    .unwrap();
    assert_eq!(
        reopened.read_model().await.unwrap().tool_selection,
        Some(SessionToolSelection::Only {
            names: vec!["read".into()]
        })
    );

    parent
        .configure_tools(SessionToolSelection::Only {
            names: vec!["write".into()],
        })
        .await
        .unwrap();
    let effective_registry = reopened.tool_registry_snapshot(".").await.unwrap();
    assert!(
        effective_registry.list_definitions().is_empty(),
        "a reopened child must not retain tools removed from its parent boundary",
    );
}

#[tokio::test]
async fn turn_setup_failure_returns_session_to_idle() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    let noop = Arc::new(NoopRuntimePorts);
    let caps = common::test_runtime_services_with_extensions(
        Arc::clone(&llm),
        SessionExtensionPorts::from_immutable_ports(
            noop.clone(),
            Arc::new(FailingPromptContributor),
            noop.clone(),
            noop,
        ),
    );
    let session = Session::create_with_params(SessionCreateParams {
        store: Arc::clone(&store),
        session_id: new_session_id(),
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        runtime: Arc::new(SessionRuntimeState::new(
            llm,
            caps.small_llm(),
            "mock-model".into(),
        )),
        runtime_services: caps,
    })
    .await
    .unwrap();

    let error = match session
        .submit("hello".into(), Vec::new(), new_turn_id())
        .await
    {
        Ok(_) => panic!("prompt contribution failure must reject turn setup"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("intentional prompt failure"));

    let model = session.read_model().await.unwrap();
    assert_eq!(model.phase, Phase::Idle);
    assert!(model.pending_tool_calls.is_empty());
}
