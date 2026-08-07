//! Public extension HTTP dispatch capability.

use std::sync::Arc;

use astrcode_extension_sdk::{
    extension::{ExtensionError, ExtensionHttpDispatchRequest, ExtensionHttpRequest},
    host::{HOST_ERROR_CODE_BACKEND_UNAVAILABLE, HOST_ERROR_CODE_TIMEOUT},
    s5r::ErrorPayload,
};
use serde_json::Value;

use super::{
    HOST_INVOKE_TIMEOUT, PublicHttpDispatcher, capability::ExtensionHttpCapability,
    decode_host_input,
};

#[derive(Default)]
pub(super) struct ExtensionHttpGroup {
    dispatcher: Option<Arc<dyn PublicHttpDispatcher>>,
}

impl ExtensionHttpGroup {
    pub(super) fn new(dispatcher: Option<Arc<dyn PublicHttpDispatcher>>) -> Self {
        Self { dispatcher }
    }

    pub(super) fn set_dispatcher(&mut self, dispatcher: Arc<dyn PublicHttpDispatcher>) {
        self.dispatcher = Some(dispatcher);
    }

    pub(super) async fn invoke(
        &self,
        capability: ExtensionHttpCapability,
        input: Value,
        caller_extension_id: &str,
    ) -> Result<Value, ErrorPayload> {
        match capability {
            ExtensionHttpCapability::PublicDispatch => {
                self.dispatch_public(input, caller_extension_id).await
            },
        }
    }

    pub(super) fn is_available(&self) -> bool {
        self.dispatcher.is_some()
    }

    async fn dispatch_public(
        &self,
        input: Value,
        caller_extension_id: &str,
    ) -> Result<Value, ErrorPayload> {
        let dispatcher = self.dispatcher.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
                "public HTTP dispatcher is not configured",
            )
        })?;
        let request: ExtensionHttpDispatchRequest = decode_host_input(input)?;
        let request = ExtensionHttpRequest::from(request);
        tokio::time::timeout(
            HOST_INVOKE_TIMEOUT,
            dispatcher.dispatch_public_http(caller_extension_id, request),
        )
        .await
        .map_err(|_| ErrorPayload::new(HOST_ERROR_CODE_TIMEOUT, "public HTTP dispatch timed out"))?
        .and_then(|response| {
            serde_json::to_value(response)
                .map_err(|error| ExtensionError::Internal(error.to_string()))
        })
        .map_err(|error| ErrorPayload::new("dispatch_failed", error.to_string()))
    }
}
