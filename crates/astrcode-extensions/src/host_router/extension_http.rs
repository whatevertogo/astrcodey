//! Public extension HTTP dispatch capability.

use std::sync::Arc;

use astrcode_core::extension::{ExtensionError, ExtensionHttpRequest};
use astrcode_extension_sdk::s5r::ErrorPayload;
use serde_json::Value;

use super::{
    HOST_INVOKE_TIMEOUT, PublicHttpDispatcher, block_on_async, capability::ExtensionHttpCapability,
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

    pub(super) fn invoke(
        &self,
        capability: ExtensionHttpCapability,
        input: Value,
        caller_extension_id: &str,
    ) -> Result<Value, ErrorPayload> {
        match capability {
            ExtensionHttpCapability::PublicDispatch => {
                self.dispatch_public(input, caller_extension_id)
            },
        }
    }

    fn dispatch_public(
        &self,
        input: Value,
        caller_extension_id: &str,
    ) -> Result<Value, ErrorPayload> {
        let dispatcher = self.dispatcher.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                "backend_unavailable",
                "public HTTP dispatcher is not configured",
            )
        })?;
        let request = serde_json::from_value::<ExtensionHttpRequest>(input)
            .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?;
        let dispatcher = Arc::clone(dispatcher);
        let caller_extension_id = caller_extension_id.to_owned();
        block_on_async(async move {
            tokio::time::timeout(
                HOST_INVOKE_TIMEOUT,
                dispatcher.dispatch_public_http(&caller_extension_id, request),
            )
            .await
            .map_err(|_| ErrorPayload::new("timeout", "public HTTP dispatch timed out"))?
            .and_then(|response| {
                serde_json::to_value(response)
                    .map_err(|error| ExtensionError::Internal(error.to_string()))
            })
            .map_err(|error| ErrorPayload::new("dispatch_failed", error.to_string()))
        })?
    }
}
