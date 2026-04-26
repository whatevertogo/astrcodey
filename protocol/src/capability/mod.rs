//! 能力 wire 描述与调用上下文。
//!
//! `CapabilitySpec` 是 canonical owner，也是运行时内部唯一能力语义真相；
//! `protocol` 只保留 `CapabilityWireDescriptor` 这一 transport 名称和
//! protocol-owned 的上下文字段。

mod descriptors;

pub use descriptors::{
    BudgetHint, CallerRef, CapabilityKind, CapabilityWireDescriptor,
    CapabilityWireDescriptorBuildError, CapabilityWireDescriptorBuilder, FilterDescriptor,
    HandlerDescriptor, InvocationContext, InvocationMode, PeerDescriptor, PeerRole, PermissionSpec,
    ProfileDescriptor, SideEffect, Stability, TriggerDescriptor, WorkspaceRef,
};
