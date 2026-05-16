//! 扩展系统类型定义。
//!
//! 扩展是 astrcode 的主要扩展机制。技能（Skills）、Agent 配置文件、
//! 自定义工具、斜杠命令等都是通过扩展来实现的。
//!
//! 本模块定义了：
//! - [`Extension`] trait：扩展的核心接口（`id` + `register`）
//! - [`Registrar`]：扩展注册能力的构建器
//! - 类型化的处理器 trait 和上下文结构体

use serde::{Deserialize, Serialize};

use crate::{
    config::ModelSelection,
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

    /// 一次性调用。扩展通过 registrar 注册工具、命令和事件处理器。
    fn register(&self, _reg: &mut Registrar) {}
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
    /// 会话关闭。
    SessionShutdown,

    // ── 轮次级别 ──
    /// 轮次开始。
    TurnStart,
    /// 轮次结束。
    TurnEnd,
    /// 用户中止正在运行的轮次。
    TurnAborted,

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
}

// ─── Extension Manifest ──────────────────────────────────────────────────

/// 从扩展的 `extension.json` 解析的清单文件。
///
/// 由文件系统加载器在加载原生库之前用于发现扩展。
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
    /// 原生库路径（相对于扩展目录，`.dll` / `.so`）。
    pub library: String,
    /// 此扩展订阅的事件列表。
    #[serde(default)]
    pub subscriptions: Vec<ManifestSubscription>,
    /// 静态工具定义列表。
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// 静态斜杠命令定义列表。
    #[serde(default)]
    pub slash_commands: Vec<SlashCommand>,
}

/// 清单 JSON 中的订阅条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSubscription {
    /// 订阅的事件类型。
    #[serde(rename = "event")]
    pub event: ExtensionEvent,
    /// 钩子执行模式。
    #[serde(rename = "mode")]
    pub mode: HookMode,
    /// 同一事件内的执行优先级，数值越大越先执行。
    pub priority: i32,
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
/// - `RunSession`：请求宿主创建子会话并运行一个轮次
///
/// 通过 FFI 边界以 JSON 传递。工具回调返回码 `2` 表示
/// `output_ptr/len` 携带的是序列化的 `ExtensionToolOutcome`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionToolOutcome {
    /// 普通文本结果——标准的 ToolResult 路径。
    Text { content: String, is_error: bool },
    /// 请求宿主创建子会话并运行一个轮次。
    ///
    /// - `parent_session_id` 来自当前的 `ToolExecutionContext`，而非插件提供——
    ///   插件无法伪造父子关系。
    /// - `system_prompt` 追加到全局系统提示词之后，而非替换。
    /// - `model_preference` 在 v1 中仅为建议值。
    RunSession {
        /// 子会话的显示名称。
        name: String,
        /// 追加到全局系统提示词之后的指令。
        system_prompt: String,
        /// 发送给子会话的用户提示词。
        user_prompt: String,
        /// 建议使用的模型（v1 中仅为建议）。
        #[serde(default)]
        model_preference: Option<String>,
        /// 是否同步阻塞等待子 agent 完成。
        ///
        /// `false`（默认）：异步执行，立即返回，子 agent 在后台运行，完成后
        /// 结果通过 durable event 机制在下一轮对话中可见。
        /// `true`：同步阻塞直到子 agent 完成并返回结果。
        #[serde(default = "default_wait_for_result")]
        wait_for_result: bool,
    },
}

/// `wait_for_result` 的 serde 默认值——`false`（异步）。
const fn default_wait_for_result() -> bool {
    false
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
#[derive(Debug, Clone)]
pub struct PreToolUseContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub available_tools: Vec<ToolDefinition>,
}

/// PostToolUse 钩子上下文。
#[derive(Debug, Clone)]
pub struct PostToolUseContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_result: ToolResult,
    pub is_error: bool,
}

/// Provider 钩子上下文。
#[derive(Debug, Clone)]
pub struct ProviderContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub messages: Vec<crate::llm::LlmMessage>,
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

/// 通用生命周期钩子上下文。
#[derive(Debug, Clone)]
pub struct LifecycleContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
}

/// 命令执行上下文。
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
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
    async fn discover(
        &self,
        working_dir: &str,
    ) -> Vec<(ToolDefinition, std::sync::Arc<dyn ToolHandler>)>;
}

/// 动态命令发现处理器。
#[async_trait::async_trait]
pub trait CommandDiscoveryHandler: Send + Sync {
    async fn discover(
        &self,
        working_dir: &str,
    ) -> Vec<(SlashCommand, std::sync::Arc<dyn CommandHandler>)>;
}

// ─── Registrar ───────────────────────────────────────────────────

/// 扩展能力注册器。
///
/// 在 `Extension::register()` 调用期间有效，扩展通过它声明自己提供的能力。
pub struct Registrar {
    pub(crate) tools: Vec<(ToolDefinition, std::sync::Arc<dyn ToolHandler>)>,
    pub(crate) tool_discovery: Vec<std::sync::Arc<dyn ToolDiscoveryHandler>>,
    pub(crate) tool_metadata: std::collections::HashMap<String, ToolPromptMetadata>,
    pub(crate) commands: Vec<(SlashCommand, std::sync::Arc<dyn CommandHandler>)>,
    pub(crate) command_discovery: Vec<std::sync::Arc<dyn CommandDiscoveryHandler>>,
    pub(crate) pre_tool_use: Vec<(HookMode, i32, std::sync::Arc<dyn PreToolUseHandler>)>,
    pub(crate) post_tool_use: Vec<(HookMode, i32, std::sync::Arc<dyn PostToolUseHandler>)>,
    pub(crate) provider: Vec<(
        ProviderEvent,
        HookMode,
        i32,
        std::sync::Arc<dyn ProviderHandler>,
    )>,
    pub(crate) prompt_build: Vec<(i32, std::sync::Arc<dyn PromptBuildHandler>)>,
    pub(crate) compact: Vec<(CompactEvent, i32, std::sync::Arc<dyn CompactHandler>)>,
    pub(crate) post_tool_use_failure: Vec<(i32, std::sync::Arc<dyn PostToolUseFailureHandler>)>,
    pub(crate) lifecycle: Vec<(
        ExtensionEvent,
        HookMode,
        i32,
        std::sync::Arc<dyn LifecycleHandler>,
    )>,
}

impl Registrar {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            tool_discovery: Vec::new(),
            tool_metadata: std::collections::HashMap::new(),
            commands: Vec::new(),
            command_discovery: Vec::new(),
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
            provider: Vec::new(),
            prompt_build: Vec::new(),
            compact: Vec::new(),
            post_tool_use_failure: Vec::new(),
            lifecycle: Vec::new(),
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
            && self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.provider.is_empty()
            && self.prompt_build.is_empty()
            && self.compact.is_empty()
            && self.post_tool_use_failure.is_empty()
            && self.lifecycle.is_empty()
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
}

impl Default for Registrar {
    fn default() -> Self {
        Self::new()
    }
}
