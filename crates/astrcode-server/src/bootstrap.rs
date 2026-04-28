//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 工具路由器、会话管理器、扩展运行器和上下文窗口管理。

use std::{sync::Arc, time::Duration};

use astrcode_ai::openai::OpenAiProvider;
use astrcode_context::{
    budget::ToolResultBudget, file_access::FileAccessTracker, settings::ContextWindowSettings,
};
use astrcode_core::{
    config::{ConfigStore, EffectiveConfig},
    event::{Event, EventPayload, ToolOutputStream},
    llm::{LlmClientConfig, LlmProvider},
    prompt::PromptProvider,
    types::{new_message_id, new_turn_id},
};
use astrcode_extensions::{
    loader::ExtensionLoader,
    runner::ExtensionRunner,
    runtime::{SessionSpawner, SpawnRequest, SpawnResult},
};
use astrcode_storage::config_store::FileConfigStore;
use astrcode_tools::registry::ToolRegistry;
use tokio::sync::mpsc;

use crate::{agent::Agent, capability::CapabilityRouter, session::SessionManager};

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// 启动时组装的所有服务集合，按领域分组。
///
/// 这是服务器运行时的核心容器，持有所有共享服务的引用。
/// 各组件通过 `Arc` 共享，支持并发访问。
pub struct ServerRuntime {
    /// 会话管理器，负责会话的创建、恢复、事件追加和删除
    pub session_manager: Arc<SessionManager>,
    /// LLM 提供者，用于生成 AI 回复
    pub llm_provider: Arc<dyn LlmProvider>,
    /// 提示词组装器，负责构建发送给 LLM 的系统提示词
    pub prompt_provider: Arc<dyn PromptProvider>,
    /// 工具路由器，管理内置工具和扩展工具的注册与调用
    pub capability: Arc<CapabilityRouter>,
    /// 扩展运行器，负责加载和分发扩展钩子事件
    pub extension_runner: Arc<ExtensionRunner>,
    /// 已解析的最终配置（只读快照）
    pub effective: EffectiveConfig,
    /// 上下文窗口管理设置
    pub context_settings: ContextWindowSettings,
    /// 工具结果预算控制器，限制工具返回数据的大小
    pub tool_result_budget: Arc<ToolResultBudget>,
    /// 文件访问追踪器，记录 Agent 访问过的文件
    pub file_access_tracker: Arc<std::sync::Mutex<FileAccessTracker>>,
}

// ─── Bootstrap ───────────────────────────────────────────────────────────

/// 引导选项，支持自定义配置路径和工作目录，主要用于测试。
#[derive(Default)]
pub struct BootstrapOptions {
    /// 自定义配置文件路径，为 None 时使用默认路径
    pub config_path: Option<std::path::PathBuf>,
    /// 自定义工作目录，为 None 时使用当前目录
    pub working_dir: Option<std::path::PathBuf>,
}

/// 使用默认选项引导服务器运行时。
pub async fn bootstrap() -> Result<ServerRuntime, BootstrapError> {
    bootstrap_with(BootstrapOptions::default()).await
}

/// 使用指定选项引导服务器运行时。
///
/// 按顺序完成以下步骤：
/// 1. 加载并解析配置
/// 2. 构建 LLM 提供者
/// 3. 构建提示词组装器
/// 4. 注册内置工具到工具路由器
/// 5. 初始化会话管理器和存储后端
/// 6. 加载扩展并绑定核心服务
/// 7. 初始化上下文窗口管理
pub async fn bootstrap_with(opts: BootstrapOptions) -> Result<ServerRuntime, BootstrapError> {
    // 1. Load + resolve config
    let config_store = if let Some(ref path) = opts.config_path {
        FileConfigStore::new(path.clone())
    } else {
        FileConfigStore::default_path()
    };
    let config = config_store.load().await?;
    let effective = config.into_effective()?;

    // 2. Build LLM provider
    let llm_config = LlmClientConfig {
        base_url: effective.llm.base_url.clone(),
        api_key: effective.llm.api_key.clone(),
        connect_timeout_secs: effective.llm.connect_timeout_secs,
        read_timeout_secs: effective.llm.read_timeout_secs,
        max_retries: effective.llm.max_retries,
        retry_base_delay_ms: effective.llm.retry_base_delay_ms,
        extra_headers: Default::default(),
    };
    let llm_provider: Arc<dyn LlmProvider> = Arc::new(OpenAiProvider::new(
        llm_config,
        effective.llm.api_mode,
        effective.llm.model_id.clone(),
        Some(effective.llm.max_tokens),
        Some(effective.llm.context_limit),
    ));

    // 3. Build prompt provider
    let prompt_provider: Arc<dyn PromptProvider> =
        Arc::new(astrcode_prompt::composer::PromptComposer::new());

    // 4. Build capability router with stable built-in tools
    let cwd = opts
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register_builtins(cwd.clone(), effective.llm.read_timeout_secs);

    let capability = Arc::new(CapabilityRouter::new());
    for tool in tool_registry.into_tools() {
        capability.register_stable(tool).await;
    }

    // 5. Session manager with storage backend
    let project_hash = astrcode_core::types::project_hash_from_path(&cwd);
    let store: Arc<dyn astrcode_core::storage::EventStore> = if opts.config_path.is_some() {
        // Test path → use memory-only store
        Arc::new(astrcode_storage::noop::NoopEventStore::new())
    } else {
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new(project_hash))
    };
    let session_manager = Arc::new(SessionManager::new(store));

    // 6. Extension runner — load from disk then bind core services
    let cwd_str = cwd.to_string_lossy().to_string();
    let load_result = ExtensionLoader::load_all(Some(&cwd_str)).await;
    let extension_runner = Arc::new(ExtensionRunner::new(
        Duration::from_secs(30),
        load_result.runtime,
    ));
    extension_runner
        .register(astrcode_extension_agent_tools::extension())
        .await;
    extension_runner
        .register(astrcode_extension_task_tools::extension())
        .await;
    for ext in load_result.extensions {
        extension_runner.register(ext).await;
    }
    for err in &load_result.errors {
        tracing::warn!("Extension load error: {err}");
    }
    let extension_tools = extension_runner.collect_tool_adapters(&cwd_str).await;
    if !extension_tools.is_empty() {
        capability.apply_dynamic(extension_tools).await;
    }

    // Bind session spawn capability so extensions can request RunSession outcomes.
    extension_runner.bind(Arc::new(ServerSessionSpawner {
        session_manager: Arc::clone(&session_manager),
        llm: Arc::clone(&llm_provider),
        capability: Arc::clone(&capability),
        prompt: Arc::clone(&prompt_provider),
        extension_runner: Arc::clone(&extension_runner),
    }));

    // 7. Context window management
    let context_settings = ContextWindowSettings::default();
    let tool_result_budget = Arc::new(ToolResultBudget::new(
        context_settings.summary_reserve_tokens * 3, // aggregate
        context_settings.max_tracked_files * 1024,   // inline
        context_settings.recovery_token_budget * 3,  // preview
    ));
    let file_access_tracker = Arc::new(std::sync::Mutex::new(FileAccessTracker::new(
        context_settings.max_tracked_files,
    )));

    Ok(ServerRuntime {
        session_manager,
        llm_provider,
        prompt_provider,
        capability,
        extension_runner,
        effective,
        context_settings,
        tool_result_budget,
        file_access_tracker,
    })
}

/// 引导过程中可能出现的错误。
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Config: {0}")]
    Config(#[from] astrcode_core::config::ConfigStoreError),
    #[error("Resolve: {0}")]
    Resolve(#[from] astrcode_core::config::ResolveError),
}

// ─── ServerSessionSpawner ─────────────────────────────────────────────────

/// 服务器端的会话派生器，实现 `SessionSpawner` trait。
///
/// 当扩展返回 `ExtensionToolOutcome::RunSession` 时，
/// 扩展运行器通过此派生器创建子会话并运行 Agent 回合。
struct ServerSessionSpawner {
    session_manager: Arc<SessionManager>,
    llm: Arc<dyn LlmProvider>,
    capability: Arc<CapabilityRouter>,
    prompt: Arc<dyn PromptProvider>,
    extension_runner: Arc<ExtensionRunner>,
}

#[async_trait::async_trait]
impl SessionSpawner for ServerSessionSpawner {
    /// 派生一个子会话，创建 Agent 并执行用户提示词。
    ///
    /// # 参数
    /// - `parent_session_id`: 父会话 ID
    /// - `request`: 派生请求，包含工作目录、提示词、工具白名单等
    ///
    /// # 返回
    /// 子会话的执行结果，包含输出文本和子会话 ID。
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let parent_progress = ParentToolProgress::from_request(&request);
        let child_name = request.name.clone();
        let user_prompt = request.user_prompt.clone();
        let model_id = match request.model_preference.clone() {
            Some(model) => model,
            None => {
                let parent_session = self
                    .session_manager
                    .get(&parent_session_id.to_string())
                    .await
                    .ok_or_else(|| format!("parent session {parent_session_id} not found"))?;
                let parent_model_id = parent_session.state.read().await.model_id.clone();
                parent_model_id
            },
        };

        let create_event = self
            .session_manager
            .create(
                &request.working_dir,
                &model_id,
                2048,
                Some(parent_session_id),
            )
            .await
            .map_err(|e| format!("create child session: {e}"))?;

        let child_sid = create_event.session_id.to_string();
        let child_turn_id = new_turn_id();

        append_child_payload(
            self.session_manager.as_ref(),
            &child_sid,
            &child_turn_id,
            EventPayload::TurnStarted,
        )
        .await?;
        append_child_payload(
            self.session_manager.as_ref(),
            &child_sid,
            &child_turn_id,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: user_prompt.clone(),
            },
        )
        .await?;

        if let Some(progress) = &parent_progress {
            progress.send(
                ToolOutputStream::Stdout,
                format!("child agent '{child_name}' started: {child_sid} using {model_id}\n"),
            );
        }

        let agent = Agent::new(
            child_sid.clone(),
            request.working_dir.clone(),
            Arc::clone(&self.llm),
            Arc::clone(&self.prompt),
            Arc::clone(&self.capability),
            Arc::clone(&self.extension_runner),
            model_id,
            8192,
        )
        .with_system_prompt_suffix(request.system_prompt)
        .with_tool_allowlist(request.allowed_tools);

        let (child_event_tx, mut child_event_rx) = mpsc::unbounded_channel();
        let agent_future = agent.process_prompt(&user_prompt, Vec::new(), Some(child_event_tx));
        tokio::pin!(agent_future);

        let mut emitted_error = false;
        let mut events_closed = false;
        let output = loop {
            tokio::select! {
                result = &mut agent_future => break result,
                payload = child_event_rx.recv(), if !events_closed => {
                    match payload {
                        Some(payload) => {
                            if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                                emitted_error = true;
                            }
                            append_child_payload(
                                self.session_manager.as_ref(),
                                &child_sid,
                                &child_turn_id,
                                payload.clone(),
                            )
                            .await?;
                            forward_child_progress(parent_progress.as_ref(), &payload);
                        },
                        None => {
                            events_closed = true;
                        },
                    }
                },
            }
        };

        while let Some(payload) = child_event_rx.recv().await {
            if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                emitted_error = true;
            }
            append_child_payload(
                self.session_manager.as_ref(),
                &child_sid,
                &child_turn_id,
                payload.clone(),
            )
            .await?;
            forward_child_progress(parent_progress.as_ref(), &payload);
        }

        match output {
            Ok(output) => {
                append_child_payload(
                    self.session_manager.as_ref(),
                    &child_sid,
                    &child_turn_id,
                    EventPayload::TurnCompleted {
                        finish_reason: output.finish_reason.clone(),
                    },
                )
                .await?;
                if let Some(progress) = &parent_progress {
                    progress.send(
                        ToolOutputStream::Stdout,
                        format!("child turn completed: {}\n", output.finish_reason),
                    );
                }
                Ok(SpawnResult {
                    content: output.text,
                    child_session_id: child_sid,
                })
            },
            Err(e) => Ok(SpawnResult {
                content: {
                    if !emitted_error {
                        append_child_payload(
                            self.session_manager.as_ref(),
                            &child_sid,
                            &child_turn_id,
                            EventPayload::ErrorOccurred {
                                code: -32603,
                                message: e.to_string(),
                                recoverable: false,
                            },
                        )
                        .await?;
                    }
                    append_child_payload(
                        self.session_manager.as_ref(),
                        &child_sid,
                        &child_turn_id,
                        EventPayload::TurnCompleted {
                            finish_reason: "error".into(),
                        },
                    )
                    .await?;
                    if let Some(progress) = &parent_progress {
                        progress.send(
                            ToolOutputStream::Stderr,
                            format!("child agent error: {e}\n"),
                        );
                    }
                    format!("child agent error: {e}")
                },
                child_session_id: child_sid,
            }),
        }
    }
}

struct ParentToolProgress {
    call_id: String,
    tx: mpsc::UnboundedSender<EventPayload>,
}

impl ParentToolProgress {
    fn from_request(request: &SpawnRequest) -> Option<Self> {
        Some(Self {
            call_id: request.parent_tool_call_id.clone()?,
            tx: request.parent_event_tx.clone()?,
        })
    }

    fn send(&self, stream: ToolOutputStream, delta: impl Into<String>) {
        let delta = delta.into();
        if delta.is_empty() {
            return;
        }
        let _ = self.tx.send(EventPayload::ToolOutputDelta {
            call_id: self.call_id.clone(),
            stream,
            delta,
        });
    }
}

async fn append_child_payload(
    session_manager: &SessionManager,
    child_sid: &str,
    child_turn_id: &str,
    payload: EventPayload,
) -> Result<(), String> {
    if payload.is_durable() {
        session_manager
            .append_event(Event::new(
                child_sid.to_string(),
                Some(child_turn_id.to_string()),
                payload,
            ))
            .await
            .map_err(|e| format!("append child event: {e}"))?;
    }
    Ok(())
}

fn forward_child_progress(progress: Option<&ParentToolProgress>, payload: &EventPayload) {
    let Some(progress) = progress else {
        return;
    };
    if let Some((stream, delta)) = child_progress_delta(payload) {
        progress.send(stream, delta);
    }
}

fn child_progress_delta(payload: &EventPayload) -> Option<(ToolOutputStream, String)> {
    match payload {
        EventPayload::AssistantMessageStarted { .. } => {
            Some((ToolOutputStream::Stdout, "child assistant started\n".into()))
        },
        EventPayload::AssistantMessageCompleted { text, .. } => {
            let summary = one_line_summary(text);
            if summary.is_empty() {
                None
            } else {
                Some((
                    ToolOutputStream::Stdout,
                    format!("child assistant completed: {summary}\n"),
                ))
            }
        },
        EventPayload::ToolCallStarted { tool_name, .. } => Some((
            ToolOutputStream::Stdout,
            format!("child tool started: {tool_name}\n"),
        )),
        EventPayload::ToolOutputDelta { stream, delta, .. } => {
            Some((*stream, format!("child tool output: {delta}")))
        },
        EventPayload::ToolCallCompleted {
            tool_name, result, ..
        } => {
            let stream = if result.is_error {
                ToolOutputStream::Stderr
            } else {
                ToolOutputStream::Stdout
            };
            let detail = one_line_summary(result.error.as_deref().unwrap_or(&result.content));
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            Some((
                stream,
                format!("child tool completed: {tool_name}{suffix}\n"),
            ))
        },
        EventPayload::ErrorOccurred { message, .. } => Some((
            ToolOutputStream::Stderr,
            format!("child error: {message}\n"),
        )),
        EventPayload::TurnCompleted { finish_reason } => Some((
            ToolOutputStream::Stdout,
            format!("child turn completed: {finish_reason}\n"),
        )),
        _ => None,
    }
}

fn one_line_summary(text: &str) -> String {
    let mut summary = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 160;
    if summary.chars().count() > MAX_CHARS {
        summary = summary.chars().take(MAX_CHARS - 1).collect();
        summary.push('…');
    }
    summary
}
