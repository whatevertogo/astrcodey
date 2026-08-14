//! Session 跨实例恢复时 extra_system_prompt 不丢失。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use astrcode_core::{
    event::{PersistedSystemPrompt, SystemPromptSource},
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    tool::{
        ExecutionMode, SessionToolSelection, Tool, ToolDefinition, ToolError, ToolExecutionContext,
        ToolOrigin,
    },
    types::{SessionId, ToolCallId, new_session_id},
};
use astrcode_extension_sdk::{
    extension::{
        ExtensionError, LifecycleEvent, PromptContributions, RuntimeLifecycleContext,
        RuntimePromptBuildContext,
    },
    runtime_ports::{
        NoopRuntimePorts, PromptContributor, ToolCatalogProvider, ToolCatalogScope,
        ToolCatalogSnapshot, TurnHooks,
    },
};
use astrcode_session::{
    Session, SessionCreateParams, SessionExtensionPorts, SessionRuntimeServices,
    SessionRuntimeState,
};
use astrcode_storage::{SessionStore, in_memory::InMemoryEventStore};
use tokio::sync::mpsc;

mod common;

struct UnusedLlm;

struct FailingPromptContributor;

struct ReadWriteToolCatalog;
struct NamedTool(&'static str);
struct RecordingLifecycleHooks(AtomicUsize);

#[async_trait::async_trait]
impl TurnHooks for RecordingLifecycleHooks {
    async fn emit_lifecycle(
        &self,
        event: LifecycleEvent,
        _ctx: RuntimeLifecycleContext,
    ) -> Result<(), ExtensionError> {
        if event == LifecycleEvent::SessionStart {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ToolCatalogProvider for ReadWriteToolCatalog {
    fn revision(&self) -> u64 {
        1
    }

    async fn tool_catalog(
        &self,
        _scope: &ToolCatalogScope,
    ) -> Result<ToolCatalogSnapshot, ExtensionError> {
        Ok(ToolCatalogSnapshot::complete(
            self.revision(),
            vec![Arc::new(NamedTool("read")), Arc::new(NamedTool("write"))],
        ))
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
        _ctx: &ToolExecutionContext,
    ) -> Result<astrcode_core::tool::ToolExecutionResult, ToolError> {
        unreachable!("selection test does not execute tools")
    }
}

#[async_trait::async_trait]
impl PromptContributor for FailingPromptContributor {
    async fn collect_prompt_contributions(
        &self,
        _ctx: RuntimePromptBuildContext,
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

fn runtime(session_id: SessionId, store: &Arc<dyn SessionStore>) -> Arc<SessionRuntimeState> {
    Arc::new(SessionRuntimeState::new(session_id, store.clone()))
}

#[tokio::test]
async fn reopen_restores_native_extra_system_prompt() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let caps = test_caps();
    let sid = new_session_id();

    let runtime_a = runtime(sid.clone(), &store);
    let session_a = Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: Some("child agent body".into()),
        initial_system_prompt: None,
        runtime: Arc::clone(&runtime_a),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    let state_after_first = session_a.read_model().await.unwrap();
    assert_eq!(
        state_after_first.system_prompt.extra.as_deref(),
        Some("child agent body"),
    );

    // 模拟跨进程重启：丢弃 runtime_a，开新 runtime + Session 实例
    drop(session_a);
    drop(runtime_a);
    let runtime_b = runtime(sid.clone(), &store);
    let session_b = Session::open(Arc::clone(&runtime_b), Arc::clone(&caps))
        .await
        .unwrap();
    let state_after_second = session_b.read_model().await.unwrap();
    assert_eq!(
        state_after_second.system_prompt.extra.as_deref(),
        Some("child agent body"),
        "extra_system_prompt must survive reopening the session",
    );
}

#[tokio::test]
async fn child_tool_selection_stays_within_parent_boundary_and_survives_reopen() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    let caps = common::test_runtime_services_with_tool_catalog(llm, Arc::new(ReadWriteToolCatalog));
    let parent_selection = SessionToolSelection::Only {
        names: vec!["write".into(), "read".into()],
    };
    let parent_id = new_session_id();
    let parent = Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: Some(parent_selection),
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime: runtime(parent_id, &store),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    let direct_child_id = new_session_id();
    let direct_child = Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: Some(parent.id().clone()),
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime: runtime(direct_child_id, &store),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();
    assert_eq!(
        direct_child
            .read_model()
            .await
            .unwrap()
            .identity
            .tool_selection,
        SessionToolSelection::Only {
            names: vec!["read".into(), "write".into()]
        },
        "every child creation path must persist the inherited parent boundary",
    );

    let child = parent
        .spawn_child(astrcode_session::SpawnChildParams {
            working_dir: ".".into(),
            model_id: "mock-model".into(),
            agent_name: "worker".into(),
            task: "test selection boundary".into(),
            extra_system_prompt: None,
            tool_selection: Some(SessionToolSelection::All {
                except: vec!["read".into()],
            }),
            source_extension: None,
            tool_call_id: Some(ToolCallId::new("call-1")),
        })
        .await
        .unwrap();
    assert_eq!(
        child.read_model().await.unwrap().identity.tool_selection,
        SessionToolSelection::Only {
            names: vec!["write".into()]
        }
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
    let reopened = Session::open(runtime(child_id, &store), caps)
        .await
        .unwrap();
    assert_eq!(
        reopened.read_model().await.unwrap().identity.tool_selection,
        SessionToolSelection::Only {
            names: vec!["read".into()]
        }
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
async fn parent_and_spawned_child_each_emit_session_start_once() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    let hooks = Arc::new(RecordingLifecycleHooks(AtomicUsize::new(0)));
    let noop = Arc::new(NoopRuntimePorts);
    let caps = common::test_runtime_services_with_extensions(
        llm,
        SessionExtensionPorts::from_immutable_ports(
            noop.clone(),
            noop.clone(),
            hooks.clone(),
            noop,
        ),
    );
    let parent_id = new_session_id();
    let parent = Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime: runtime(parent_id, &store),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    parent
        .ensure_lifecycle_initialized(LifecycleEvent::SessionStart)
        .await
        .unwrap();
    parent
        .ensure_lifecycle_initialized(LifecycleEvent::SessionResume)
        .await
        .unwrap();
    let child = parent
        .spawn_child(astrcode_session::SpawnChildParams {
            working_dir: ".".into(),
            model_id: "mock-model".into(),
            agent_name: "worker".into(),
            task: "verify lifecycle".into(),
            extra_system_prompt: None,
            tool_selection: None,
            source_extension: None,
            tool_call_id: Some(ToolCallId::new("call-lifecycle")),
        })
        .await
        .unwrap();
    child
        .ensure_lifecycle_initialized(LifecycleEvent::SessionResume)
        .await
        .unwrap();

    assert_eq!(hooks.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn prompt_failure_does_not_create_session() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
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
    let session_id = new_session_id();
    let error = match Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime: runtime(session_id, &store),
        runtime_services: caps,
    })
    .await
    {
        Ok(_) => panic!("prompt contribution failure must reject session creation"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("intentional prompt failure"));
    assert!(store.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn inherited_initial_prompt_survives_initialization_and_reopen() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    let caps = common::test_runtime_services(Arc::clone(&llm));
    let session_id = new_session_id();
    let inherited = PersistedSystemPrompt {
        text: "[Identity]\n  inherited".into(),
        fingerprint: "inherited-fingerprint".into(),
        extra_system_prompt: Some("child body".into()),
        source: SystemPromptSource::Inherited,
    };
    let session = Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: Some(inherited.clone()),
        runtime: runtime(session_id.clone(), &store),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    assert_eq!(store.replay_events(&session_id).await.unwrap().len(), 1);
    let model = session.read_model().await.unwrap();
    assert_eq!(model.system_prompt.text, inherited.text);
    assert_eq!(model.system_prompt.source, SystemPromptSource::Inherited);

    let reopened = Session::open(runtime(session_id, &store), caps)
        .await
        .unwrap();
    let reopened_model = reopened.read_model().await.unwrap();
    assert_eq!(
        reopened_model.system_prompt.extra.as_deref(),
        inherited.extra_system_prompt.as_deref()
    );
    assert_eq!(
        reopened_model.system_prompt.source,
        SystemPromptSource::Inherited
    );
}
