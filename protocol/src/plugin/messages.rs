//! JSON-RPC 消息类型
//!
//! 定义插件协议中调用阶段的所有消息类型。
//!
//! ## 消息流
//!
//! ```text
//! Host                              Plugin
//!  |                                  |
//!  |--- InvokeMessage --------------->|  调用能力
//!  |<-- EventMessage (Started) -------|  开始通知
//!  |<-- EventMessage (Delta)* --------|  增量输出（可选）
//!  |<-- ResultMessage ---------------|  最终结果
//!  |                                  |
//!  |--- CancelMessage --------------->|  取消请求（可选，随时可发）
//! ```
//!
//! `PluginMessage` 是所有消息类型的 tagged enum，用于传输层的统一序列化/反序列化。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ErrorPayload, InitializeMessage, InvocationContext, ProtocolError};

/// 能力调用请求消息。
///
/// 由 host 发送给插件，请求执行指定能力。
/// `stream` 字段为 true 时，插件应通过 `EventMessage` 返回增量输出。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InvokeMessage {
    /// 调用唯一标识，与后续 `ResultMessage` 和 `EventMessage` 的 `id` 对应
    pub id: String,
    /// 要调用的能力名称
    pub capability: String,
    /// 输入参数
    pub input: Value,
    /// 调用上下文（调用方、工作区、预算等）
    pub context: InvocationContext,
    /// 是否请求流式输出
    #[serde(default)]
    pub stream: bool,
}

/// 事件阶段。
///
/// 用于 `EventMessage` 标识当前事件在调用生命周期中的位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventPhase {
    /// 调用开始
    Started,
    /// 增量输出
    Delta,
    /// 调用成功完成
    Completed,
    /// 调用失败
    Failed,
}

/// 事件消息，用于流式输出中间结果。
///
/// 当 `InvokeMessage` 的 `stream` 为 true 时，插件通过此消息返回增量输出。
/// `seq` 字段保证事件的有序性，前端可据此检测丢包。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventMessage {
    /// 关联的调用 ID（与 `InvokeMessage.id` 对应）
    pub id: String,
    /// 当前事件阶段
    pub phase: EventPhase,
    /// 事件类型名称
    pub event: String,
    /// 事件载荷
    #[serde(default)]
    pub payload: Value,
    /// 序列号，用于保证事件有序
    pub seq: u64,
    /// 失败时的错误信息（仅在 `phase = Failed` 时存在）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

/// 取消请求消息。
///
/// 由 host 发送给插件，请求中断进行中的调用。
/// 插件收到后应尽快停止执行并通过 `ResultMessage` 返回取消状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CancelMessage {
    /// 要取消的调用 ID
    pub id: String,
    /// 取消原因（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 调用结果消息。
///
/// 标志一次能力调用的结束，携带成功/失败状态和输出数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResultMessage {
    /// 关联的调用 ID（与 `InvokeMessage.id` 对应）
    pub id: String,
    /// 结果类型（可选，用于区分不同种类的成功结果）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// 调用是否成功
    pub success: bool,
    /// 输出数据（成功时为结果，失败时为 null）
    #[serde(default)]
    pub output: Value,
    /// 失败时的错误信息
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    /// 扩展元数据
    #[serde(default)]
    pub metadata: Value,
}

/// 插件消息的 tagged enum。
///
/// 采用 `#[serde(tag = "type")]` 序列化策略，通过 `type` 字段区分消息类型。
/// 这是传输层统一序列化/反序列化的根类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PluginMessage {
    /// 握手初始化消息
    Initialize(InitializeMessage),
    /// 能力调用请求
    Invoke(InvokeMessage),
    /// 调用结果
    Result(ResultMessage),
    /// 流式事件
    Event(EventMessage),
    /// 取消请求
    Cancel(CancelMessage),
    /// Hook dispatch 请求（host -> plugin）
    #[serde(rename = "dispatch_hook")]
    HookDispatch(HookDispatchMessage),
    /// Hook 执行结果（plugin -> host）
    #[serde(rename = "hook_result")]
    HookResult(HookResultMessage),
}

// ============================================================================
// Hook Dispatch 消息
// ============================================================================

/// Host 向 external plugin 发送的 hook dispatch 请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookDispatchMessage {
    /// 请求唯一标识，用于关联响应。
    pub correlation_id: String,
    /// 当前 active snapshot id。
    pub snapshot_id: String,
    /// 目标 plugin id。
    pub plugin_id: String,
    /// 目标 hook id。
    pub hook_id: String,
    /// 正式事件名（如 "tool_call"）。
    pub event: String,
    /// typed payload 的序列化表示。
    pub payload: serde_json::Value,
}

/// External plugin 返回的 hook 执行结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookResultMessage {
    /// 与 request 对应的 correlation id。
    pub correlation_id: String,
    /// handler 输出的 effect 列表。
    #[serde(default)]
    pub effects: Vec<HookEffectWire>,
    /// 诊断信息。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<HookDiagnosticWire>,
}

/// Wire 格式的 hook effect。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookEffectWire {
    /// Effect 类型名，如 "Continue", "BlockToolResult", "CancelTurn"。
    pub kind: String,
    /// Effect 负载。
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Wire 格式的诊断消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookDiagnosticWire {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
}

impl ResultMessage {
    /// 构造成功结果。
    ///
    /// `output` 为工具执行的返回值，`metadata` 默认为 `Value::Null`。
    pub fn success(id: impl Into<String>, output: Value) -> Self {
        Self {
            id: id.into(),
            kind: None,
            success: true,
            output,
            error: None,
            metadata: Value::Null,
        }
    }

    /// 构造失败结果。
    ///
    /// `error` 为结构化错误载荷，`output` 设为 `Value::Null`。
    pub fn failure(id: impl Into<String>, error: ErrorPayload) -> Self {
        Self {
            id: id.into(),
            kind: None,
            success: false,
            output: Value::Null,
            error: Some(error),
            metadata: Value::Null,
        }
    }

    /// 解析输出为指定类型。
    ///
    /// 当调用方期望输出为特定 Rust 类型时使用此方法，
    /// 反序列化失败时返回 `ProtocolError::InvalidMessage`。
    pub fn parse_output<T>(&self) -> Result<T, ProtocolError>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.output.clone())
            .map_err(|error| ProtocolError::InvalidMessage(error.to_string()))
    }
}
