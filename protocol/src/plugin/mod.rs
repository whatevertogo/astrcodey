//! 插件协议模块
//!
//! 定义 host（runtime）与插件进程之间基于 JSON-RPC 的通信协议。
//!
//! ## 协议流程
//!
//! 1. **握手阶段**: host 发送 `InitializeMessage`，插件回复 `InitializeResultData` （通过
//!    `ResultMessage` 包装），双方交换能力描述和 peer 信息
//! 2. **调用阶段**: host 发送 `InvokeMessage` 调用插件能力，插件通过 `EventMessage`
//!    流式返回中间结果，最终以 `ResultMessage` 结束
//! 3. **取消**: host 可随时发送 `CancelMessage` 中断进行中的调用
//!
//! ## 传输方式
//!
//! 本模块只定义插件通信的消息格式。
//! 具体传输实现与传输抽象属于插件宿主侧，不属于协议 crate。
//!
//! 与 `http::conversation::v1` / `http::terminal::v1` 一样，插件协议也通过
//! `plugin::v1` 命名空间暴露稳定线缆形状，避免测试与文档围绕历史裸版本号组织。

mod error;
mod handshake;
mod messages;
mod skill_descriptor;
#[cfg(test)]
mod tests;

// Re-export capability types from the capability module
pub use error::{ErrorPayload, ProtocolError};
pub use handshake::{
    InitializeMessage, InitializeResultData, PLUGIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
pub use messages::{
    CancelMessage, EventMessage, EventPhase, HookDiagnosticWire, HookDispatchMessage,
    HookEffectWire, HookResultMessage, InvokeMessage, PluginMessage, ResultMessage,
};
pub use skill_descriptor::{SkillAssetDescriptor, SkillDescriptor};

pub use crate::capability::{
    BudgetHint, CallerRef, CapabilityKind, CapabilityWireDescriptor,
    CapabilityWireDescriptorBuildError, CapabilityWireDescriptorBuilder, FilterDescriptor,
    HandlerDescriptor, InvocationContext, InvocationMode, PeerDescriptor, PeerRole, PermissionSpec,
    ProfileDescriptor, SideEffect, Stability, TriggerDescriptor, WorkspaceRef,
};

/// 插件协议 v1 命名空间。
///
/// 新消费者应优先通过 `astrcode_protocol::plugin::v1::*` 引用线缆类型；
/// 顶层 re-export 保留给现有调用方，避免一次性打断所有使用点。
pub mod v1 {
    pub use super::{
        BudgetHint, CallerRef, CancelMessage, CapabilityKind, CapabilityWireDescriptor,
        CapabilityWireDescriptorBuildError, CapabilityWireDescriptorBuilder, ErrorPayload,
        EventMessage, EventPhase, FilterDescriptor, HandlerDescriptor, HookDiagnosticWire,
        HookDispatchMessage, HookEffectWire, HookResultMessage, InitializeMessage,
        InitializeResultData, InvocationContext, InvocationMode, PLUGIN_PROTOCOL_VERSION,
        PROTOCOL_VERSION, PeerDescriptor, PeerRole, PermissionSpec, PluginMessage,
        ProfileDescriptor, ProtocolError, ResultMessage, SideEffect, SkillAssetDescriptor,
        SkillDescriptor, Stability, TriggerDescriptor, WorkspaceRef,
    };
}
