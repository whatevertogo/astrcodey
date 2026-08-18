mod client;
mod domain_client;
mod error;
mod llm_mapping;
mod workspace_patch;

use std::sync::Arc;

pub use client::{
    ExtensionHttpClient, ModelClient, NetworkClient, ProcessClient, SessionControlClient,
    SessionHistoryClient, SessionInspectClient, SessionStateClient, ToolResultClient,
    WorkspaceClient,
};
use domain_client::HostClientTransport;
pub use error::HostError;
pub use llm_mapping::llm_chat_request;
use serde_json::Value;
pub use workspace_patch::{
    WorkspacePatchPathError, WorkspacePatchPaths, analyze_unified_diff_paths, is_patch_metadata,
    normalize_unified_diff_path,
};

pub use crate::wire::HostOperation;
// Author-facing re-export of the host wire contract. `wire::host` is the
// canonical definition site; this glob keeps the authoring path (`host::X`)
// in sync without a hand-maintained name list.
pub use crate::wire::host::*;
use crate::{
    extension::ExtensionCapability,
    model_stream::ModelStream,
    wire::{HostContextRequirement, WireErrorCode},
};

/// Instance-scoped access to typed host domains.
#[derive(Clone)]
pub struct ExtensionHost {
    inner: Arc<ExtensionHostInner>,
}

struct ExtensionHostInner {
    invoker: Arc<dyn internal::HostInvoker>,
    scope: internal::HostScope,
}

impl ExtensionHost {
    /// Access the model domain. One of [`ExtensionCapability::MainModel`] or
    /// [`ExtensionCapability::SmallModel`] must be granted; per-operation
    /// availability is reported by [`ModelClient::main_available`] and
    /// [`ModelClient::small_available`].
    pub fn models(&self) -> Result<ModelClient, HostError> {
        self.inner.scope.preflight_any_capability(
            &[
                ExtensionCapability::MainModel,
                ExtensionCapability::SmallModel,
            ],
            "models",
        )?;
        Ok(ModelClient::new(self.clone()))
    }

    pub fn session_control(&self) -> Result<SessionControlClient, HostError> {
        if !self
            .inner
            .scope
            .is_granted(ExtensionCapability::SessionControl)
            && !self
                .inner
                .scope
                .is_granted(ExtensionCapability::InputDelivery)
        {
            self.inner.scope.preflight_any_capability(
                &[
                    ExtensionCapability::SessionControl,
                    ExtensionCapability::InputDelivery,
                ],
                "session_control",
            )?;
        }
        if self
            .inner
            .scope
            .has_callable_operation_for(ExtensionCapability::SessionControl)
            || self
                .inner
                .scope
                .has_callable_operation_for(ExtensionCapability::InputDelivery)
        {
            return Ok(SessionControlClient::new(self.clone()));
        }
        if self
            .inner
            .scope
            .is_granted(ExtensionCapability::SessionControl)
        {
            self.inner
                .scope
                .preflight_context(HostContextRequirement::Session, "session_control")?;
        }
        if self
            .inner
            .scope
            .is_granted(ExtensionCapability::InputDelivery)
        {
            self.inner.scope.preflight_available_operation_context(
                ExtensionCapability::InputDelivery,
                "session_control",
            )?;
        }
        Err(HostError::new(
            WireErrorCode::BackendUnavailable,
            "session_control host domain is unavailable",
        ))
    }

    pub fn session_history(&self) -> Result<SessionHistoryClient, HostError> {
        self.inner
            .scope
            .require_grant(ExtensionCapability::SessionHistory, "session_history")?;
        self.inner
            .scope
            .preflight_context(HostContextRequirement::Session, "session_history")?;
        self.inner
            .scope
            .preflight_capability(ExtensionCapability::SessionHistory)?;
        Ok(SessionHistoryClient::new(self.clone()))
    }

    pub fn session_state(&self) -> Result<SessionStateClient, HostError> {
        self.inner
            .scope
            .preflight_context(HostContextRequirement::Session, "session_state")?;
        let available = [
            HostOperation::SessionStateRead,
            HostOperation::SessionStateWrite,
        ]
        .into_iter()
        .any(|operation| self.inner.scope.is_operation_available(operation));
        if !available {
            return Err(HostError::new(
                WireErrorCode::BackendUnavailable,
                "session_state host domain is unavailable",
            ));
        }
        Ok(SessionStateClient::new(self.clone()))
    }

    pub fn session_inspect(&self) -> Result<SessionInspectClient, HostError> {
        self.inner
            .scope
            .preflight_capability(ExtensionCapability::SessionInspect)?;
        Ok(SessionInspectClient::new(self.clone()))
    }

    pub fn workspace(&self) -> Result<WorkspaceClient, HostError> {
        self.inner.scope.preflight_any_capability(
            &[
                ExtensionCapability::WorkspaceRead,
                ExtensionCapability::WorkspaceWrite,
            ],
            "workspace",
        )?;
        self.inner
            .scope
            .preflight_context(HostContextRequirement::Workspace, "workspace")?;
        Ok(WorkspaceClient::new(self.clone()))
    }

    pub fn tool_results(&self) -> Result<ToolResultClient, HostError> {
        self.inner.scope.preflight(HostOperation::ToolResultRead)?;
        Ok(ToolResultClient::new(self.clone()))
    }

    pub fn process(&self) -> Result<ProcessClient, HostError> {
        self.inner
            .scope
            .require_grant(ExtensionCapability::ProcessSpawn, "process")?;
        self.inner
            .scope
            .preflight_context(HostContextRequirement::Workspace, "process")?;
        self.inner.scope.preflight(HostOperation::ProcessSpawn)?;
        Ok(ProcessClient::new(self.clone()))
    }

    pub fn network(&self) -> Result<NetworkClient, HostError> {
        self.inner.scope.preflight(HostOperation::NetworkClient)?;
        Ok(NetworkClient::new(self.clone()))
    }

    pub fn extension_http(&self) -> Result<ExtensionHttpClient, HostError> {
        self.inner
            .scope
            .preflight(HostOperation::ExtensionHttpPublic)?;
        Ok(ExtensionHttpClient::new(self.clone()))
    }

    fn operation_available(&self, operation: HostOperation) -> Result<bool, HostError> {
        if let Some(required) = operation.required_capability() {
            self.inner
                .scope
                .require_grant(required, operation.wire_name())?;
        }
        self.inner
            .scope
            .preflight_context(operation.spec().context, operation.wire_name())?;
        Ok(self.inner.scope.is_operation_available(operation))
    }
}

#[async_trait::async_trait]
impl HostClientTransport for ExtensionHost {
    type Error = HostError;

    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, Self::Error> {
        self.inner.scope.preflight(operation)?;
        self.inner.invoker.invoke(operation, input).await
    }

    async fn invoke_stream(
        &self,
        operation: HostOperation,
        input: Value,
    ) -> Result<ModelStream, Self::Error> {
        self.inner.scope.preflight(operation)?;
        self.inner.invoker.invoke_stream(operation, input).await
    }

    fn client_error(code: WireErrorCode, message: String) -> Self::Error {
        HostError::new(code, message)
    }

    fn payload_error(error: crate::wire::protocol::ErrorPayload) -> Self::Error {
        HostError::from(error)
    }
}

/// Runtime construction boundary. This module is intentionally absent from author preludes.
#[doc(hidden)]
pub mod internal {
    use std::{any::Any, collections::BTreeMap, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::{
        ExtensionCapability, ExtensionHost, ExtensionHostInner, HostContextRequirement, HostError,
        HostOperation, ModelStream,
    };
    pub use super::{
        domain_client::{
            EventClient as TypedEventClient, ExtensionHttpClient as TypedExtensionHttpClient,
            HostClientTransport, ModelClient as TypedModelClient,
            NetworkClient as TypedNetworkClient, ProcessClient as TypedProcessClient,
            SessionControlClient as TypedSessionControlClient,
            SessionHistoryClient as TypedSessionHistoryClient,
            SessionInspectClient as TypedSessionInspectClient,
            SessionStateClient as TypedSessionStateClient,
            ToolResultClient as TypedToolResultClient, WorkspaceClient as TypedWorkspaceClient,
        },
        llm_mapping::{llm_message_to_wire, llm_messages_from_wire, llm_messages_to_wire},
    };
    use crate::wire::WireErrorCode;
    pub use crate::wire::{
        HOST_OPERATION_SPECS, HostBackendRequirement, HostOp, HostOperationGroup,
        HostOperationSpec, operations,
    };

    /// Host-only redirect policy used by the outbound-network backend port.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum NetworkRedirectPolicy {
        Follow,
        Manual,
    }

    /// Host-only request passed to the single restricted outbound-network backend.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OutboundNetworkRequest {
        pub url: String,
        pub method: String,
        pub headers: BTreeMap<String, String>,
        pub body: Vec<u8>,
        pub max_bytes: usize,
        pub timeout: Duration,
        pub redirect_policy: NetworkRedirectPolicy,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OutboundNetworkResponse {
        pub final_url: String,
        pub status: u16,
        pub headers: BTreeMap<String, String>,
        pub body: Vec<u8>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("{message}")]
    pub struct OutboundNetworkError {
        pub code: WireErrorCode,
        pub retryable: bool,
        pub message: String,
    }

    impl OutboundNetworkError {
        pub fn new(code: WireErrorCode, retryable: bool, message: impl Into<String>) -> Self {
            Self {
                code,
                retryable,
                message: message.into(),
            }
        }
    }

    /// Host-only port for the single restricted outbound-network implementation.
    #[async_trait]
    pub trait OutboundNetworkService: Send + Sync {
        async fn request(
            &self,
            request: OutboundNetworkRequest,
            cancellation: Option<CancellationToken>,
        ) -> Result<OutboundNetworkResponse, OutboundNetworkError>;
    }

    /// Runtime-owned facts used only for synchronous, best-effort early failure.
    /// HostRouter remains authoritative and rechecks authorization during invoke.
    pub struct HostScope {
        granted: Box<[ExtensionCapability]>,
        available: [bool; HostOperation::COUNT],
        session_context_available: bool,
        workspace_context_available: bool,
    }

    impl HostScope {
        pub fn new(
            granted: impl IntoIterator<Item = ExtensionCapability>,
            available: impl IntoIterator<Item = HostOperation>,
            session_context_available: bool,
            workspace_context_available: bool,
        ) -> Self {
            let mut operation_availability = [false; HostOperation::COUNT];
            for operation in available {
                operation_availability[operation as usize] = true;
            }
            Self {
                granted: granted.into_iter().collect(),
                available: operation_availability,
                session_context_available,
                workspace_context_available,
            }
        }

        pub(super) fn preflight(&self, operation: HostOperation) -> Result<(), HostError> {
            if let Some(required) = operation.required_capability() {
                self.require_grant(required, operation.wire_name())?;
            }
            self.preflight_context(operation.spec().context, operation.wire_name())?;
            if !self.available[operation as usize] {
                return Err(HostError::new(
                    WireErrorCode::BackendUnavailable,
                    format!("{} backend is unavailable", operation.wire_name()),
                ));
            }
            Ok(())
        }

        pub(super) fn preflight_capability(
            &self,
            capability: ExtensionCapability,
        ) -> Result<(), HostError> {
            self.preflight_any_capability(&[capability], capability.as_str())
        }

        pub(super) fn is_granted(&self, capability: ExtensionCapability) -> bool {
            self.granted.contains(&capability)
        }

        pub(super) fn has_callable_operation_for(&self, capability: ExtensionCapability) -> bool {
            self.is_granted(capability)
                && HOST_OPERATION_SPECS.iter().any(|spec| {
                    spec.required == Some(capability)
                        && self.available[spec.operation as usize]
                        && self.is_context_available(spec.context)
                })
        }

        pub(super) fn preflight_available_operation_context(
            &self,
            capability: ExtensionCapability,
            target: &str,
        ) -> Result<(), HostError> {
            let Some(operation) = HOST_OPERATION_SPECS
                .iter()
                .find(|spec| {
                    spec.required == Some(capability) && self.available[spec.operation as usize]
                })
                .map(|spec| spec.operation)
            else {
                return Ok(());
            };
            self.preflight_context(operation.spec().context, target)
        }

        pub(super) fn is_operation_available(&self, operation: HostOperation) -> bool {
            self.available[operation as usize]
        }

        pub(super) fn preflight_any_capability(
            &self,
            capabilities: &[ExtensionCapability],
            target: &str,
        ) -> Result<(), HostError> {
            let granted = capabilities
                .iter()
                .copied()
                .filter(|capability| self.granted.contains(capability))
                .collect::<Vec<_>>();
            if granted.is_empty() {
                let required = capabilities
                    .iter()
                    .map(|capability| capability.as_str())
                    .collect::<Vec<_>>()
                    .join(" or ");
                return Err(HostError::new(
                    WireErrorCode::PermissionDenied,
                    format!("{target} requires declared capability {required}"),
                ));
            }
            let available = HOST_OPERATION_SPECS.iter().any(|spec| {
                spec.required
                    .is_some_and(|required| granted.contains(&required))
                    && self.available[spec.operation as usize]
            });
            if available {
                Ok(())
            } else {
                Err(HostError::new(
                    WireErrorCode::BackendUnavailable,
                    format!("{target} host domain is unavailable"),
                ))
            }
        }

        pub(super) fn preflight_context(
            &self,
            requirement: HostContextRequirement,
            target: &str,
        ) -> Result<(), HostError> {
            match requirement {
                HostContextRequirement::None => Ok(()),
                HostContextRequirement::Session if self.session_context_available => Ok(()),
                HostContextRequirement::Workspace if self.workspace_context_available => Ok(()),
                HostContextRequirement::Session => Err(HostError::new(
                    WireErrorCode::ContextUnavailable,
                    format!("{target} requires a session-scoped call context"),
                )),
                HostContextRequirement::Workspace => Err(HostError::new(
                    WireErrorCode::ContextUnavailable,
                    format!("{target} requires a workspace-scoped call context"),
                )),
            }
        }

        fn is_context_available(&self, requirement: HostContextRequirement) -> bool {
            match requirement {
                HostContextRequirement::None => true,
                HostContextRequirement::Session => self.session_context_available,
                HostContextRequirement::Workspace => self.workspace_context_available,
            }
        }

        pub(super) fn require_grant(
            &self,
            capability: ExtensionCapability,
            target: &str,
        ) -> Result<(), HostError> {
            if self.granted.contains(&capability) {
                return Ok(());
            }
            Err(HostError::new(
                WireErrorCode::PermissionDenied,
                format!(
                    "{target} requires declared capability {}",
                    capability.as_str()
                ),
            ))
        }
    }

    #[async_trait]
    pub trait HostInvoker: Send + Sync {
        async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, HostError>;

        async fn invoke_stream(
            &self,
            operation: HostOperation,
            _input: Value,
        ) -> Result<ModelStream, HostError> {
            Err(HostError::new(
                WireErrorCode::Unsupported,
                format!(
                    "{} transport does not support streaming",
                    operation.wire_name()
                ),
            ))
        }

        fn as_any(&self) -> &dyn Any;
    }

    pub fn extension_host(invoker: Arc<dyn HostInvoker>, scope: HostScope) -> ExtensionHost {
        ExtensionHost {
            inner: Arc::new(ExtensionHostInner { invoker, scope }),
        }
    }

    /// Returns the runtime transport behind a scoped host handle.
    ///
    /// This is intentionally confined to the runtime-only module so transport adapters can
    /// preserve invocation context without exposing raw host operations in author contexts.
    pub fn invoker(host: &ExtensionHost) -> &dyn HostInvoker {
        host.inner.invoker.as_ref()
    }
}
