//! 能力 wire 描述与调用上下文。
//!
//! `CapabilityWireDescriptor` 是插件协议中的 transport 载荷名称；
//! 真正的 canonical 能力模型在 `astrcode_core::CapabilitySpec`。
//! 这里不再维护第二套语义重复的枚举、builder 与校验逻辑。

/// 插件握手阶段交换的能力 wire 描述。
pub use astrcode_core::CapabilitySpec as CapabilityWireDescriptor;
/// `CapabilityWireDescriptor` 的校验错误，直接复用 core 校验错误。
pub use astrcode_core::CapabilitySpecBuildError as CapabilityWireDescriptorBuildError;
/// `CapabilityWireDescriptor` 的构建器，直接复用 core builder。
pub use astrcode_core::CapabilitySpecBuilder as CapabilityWireDescriptorBuilder;
pub use astrcode_core::{CapabilityKind, InvocationMode, PermissionSpec, SideEffect, Stability};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 通信对等方的角色类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    Core,
    Plugin,
    Runtime,
    Worker,
    Supervisor,
}

/// 通信对等方的描述信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerDescriptor {
    pub id: String,
    pub name: String,
    pub role: PeerRole,
    pub version: String,
    #[serde(default)]
    pub supported_profiles: Vec<String>,
    #[serde(default)]
    pub metadata: Value,
}

/// 触发器描述符。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TriggerDescriptor {
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub metadata: Value,
}

/// 过滤器描述符。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FilterDescriptor {
    pub field: String,
    pub op: String,
    pub value: String,
}

/// 事件处理器描述符。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HandlerDescriptor {
    pub id: String,
    pub trigger: TriggerDescriptor,
    pub input_schema: Value,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub filters: Vec<FilterDescriptor>,
    #[serde(default)]
    pub permissions: Vec<PermissionSpec>,
}

/// Profile 描述符。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDescriptor {
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub context_schema: Value,
    #[serde(default)]
    pub metadata: Value,
}

/// 调用方引用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CallerRef {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub metadata: Value,
}

/// 工作区引用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

/// 预算提示。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BudgetHint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_events: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// 调用上下文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InvocationContext {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<CallerRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetHint>,
    pub profile: String,
    #[serde(default)]
    pub profile_context: Value,
    #[serde(default)]
    pub metadata: Value,
}
