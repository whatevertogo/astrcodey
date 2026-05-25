//! astrcode-extension-api — 扩展的稳定 API 边界。
//!
//! 扩展 crate 应依赖此 crate 而非直接依赖 `astrcode-core`。
//!
//! 此 crate 是门面（facade）：所有类型当前从 `astrcode-core` re-export，
//! 但扩展只通过此 crate 访问它们。这建立了稳定性边界——
//! core 内部重构不会直接破坏扩展，只要此 crate 的 API 不变。
//!
//! 未来可以逐步将类型从 core 搬入此 crate，扩展无感。

// ─── Capability 系统 ──────────────────────────────────────────────────

pub use astrcode_core::capability::{
    CapabilityRegistry, Capability, TrustLevel,
    // 能力 newtype
    SessionOpsCap, SessionOpsInner,
    EventQueryCap, EventQueryInner,
    LlmInvokerCap, LlmInvokerInner, LlmStreamEvent,
    // 轻量视图类型
    ConversationView, ModelInfo, PromptMessage, PromptRole,
    SessionSummaryView, TurnView,
};

// ─── Extension 系统 ───────────────────────────────────────────────────

pub use astrcode_core::extension::{
    // 核心 trait
    Extension, ExtensionError,
    // 注册
    Registrar,
    // 上下文
    ExtensionCtx, ExtensionTasks, ExtensionConfig, StopReason,
    // Handler traits
    ToolHandler, CommandHandler, CommandDiscoveryHandler, ToolDiscoveryHandler,
    PreToolUseHandler, PostToolUseHandler, PostToolUseFailureHandler,
    ProviderHandler, PromptBuildHandler, CompactHandler, LifecycleHandler,
    // 上下文结构体
    PreToolUseContext, PostToolUseContext,
    PostToolUseFailureContext, ProviderContext, PromptBuildContext,
    CompactContext, LifecycleContext, CommandContext,
    // 结果枚举
    PreToolUseResult, PostToolUseResult, ProviderResult, CompactResult,
    HookResult, ExtensionCommandResult,
    // Hook 相关
    HookMode, ProviderEvent, CompactEvent, CompactTrigger, ExtensionEvent,
    // Provider 钩子
    ExchangeSummary,
    // 工具发现
    DiscoveredTool,
    // 命令
    SlashCommand,
    // 状态项
    Keybinding, StatusItem,
    // HostServices (Tier 3, 过渡期)
    ExtensionHostServices,
    // 事件
    ExtensionEventSink, ExtensionEventDecl,
    // 子 session 策略
    ChildToolPolicy,
};

// ─── Tool 系统 ────────────────────────────────────────────────────────

pub use astrcode_core::tool::{
    // 核心类型
    ToolDefinition, ToolResult, ToolError, ToolExecutionContext, ToolCapabilities,
    // 执行模式
    ExecutionMode, ToolOrigin, BackgroundPolicy,
    // Session 操作类型 (扩展通过 SessionOpsCap 访问)
    CreateSessionRequest, SubmitTurnRequest, SubmitTurnResult,
    SessionHandle, SessionStatus, SessionApiError, SessionOperations,
    // 元数据
    ToolPromptMetadata, ToolPromptTag, tool_metadata,
    DEFERRED_TOOLS_METADATA_KEY,
    // 依赖 trait
    BackgroundTaskReader, FileObservation, FileObservationStore,
};

// ─── Render 系统 ──────────────────────────────────────────────────────

pub use astrcode_core::render::{
    RenderSpec, RenderKeyValue, RenderTone,
    UI_RENDER_METADATA_KEY, UI_SUMMARY_METADATA_KEY,
};
