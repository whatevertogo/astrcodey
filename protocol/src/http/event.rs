//! HTTP 事件与执行控制相关 DTO。
//!
//! 本模块保留仍被真实传输链路消费的阶段、子运行结果、父子交付与执行控制 DTO。
//! 已删除未落生产链路的 Agent 事件包装层，避免 `protocol` 继续维护一整套空转镜像。

use serde::{Deserialize, Serialize};

/// 协议版本号，用于事件格式的版本控制。
///
/// 每个 `AgentEventEnvelope` 都携带此版本号，前端可根据版本号决定如何解析事件。
pub const PROTOCOL_VERSION: u32 = 1;

/// TODO: 暂时保留 enum 形状与 core 对齐。
/// 等真正出现第二种存储模式时，再一起设计协议面的判别语义，
/// 不要为了“当前只有一个值”提前把它压扁成难扩展的常量字段。
pub use astrcode_core::SubRunStorageMode as SubRunStorageModeDto;
pub use astrcode_core::{
    ArtifactRef as ArtifactRefDto,
    CloseRequestParentDeliveryPayload as CloseRequestParentDeliveryPayloadDto,
    CompletedParentDeliveryPayload as CompletedParentDeliveryPayloadDto,
    ExecutionControl as ExecutionControlDto,
    FailedParentDeliveryPayload as FailedParentDeliveryPayloadDto, ForkMode as ForkModeDto,
    ParentDelivery as ParentDeliveryDto, ParentDeliveryOrigin as ParentDeliveryOriginDto,
    ParentDeliveryPayload as ParentDeliveryPayloadDto,
    ParentDeliveryTerminalSemantics as ParentDeliveryTerminalSemanticsDto, Phase as PhaseDto,
    ProgressParentDeliveryPayload as ProgressParentDeliveryPayloadDto,
    ResolvedSubagentContextOverrides as ResolvedSubagentContextOverridesDto,
    SubRunFailure as SubRunFailureDto, SubRunFailureCode as SubRunFailureCodeDto,
    SubRunHandoff as SubRunHandoffDto, ToolOutputStream as ToolOutputStreamDto,
};

/// `resolvedLimits` 是 presence marker。
///
/// 不能直接复用 unit struct，否则 `Option<ResolvedExecutionLimitsDto>` 会被序列化成 `null`，
/// 反序列化后无法区分 `Some` 与 `None`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedExecutionLimitsDto {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubRunOutcomeDto {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubRunResultDto {
    Running { handoff: SubRunHandoffDto },
    Completed { handoff: SubRunHandoffDto },
    Failed { failure: SubRunFailureDto },
    Cancelled { failure: SubRunFailureDto },
}
