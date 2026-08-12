use std::sync::{Arc, Weak};

use astrcode_extension_sdk::extension::{internal::match_extension_http_route, *};

use super::{ExtensionCallContextInput, ExtensionRunner, ExtensionView};

#[derive(Debug, Clone)]
pub enum ExtensionHttpDispatchResult {
    NotFound,
    MethodNotAllowed,
    PayloadTooLarge { max_body_bytes: usize },
    InvalidJson { message: String },
    Response(ExtensionHttpResponse),
}

impl ExtensionView {
    pub async fn dispatch_public_http_route(
        &self,
        request: ExtensionHttpRequest,
        body: &[u8],
    ) -> Result<ExtensionHttpDispatchResult, ExtensionError> {
        self.dispatch_http_route(ExtensionHttpAccess::Public, None, None, request, body)
            .await
    }

    pub async fn dispatch_authenticated_http_route(
        &self,
        extension_id: &str,
        request: ExtensionHttpRequest,
        body: &[u8],
    ) -> Result<ExtensionHttpDispatchResult, ExtensionError> {
        self.dispatch_http_route(
            ExtensionHttpAccess::Authenticated,
            Some(extension_id),
            None,
            request,
            body,
        )
        .await
    }

    pub async fn dispatch_public_http_route_from(
        &self,
        caller_extension_id: &str,
        request: ExtensionHttpRequest,
        body: &[u8],
    ) -> Result<ExtensionHttpDispatchResult, ExtensionError> {
        self.dispatch_http_route(
            ExtensionHttpAccess::Public,
            None,
            Some(caller_extension_id),
            request,
            body,
        )
        .await
    }

    async fn dispatch_http_route(
        &self,
        access: ExtensionHttpAccess,
        target_extension_id: Option<&str>,
        caller_extension_id: Option<&str>,
        mut request: ExtensionHttpRequest,
        body: &[u8],
    ) -> Result<ExtensionHttpDispatchResult, ExtensionError> {
        let index = &self.index;
        let mut path_matched = false;
        let matched = index.http_routes.iter().find_map(|entry| {
            if entry.route.access != access
                || target_extension_id
                    .is_some_and(|extension_id| extension_id != entry.extension_id)
            {
                return None;
            }
            let params = match_extension_http_route(&entry.route.path, &request.path)?;
            path_matched = true;
            (entry.route.method == request.method).then_some((entry.clone(), params))
        });
        let Some((entry, path_params)) = matched else {
            return Ok(if path_matched {
                ExtensionHttpDispatchResult::MethodNotAllowed
            } else {
                ExtensionHttpDispatchResult::NotFound
            });
        };
        if caller_extension_id.is_some_and(|caller| caller == entry.extension_id) {
            return Err(ExtensionError::Internal(
                "an extension cannot synchronously dispatch its own public HTTP route".into(),
            ));
        }
        if body.len() > entry.route.max_body_bytes {
            return Ok(ExtensionHttpDispatchResult::PayloadTooLarge {
                max_body_bytes: entry.route.max_body_bytes,
            });
        }
        request.body = if body.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_slice(body) {
                Ok(body) => body,
                Err(error) => {
                    return Ok(ExtensionHttpDispatchResult::InvalidJson {
                        message: error.to_string(),
                    });
                },
            }
        };
        request.path_params = path_params;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let call = self.make_registered_extension_call_context(
            &entry.extension_id,
            ExtensionCallContextInput::unscoped(cancellation.clone()),
        )?;
        let ctx = HttpContext::from_runtime(
            call,
            entry.route.clone(),
            request,
            caller_extension_id.map(str::to_owned),
        );
        let response = self
            .run_recorded_hook(
                &entry.extension_id,
                "http_route",
                cancellation,
                entry.handler.handle(ctx),
            )
            .await?;
        if !(100..=599).contains(&response.status) {
            return Err(ExtensionError::Internal(format!(
                "extension {} returned invalid HTTP status {}",
                entry.extension_id, response.status
            )));
        }
        let response_bytes = serde_json::to_vec(&response.body)
            .map_err(|error| ExtensionError::Internal(error.to_string()))?
            .len();
        if response_bytes > MAX_EXTENSION_HTTP_BODY_BYTES {
            return Err(ExtensionError::Internal(format!(
                "extension {} HTTP response exceeds {} bytes",
                entry.extension_id, MAX_EXTENSION_HTTP_BODY_BYTES
            )));
        }
        Ok(ExtensionHttpDispatchResult::Response(response))
    }
}

impl ExtensionRunner {
    /// Returns a dispatcher that does not keep the runner alive through its bound host router.
    pub fn public_http_dispatcher(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::host_router::PublicHttpDispatcher> {
        Arc::new(WeakRunnerPublicHttpDispatcher {
            runner: Arc::downgrade(self),
        })
    }

    pub async fn dispatch_public_http_route(
        &self,
        request: ExtensionHttpRequest,
        body: &[u8],
    ) -> Result<ExtensionHttpDispatchResult, ExtensionError> {
        self.extension_view()
            .await
            .dispatch_public_http_route(request, body)
            .await
    }

    pub async fn dispatch_authenticated_http_route(
        &self,
        extension_id: &str,
        request: ExtensionHttpRequest,
        body: &[u8],
    ) -> Result<ExtensionHttpDispatchResult, ExtensionError> {
        self.extension_view()
            .await
            .dispatch_authenticated_http_route(extension_id, request, body)
            .await
    }

    pub async fn dispatch_public_http_route_from(
        &self,
        caller_extension_id: &str,
        request: ExtensionHttpRequest,
        body: &[u8],
    ) -> Result<ExtensionHttpDispatchResult, ExtensionError> {
        self.extension_view()
            .await
            .dispatch_public_http_route_from(caller_extension_id, request, body)
            .await
    }
}

struct WeakRunnerPublicHttpDispatcher {
    runner: Weak<ExtensionRunner>,
}

#[async_trait::async_trait]
impl crate::host_router::PublicHttpDispatcher for WeakRunnerPublicHttpDispatcher {
    async fn dispatch_public_http(
        &self,
        caller_extension_id: &str,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError> {
        let runner = self.runner.upgrade().ok_or_else(|| {
            ExtensionError::Internal("extension runner is no longer available".into())
        })?;
        crate::host_router::PublicHttpDispatcher::dispatch_public_http(
            &*runner,
            caller_extension_id,
            request,
        )
        .await
    }
}

#[async_trait::async_trait]
impl crate::host_router::PublicHttpDispatcher for ExtensionRunner {
    async fn dispatch_public_http(
        &self,
        caller_extension_id: &str,
        mut request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError> {
        let body = if request.body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&request.body)
                .map_err(|error| ExtensionError::Internal(error.to_string()))?
        };
        request.body = serde_json::Value::Null;
        match self
            .dispatch_public_http_route_from(caller_extension_id, request, &body)
            .await?
        {
            ExtensionHttpDispatchResult::Response(response) => Ok(response),
            ExtensionHttpDispatchResult::NotFound => Ok(ExtensionHttpResponse::error(
                404,
                "extension_route_not_found",
                "extension public HTTP route not found",
            )),
            ExtensionHttpDispatchResult::MethodNotAllowed => Ok(ExtensionHttpResponse::error(
                405,
                "extension_http_method_not_allowed",
                "extension public HTTP route does not support this method",
            )),
            ExtensionHttpDispatchResult::PayloadTooLarge { max_body_bytes } => {
                Ok(ExtensionHttpResponse::error(
                    413,
                    "extension_http_body_too_large",
                    format!("extension HTTP body exceeds {max_body_bytes} bytes"),
                ))
            },
            ExtensionHttpDispatchResult::InvalidJson { message } => Ok(
                ExtensionHttpResponse::error(400, "invalid_extension_http_json", message),
            ),
        }
    }
}
