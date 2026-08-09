use std::sync::Arc;

// ─── Extension HTTP ─────────────────────────────────────────────────────
pub use astrcode_extension_contract::extension_http::{
    DEFAULT_EXTENSION_HTTP_BODY_BYTES, ExtensionHttpAccess, ExtensionHttpDispatchRequest,
    ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
    MAX_EXTENSION_HTTP_BODY_BYTES, extension_http_route_patterns_conflict,
    match_extension_http_route,
};
#[cfg(test)]
use serde::Serialize;
use serde::de::DeserializeOwned;
#[cfg(test)]
use serde_json::{Value, json};

use super::{ExtensionCall, ExtensionCallContext, ExtensionError};

/// Validated request context for one extension HTTP route invocation.
///
/// The runtime constructs this only after route matching, path parameter extraction, body-size
/// enforcement, and JSON parsing have succeeded.
#[derive(Clone)]
pub struct HttpContext {
    call: ExtensionCallContext,
    route: ExtensionHttpRoute,
    request: ExtensionHttpRequest,
    caller_extension_id: Option<String>,
}

impl HttpContext {
    #[doc(hidden)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        route: ExtensionHttpRoute,
        request: ExtensionHttpRequest,
        caller_extension_id: Option<String>,
    ) -> Self {
        Self {
            call,
            route,
            request,
            caller_extension_id,
        }
    }

    pub fn caller_extension_id(&self) -> Option<&str> {
        self.caller_extension_id.as_deref()
    }

    pub fn route(&self) -> &ExtensionHttpRoute {
        &self.route
    }

    pub fn request(&self) -> &ExtensionHttpRequest {
        &self.request
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, ExtensionError> {
        serde_json::from_value(self.request.body.clone()).map_err(|error| {
            ExtensionError::InvalidInput {
                code: "invalid_http_body".into(),
                message: error.to_string(),
                hint: Some("check the JSON body against this route's request schema".into()),
            }
        })
    }
}

impl ExtensionCall for HttpContext {
    fn call(&self) -> &ExtensionCallContext {
        &self.call
    }
}

impl std::fmt::Debug for HttpContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpContext")
            .field("call", &self.call)
            .field("route", &self.route)
            .field("request", &self.request)
            .field("caller_extension_id", &self.caller_extension_id)
            .finish()
    }
}

#[async_trait::async_trait]
pub trait ExtensionHttpHandler: Send + Sync {
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError>;
}

#[derive(Clone)]
pub struct ExtensionHttpRouteRegistration {
    pub route: ExtensionHttpRoute,
    pub handler: Arc<dyn ExtensionHttpHandler>,
}

#[cfg(test)]
mod extension_http_tests {
    use super::*;

    fn assert_strict_wire<T>(valid: Value)
    where
        T: Serialize + DeserializeOwned,
    {
        let decoded: T = serde_json::from_value(valid).unwrap();
        let mut wire = serde_json::to_value(decoded).unwrap();
        wire.as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<T>(wire).is_err());
    }

    #[test]
    fn public_dispatch_contracts_reject_unknown_fields() {
        assert_strict_wire::<ExtensionHttpDispatchRequest>(
            json!({ "method": "GET", "path": "/health" }),
        );
        assert_strict_wire::<ExtensionHttpRequest>(json!({ "method": "GET", "path": "/health" }));
        assert_strict_wire::<ExtensionHttpResponse>(
            json!({ "status": 200, "body": { "ok": true } }),
        );
        assert!(
            serde_json::from_value::<ExtensionHttpDispatchRequest>(json!({
                "method": "GET",
                "path": "/users/42",
                "pathParams": { "id": "forged" }
            }))
            .is_err()
        );
    }

    #[test]
    fn response_status_serde_enforces_http_bounds() {
        for status in [99, 600] {
            assert!(
                serde_json::from_value::<ExtensionHttpResponse>(json!({
                    "status": status,
                    "body": null
                }))
                .is_err()
            );
            assert!(
                serde_json::to_value(ExtensionHttpResponse::json(status, Value::Null)).is_err()
            );
        }
        for status in [100, 599] {
            assert!(
                serde_json::from_value::<ExtensionHttpResponse>(json!({
                    "status": status,
                    "body": null
                }))
                .is_ok()
            );
        }
    }

    #[test]
    fn route_validation_and_matching_are_segment_scoped() {
        let route = ExtensionHttpRoute::public(ExtensionHttpMethod::Patch, "/future-tasks/{jobId}");
        route.validate().expect("valid route");

        let params =
            match_extension_http_route(&route.path, "/future-tasks/job-1").expect("matching route");
        assert_eq!(params.get("jobId").map(String::as_str), Some("job-1"));
        assert!(match_extension_http_route(&route.path, "/future-tasks/job-1/run").is_none());
    }

    #[test]
    fn omitted_http_access_defaults_to_authenticated() {
        let route: ExtensionHttpRoute = serde_json::from_value(json!({
            "method": "POST",
            "path": "/jobs"
        }))
        .expect("route without access");

        assert_eq!(route.access, ExtensionHttpAccess::Authenticated);
    }

    #[test]
    fn route_validation_rejects_traversal_and_duplicate_params() {
        let traversal = ExtensionHttpRoute::public(ExtensionHttpMethod::Get, "/files/../secret");
        assert!(traversal.validate().is_err());

        let duplicate = ExtensionHttpRoute::public(ExtensionHttpMethod::Get, "/{id}/{id}");
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn overlapping_parameter_routes_conflict() {
        assert!(extension_http_route_patterns_conflict(
            "/future-tasks/{id}",
            "/future-tasks/{jobId}"
        ));
        assert!(!extension_http_route_patterns_conflict(
            "/future-tasks/{id}",
            "/notes/{id}"
        ));
    }
}
