use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{EventDeliveryReceipt, EventSendError},
    types::{EventId, SessionId},
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{ExtensionCallContext, ExtensionError, HookMode, internal::CustomEventSink};

// ─── Lifecycle Events ────────────────────────────────────────────────────

/// 扩展可订阅的核心生命周期事件。
///
/// 覆盖会话/轮次/工具/LLM 提供者/prompt 组装的完整生命周期。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
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

    // ── Recap ──
    /// Recap 生成完成后通知扩展（非阻塞）。
    PostRecap,
}

/// Returns whether a lifecycle event may gate host control flow.
///
/// Only turn-entry events run before the corresponding work begins. All other
/// lifecycle events are observations of work that has already started or
/// finished and therefore cannot safely fail closed.
pub fn lifecycle_event_allows_blocking(event: &LifecycleEvent) -> bool {
    matches!(
        event,
        LifecycleEvent::TurnStart | LifecycleEvent::UserPromptSubmit
    )
}

/// Returns the mode encoded by hook families whose dispatcher semantics are fixed.
///
/// Variable-mode hooks return `None`; their accepted modes are described by
/// [`hook_mode_is_supported`].
#[doc(hidden)]
pub fn fixed_hook_mode(event: &LifecycleEvent) -> Option<HookMode> {
    match event {
        LifecycleEvent::AfterProviderResponse => Some(HookMode::Advisory),
        LifecycleEvent::ContinueAfterStop
        | LifecycleEvent::UserMessageEnvelope
        | LifecycleEvent::PromptBuild => Some(HookMode::Blocking),
        _ => None,
    }
}

/// Returns whether the runtime dispatcher implements `mode` for `event`.
#[doc(hidden)]
pub fn hook_mode_is_supported(event: &LifecycleEvent, mode: HookMode) -> bool {
    if let Some(required) = fixed_hook_mode(event) {
        return mode == required;
    }

    mode != HookMode::Blocking
        || matches!(
            event,
            LifecycleEvent::PreToolUse
                | LifecycleEvent::PostToolUse
                | LifecycleEvent::BeforeProviderRequest
        )
        || lifecycle_event_allows_blocking(event)
}

// ─── Custom Event System ───────────────────────────────────────────────────

/// 插件在 [`Registrar`] 中声明的 custom event 类型。
///
/// 声明是 emit 时校验的依据：未声明的事件类型会被拒绝，payload 超限也会被拒绝。
/// `extension_id` 不在声明中——它由 runtime internal event sink 注入。
pub const DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CUSTOM_EVENT_DURABLE: bool = true;
pub const DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_CUSTOM_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
/// In-process 与 worker 两条注册路径共用的订阅 id 长度上限。
pub const MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CustomEventDeclaration {
    pub event_type: String,
    #[serde(default = "default_custom_event_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_custom_event_durable")]
    pub durable: bool,
    #[serde(default = "default_custom_event_max_payload_bytes")]
    pub max_payload_bytes: usize,
}

/// Restricts a custom-event subscription to one producer or accepts every producer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CustomEventSourceFilter {
    Any,
    Extension { extension_id: String },
}

/// Exact custom-event subscription registered by a consuming extension.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CustomEventSubscription {
    pub id: String,
    pub event_type: String,
    pub source: CustomEventSourceFilter,
}

impl CustomEventSubscription {
    /// 订阅任意来源的 `event_type`；订阅 id 派生为 `event_type` 本身。
    pub fn any(event_type: impl Into<String>) -> Self {
        let event_type = event_type.into();
        Self {
            id: event_type.clone(),
            event_type,
            source: CustomEventSourceFilter::Any,
        }
    }

    /// 订阅指定扩展的 `event_type`；订阅 id 派生为 `{extension_id}:{event_type}`。
    pub fn from_extension(extension_id: impl Into<String>, event_type: impl Into<String>) -> Self {
        let extension_id = extension_id.into();
        let event_type = event_type.into();
        Self {
            id: format!("{extension_id}:{event_type}"),
            event_type,
            source: CustomEventSourceFilter::Extension { extension_id },
        }
    }

    pub fn named(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// 注册路径共用的规范化：裁剪作者侧可能带入的空白。
    pub(crate) fn normalize(&mut self) {
        self.id = self.id.trim().to_owned();
        self.event_type = self.event_type.trim().to_owned();
        if let CustomEventSourceFilter::Extension { extension_id } = &mut self.source {
            *extension_id = extension_id.trim().to_owned();
        }
    }

    /// 注册路径共用的字段校验；重复 id 由各注册路径按自身错误语义检查。
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN {
            return Err(format!(
                "invalid custom event subscription id `{}`",
                self.id
            ));
        }
        if self.event_type.is_empty() {
            return Err("custom event subscription type cannot be empty".to_owned());
        }
        if matches!(
            &self.source,
            CustomEventSourceFilter::Extension { extension_id } if extension_id.is_empty()
        ) {
            return Err("custom event subscription source extension cannot be empty".to_owned());
        }
        Ok(())
    }

    pub fn matches(&self, extension_id: &str, event_type: &str) -> bool {
        self.event_type == event_type
            && match &self.source {
                CustomEventSourceFilter::Any => true,
                CustomEventSourceFilter::Extension {
                    extension_id: expected,
                } => expected == extension_id,
            }
    }
}

/// Host-attributed input for a custom-event consumer.
#[derive(Clone)]
pub struct CustomEventContext {
    call: ExtensionCallContext,
    session_id: SessionId,
    event_id: EventId,
    seq: Option<u64>,
    source_extension_id: String,
    event_type: String,
    schema_version: u32,
    causation_id: Option<EventId>,
    cascade_depth: u8,
    payload: serde_json::Value,
}

impl CustomEventContext {
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        session_id: SessionId,
        event_id: EventId,
        seq: Option<u64>,
        source_extension_id: String,
        event_type: String,
        schema_version: u32,
        causation_id: Option<EventId>,
        cascade_depth: u8,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            call,
            session_id,
            event_id,
            seq,
            source_extension_id,
            event_type,
            schema_version,
            causation_id,
            cascade_depth,
            payload,
        }
    }

    pub fn call(&self) -> &ExtensionCallContext {
        &self.call
    }

    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.call.turn_id()
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    pub fn source_extension_id(&self) -> &str {
        &self.source_extension_id
    }

    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn causation_id(&self) -> Option<&EventId> {
        self.causation_id.as_ref()
    }

    pub fn cascade_depth(&self) -> u8 {
        self.cascade_depth
    }

    pub fn is_durable(&self) -> bool {
        self.seq.is_some()
    }

    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }

    pub fn events(&self) -> &CustomEventEmitter {
        self.call.events()
    }
}

#[async_trait]
pub trait CustomEventHandler: Send + Sync {
    async fn handle(&self, ctx: CustomEventContext) -> Result<(), ExtensionError>;
}

const fn default_custom_event_schema_version() -> u32 {
    DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION
}

const fn default_custom_event_durable() -> bool {
    DEFAULT_CUSTOM_EVENT_DURABLE
}

const fn default_custom_event_max_payload_bytes() -> usize {
    DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES
}

/// Extension-scoped event emitter with immutable declaration attribution.
///
/// The runtime constructs this value from the same registration aggregate used by dispatch.
/// Authors choose only the event name and payload; schema version and durability come from the
/// declaration and cannot be changed per emission.
#[derive(Clone, Default)]
pub struct CustomEventEmitter {
    declarations: Arc<HashMap<String, CustomEventDeclaration>>,
    sink: Option<Arc<dyn CustomEventSink>>,
}

impl CustomEventEmitter {
    pub(super) fn from_runtime(
        declarations: impl IntoIterator<Item = CustomEventDeclaration>,
        sink: Option<Arc<dyn CustomEventSink>>,
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

    /// Emit an event and wait until the host reports its publication state.
    ///
    /// Session-scoped durable events return [`EventDeliveryReceipt::Persisted`] only after they
    /// have a storage sequence. Unscoped hosts that cannot expose completion return
    /// [`EventDeliveryReceipt::Accepted`].
    pub async fn emit<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<EventDeliveryReceipt, CustomEventEmitError> {
        let (declaration, payload) = self.prepare(event_type, payload)?;
        self.sink()?
            .emit(
                event_type,
                declaration.schema_version,
                declaration.durable,
                payload,
            )
            .await
            .map_err(|error| map_send_error(event_type, error))
    }

    /// Try to enqueue an event from a synchronous lifecycle boundary such as a cancellation
    /// guard's `Drop`.
    ///
    /// Success confirms queue admission only. Async handlers should use [`Self::emit`] so queue
    /// pressure and publication failures are observable.
    pub fn try_emit<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<(), CustomEventEmitError> {
        let (declaration, payload) = self.prepare(event_type, payload)?;
        self.sink()?
            .try_emit(
                event_type,
                declaration.schema_version,
                declaration.durable,
                payload,
            )
            .map_err(|error| map_send_error(event_type, error))
    }

    fn prepare<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<(&CustomEventDeclaration, serde_json::Value), CustomEventEmitError> {
        let declaration =
            self.declarations
                .get(event_type)
                .ok_or_else(|| CustomEventEmitError::Undeclared {
                    event_type: event_type.to_owned(),
                })?;
        let payload = serde_json::to_value(payload).map_err(|error| {
            CustomEventEmitError::InvalidPayload {
                event_type: event_type.to_owned(),
                message: error.to_string(),
            }
        })?;
        let payload_bytes = serde_json::to_vec(&payload)
            .map_err(|error| CustomEventEmitError::InvalidPayload {
                event_type: event_type.to_owned(),
                message: error.to_string(),
            })?
            .len();
        if payload_bytes > declaration.max_payload_bytes {
            return Err(CustomEventEmitError::PayloadTooLarge {
                event_type: event_type.to_owned(),
                actual_bytes: payload_bytes,
                max_bytes: declaration.max_payload_bytes,
            });
        }
        Ok((declaration, payload))
    }

    fn sink(&self) -> Result<&dyn CustomEventSink, CustomEventEmitError> {
        self.sink
            .as_deref()
            .ok_or(CustomEventEmitError::ContextUnavailable)
    }
}

impl std::fmt::Debug for CustomEventEmitter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CustomEventEmitter")
            .field("declarations", &self.declarations.keys())
            .field("sink", &self.sink.as_ref().map(|_| "<event_sink>"))
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CustomEventEmitError {
    #[error("custom event `{event_type}` was not declared")]
    Undeclared { event_type: String },
    #[error("custom event emission is unavailable in this call context")]
    ContextUnavailable,
    #[error("custom event `{event_type}` payload is invalid: {message}")]
    InvalidPayload { event_type: String, message: String },
    #[error(
        "custom event `{event_type}` payload is {actual_bytes} bytes, exceeding {max_bytes} bytes"
    )]
    PayloadTooLarge {
        event_type: String,
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("custom event `{event_type}` ingress is full")]
    QueueFull { event_type: String },
    #[error("custom event `{event_type}` ingress is closed")]
    IngressClosed { event_type: String },
    #[error("custom event `{event_type}` publication failed: {message}")]
    Publication { event_type: String, message: String },
}

fn map_send_error(event_type: &str, error: EventSendError) -> CustomEventEmitError {
    match error {
        EventSendError::Full => CustomEventEmitError::QueueFull {
            event_type: event_type.to_owned(),
        },
        EventSendError::Closed => CustomEventEmitError::IngressClosed {
            event_type: event_type.to_owned(),
        },
        EventSendError::PublishFailed(message) => CustomEventEmitError::Publication {
            event_type: event_type.to_owned(),
            message,
        },
    }
}

#[cfg(test)]
mod emitter_tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(String, u32, serde_json::Value)>>);

    impl RecordingSink {
        fn record(&self, event_type: &str, schema_version: u32, payload: serde_json::Value) {
            self.0
                .lock()
                .unwrap()
                .push((event_type.to_owned(), schema_version, payload));
        }
    }

    #[async_trait::async_trait]
    impl CustomEventSink for RecordingSink {
        async fn emit(
            &self,
            event_type: &str,
            schema_version: u32,
            _durable: bool,
            payload: serde_json::Value,
        ) -> Result<EventDeliveryReceipt, EventSendError> {
            self.record(event_type, schema_version, payload);
            Ok(EventDeliveryReceipt::Accepted)
        }

        fn try_emit(
            &self,
            event_type: &str,
            schema_version: u32,
            _durable: bool,
            payload: serde_json::Value,
        ) -> Result<(), EventSendError> {
            self.record(event_type, schema_version, payload);
            Ok(())
        }
    }

    #[tokio::test]
    async fn emitter_owns_declaration_version_and_reports_missing_declaration_or_sink() {
        let sink = Arc::new(RecordingSink::default());
        let emitter = CustomEventEmitter::from_runtime(
            [CustomEventDeclaration {
                event_type: "review.completed".into(),
                schema_version: 3,
                durable: true,
                max_payload_bytes: 1024,
            }],
            Some(sink.clone()),
        );
        assert_eq!(
            emitter
                .emit("review.completed", &serde_json::json!({ "status": "ok" }))
                .await
                .unwrap(),
            EventDeliveryReceipt::Accepted
        );
        emitter
            .try_emit(
                "review.completed",
                &serde_json::json!({ "status": "cancelled" }),
            )
            .unwrap();
        emitter
            .try_emit(
                "review.completed",
                &serde_json::json!({ "status": "published" }),
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
                ),
                (
                    "review.completed".into(),
                    3,
                    serde_json::json!({ "status": "published" })
                )
            ]
        );
        assert!(matches!(
            emitter.try_emit("review.failed", &()),
            Err(CustomEventEmitError::Undeclared { .. })
        ));

        let detached = CustomEventEmitter::from_runtime(
            [CustomEventDeclaration {
                event_type: "review.completed".into(),
                schema_version: 1,
                durable: false,
                max_payload_bytes: 1024,
            }],
            None,
        );
        assert!(matches!(
            detached.try_emit("review.completed", &()),
            Err(CustomEventEmitError::ContextUnavailable)
        ));

        let bounded = CustomEventEmitter::from_runtime(
            [CustomEventDeclaration {
                event_type: "review.completed".into(),
                schema_version: 1,
                durable: false,
                max_payload_bytes: 2,
            }],
            Some(sink),
        );
        assert!(matches!(
            bounded.try_emit(
                "review.completed",
                &serde_json::json!({ "status": "too-large" })
            ),
            Err(CustomEventEmitError::PayloadTooLarge { .. })
        ));
        assert!(matches!(
            map_send_error("review.completed", EventSendError::Full),
            CustomEventEmitError::QueueFull { .. }
        ));
        assert!(matches!(
            map_send_error("review.completed", EventSendError::Closed),
            CustomEventEmitError::IngressClosed { .. }
        ));
        assert!(matches!(
            map_send_error(
                "review.completed",
                EventSendError::PublishFailed("storage unavailable".into())
            ),
            CustomEventEmitError::Publication { .. }
        ));
    }
}
