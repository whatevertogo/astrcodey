use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};

use super::{ExtensionError, HookMode, internal::ExtensionEventSink};

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

    // ── LLM 提供者钩子 ──
    /// LLM 请求发送前。
    BeforeProviderRequest,
    /// LLM 响应接收后。
    AfterProviderResponse,
    /// LLM 自然结束（无 tool call）后是否再跑一个 agent step。
    ContinueAfterStop,

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

/// Returns whether a lifecycle event may gate host control flow.
///
/// Only turn-entry events run before the corresponding work begins. All other
/// lifecycle events are observations of work that has already started or
/// finished and therefore cannot safely fail closed.
pub fn lifecycle_event_allows_blocking(event: &ExtensionEvent) -> bool {
    matches!(
        event,
        ExtensionEvent::TurnStart | ExtensionEvent::UserPromptSubmit
    )
}

/// Returns the mode encoded by hook families whose dispatcher semantics are fixed.
///
/// Variable-mode hooks return `None`; their accepted modes are described by
/// [`hook_mode_is_supported`].
#[doc(hidden)]
pub fn fixed_hook_mode(event: &ExtensionEvent) -> Option<HookMode> {
    match event {
        ExtensionEvent::AfterProviderResponse => Some(HookMode::Advisory),
        ExtensionEvent::ContinueAfterStop
        | ExtensionEvent::UserMessageEnvelope
        | ExtensionEvent::PromptBuild
        | ExtensionEvent::PreCompact
        | ExtensionEvent::PostCompact => Some(HookMode::Blocking),
        _ => None,
    }
}

/// Returns whether the runtime dispatcher implements `mode` for `event`.
#[doc(hidden)]
pub fn hook_mode_is_supported(event: &ExtensionEvent, mode: HookMode) -> bool {
    if let Some(required) = fixed_hook_mode(event) {
        return mode == required;
    }

    mode != HookMode::Blocking
        || matches!(
            event,
            ExtensionEvent::PreToolUse
                | ExtensionEvent::PostToolUse
                | ExtensionEvent::BeforeProviderRequest
        )
        || lifecycle_event_allows_blocking(event)
}

// ─── extension Event System ────────────────────────────────────────────────

/// 插件在 [`Registrar`] 中声明的事件类型。
///
/// 声明是 emit 时校验的依据：未声明的事件类型会被拒绝，payload 超限也会被拒绝。
/// `extension_id` 不在声明中——它由 runtime internal event sink 注入。
pub const DEFAULT_EXTENSION_EVENT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_EXTENSION_EVENT_DURABLE: bool = true;
pub const DEFAULT_EXTENSION_EVENT_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_EXTENSION_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    DEFAULT_EXTENSION_EVENT_SCHEMA_VERSION
}

const fn default_extension_event_durable() -> bool {
    DEFAULT_EXTENSION_EVENT_DURABLE
}

const fn default_extension_event_max_payload_bytes() -> usize {
    DEFAULT_EXTENSION_EVENT_MAX_PAYLOAD_BYTES
}

/// Extension-scoped event emitter with immutable declaration attribution.
///
/// The runtime constructs this value from the same registration aggregate used by dispatch.
/// Authors choose only the event name and payload; schema version and durability come from the
/// declaration and cannot be changed per emission.
#[derive(Clone, Default)]
pub struct ExtensionEventEmitter {
    declarations: Arc<HashMap<String, ExtensionEventDecl>>,
    sink: Option<Arc<dyn ExtensionEventSink>>,
}

impl ExtensionEventEmitter {
    pub(super) fn from_runtime(
        declarations: impl IntoIterator<Item = ExtensionEventDecl>,
        sink: Option<Arc<dyn ExtensionEventSink>>,
    ) -> Self {
        Self {
            declarations: Arc::new(
                declarations
                    .into_iter()
                    .map(|declaration| (declaration.event_type.clone(), declaration))
                    .collect(),
            ),
            sink,
        }
    }

    pub async fn emit<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<(), ExtensionEventError> {
        self.emit_now(event_type, payload)
    }

    /// Emit from a synchronous lifecycle boundary such as a cancellation guard's `Drop`.
    ///
    /// This shares the exact declaration and attribution checks used by [`Self::emit`]. The
    /// underlying ingress is synchronous, so callers do not need an untracked task merely to
    /// finish cancellation cleanup.
    pub fn emit_now<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<(), ExtensionEventError> {
        let declaration =
            self.declarations
                .get(event_type)
                .ok_or_else(|| ExtensionEventError::Undeclared {
                    event_type: event_type.to_owned(),
                })?;
        let sink = self
            .sink
            .as_ref()
            .ok_or(ExtensionEventError::ContextUnavailable)?;
        let payload =
            serde_json::to_value(payload).map_err(|error| ExtensionEventError::InvalidPayload {
                event_type: event_type.to_owned(),
                message: error.to_string(),
            })?;
        sink.emit(event_type, declaration.schema_version, payload)
            .map_err(|error| ExtensionEventError::Emission {
                event_type: event_type.to_owned(),
                source: Box::new(error),
            })
    }

    pub fn is_declared(&self, event_type: &str) -> bool {
        self.declarations.contains_key(event_type)
    }
}

impl std::fmt::Debug for ExtensionEventEmitter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionEventEmitter")
            .field("declarations", &self.declarations.keys())
            .field("sink", &self.sink.as_ref().map(|_| "<event_sink>"))
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExtensionEventError {
    #[error("extension event `{event_type}` was not declared")]
    Undeclared { event_type: String },
    #[error("extension event emission is unavailable in this call context")]
    ContextUnavailable,
    #[error("extension event `{event_type}` payload is invalid: {message}")]
    InvalidPayload { event_type: String, message: String },
    #[error("failed to emit extension event `{event_type}`: {source}")]
    Emission {
        event_type: String,
        #[source]
        source: Box<ExtensionError>,
    },
}

#[cfg(test)]
mod emitter_tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(String, u32, serde_json::Value)>>);

    impl ExtensionEventSink for RecordingSink {
        fn emit(
            &self,
            event_type: &str,
            schema_version: u32,
            payload: serde_json::Value,
        ) -> Result<(), ExtensionError> {
            self.0
                .lock()
                .unwrap()
                .push((event_type.to_owned(), schema_version, payload));
            Ok(())
        }
    }

    #[tokio::test]
    async fn emitter_owns_declaration_version_and_reports_missing_declaration_or_sink() {
        let sink = Arc::new(RecordingSink::default());
        let emitter = ExtensionEventEmitter::from_runtime(
            [ExtensionEventDecl {
                event_type: "review.completed".into(),
                schema_version: 3,
                durable: true,
                max_payload_bytes: 1024,
            }],
            Some(sink.clone()),
        );
        emitter
            .emit("review.completed", &serde_json::json!({ "status": "ok" }))
            .await
            .unwrap();
        emitter
            .emit_now(
                "review.completed",
                &serde_json::json!({ "status": "cancelled" }),
            )
            .unwrap();
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[
                (
                    "review.completed".into(),
                    3,
                    serde_json::json!({ "status": "ok" })
                ),
                (
                    "review.completed".into(),
                    3,
                    serde_json::json!({ "status": "cancelled" })
                )
            ]
        );
        assert!(matches!(
            emitter.emit("review.failed", &()).await,
            Err(ExtensionEventError::Undeclared { .. })
        ));

        let detached = ExtensionEventEmitter::from_runtime(
            [ExtensionEventDecl {
                event_type: "review.completed".into(),
                schema_version: 1,
                durable: false,
                max_payload_bytes: 1024,
            }],
            None,
        );
        assert!(matches!(
            detached.emit("review.completed", &()).await,
            Err(ExtensionEventError::ContextUnavailable)
        ));
    }
}
