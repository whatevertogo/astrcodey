use serde::{Deserialize, Serialize};

use super::{ExtensionError, Registrar};

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
    ///
    /// 若本 step 前有 mid-turn inject 刚并入上下文，见
    /// [`LifecycleContext::mid_turn_user_messages_synced`]。
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
    /// LLM 自然结束（无 tool call）后是否再跑一个 agent step。
    ContinueAfterStop,
    /// 一批工具结果落盘后是否继续 agent loop。
    AfterToolResults,

    // ── 用户输入 ──
    /// 用户提交提示词。
    UserPromptSubmit,
    /// 用户消息写入 durable transcript 前的 envelope 变换。
    UserMessageEnvelope,

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


// ─── extension Event System ────────────────────────────────────────────────

/// 插件在 [`Registrar`] 中声明的事件类型。
///
/// 声明是 emit 时校验的依据：未声明的事件类型会被拒绝，payload 超限也会被拒绝。
/// `extension_id` 不在声明中——它由 runtime 在构造 [`ExtensionEventSink`] 时注入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionEventDecl {
    pub event_type: String,
    #[serde(default = "default_extension_event_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_extension_event_durable")]
    pub durable: bool,
    #[serde(default = "default_extension_event_max_payload_bytes")]
    pub max_payload_bytes: usize,
}

const fn default_extension_event_schema_version() -> u32 {
    1
}

const fn default_extension_event_durable() -> bool {
    true
}

const fn default_extension_event_max_payload_bytes() -> usize {
    64 * 1024
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
    pub(super) fn new(registrar: &'a mut Registrar, event_type: &str) -> Self {
        Self {
            registrar,
            event_type: event_type.to_owned(),
            schema_version: 1,
            durable: true,
            max_payload_bytes: 64 * 1024,
        }
    }

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
            .register_extension_event_decl(ExtensionEventDecl {
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
