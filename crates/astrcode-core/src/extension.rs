//! 扩展系统类型定义。
//!
//! 扩展是 astrcode 的主要扩展机制。技能（Skills）、Agent 配置文件、
//! 自定义工具、斜杠命令等都是通过扩展来实现的。
//!
//! 本模块定义了：
//! - [`Extension`] trait：扩展的核心接口（`id` + `register`）
//! - [`Registrar`]：扩展注册能力的构建器
//! - 类型化的处理器 trait 和上下文结构体

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    config::ModelSelection,
    llm::LlmProvider,
    storage::{EventReader, EventStore},
    tool::{ToolDefinition, ToolPromptMetadata, ToolResult},
};

// ─── Extension Trait ─────────────────────────────────────────────────────

/// 扩展 trait，定义了挂入 astrcode 生命周期的核心接口。
///
/// 扩展从 `~/.astrcode/extensions/`（全局）和 `.astrcode/extensions/`（项目级）加载。
/// 它们可以订阅生命周期事件、注册工具、斜杠命令和上下文提供者。
#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    /// 返回扩展的唯一标识符。
    fn id(&self) -> &str;

    /// 声明扩展需要宿主授予的能力。
    ///
    /// 宿主以此限制注入到扩展工具和生命周期上下文中的敏感能力。
    fn capabilities(&self) -> &[ExtensionCapability] {
        &[]
    }

    /// 一次性调用。扩展通过 registrar 注册工具、命令和事件处理器。
    fn register(&self, _reg: &mut Registrar) {}

    /// 扩展进入运行态。默认 no-op。
    ///
    /// 在此处通过 `ctx.config.deserialize::<T>()` 读取用户配置。
    async fn start(&self, _ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// 扩展退出运行态。默认 no-op。
    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// 检查扩展当前是否可用。宿主可周期性调用用于健康观测。
    ///
    /// 默认认为不持有外部运行态资源的扩展始终健康。
    async fn health(&self) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// 扩展配置发生热更新时调用。
    ///
    /// 当用户修改 `config.json` 中的 `extensions.<id>` 并触发重载时，
    /// 运行器会调用此方法通知扩展更新内部状态。
    /// 默认 no-op（兼容不支持热更新的扩展）。
    async fn on_config_changed(&self, _config: ExtensionConfig) -> Result<(), ExtensionError> {
        Ok(())
    }
}

/// 扩展可以显式申请的宿主能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCapability {
    /// 访问该 session 下命名空间隔离的持久状态。
    SessionState,
    /// 创建子 session、提交 turn 与回收 session。
    SessionControl,
    /// 调用宿主配置的小模型。
    SmallModel,
    /// 只读查询历史 session 投影。
    SessionHistory,
    /// 发射已声明的扩展事件。
    EmitEvents,
    /// 读取工作区或扩展发现目录。
    WorkspaceRead,
    /// 启动受扩展管理的子进程。
    ProcessSpawn,
    /// 发起网络客户端请求。
    NetworkClient,
}

/// 扩展专有配置的包装类型。
///
/// 包装用户 `config.json` 中 `extensions.<id>` 下的任意 JSON，
/// 扩展在 `start()` 或 `on_config_changed()` 时通过 `deserialize::<T>()` 获取。
#[derive(Clone, Debug, Default)]
pub struct ExtensionConfig(pub serde_json::Value);

impl ExtensionConfig {
    /// 将配置反序列化为具体类型。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// #[derive(Deserialize)]
    /// struct MyConfig { timeout: u64, retry: bool }
    /// let cfg: MyConfig = ctx.config.deserialize()?;
    /// ```
    pub fn deserialize<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.0.clone())
    }

    /// 如果配置为空对象 `{}` 则返回 `true`。
    pub fn is_empty(&self) -> bool {
        self.0.as_object().is_some_and(|o| o.is_empty())
    }
}

/// 插件运行态上下文。
#[derive(Clone)]
pub struct ExtensionCtx {
    tasks: ExtensionTasks,
    /// 扩展专有配置。用户配置文件中 `extensions.<id>` 对应的 JSON 值。
    /// 若用户未配置该扩展，则为空对象 `{}`。
    pub config: ExtensionConfig,
    /// 宿主启动时绑定的工作目录；不绑定工作区的宿主可为 `None`。
    startup_working_dir: Option<String>,
    /// 启动阶段可用的扩展事件发送端；由宿主显式绑定。
    event_sink: Option<Arc<dyn ExtensionEventSink>>,
    /// 由宿主统一绑定的受信运行态服务。
    ///
    /// 扩展只能在标准启动生命周期内取得这些能力，组合根不得为单个扩展
    /// 另开构造参数注入路径。
    host_services: Option<Arc<ExtensionHostServices>>,
}

impl ExtensionCtx {
    pub fn new(tasks: ExtensionTasks) -> Self {
        Self {
            tasks,
            config: ExtensionConfig::default(),
            startup_working_dir: None,
            event_sink: None,
            host_services: None,
        }
    }

    pub fn with_config(tasks: ExtensionTasks, config: ExtensionConfig) -> Self {
        Self::with_startup_working_dir(tasks, config, None)
    }

    pub fn with_startup_working_dir(
        tasks: ExtensionTasks,
        config: ExtensionConfig,
        startup_working_dir: Option<String>,
    ) -> Self {
        Self::with_startup_services(tasks, config, startup_working_dir, None)
    }

    pub fn with_startup_services(
        tasks: ExtensionTasks,
        config: ExtensionConfig,
        startup_working_dir: Option<String>,
        event_sink: Option<Arc<dyn ExtensionEventSink>>,
    ) -> Self {
        Self::with_host_services(tasks, config, startup_working_dir, event_sink, None)
    }

    pub fn with_host_services(
        tasks: ExtensionTasks,
        config: ExtensionConfig,
        startup_working_dir: Option<String>,
        event_sink: Option<Arc<dyn ExtensionEventSink>>,
        host_services: Option<Arc<ExtensionHostServices>>,
    ) -> Self {
        Self {
            tasks,
            config,
            startup_working_dir,
            event_sink,
            host_services,
        }
    }

    pub fn tasks(&self) -> &ExtensionTasks {
        &self.tasks
    }

    /// 启动时宿主已知的工作目录，供扩展预加载该项目的资源。
    pub fn startup_working_dir(&self) -> Option<&str> {
        self.startup_working_dir.as_deref()
    }

    /// 返回启动阶段由宿主绑定的扩展事件发送端。
    pub fn event_sink(&self) -> Option<&Arc<dyn ExtensionEventSink>> {
        self.event_sink.as_ref()
    }

    /// 返回宿主授予扩展的运行态服务。
    pub fn host_services(&self) -> Option<&Arc<ExtensionHostServices>> {
        self.host_services.as_ref()
    }

    pub fn shutdown(&self) -> CancellationToken {
        self.tasks.shutdown()
    }
}

/// 插件退出原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// 同一个扩展 id 被重新加载的新实例替换。
    Reload,
    /// 配置关闭或 source 不再提供该扩展。
    Disabled,
    /// 宿主进程关闭。
    Shutdown,
}

/// 宿主管理的插件后台任务集合。
#[derive(Clone)]
pub struct ExtensionTasks {
    extension_id: Arc<str>,
    shutdown: CancellationToken,
    handles: Arc<Mutex<Vec<ExtensionTask>>>,
}

struct ExtensionTask {
    name: String,
    handle: JoinHandle<()>,
}

impl ExtensionTasks {
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self {
            extension_id: Arc::from(extension_id.into()),
            shutdown: CancellationToken::new(),
            handles: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn shutdown(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub fn spawn<F>(&self, name: impl Into<String>, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.shutdown.is_cancelled() {
            tracing::debug!(
                extension_id = %self.extension_id,
                "skip spawning extension task after shutdown"
            );
            return;
        }

        let name = name.into();
        let handle = tokio::spawn(fut);
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.push(ExtensionTask { name, handle });
    }

    pub fn cancel(&self) {
        self.shutdown.cancel();
    }

    pub async fn wait(&self, timeout: Duration) {
        let tasks = {
            let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *handles)
        };

        let deadline = tokio::time::Instant::now() + timeout;
        for task in tasks {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                self.abort_one(task).await;
            } else {
                self.wait_one(task, deadline - now).await;
            }
        }
    }

    async fn wait_one(&self, task: ExtensionTask, timeout: Duration) {
        let ExtensionTask { name, mut handle } = task;
        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(Ok(())) => {},
            Ok(Err(join_err)) if join_err.is_cancelled() => {
                tracing::debug!(
                    extension_id = %self.extension_id,
                    task = %name,
                    "extension task cancelled"
                );
            },
            Ok(Err(join_err)) if join_err.is_panic() => {
                tracing::error!(
                    extension_id = %self.extension_id,
                    task = %name,
                    "extension task panicked"
                );
            },
            Ok(Err(join_err)) => {
                tracing::warn!(
                    extension_id = %self.extension_id,
                    task = %name,
                    error = %join_err,
                    "extension task failed"
                );
            },
            Err(_) => {
                tracing::warn!(
                    extension_id = %self.extension_id,
                    task = %name,
                    "extension task did not stop before timeout; aborting"
                );
                handle.abort();
                let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
            },
        }
    }

    async fn abort_one(&self, task: ExtensionTask) {
        let ExtensionTask { name, handle } = task;
        tracing::warn!(
            extension_id = %self.extension_id,
            task = %name,
            "extension task did not stop before shared timeout; aborting"
        );
        handle.abort();
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }
}

// ─── Host Services ──────────────────────────────────────────────────────

/// 扩展运行时可用的宿主服务。
///
/// 只注入给 trusted bundled extension，不暴露给 untrusted source（disk/wasm）。
pub struct ExtensionHostServices {
    /// 可信内置扩展可用的只读会话投影数据源。
    ///
    /// 由 `Arc<dyn EventStore>` 通过 trait upcasting 转换而来
    /// （Rust 1.86+，`EventStore: EventReader` 建立 supertrait 关系）。
    pub session_read: Option<Arc<dyn EventReader>>,
    /// 小模型 provider，用于记忆提取。
    pub small_llm: Option<Arc<dyn LlmProvider>>,
}

impl ExtensionHostServices {
    pub fn new(event_store: Arc<dyn EventStore>, small_llm: Option<Arc<dyn LlmProvider>>) -> Self {
        Self {
            // Arc<dyn EventStore> → Arc<dyn EventReader> 由 trait upcasting 自动完成。
            session_read: Some(event_store),
            small_llm,
        }
    }
}

// ─── Lifecycle Events ────────────────────────────────────────────────────

/// 扩展可订阅的核心生命周期事件。
///
/// 覆盖会话/轮次/工具/LLM 提供者/prompt 组装的完整生命周期。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionEvent {
    // ── 会话级别 ──
    /// 会话启动。
    SessionStart,
    /// 已持久化的会话首次恢复到当前进程运行态。
    SessionResume,
    /// 会话关闭。
    SessionShutdown,

    // ── 轮次级别 ──
    /// 轮次开始。
    TurnStart,
    /// 轮次结束。
    TurnEnd,
    /// 用户中止正在运行的轮次。
    TurnAborted,

    // ── Step 级别 ──
    /// Step 开始（loop 迭代顶部，prepare_stage 之前）。
    StepStart,
    /// Step 结束（loop 迭代末尾，tool_calls 执行完毕或 LLM 返回 Complete 后）。
    StepEnd,

    // ── 工具级别（主要钩子点） ──
    /// 工具执行前。
    PreToolUse,
    /// 工具执行后。
    PostToolUse,
    /// 工具执行失败后（is_error = true）。
    ///
    /// 在 `PostToolUse` 之后触发，仅当工具结果标记为错误时。
    /// 扩展可以用于错误日志、告警通知、自动重试策略等。
    PostToolUseFailure,

    // ── LLM 提供者钩子 ──
    /// LLM 请求发送前。
    BeforeProviderRequest,
    /// LLM 响应接收后。
    AfterProviderResponse,

    // ── 用户输入 ──
    /// 用户提交提示词。
    UserPromptSubmit,

    // ── Prompt 组装 ──
    /// 构建 system prompt 前收集插件提供的提示词片段。
    PromptBuild,

    // ── 上下文压缩 ──
    /// 上下文压缩前收集额外摘要指令。
    PreCompact,
    /// 上下文压缩完成后通知扩展。
    PostCompact,

    // ── Recap ──
    /// Recap 生成完成后通知扩展（非阻塞）。
    PostRecap,
}

// ─── Extension Manifest ──────────────────────────────────────────────────

/// 磁盘扩展目录中的 `extension.json` 契约（发现阶段元数据）。
///
/// **当前 loader 行为（s5r）**：`protocol.s5r`（须为 `"1.0"`）与 **`library`**（WASM
/// 相对路径）为必填； 扩展的真实 `id`、能力、工具与 hook 均由 guest 的 `extension_init` 握手返回。
/// 本结构中的 `id` / `name` / `capabilities` 等字段可被 serde 解析，供 UI、诊断或
/// 未来校验使用，但**不会**替代 WASM manifest。磁盘路径仅支持 `.wasm`，无 native dlopen。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// 扩展唯一标识符。
    pub id: String,
    /// 扩展显示名称。
    pub name: String,
    /// 可选的扩展版本号，用于诊断/UI 展示。
    #[serde(default)]
    pub version: Option<String>,
    /// 可选的人类可读描述。
    #[serde(default)]
    pub description: Option<String>,
    /// 可选的宿主版本提示。目前仅作为元数据，不做硬性校验。
    #[serde(default)]
    pub astrcode_version: Option<String>,
    /// 宿主必须授予此扩展的能力。
    #[serde(default)]
    pub capabilities: Vec<ExtensionCapability>,
    /// 原生库路径（相对于扩展目录，`.dll` / `.so` / `.wasm`）。
    pub library: String,
}

// ─── extension Event System ────────────────────────────────────────────────

/// 插件在 [`Registrar`] 中声明的事件类型。
///
/// 声明是 emit 时校验的依据：未声明的事件类型会被拒绝，payload 超限也会被拒绝。
/// `extension_id` 不在声明中——它由 runtime 在构造 [`ExtensionEventSink`] 时注入。
#[derive(Debug, Clone)]
pub struct ExtensionEventDecl {
    pub event_type: String,
    pub schema_version: u32,
    pub durable: bool,
    pub max_payload_bytes: usize,
}

/// [`Registrar::extension_event`] 返回的构建器。
pub struct ExtensionEventDeclBuilder<'a> {
    registrar: &'a mut Registrar,
    event_type: String,
    schema_version: u32,
    durable: bool,
    max_payload_bytes: usize,
}

impl<'a> ExtensionEventDeclBuilder<'a> {
    pub fn schema_version(mut self, v: u32) -> Self {
        self.schema_version = v;
        self
    }
    pub fn durable(mut self, d: bool) -> Self {
        self.durable = d;
        self
    }
    pub fn max_payload_bytes(mut self, n: usize) -> Self {
        self.max_payload_bytes = n;
        self
    }
    pub fn register(self) {
        self.registrar
            .extension_event_decls
            .push(ExtensionEventDecl {
                event_type: self.event_type,
                schema_version: self.schema_version,
                durable: self.durable,
                max_payload_bytes: self.max_payload_bytes,
            });
    }
}

/// 插件事件发射器。`extension_id` 在构造时由 runtime 绑定，调用方无法伪造身份。
#[async_trait::async_trait]
pub trait ExtensionEventSink: Send + Sync {
    async fn emit(
        &self,
        event_type: &str,
        schema_version: u32,
        payload: serde_json::Value,
    ) -> Result<(), ExtensionError>;
}

// ─── Compact Trigger ─────────────────────────────────────────────────────

/// 触发 compact 的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    /// 自动阈值触发的 compact。
    AutoThreshold,
    /// 用户手动执行 compact 命令。
    ManualCommand,
    /// LLM 返回 prompt_too_long 后的强制补救 compact。
    ReactivePromptTooLong,
}

/// Compact 使用的策略，记录在事件中用于 replay 和审计。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactStrategy {
    Auto,
    Manual {
        #[serde(skip_serializing_if = "Option::is_none")]
        keep_recent_turns: Option<usize>,
    },
    ReactivePromptTooLong,
}

// ─── Hook Mode ───────────────────────────────────────────────────────────

/// 钩子订阅的执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    /// 同步执行，可以阻止操作。
    /// 适用于：安全审查、权限校验。
    Blocking,

    /// 异步执行（即发即弃），不能阻止操作。
    /// 适用于：日志记录、分析统计、通知。
    NonBlocking,

    /// 执行但结果仅供参考。
    /// 适用于：风格建议、可选指导。
    Advisory,
}

/// 插件在 PromptBuild hook 中提供的 prompt 片段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptContributions {
    /// 插件系统提示词。宿主会放在 system prompt 最前面，即模型可见工具声明之后。
    #[serde(default)]
    pub system_prompts: Vec<String>,
    /// 追加到 Additional Instructions section 的运行时指令。
    #[serde(default)]
    pub additional_instructions: Vec<String>,
    /// Skills section 内容。
    #[serde(default)]
    pub skills: Vec<String>,
    /// Agents section 内容。
    #[serde(default)]
    pub agents: Vec<String>,
}

impl PromptContributions {
    pub fn merge(&mut self, other: PromptContributions) {
        self.system_prompts.extend(other.system_prompts);
        self.additional_instructions
            .extend(other.additional_instructions);
        self.skills.extend(other.skills);
        self.agents.extend(other.agents);
    }
}

/// 插件在 PreCompact hook 中提供的 compact 摘要指令。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactContributions {
    /// 追加到 compact prompt 的摘要指令。
    #[serde(default)]
    pub instructions: Vec<String>,
}

impl CompactContributions {
    pub fn merge(&mut self, other: CompactContributions) {
        self.instructions.extend(other.instructions);
    }
}

// ─── Slash Command ───────────────────────────────────────────────────────

/// 扩展注册的斜杠命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// 命令名称（不含前导斜杠 `/`）。
    pub name: String,
    /// 人类可读的命令描述。
    pub description: String,
    /// 参数的 JSON Schema 定义。
    pub args_schema: Option<serde_json::Value>,
}

/// 扩展斜杠命令的执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionCommandResult {
    /// 只展示文本，不启动 agent turn。兼容现有原生插件命令语义。
    Display {
        /// 输出文本，展示给用户。
        content: String,
        /// 是否为错误结果。
        is_error: bool,
    },
    /// 同步处理完成，不启动 agent turn。
    Handled {
        /// 说明文本。
        message: String,
    },
    /// 启动一个 agent turn，携带附加指令合并到用户消息中。
    StartTurn {
        /// 附加指令，合并到用户消息末尾。
        instructions: String,
    },
}

/// 命令结果附带的状态栏更新。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItemUpdatePayload {
    pub id: String,
    pub text: String,
}

impl ExtensionCommandResult {
    pub fn display(content: impl Into<String>, is_error: bool) -> Self {
        Self::Display {
            content: content.into(),
            is_error,
        }
    }

    pub fn handled(message: impl Into<String>) -> Self {
        Self::Handled {
            message: message.into(),
        }
    }

    pub fn start_turn(instructions: impl Into<String>) -> Self {
        Self::StartTurn {
            instructions: instructions.into(),
        }
    }
}

// ─── Extension Error ─────────────────────────────────────────────────────

/// 扩展操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    /// 找不到指定的扩展或工具。
    #[error("Extension not found: {0}")]
    NotFound(String),
    /// 钩子执行超时。
    #[error("Hook timed out after {0}ms")]
    Timeout(u64),
    /// 钩子显式阻止了操作——属于正常流程，非崩溃。
    #[error("blocked by hook: {reason}")]
    Blocked { reason: String },
    /// 内部错误（如 panic、无效状态、序列化失败）。
    #[error("extension error: {0}")]
    Internal(String),
}

// ─── Extension Tool Outcome ───────────────────────────────────────────────

/// ToolResult.metadata 中用于携带 [`ExtensionToolOutcome`] 的键名。
///
/// 扩展工具通过此键将声明式结果（如 `RunSession`）传递给运行器，
/// 运行器再解释并执行对应的副作用。
pub const EXTENSION_TOOL_OUTCOME_KEY: &str = "extension_tool_outcome";

/// 扩展工具回调返回的声明式结果。
///
/// 扩展返回这些变体而非直接调用宿主原语，由运行器解释每个变体：
/// - `Text`：普通文本结果（当前默认行为）
///
/// 通过 FFI 边界以 JSON 传递。工具回调返回码 `2` 表示
/// `output_ptr/len` 携带的是序列化的 `ExtensionToolOutcome`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionToolOutcome {
    /// 普通文本结果——标准的 ToolResult 路径。
    Text { content: String, is_error: bool },
}

/// 子 session 的工具集策略。
///
/// 由 [`ExtensionToolOutcome::RunSession::tool_policy`] 携带，决定子 session 在
/// `build_tool_registry_snapshot` 时如何裁剪工具表。
///
/// 语义：
/// - `Deny`：从父全集中排除指定工具。常见场景是 `["agent"]` 防止递归生 agent。
/// - `Allow`：仅保留指定工具。空白名单视为非法配置，spawner 应拒绝。
///
/// 过滤在工具表构建阶段一次性完成，避免 LLM 拿到的 schema 与运行时可见性脱节。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ChildToolPolicy {
    /// 从父工具全集中排除这些工具。空数组等价于不传。
    Deny { tools: Vec<String> },
    /// 仅保留这些工具。空数组在 spawner 处会被拒绝。
    Allow { tools: Vec<String> },
}


// ───  Typed Extension API ────────────────────────────────

/// Provider hook 触发时机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEvent {
    BeforeRequest,
    AfterResponse,
}

/// Compact hook 触发时机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactEvent {
    PreCompact,
    PostCompact,
}

// ─── Result Enums ────────────────────────────────────────────────

/// 通用钩子结果。
#[derive(Debug, Clone)]
pub enum HookResult {
    Allow,
    Block { reason: String },
}

/// PreToolUse 钩子结果。
#[derive(Debug, Clone)]
pub enum PreToolUseResult {
    Allow,
    Block { reason: String },
    ModifyInput { tool_input: serde_json::Value },
}

/// PostToolUse 钩子结果。
///
/// `ModifyResult` 仅替换 ToolResult 的文本内容（`content` 字段）；其它结构化
/// 字段——`is_error` / `metadata` / `artifact_ref` / `duration_ms`——保持不变。
/// 想要修改错误标记或附加元数据时，本变体不够用：当前协议没有「全字段替换」入口。
/// 如果未来出现需求，应该新增 `ReplaceFully(ToolResult)` 而非扩张这里的语义。
#[derive(Debug, Clone)]
pub enum PostToolUseResult {
    Allow,
    Block { reason: String },
    ModifyResult { content: String },
}

/// Provider 钩子结果。
#[derive(Debug, Clone)]
pub enum ProviderResult {
    Allow,
    Block {
        reason: String,
    },
    ReplaceMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
    AppendMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
}

/// Compact 钩子结果。
#[derive(Debug, Clone)]
pub enum CompactResult {
    Allow,
    Block { reason: String },
    Contributions(CompactContributions),
}

// ─── Context Structs ─────────────────────────────────────────────

/// PostToolUseFailure 钩子上下文。
#[derive(Debug, Clone)]
pub struct PostToolUseFailureContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub error: String,
    pub tool_result: ToolResult,
}

/// PreToolUse 钩子上下文。
#[derive(Clone)]
pub struct PreToolUseContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub available_tools: Vec<ToolDefinition>,
    /// 当前 turn 事件通道；宿主按扩展能力派生 [`extension_event_sink`]。
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EventPayload>>,
    /// 插件事件发射器（按扩展 id 绑定，内置与 WASM 扩展共用）。
    pub extension_event_sink: Option<std::sync::Arc<dyn ExtensionEventSink>>,
    /// session 在存储层的真实目录路径。
    pub session_store_dir: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for PreToolUseContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreToolUseContext")
            .field("session_id", &self.session_id)
            .field("tool_name", &self.tool_name)
            .field(
                "extension_event_sink",
                &self.extension_event_sink.as_ref().map(|_| "<sink>"),
            )
            .finish_non_exhaustive()
    }
}

/// PostToolUse 钩子上下文。
#[derive(Clone)]
pub struct PostToolUseContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_result: ToolResult,
    pub is_error: bool,
    /// 当前 turn 事件通道；宿主按扩展能力派生 [`extension_event_sink`]。
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EventPayload>>,
    /// 插件事件发射器（按扩展 id 绑定，内置与 WASM 扩展共用）。
    pub extension_event_sink: Option<std::sync::Arc<dyn ExtensionEventSink>>,
    /// session 在存储层的真实目录路径。
    pub session_store_dir: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for PostToolUseContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostToolUseContext")
            .field("session_id", &self.session_id)
            .field("tool_name", &self.tool_name)
            .field("is_error", &self.is_error)
            .field(
                "extension_event_sink",
                &self.extension_event_sink.as_ref().map(|_| "<sink>"),
            )
            .finish_non_exhaustive()
    }
}

/// Provider 钩子上下文。
#[derive(Debug, Clone)]
pub struct ProviderContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub messages: Vec<crate::llm::LlmMessage>,
    /// session 在存储层的真实目录路径。
    pub session_store_dir: Option<std::path::PathBuf>,
}

/// PromptBuild 钩子上下文。
#[derive(Debug, Clone)]
pub struct PromptBuildContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tools: Vec<ToolDefinition>,
}

/// Compact 钩子上下文。
#[derive(Debug, Clone)]
pub struct CompactContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub trigger: CompactTrigger,
    pub message_count: usize,
    pub pre_tokens: Option<usize>,
    pub post_tokens: Option<usize>,
    pub summary: Option<String>,
}

/// 当轮 user/assistant 消息摘要，仅 TurnEnd 事件填充。
#[derive(Debug, Clone)]
pub struct ExchangeSummary {
    pub user_message: String,
    pub assistant_message: String,
}

/// 通用生命周期钩子上下文。
#[derive(Clone)]
pub struct LifecycleContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    /// 当前 turn 事件通道；宿主按扩展能力派生 [`extension_event_sink`]。
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EventPayload>>,
    /// 插件事件发射器（按扩展 id 绑定，内置与 WASM 扩展共用）。
    pub extension_event_sink: Option<std::sync::Arc<dyn ExtensionEventSink>>,
    /// 仅 TurnEnd 事件填充：当轮最后一条 user 和 assistant 消息文本。
    pub last_exchange: Option<ExchangeSummary>,
}

impl std::fmt::Debug for LifecycleContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleContext")
            .field("session_id", &self.session_id)
            .field(
                "extension_event_sink",
                &self.extension_event_sink.as_ref().map(|_| "<sink>"),
            )
            .field("last_exchange", &self.last_exchange)
            .finish_non_exhaustive()
    }
}

/// 命令执行上下文。
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    /// session 在存储层的真实目录路径。
    pub session_store_dir: Option<std::path::PathBuf>,
}

// ─── Handler Traits ──────────────────────────────────────────────

/// PreToolUse 钩子处理器。
#[async_trait::async_trait]
pub trait PreToolUseHandler: Send + Sync {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError>;
}

/// PostToolUse 钩子处理器。
#[async_trait::async_trait]
pub trait PostToolUseHandler: Send + Sync {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError>;
}

/// Provider 钩子处理器。
#[async_trait::async_trait]
pub trait ProviderHandler: Send + Sync {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError>;
}

/// PromptBuild 钩子处理器。
#[async_trait::async_trait]
pub trait PromptBuildHandler: Send + Sync {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError>;
}

/// Compact 钩子处理器。
#[async_trait::async_trait]
pub trait CompactHandler: Send + Sync {
    async fn handle(&self, ctx: CompactContext) -> Result<CompactResult, ExtensionError>;
}

/// PostToolUseFailure 通知型钩子处理器。
#[async_trait::async_trait]
pub trait PostToolUseFailureHandler: Send + Sync {
    async fn handle(&self, ctx: PostToolUseFailureContext) -> Result<(), ExtensionError>;
}

/// 通用生命周期钩子处理器。
#[async_trait::async_trait]
pub trait LifecycleHandler: Send + Sync {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError>;
}

/// 工具执行处理器。
#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &crate::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError>;
}

/// 命令执行处理器。
#[async_trait::async_trait]
pub trait CommandHandler: Send + Sync {
    async fn execute(
        &self,
        command_name: &str,
        args: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError>;
}

/// 动态工具发现处理器。
#[async_trait::async_trait]
pub trait ToolDiscoveryHandler: Send + Sync {
    async fn discover(&self, working_dir: &str) -> Vec<DiscoveredTool>;
}

/// 动态命令发现处理器。
#[async_trait::async_trait]
pub trait CommandDiscoveryHandler: Send + Sync {
    async fn discover(
        &self,
        working_dir: &str,
    ) -> Vec<(SlashCommand, std::sync::Arc<dyn CommandHandler>)>;
}

/// Tool contributed by dynamic discovery.
#[derive(Clone)]
pub struct DiscoveredTool {
    pub definition: ToolDefinition,
    pub handler: std::sync::Arc<dyn ToolHandler>,
    pub prompt_metadata: Option<ToolPromptMetadata>,
}

// ─── Registrar ───────────────────────────────────────────────────

/// 扩展能力注册器。
///
/// 在 `Extension::register()` 调用期间有效，扩展通过它声明自己提供的能力。
///
/// 字段全部私有，外部只能通过 `tool` / `command` / `on_pre_tool_use` 等
/// 写入方法和 `tools()` / `commands()` 等读取 accessor 访问。这样保证：
/// 1. 扩展作者只能用受控 API 注册能力，无法旁路构造非法状态；
/// 2. 字段重构（合并、增加索引）不会破坏外部代码；
/// 3. `Registrar` 只在 `Extension::register()` 生命周期内有效，私有字段
///    阻止外部把它当成长寿数据持有。
pub struct Registrar {
    tools: Vec<(ToolDefinition, std::sync::Arc<dyn ToolHandler>)>,
    tool_discovery: Vec<std::sync::Arc<dyn ToolDiscoveryHandler>>,
    tool_metadata: std::collections::HashMap<String, ToolPromptMetadata>,
    commands: Vec<(SlashCommand, std::sync::Arc<dyn CommandHandler>)>,
    command_discovery: Vec<std::sync::Arc<dyn CommandDiscoveryHandler>>,
    keybindings: Vec<Keybinding>,
    status_items: Vec<StatusItem>,
    pre_tool_use: Vec<(HookMode, i32, std::sync::Arc<dyn PreToolUseHandler>)>,
    post_tool_use: Vec<(HookMode, i32, std::sync::Arc<dyn PostToolUseHandler>)>,
    provider: Vec<(
        ProviderEvent,
        HookMode,
        i32,
        std::sync::Arc<dyn ProviderHandler>,
    )>,
    prompt_build: Vec<(i32, std::sync::Arc<dyn PromptBuildHandler>)>,
    compact: Vec<(CompactEvent, i32, std::sync::Arc<dyn CompactHandler>)>,
    post_tool_use_failure: Vec<(i32, std::sync::Arc<dyn PostToolUseFailureHandler>)>,
    lifecycle: Vec<(
        ExtensionEvent,
        HookMode,
        i32,
        std::sync::Arc<dyn LifecycleHandler>,
    )>,
    extension_event_decls: Vec<ExtensionEventDecl>,
    needs_extension_data_dir: bool,
}

impl Registrar {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            tool_discovery: Vec::new(),
            tool_metadata: std::collections::HashMap::new(),
            commands: Vec::new(),
            command_discovery: Vec::new(),
            keybindings: Vec::new(),
            status_items: Vec::new(),
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
            provider: Vec::new(),
            prompt_build: Vec::new(),
            compact: Vec::new(),
            post_tool_use_failure: Vec::new(),
            lifecycle: Vec::new(),
            extension_event_decls: Vec::new(),
            needs_extension_data_dir: false,
        }
    }

    pub fn tool(&mut self, def: ToolDefinition, handler: std::sync::Arc<dyn ToolHandler>) {
        self.tools.push((def, handler));
    }

    pub fn tool_discovery(&mut self, handler: std::sync::Arc<dyn ToolDiscoveryHandler>) {
        self.tool_discovery.push(handler);
    }

    pub fn tool_metadata(&mut self, meta: std::collections::HashMap<String, ToolPromptMetadata>) {
        self.tool_metadata.extend(meta);
    }

    pub fn command(&mut self, cmd: SlashCommand, handler: std::sync::Arc<dyn CommandHandler>) {
        self.commands.push((cmd, handler));
    }

    pub fn command_discovery(&mut self, handler: std::sync::Arc<dyn CommandDiscoveryHandler>) {
        self.command_discovery.push(handler);
    }

    pub fn keybinding(&mut self, binding: Keybinding) {
        self.keybindings.push(binding);
    }

    pub fn status_item(&mut self, item: StatusItem) {
        self.status_items.push(item);
    }

    pub fn on_pre_tool_use(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: std::sync::Arc<dyn PreToolUseHandler>,
    ) {
        self.pre_tool_use.push((mode, priority, handler));
    }

    pub fn on_post_tool_use(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: std::sync::Arc<dyn PostToolUseHandler>,
    ) {
        self.post_tool_use.push((mode, priority, handler));
    }

    pub fn on_provider(
        &mut self,
        event: ProviderEvent,
        mode: HookMode,
        priority: i32,
        handler: std::sync::Arc<dyn ProviderHandler>,
    ) {
        self.provider.push((event, mode, priority, handler));
    }

    pub fn on_prompt_build(
        &mut self,
        priority: i32,
        handler: std::sync::Arc<dyn PromptBuildHandler>,
    ) {
        self.prompt_build.push((priority, handler));
    }

    pub fn on_compact(
        &mut self,
        event: CompactEvent,
        priority: i32,
        handler: std::sync::Arc<dyn CompactHandler>,
    ) {
        self.compact.push((event, priority, handler));
    }

    pub fn on_post_tool_use_failure(
        &mut self,
        priority: i32,
        handler: std::sync::Arc<dyn PostToolUseFailureHandler>,
    ) {
        self.post_tool_use_failure.push((priority, handler));
    }

    pub fn on_event(
        &mut self,
        event: ExtensionEvent,
        mode: HookMode,
        priority: i32,
        handler: std::sync::Arc<dyn LifecycleHandler>,
    ) {
        self.lifecycle.push((event, mode, priority, handler));
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
            && self.tool_discovery.is_empty()
            && self.tool_metadata.is_empty()
            && self.commands.is_empty()
            && self.command_discovery.is_empty()
            && self.keybindings.is_empty()
            && self.status_items.is_empty()
            && self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.provider.is_empty()
            && self.prompt_build.is_empty()
            && self.compact.is_empty()
            && self.post_tool_use_failure.is_empty()
            && self.lifecycle.is_empty()
            && self.extension_event_decls.is_empty()
            && !self.needs_extension_data_dir
    }

    /// 声明插件需要专属数据目录（`~/.astrcode/extension_data/<extension_id>/`）。
    ///
    /// 注册后由 runtime 自动创建目录。插件通过 `hostpaths::extension_data_dir()` 获取路径。
    pub fn extension_data_dir(&mut self) {
        self.needs_extension_data_dir = true;
    }

    /// 声明插件可发出的事件类型，返回构建器。
    pub fn extension_event(&mut self, event_type: &str) -> ExtensionEventDeclBuilder<'_> {
        ExtensionEventDeclBuilder {
            registrar: self,
            event_type: event_type.to_owned(),
            schema_version: 1,
            durable: true,
            max_payload_bytes: 64 * 1024,
        }
    }

    pub fn tools(&self) -> &[(ToolDefinition, std::sync::Arc<dyn ToolHandler>)] {
        &self.tools
    }

    pub fn tool_discoveries(&self) -> &[std::sync::Arc<dyn ToolDiscoveryHandler>] {
        &self.tool_discovery
    }

    pub fn all_tool_metadata(&self) -> &std::collections::HashMap<String, ToolPromptMetadata> {
        &self.tool_metadata
    }

    pub fn commands(&self) -> &[(SlashCommand, std::sync::Arc<dyn CommandHandler>)] {
        &self.commands
    }

    pub fn command_discoveries(&self) -> &[std::sync::Arc<dyn CommandDiscoveryHandler>] {
        &self.command_discovery
    }

    pub fn pre_tool_use(&self) -> &[(HookMode, i32, std::sync::Arc<dyn PreToolUseHandler>)] {
        &self.pre_tool_use
    }

    pub fn post_tool_use(&self) -> &[(HookMode, i32, std::sync::Arc<dyn PostToolUseHandler>)] {
        &self.post_tool_use
    }

    pub fn provider(
        &self,
    ) -> &[(
        ProviderEvent,
        HookMode,
        i32,
        std::sync::Arc<dyn ProviderHandler>,
    )] {
        &self.provider
    }

    pub fn prompt_build(&self) -> &[(i32, std::sync::Arc<dyn PromptBuildHandler>)] {
        &self.prompt_build
    }

    pub fn compact(&self) -> &[(CompactEvent, i32, std::sync::Arc<dyn CompactHandler>)] {
        &self.compact
    }

    pub fn post_tool_use_failure(&self) -> &[(i32, std::sync::Arc<dyn PostToolUseFailureHandler>)] {
        &self.post_tool_use_failure
    }

    pub fn lifecycle(
        &self,
    ) -> &[(
        ExtensionEvent,
        HookMode,
        i32,
        std::sync::Arc<dyn LifecycleHandler>,
    )] {
        &self.lifecycle
    }

    pub fn keybindings(&self) -> &[Keybinding] {
        &self.keybindings
    }

    pub fn status_items(&self) -> &[StatusItem] {
        &self.status_items
    }

    pub fn extension_event_decls(&self) -> &[ExtensionEventDecl] {
        &self.extension_event_decls
    }

    pub fn needs_extension_data_dir(&self) -> bool {
        self.needs_extension_data_dir
    }
}

impl Default for Registrar {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Keybinding ──────────────────────────────────────────────────────────

/// 插件注册的快捷键绑定。
///
/// 当用户按下对应组合键时，TUI 将执行关联的斜杠命令（如同用户输入该命令）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    /// 快捷键描述（如 "shift+tab", "ctrl+p"）。
    pub key: String,
    /// 按下时执行的斜杠命令名（不含 `/`）。
    pub command: String,
    /// 可选的命令参数。
    #[serde(default)]
    pub arguments: String,
    /// 人类可读描述（用于帮助/UI 展示）。
    pub description: String,
}

// ─── Status Item ─────────────────────────────────────────────────────────

/// 插件注册的状态栏项。
///
/// 显示在 TUI footer 和前端状态栏中。插件可以通过 `StatusItemUpdate`
/// 通知动态更新内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItem {
    /// 唯一标识符（如 "mode"、"git-branch"）。
    pub id: String,
    /// 初始显示文本。
    pub text: String,
    /// 排序优先级（越小越靠左）。
    #[serde(default)]
    pub priority: i32,
    /// 可选的 tooltip 描述。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}
