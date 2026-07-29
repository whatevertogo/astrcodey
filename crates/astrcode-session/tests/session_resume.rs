//! Session 跨实例恢复时 extra_system_prompt 不丢失。

use std::sync::Arc;

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
    extension::{ExtensionError, PromptBuildContext, PromptContributions},
    runtime_ports::{
        NoopRuntimePorts, PromptContributor, ToolCatalogProvider, ToolCatalogScope,
        ToolCatalogSnapshot,
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
            origin: ToolOrigin::Sdk,
            execution_mode: ExecutionMode::Sequential,
        }
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

fn runtime(
    session_id: SessionId,
    store: &Arc<dyn SessionStore>,
    caps: &SessionRuntimeServices,
) -> Arc<SessionRuntimeState> {
    Arc::new(SessionRuntimeState::new(
        session_id,
        store.clone(),
        caps.llm(),
        caps.small_llm(),
        "mock-model".into(),
    ))
}

#[tokio::test]
async fn reopen_restores_native_extra_system_prompt() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let caps = test_caps();
    let sid = new_session_id();

    let runtime_a = runtime(sid.clone(), &store, &caps);
    runtime_a.update_prompt_extra(Some("child agent body".into()));
    let session_a = Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
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
    let runtime_b = runtime(sid.clone(), &store, &caps);
    assert!(runtime_b.prompt_extra().is_none());
    let session_b = Session::open(Arc::clone(&runtime_b), Arc::clone(&caps))
        .await
        .unwrap();
    let state_after_second = session_b.read_model().await.unwrap();
    assert_eq!(
        state_after_second.system_prompt.extra.as_deref(),
        Some("child agent body"),
        "extra_system_prompt must survive reopening the session",
    );
    assert_eq!(
        runtime_b.prompt_extra().as_deref(),
        Some("child agent body"),
        "runtime_b should be hydrated from projection",
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
        parent_session_id: None,
        tool_selection: Some(parent_selection),
        source_extension: None,
        initial_system_prompt: None,
        runtime: runtime(parent_id, &store, &caps),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    let direct_child_id = new_session_id();
    let direct_child = Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        parent_session_id: Some(parent.id().clone()),
        tool_selection: None,
        source_extension: None,
        initial_system_prompt: None,
        runtime: runtime(direct_child_id, &store, &caps),
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
    let reopened = Session::open(runtime(child_id, &store, &caps), caps)
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
async fn prompt_failure_does_not_create_session() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
    let noop = Arc::new(NoopRuntimePorts);
    let caps = common::test_runtime_services_with_extensions(
        Arc::clone(&llm),
        SessionExtensionPorts::from_immutable_ports(
            Arc::new(FailingPromptContributor),
            noop.clone(),
            noop,
        ),
    );
    let session_id = new_session_id();
    let error = match Session::create_with_params(SessionCreateParams {
        working_dir: ".".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        initial_system_prompt: None,
        runtime: runtime(session_id, &store, &caps),
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
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        initial_system_prompt: Some(inherited.clone()),
        runtime: runtime(session_id.clone(), &store, &caps),
        runtime_services: Arc::clone(&caps),
    })
    .await
    .unwrap();

    assert_eq!(store.replay_events(&session_id).await.unwrap().len(), 1);
    let model = session.read_model().await.unwrap();
    assert_eq!(model.system_prompt.text, inherited.text);
    assert_eq!(model.system_prompt.source, SystemPromptSource::Inherited);

    let reopened = Session::open(runtime(session_id, &store, &caps), caps)
        .await
        .unwrap();
    assert_eq!(
        reopened.runtime().prompt_extra().as_deref(),
        inherited.extra_system_prompt.as_deref()
    );
    assert_eq!(
        reopened.read_model().await.unwrap().system_prompt.source,
        SystemPromptSource::Inherited
    );
}
