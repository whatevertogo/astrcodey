mod client;
mod contracts;
mod error;
mod operation;

use std::sync::Arc;

pub use client::{
    ExtensionHttpClient, ModelClient, NetworkClient, ProcessClient, SessionControlClient,
    SessionHistoryClient, SessionInspectClient, WorkspaceClient,
};
pub(crate) use contracts::HostAcknowledgement;
pub use contracts::*;
pub use error::{HostError, HostErrorClass};
pub use operation::{HOST_OPERATION_SPECS, HostOperation, HostOperationSpec};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::extension::ExtensionCapability;

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
    pub fn models(&self) -> ModelClient {
        ModelClient::new(self)
    }

    pub fn session_control(&self) -> Result<SessionControlClient, HostError> {
        let session_control = self
            .inner
            .scope
            .is_granted(ExtensionCapability::SessionControl);
        let input_delivery = self
            .inner
            .scope
            .is_granted(ExtensionCapability::InputDelivery);
        if !session_control && !input_delivery {
            self.inner.scope.preflight_any_capability(
                &[
                    ExtensionCapability::SessionControl,
                    ExtensionCapability::InputDelivery,
                ],
                "session_control",
            )?;
        }
        if input_delivery
            && self
                .inner
                .scope
                .has_available_operation_for(ExtensionCapability::InputDelivery)
        {
            return Ok(SessionControlClient::new(self));
        }
        if session_control {
            self.inner.scope.preflight_context(
                operation::HostContextRequirement::Session,
                "session_control",
            )?;
            self.preflight_capability(ExtensionCapability::SessionControl)?;
        } else {
            self.preflight_capability(ExtensionCapability::InputDelivery)?;
        }
        Ok(SessionControlClient::new(self))
    }

    pub fn session_history(&self) -> Result<SessionHistoryClient, HostError> {
        self.inner
            .scope
            .require_grant(ExtensionCapability::SessionHistory, "session_history")?;
        self.inner.scope.preflight_context(
            operation::HostContextRequirement::Session,
            "session_history",
        )?;
        self.preflight_capability(ExtensionCapability::SessionHistory)?;
        Ok(SessionHistoryClient::new(self))
    }

    pub fn session_inspect(&self) -> Result<SessionInspectClient, HostError> {
        self.preflight_capability(ExtensionCapability::SessionInspect)?;
        Ok(SessionInspectClient::new(self))
    }

    pub fn workspace(&self) -> Result<WorkspaceClient, HostError> {
        let capabilities = [
            ExtensionCapability::WorkspaceRead,
            ExtensionCapability::WorkspaceWrite,
        ];
        if !capabilities
            .iter()
            .copied()
            .any(|capability| self.inner.scope.is_granted(capability))
        {
            self.inner
                .scope
                .preflight_any_capability(&capabilities, "workspace")?;
        }
        self.inner
            .scope
            .preflight_context(operation::HostContextRequirement::Workspace, "workspace")?;
        self.inner
            .scope
            .preflight_any_capability(&capabilities, "workspace")?;
        Ok(WorkspaceClient::new(self))
    }

    pub fn process(&self) -> Result<ProcessClient, HostError> {
        self.inner
            .scope
            .require_grant(ExtensionCapability::ProcessSpawn, "process")?;
        self.inner
            .scope
            .preflight_context(operation::HostContextRequirement::Workspace, "process")?;
        self.preflight(HostOperation::ProcessSpawn)?;
        Ok(ProcessClient::new(self))
    }

    pub fn network(&self) -> Result<NetworkClient, HostError> {
        self.preflight(HostOperation::NetworkClient)?;
        Ok(NetworkClient::new(self))
    }

    pub fn extension_http(&self) -> Result<ExtensionHttpClient, HostError> {
        self.preflight(HostOperation::ExtensionHttpPublic)?;
        Ok(ExtensionHttpClient::new(self))
    }

    async fn invoke<I, O>(&self, operation: HostOperation, input: &I) -> Result<O, HostError>
    where
        I: Serialize + ?Sized,
        O: DeserializeOwned,
    {
        self.preflight(operation)?;
        let input = serialize_request(operation, input)?;
        let output = self.inner.invoker.invoke(operation, input).await?;
        deserialize_response(operation, output)
    }

    async fn invoke_collected_stream<I, O>(
        &self,
        operation: HostOperation,
        input: &I,
    ) -> Result<O, HostError>
    where
        I: Serialize + ?Sized,
        O: DeserializeOwned,
    {
        self.preflight(operation)?;
        let input = serialize_request(operation, input)?;
        let output = self
            .inner
            .invoker
            .invoke_collected_stream(operation, input)
            .await?;
        deserialize_response(operation, output)
    }

    async fn invoke_unit<I>(&self, operation: HostOperation, input: &I) -> Result<(), HostError>
    where
        I: Serialize + ?Sized,
    {
        self.invoke::<I, contracts::HostAcknowledgement>(operation, input)
            .await
            .map(|_| ())
    }

    fn preflight(&self, operation: HostOperation) -> Result<(), HostError> {
        self.inner.scope.preflight(operation)
    }

    fn operation_available(&self, operation: HostOperation) -> Result<bool, HostError> {
        if let Some(required) = operation.required_capability() {
            self.inner
                .scope
                .require_grant(required, operation.wire_name())?;
        }
        self.inner
            .scope
            .preflight_context(operation.context_requirement(), operation.wire_name())?;
        Ok(self.inner.scope.is_operation_available(operation))
    }

    fn preflight_capability(&self, capability: ExtensionCapability) -> Result<(), HostError> {
        self.inner.scope.preflight_capability(capability)
    }
}

fn serialize_request<I>(operation: HostOperation, input: &I) -> Result<Value, HostError>
where
    I: Serialize + ?Sized,
{
    serde_json::to_value(input).map_err(|error| {
        HostError::new(
            "serialization_failed",
            format!(
                "failed to serialize {} request: {error}",
                operation.wire_name()
            ),
        )
    })
}

fn deserialize_response<O>(operation: HostOperation, output: Value) -> Result<O, HostError>
where
    O: DeserializeOwned,
{
    serde_json::from_value(output).map_err(|error| {
        HostError::new(
            "invalid_host_response",
            format!("invalid {} response: {error}", operation.wire_name()),
        )
    })
}

/// Runtime construction boundary. This module is intentionally absent from author preludes.
#[doc(hidden)]
pub mod internal {
    use std::{any::Any, collections::BTreeMap, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::{
        ExtensionCapability, ExtensionHost, ExtensionHostInner, HOST_OPERATION_SPECS, HostError,
        HostOperation, operation::HostContextRequirement,
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OutboundNetworkErrorKind {
        InvalidRequest,
        PermissionDenied,
        Unavailable,
        RequestFailed,
        Timeout,
        ResponseTooLarge,
        Cancelled,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("{message}")]
    pub struct OutboundNetworkError {
        pub kind: OutboundNetworkErrorKind,
        pub message: String,
    }

    impl OutboundNetworkError {
        pub fn new(kind: OutboundNetworkErrorKind, message: impl Into<String>) -> Self {
            Self {
                kind,
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
    #[derive(Clone)]
    pub struct HostScope {
        granted: Arc<[ExtensionCapability]>,
        available: Arc<[bool; HostOperation::COUNT]>,
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
                granted: granted.into_iter().collect::<Vec<_>>().into(),
                available: Arc::new(operation_availability),
                session_context_available,
                workspace_context_available,
            }
        }

        pub(super) fn preflight(&self, operation: HostOperation) -> Result<(), HostError> {
            if let Some(required) = operation.required_capability() {
                self.require_grant(required, operation.wire_name())?;
            }
            if !self.available[operation as usize] {
                return Err(HostError::new(
                    "backend_unavailable",
                    format!("{} backend is unavailable", operation.wire_name()),
                ));
            }
            self.preflight_context(operation.context_requirement(), operation.wire_name())
        }

        pub(super) fn preflight_capability(
            &self,
            capability: ExtensionCapability,
        ) -> Result<(), HostError> {
            self.preflight_any_capability(&[capability], crate::s5r::capability_to_wire(capability))
        }

        pub(super) fn is_granted(&self, capability: ExtensionCapability) -> bool {
            self.granted.contains(&capability)
        }

        pub(super) fn has_available_operation_for(&self, capability: ExtensionCapability) -> bool {
            HOST_OPERATION_SPECS.iter().any(|spec| {
                spec.required == Some(capability) && self.available[spec.operation as usize]
            })
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
                    .map(|capability| crate::s5r::capability_to_wire(*capability))
                    .collect::<Vec<_>>()
                    .join(" or ");
                return Err(HostError::new(
                    "permission_denied",
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
                    "backend_unavailable",
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
                    "context_unavailable",
                    format!("{target} requires a session-scoped call context"),
                )),
                HostContextRequirement::Workspace => Err(HostError::new(
                    "context_unavailable",
                    format!("{target} requires a workspace-scoped call context"),
                )),
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
                "permission_denied",
                format!(
                    "{target} requires declared capability {}",
                    crate::s5r::capability_to_wire(capability)
                ),
            ))
        }
    }

    #[async_trait]
    pub trait HostInvoker: Send + Sync {
        async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, HostError>;

        async fn invoke_collected_stream(
            &self,
            operation: HostOperation,
            _input: Value,
        ) -> Result<Value, HostError> {
            Err(HostError::new(
                "stream_unavailable",
                format!(
                    "{} transport does not support collected streaming",
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
