//! 扩展与钩子系统类型定义。
//!
//! 扩展是 astrcode 的主要扩展机制。技能（Skills）、Agent 配置文件、
//! 自定义工具、斜杠命令等都是通过扩展来实现的。
//!
//! 本模块定义了：
//! - [`Extension`] trait：扩展的核心接口
//! - [`ExtensionEvent`]：扩展可订阅的生命周期事件
//! - [`HookMode`] / [`HookEffect`]：钩子的执行模式和返回结果
//! - [`ExtensionContext`]：扩展可访问的受限上下文
//! - [`AgentProfile`]：Agent 协作配置文件

use serde::{Deserialize, Serialize};

use crate::{
    config::ModelSelection,
    tool::{ToolDefinition, ToolResult},
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

    /// 返回此扩展订阅的事件及其钩子模式。
    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)>;

    /// 处理事件。
    ///
    /// 返回 [`HookEffect`] 以允许、阻止或修改操作。
    async fn on_event(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError>;

    /// 可选：返回此扩展注册的工具列表。
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }

    /// 可选：执行 `tools()` 返回的某个工具。
    ///
    /// 默认实现返回 `NotFound` 错误，保持仅元数据的扩展有效。
    /// 运行器会将可执行扩展工具适配到正常的工具管道中。
    /// `ctx` 携带每次调用的会话上下文（session_id、model、可用工具）。
    async fn execute_tool(
        &self,
        tool_name: &str,
        _arguments: serde_json::Value,
        _working_dir: &str,
        _ctx: &crate::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        Err(ExtensionError::NotFound(tool_name.into()))
    }

    /// 可选：返回此扩展注册的斜杠命令列表。
    fn slash_commands(&self) -> Vec<SlashCommand> {
        vec![]
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
    /// 会话关闭。
    SessionShutdown,

    // ── 轮次级别 ──
    /// 轮次开始。
    TurnStart,
    /// 轮次结束。
    TurnEnd,

    // ── 工具级别（主要钩子点） ──
    /// 工具执行前。
    PreToolUse,
    /// 工具执行后。
    PostToolUse,

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
}

// ─── Hook Input / Output ─────────────────────────────────────────────────

/// PreToolUse 钩子的输入数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreToolUseInput {
    /// 即将执行的工具名称。
    pub tool_name: String,
    /// 工具的输入参数。
    pub tool_input: serde_json::Value,
}

/// PostToolUse 钩子的输入数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostToolUseInput {
    /// 已执行的工具名称。
    pub tool_name: String,
    /// 工具的输入参数。
    pub tool_input: serde_json::Value,
    /// 工具执行结果。
    pub tool_result: ToolResult,
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

// ─── Hook Effect ─────────────────────────────────────────────────────────

/// 钩子执行的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEffect {
    /// 允许操作正常继续。
    Allow,

    /// 阻止操作并附带原因。仅 Blocking 钩子有效。
    Block { reason: String },

    /// 修改工具输入后再执行（仅 PreToolUse）。
    ModifiedInput { tool_input: serde_json::Value },

    /// 修改工具执行后的结果内容（仅 PostToolUse）。
    ModifiedResult { content: String },

    /// 修改发送给 LLM 的消息列表（仅 BeforeProviderRequest）。
    ModifiedMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },

    /// 向发送给 LLM 的消息列表尾部追加消息（仅 BeforeProviderRequest）。
    AppendMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },

    /// 修改 LLM 流式输出后的文本（仅 AfterProviderResponse）。
    ModifiedOutput { text: String },

    /// 为 prompt 组装提供受控片段（仅 PromptBuild）。
    PromptContributions(PromptContributions),
}

/// 插件在 PromptBuild hook 中提供的 prompt 片段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptContributions {
    /// 插件系统提示词。宿主会放在 system prompt 最前面，即模型可见工具声明之后。
    #[serde(default)]
    pub system_prompts: Vec<String>,
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
        self.skills.extend(other.skills);
        self.agents.extend(other.agents);
    }
}

// ─── Extension Capabilities Summary ──────────────────────────────────────

/// 扩展提供的能力摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionCapabilities {
    /// 扩展 ID。
    pub id: String,
    /// 订阅的事件及其模式。
    pub events: Vec<(ExtensionEvent, HookMode)>,
    /// 注册的工具数量。
    pub tool_count: usize,
    /// 注册的斜杠命令数量。
    pub command_count: usize,
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

// ─── Extension Context ───────────────────────────────────────────────────

/// 扩展处理器可访问的受限会话和服务视图。
///
/// 扩展获得有限的 API 接口，以防止它们破坏核心系统的稳定性。
#[async_trait::async_trait]
pub trait ExtensionContext: Send + Sync {
    /// 获取当前会话 ID。
    fn session_id(&self) -> &str;

    /// 获取当前会话的工作目录。
    fn working_dir(&self) -> &str;

    /// 获取当前的模型选择配置。
    fn model_selection(&self) -> ModelSelection;

    /// 按键名读取配置值。
    fn config_value(&self, key: &str) -> Option<String>;

    /// 向会话日志发送自定义事件。
    async fn emit_custom_event(&self, name: &str, data: serde_json::Value);

    /// 从工具注册表中按名称查找工具定义。
    fn find_tool(&self, name: &str) -> Option<ToolDefinition>;

    /// 获取当前 PreToolUse 载荷（仅在工具钩子上下文中可用）。
    fn pre_tool_use_input(&self) -> Option<PreToolUseInput> {
        None
    }

    /// 获取当前 PostToolUse 载荷（仅在工具钩子上下文中可用）。
    fn post_tool_use_input(&self) -> Option<PostToolUseInput> {
        None
    }

    /// 注册工具以注入到当前工具快照中。
    ///
    /// 通过此方法注册的工具会由宿主收集，并在构建工具快照时应用。
    fn register_tool(&self, _def: ToolDefinition) {}

    /// 排空所有通过 `register_tool()` 注册的工具。
    fn drain_registered_tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }

    /// 获取即将发送给 LLM 的消息列表（用于 BeforeProviderRequest 钩子）。
    fn provider_messages(&self) -> Option<Vec<crate::llm::LlmMessage>> {
        None
    }

    /// 记录警告诊断信息（在服务器日志中可见）。
    fn log_warn(&self, msg: &str);

    /// 创建此上下文的快照，适用于即发即弃钩子中使用。
    fn snapshot(&self) -> std::sync::Arc<dyn ExtensionContext>;
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
    /// - `allowed_tools` 为空表示继承父会话的工具集。
    /// - `model_preference` 在 v1 中仅为建议值。
    RunSession {
        /// 子会话的显示名称。
        name: String,
        /// 追加到全局系统提示词之后的指令。
        system_prompt: String,
        /// 发送给子会话的用户提示词。
        user_prompt: String,
        /// 允许使用的工具列表（空 = 继承父会话）。
        #[serde(default)]
        allowed_tools: Vec<String>,
        /// 建议使用的模型（v1 中仅为建议）。
        #[serde(default)]
        model_preference: Option<String>,
    },
}

// ─── Agent Profile (basic type for collaboration tools) ──────────────────

/// Agent 配置文件——一个命名的 Agent 配置。
///
/// 核心层仅定义类型，加载和管理由扩展完成。
/// Agent 协作工具（spawn/send/observe/close）使用此类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    /// 配置文件标识符。
    pub id: String,
    /// 显示名称。
    pub name: String,
    /// 此 Agent 的功能描述。
    pub description: String,
    /// 此 Agent 类型的指导指令。
    pub guide: String,
    /// 此 Agent 可使用的工具列表（空 = 所有可用工具）。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// 此 Agent 类型偏好的模型。
    pub model_preference: Option<String>,
}
