//! 扩展系统类型定义。
//!
//! 扩展是 astrcode 的主要扩展机制。技能、Agent 配置、自定义工具和斜杠命令
//! 都通过这里定义的稳定契约挂入宿主。
//!
//! 本模块只定义契约（trait、capability、hook 类型）：扩展的发现、加载、
//! 路由与进程管理位于 `astrcode-extensions`。

mod call_context;
mod events;
mod hooks;
mod http;
mod lifecycle;
mod package_manifest;
mod paths;
mod registrar;
mod registration_validation;
mod runtime;
mod tool_context;
mod tool_plan_context;

/// Runtime-only construction seam. This module is intentionally absent from author preludes.
#[doc(hidden)]
pub mod internal {
    use std::sync::Arc;

    use astrcode_core::event::{EventDeliveryReceipt, EventSendError};
    use async_trait::async_trait;

    pub use super::registration_validation::{
        canonical_registration_name, custom_event_subscription_matches,
        extension_http_route_patterns_conflict, fixed_hook_mode, has_duplicate_registration_name,
        hook_mode_is_supported, match_extension_http_route, normalize_custom_event_subscription,
        validate_custom_event_subscription, validate_extension_http_route,
    };
    use super::{CustomEventDeclaration, CustomEventEmitter};

    /// Host-bound event ingress. Extension authors emit through [`CustomEventEmitter`].
    #[async_trait]
    pub trait CustomEventSink: Send + Sync {
        async fn emit(
            &self,
            event_type: &str,
            schema_version: u32,
            durable: bool,
            payload: serde_json::Value,
        ) -> Result<EventDeliveryReceipt, EventSendError>;

        fn try_emit(
            &self,
            event_type: &str,
            schema_version: u32,
            durable: bool,
            payload: serde_json::Value,
        ) -> Result<(), EventSendError>;
    }

    pub fn custom_event_emitter(
        declarations: impl IntoIterator<Item = CustomEventDeclaration>,
        sink: Option<Arc<dyn CustomEventSink>>,
    ) -> CustomEventEmitter {
        CustomEventEmitter::from_runtime(declarations, sink)
    }
}

pub use astrcode_core::{
    compaction::{CompactStrategy, CompactTrigger},
    tool::SessionToolSelection,
};
pub use call_context::*;
pub use events::*;
pub use hooks::*;
pub use http::*;
pub use lifecycle::*;
pub use package_manifest::*;
pub use paths::*;
pub use registrar::*;
pub use runtime::*;
pub use tool_context::*;
pub use tool_plan_context::*;

pub use crate::manifest::{ExtensionManifest, ExtensionManifestError};
