//! 运行时状态与指标 DTO
//!
//! 定义运行时健康检查、指标查询、插件状态等接口的响应结构。
//! 这些数据用于前端展示系统运行状态、性能指标和插件健康度。

pub use astrcode_core::{
    AgentCollaborationScorecardSnapshot as AgentCollaborationScorecardDto,
    ExecutionDiagnosticsSnapshot as ExecutionDiagnosticsDto,
    OperationMetricsSnapshot as OperationMetricsDto, ReplayMetricsSnapshot as ReplayMetricsDto,
    RuntimeObservabilitySnapshot as RuntimeMetricsDto,
    SubRunExecutionMetricsSnapshot as SubRunExecutionMetricsDto,
};
use serde::{Deserialize, Serialize};

/// 插件生命周期状态 wire DTO。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginRuntimeStateDto {
    Discovered,
    Initialized,
    Failed,
}

/// 插件健康状态 wire DTO。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginHealthDto {
    Unknown,
    Healthy,
    Degraded,
    Unavailable,
}

/// 运行时能力的摘要信息。
///
/// 用于 `GET /api/runtime/status` 响应中列出 runtime 暴露的所有能力。
/// `profiles` 字段指示此能力在哪些 profile 下可用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCapabilityDto {
    /// 能力名称
    pub name: String,
    /// 能力类型（如 "tool", "context_provider" 等）
    pub kind: String,
    /// 能力描述
    pub description: String,
    /// 此能力可用的 profile 列表
    pub profiles: Vec<String>,
    /// 是否支持流式输出
    pub streaming: bool,
}

/// 插件运行时状态。
/// 运行时中单个插件的状态摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePluginDto {
    /// 插件名称
    pub name: String,
    /// 插件版本
    pub version: String,
    /// 插件描述
    pub description: String,
    /// 当前运行时状态
    pub state: PluginRuntimeStateDto,
    /// 健康度
    pub health: PluginHealthDto,
    /// 累计失败次数
    pub failure_count: u32,
    /// 最近一次失败的错误信息
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// 非致命 warning 列表。
    ///
    /// 这里显式保留 warning，是为了让前端能展示“插件已加载，但 skill 资源或
    /// allowed_tools 校验有降级”的状态，而不必把它误判为插件彻底失败。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// 最近一次健康检查的时间戳（ISO 8601）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// 此插件注册的能力列表
    pub capabilities: Vec<RuntimeCapabilityDto>,
}

/// `GET /api/runtime/status` 响应体——运行时完整状态。
///
/// 包含运行时标识、活跃会话、插件状态、指标和能力列表。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatusDto {
    /// 运行时名称
    pub runtime_name: String,
    /// 运行时类型（如 "local", "remote" 等）
    pub runtime_kind: String,
    /// 当前加载的会话数量
    pub loaded_session_count: usize,
    /// 正在运行的会话 ID 列表
    pub running_session_ids: Vec<String>,
    /// 插件搜索路径列表
    pub plugin_search_paths: Vec<String>,
    /// 运行时指标
    pub metrics: RuntimeMetricsDto,
    /// 暴露的能力列表
    pub capabilities: Vec<RuntimeCapabilityDto>,
    /// 已加载的插件列表
    pub plugins: Vec<RuntimePluginDto>,
}

/// `POST /api/runtime/reload` 响应体——运行时重载后的状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeReloadResponseDto {
    /// 重载完成的时间戳（ISO 8601）
    pub reloaded_at: String,
    /// 重载后的运行时状态
    pub status: RuntimeStatusDto,
}
