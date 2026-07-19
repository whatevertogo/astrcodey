use std::{collections::BTreeSet, fmt, path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

use super::ExtensionEventSink;
use crate::{
    config::ModelSelection,
    message_attachment::MessageAttachment,
    tool::{ToolDefinition, ToolPromptMetadata, ToolResult},
};

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

impl CompactTrigger {
    /// 返回触发来源的字符串标识，用于事件记录和审计。
    pub fn as_str(&self) -> &'static str {
        match self {
            CompactTrigger::AutoThreshold => "auto_threshold",
            CompactTrigger::ManualCommand => "manual_command",
            CompactTrigger::ReactivePromptTooLong => "reactive_prompt_too_long",
        }
    }
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

/// Tool hook 作用范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolHookTarget {
    All,
    Names(BTreeSet<String>),
}

impl ToolHookTarget {
    pub fn all() -> Self {
        Self::All
    }

    pub fn names(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Names(names.into_iter().map(Into::into).collect())
    }

    pub fn matches(&self, tool_name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Names(names) => names.contains(tool_name),
        }
    }
}

#[derive(Clone)]
pub struct ToolHookRegistration<H: ?Sized> {
    pub mode: HookMode,
    pub priority: i32,
    pub target: ToolHookTarget,
    pub handler: Arc<H>,
}

#[derive(Clone)]
pub struct ContinueAfterStopRegistration<H: ?Sized> {
    pub priority: i32,
    pub options: ContinueAfterStopOptions,
    pub handler: Arc<H>,
}

#[derive(Clone)]
pub struct UserMessageEnvelopeRegistration<H: ?Sized> {
    pub priority: i32,
    pub handler: Arc<H>,
}

#[derive(Clone)]
pub struct AfterToolResultsRegistration<H: ?Sized> {
    pub priority: i32,
    pub handler: Arc<H>,
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
    /// 是否要求当前 session 空闲。
    #[serde(default)]
    pub requires_idle: bool,
    /// 是否提供参数补全。
    #[serde(default)]
    pub argument_completions: bool,
    /// 同来源命令冲突时的优先级，数值越高优先级越高。
    #[serde(default)]
    pub priority: i32,
}

/// 斜杠命令参数补全项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCompletionItem {
    pub label: String,
    pub insert_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 斜杠命令参数补全结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCompletions {
    #[serde(default)]
    pub items: Vec<CommandCompletionItem>,
    #[serde(default)]
    pub truncated: bool,
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
        /// 可选状态栏更新；避免宿主解析展示文案（如 `/mode` 切换）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_update: Option<StatusItemUpdatePayload>,
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
            status_update: None,
        }
    }

    pub fn display_with_status(
        content: impl Into<String>,
        is_error: bool,
        status_update: StatusItemUpdatePayload,
    ) -> Self {
        Self::Display {
            content: content.into(),
            is_error,
            status_update: Some(status_update),
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
    Block {
        reason: String,
    },
    ModifyInput {
        tool_input: serde_json::Value,
    },
    /// 请求用户审批后再执行（扩展 Gate Ask）。
    Ask {
        prompt: String,
        rule_key: Option<String>,
    },
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

/// LLM 自然结束（无 tool call）后，扩展是否再跑一个 agent step。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueAfterStopResult {
    /// 结束 turn（默认）。
    EndTurn,
    /// 再执行一个 step（可由注册该 hook 时声明的 options 限制）。
    ContinueOneStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinueAfterStopOptions {
    pub max_per_turn: ContinueAfterStopLimit,
}

impl ContinueAfterStopOptions {
    pub const fn limited(max_per_turn: u32) -> Self {
        Self {
            max_per_turn: ContinueAfterStopLimit::limited(max_per_turn),
        }
    }

    pub const fn unlimited() -> Self {
        Self {
            max_per_turn: ContinueAfterStopLimit::unlimited(),
        }
    }

    pub const fn allows(self, continuations_this_turn: u32) -> bool {
        self.max_per_turn.allows(continuations_this_turn)
    }
}

impl Default for ContinueAfterStopOptions {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// 单个 `ContinueAfterStop` hook 在同一个 turn 内可请求的续跑上限。
///
/// S5R manifest 中用数字表示：`-1` 表示无限，非负数表示每 turn 上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub enum ContinueAfterStopLimit {
    Limited { max_per_turn: u32 },
    Unlimited,
}

impl ContinueAfterStopLimit {
    pub const fn limited(max_per_turn: u32) -> Self {
        Self::Limited { max_per_turn }
    }

    pub const fn unlimited() -> Self {
        Self::Unlimited
    }

    pub const fn allows(self, continuations_this_turn: u32) -> bool {
        match self {
            Self::Limited { max_per_turn } => continuations_this_turn < max_per_turn,
            Self::Unlimited => true,
        }
    }
}

impl TryFrom<i64> for ContinueAfterStopLimit {
    type Error = String;

    fn try_from(max_per_turn: i64) -> Result<Self, Self::Error> {
        match max_per_turn {
            -1 => Ok(Self::Unlimited),
            value if (0..=i64::from(u32::MAX)).contains(&value) => Ok(Self::limited(value as u32)),
            _ => {
                Err("continue_after_stop max_per_turn must be -1 or a non-negative integer".into())
            },
        }
    }
}

impl From<ContinueAfterStopLimit> for i64 {
    fn from(budget: ContinueAfterStopLimit) -> Self {
        match budget {
            ContinueAfterStopLimit::Limited { max_per_turn } => i64::from(max_per_turn),
            ContinueAfterStopLimit::Unlimited => -1,
        }
    }
}

#[cfg(test)]
mod continue_after_stop_limit_tests {
    use super::*;

    #[test]
    fn default_options_are_unlimited() {
        assert!(ContinueAfterStopOptions::default().allows(u32::MAX));
    }

    #[test]
    fn serializes_unlimited_as_negative_one() {
        let value = serde_json::to_value(ContinueAfterStopLimit::unlimited()).unwrap();
        assert_eq!(value, serde_json::json!(-1));
    }

    #[test]
    fn deserializes_non_negative_values_as_limited_limit() {
        let limit: ContinueAfterStopLimit = serde_json::from_value(serde_json::json!(7)).unwrap();
        assert_eq!(limit, ContinueAfterStopLimit::limited(7));
    }

    #[test]
    fn rejects_negative_values_other_than_unlimited_sentinel() {
        let error = serde_json::from_value::<ContinueAfterStopLimit>(serde_json::json!(-2))
            .expect_err("negative values other than -1 should be invalid");

        assert!(error.to_string().contains("must be -1"));
    }
}

/// LLM 自然结束后的扩展决策钩子上下文。
#[derive(Debug, Clone)]
pub struct ContinueAfterStopContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub assistant_text: String,
    pub finish_reason: String,
    pub continuations_this_turn: u32,
}

/// 用户消息写入 transcript 前的扩展变换上下文。
#[derive(Debug, Clone)]
pub struct UserMessageEnvelopeContext {
    pub session_id: String,
    pub turn_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub text: String,
    pub attachments: Vec<MessageAttachment>,
    pub session_store_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMessageEnvelopeResult {
    Allow,
    ReplaceText { text: String },
    AppendText { text: String },
    Block { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AfterToolResult {
    pub call_id: crate::types::ToolCallId,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_result: ToolResult,
}

#[derive(Debug, Clone)]
pub struct AfterToolResultsContext {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_results: Vec<AfterToolResult>,
    pub session_store_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterToolResultsResult {
    Continue,
    EndTurn { reason: String },
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
    pub approval_mode: crate::permission::ApprovalMode,
    pub available_tools: Vec<ToolDefinition>,
    /// 当前 turn 事件通道；宿主按扩展能力派生 [`extension_event_sink`]。
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EventPayload>>,
    /// 插件事件发射器（按扩展 id 绑定，内置与磁盘 IPC 扩展共用）。
    pub extension_event_sink: Option<Arc<dyn ExtensionEventSink>>,
    /// session 在存储层的真实目录路径。
    pub session_store_dir: Option<PathBuf>,
}

impl fmt::Debug for PreToolUseContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    /// 插件事件发射器（按扩展 id 绑定，内置与磁盘 IPC 扩展共用）。
    pub extension_event_sink: Option<Arc<dyn ExtensionEventSink>>,
    /// session 在存储层的真实目录路径。
    pub session_store_dir: Option<PathBuf>,
}

impl fmt::Debug for PostToolUseContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    pub session_store_dir: Option<PathBuf>,
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
    /// 插件事件发射器（按扩展 id 绑定，内置与磁盘 IPC 扩展共用）。
    pub extension_event_sink: Option<Arc<dyn ExtensionEventSink>>,
    /// 仅 TurnEnd 事件填充：当轮最后一条 user 和 assistant 消息文本。
    pub last_exchange: Option<ExchangeSummary>,
    /// 仅 StepStart 填充：自上一 agent step 以来新并入上下文的 mid-turn user 消息条数。
    pub mid_turn_user_messages_synced: u32,
}

impl LifecycleContext {
    /// 构造 StepStart 用 ctx，携带本 step 前 sync 的 mid-turn user 消息数。
    pub fn for_step_start(mut self, mid_turn_user_messages_synced: u32) -> Self {
        self.mid_turn_user_messages_synced = mid_turn_user_messages_synced;
        self
    }
}

impl fmt::Debug for LifecycleContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LifecycleContext")
            .field("session_id", &self.session_id)
            .field(
                "extension_event_sink",
                &self.extension_event_sink.as_ref().map(|_| "<sink>"),
            )
            .field("last_exchange", &self.last_exchange)
            .field(
                "mid_turn_user_messages_synced",
                &self.mid_turn_user_messages_synced,
            )
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
    pub session_store_dir: Option<PathBuf>,
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

/// LLM 返回纯文本结束后的继续决策钩子。
#[async_trait::async_trait]
pub trait ContinueAfterStopHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError>;
}

/// 用户消息 envelope 变换钩子。
#[async_trait::async_trait]
pub trait UserMessageEnvelopeHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError>;
}

/// 工具结果批次落盘后的继续/结束决策钩子。
#[async_trait::async_trait]
pub trait AfterToolResultsHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError>;
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

    async fn complete(
        &self,
        _command_name: &str,
        _argument: &str,
        _cursor: usize,
        _working_dir: &str,
        _ctx: &CommandContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        Ok(CommandCompletions::default())
    }
}

/// 动态工具发现处理器。
#[async_trait::async_trait]
pub trait ToolDiscoveryHandler: Send + Sync {
    async fn discover(&self, working_dir: &str) -> Vec<DiscoveredTool>;
}

/// 动态命令发现处理器。
#[async_trait::async_trait]
pub trait CommandDiscoveryHandler: Send + Sync {
    async fn discover(&self, working_dir: &str) -> Vec<(SlashCommand, Arc<dyn CommandHandler>)>;
}

/// Tool contributed by dynamic discovery.
#[derive(Clone)]
pub struct DiscoveredTool {
    pub definition: ToolDefinition,
    pub handler: Arc<dyn ToolHandler>,
    pub prompt_metadata: Option<ToolPromptMetadata>,
}
