//! Public extension HTTP dispatch capability.

use std::sync::Arc;

use astrcode_extension_sdk::{
    self,
    extension::{ExtensionError, ExtensionHttpDispatchRequest, ExtensionHttpRequest},
    host::{HostOperation, internal::HostOperationGroup},
    s5r::ErrorPayload,
    wire::WireErrorCode,
};
use serde_json::Value;

use super::{
    HOST_INVOKE_TIMEOUT, InvokeContext, PublicHttpDispatcher, backend_unavailable,
    invalid_group_operation, parse_wire_request,
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
        operation: HostOperation,
        input: Value,
        context: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match operation {
            HostOperation::ExtensionHttpPublic => {
                self.dispatch_public(operation, input, context).await
            },
            _ => Err(invalid_group_operation(
                operation,
                HostOperationGroup::ExtensionHttp,
            )),
        }
    }

    pub(super) fn is_available(
        &self,
        scoped_dispatcher: Option<&Arc<dyn PublicHttpDispatcher>>,
    ) -> bool {
        scoped_dispatcher.is_some() || self.dispatcher.is_some()
    }

    async fn dispatch_public(
        &self,
        operation: HostOperation,
        input: Value,
        context: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let dispatcher = context
            .public_http_dispatcher
            .as_ref()
            .or(self.dispatcher.as_ref())
            .ok_or_else(|| backend_unavailable("public HTTP dispatcher is not configured"))?;
        let request: ExtensionHttpDispatchRequest =
            parse_wire_request(&input, operation.wire_name())?;
        let request = ExtensionHttpRequest::from(request);
        tokio::time::timeout(
            HOST_INVOKE_TIMEOUT,
            dispatcher.dispatch_public_http(&context.extension_id, request),
        )
        .await
        .map_err(|_| ErrorPayload::new(WireErrorCode::Timeout, "public HTTP dispatch timed out"))?
        .and_then(|response| {
            serde_json::to_value(response)
                .map_err(|error| ExtensionError::Internal(error.to_string()))
        })
        .map_err(|error| ErrorPayload::new(WireErrorCode::DispatchFailed, error.to_string()))
    }
}
