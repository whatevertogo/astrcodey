use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    ExtensionCallContext, ExtensionError, ExtensionEventEmitter, ExtensionPaths, ExtensionTasks,
};
use crate::host::ExtensionHost;

// ─── Extension Manifest ──────────────────────────────────────────────────

/// 磁盘扩展目录中的 `extension.json` 契约（发现阶段元数据）。
///
/// `extension_id` 是宿主在启动子进程前使用的权威身份；Worker 在
/// `Initialize.metadata` 中上报的 id 必须与它一致。能力、工具与 hook 仍只由 Worker
/// 在握手中声明。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPackageManifest {
    pub extension_id: String,
    pub protocol: ExtensionPackageProtocol,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPackageProtocol {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s5r: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<String>,
}

// ─── Extension HTTP ─────────────────────────────────────────────────────

pub const DEFAULT_EXTENSION_HTTP_BODY_BYTES: usize = 64 * 1024;
pub const MAX_EXTENSION_HTTP_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ExtensionHttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionHttpAccess {
    #[default]
    Public,
    Authenticated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionHttpRoute {
    pub method: ExtensionHttpMethod,
    pub path: String,
    #[serde(default)]
    pub access: ExtensionHttpAccess,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_extension_http_body_bytes")]
    pub max_body_bytes: usize,
}

const fn default_extension_http_body_bytes() -> usize {
    DEFAULT_EXTENSION_HTTP_BODY_BYTES
}

impl ExtensionHttpRoute {
    pub fn public(method: ExtensionHttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            access: ExtensionHttpAccess::Public,
            description: String::new(),
            max_body_bytes: DEFAULT_EXTENSION_HTTP_BODY_BYTES,
        }
    }

    pub fn authenticated(method: ExtensionHttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            access: ExtensionHttpAccess::Authenticated,
            description: String::new(),
            max_body_bytes: DEFAULT_EXTENSION_HTTP_BODY_BYTES,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    pub fn validate(&self) -> Result<(), String> {
        if !valid_extension_http_route_path(&self.path) {
            return Err(format!("invalid extension HTTP route path: {}", self.path));
        }
        if self.max_body_bytes == 0 || self.max_body_bytes > MAX_EXTENSION_HTTP_BODY_BYTES {
            return Err(format!(
                "extension HTTP max_body_bytes must be between 1 and \
                 {MAX_EXTENSION_HTTP_BODY_BYTES}"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionHttpRequest {
    pub method: ExtensionHttpMethod,
    pub path: String,
    #[serde(default)]
    pub path_params: BTreeMap<String, String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub body: serde_json::Value,
}

impl ExtensionHttpRequest {
    pub fn new(method: ExtensionHttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            path_params: BTreeMap::new(),
            query: None,
            body: serde_json::Value::Null,
        }
    }

    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    pub fn json_body(mut self, body: serde_json::Value) -> Self {
        self.body = body;
        self
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                "path": { "type": "string" },
                "pathParams": { "type": "object", "additionalProperties": { "type": "string" } },
                "query": { "type": ["string", "null"] },
                "body": {}
            },
            "required": ["method", "path"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionHttpResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

impl ExtensionHttpResponse {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self { status, body }
    }

    pub fn error(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::json(
            status,
            serde_json::json!({
                "error": { "code": code.into(), "message": message.into() }
            }),
        )
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": { "type": "integer", "minimum": 0, "maximum": u16::MAX },
                "body": {}
            },
            "required": ["status", "body"],
            "additionalProperties": false
        })
    }
}

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

    pub fn call(&self) -> &ExtensionCallContext {
        &self.call
    }

    pub fn extension_id(&self) -> &str {
        self.call.extension_id()
    }

    pub fn caller_extension_id(&self) -> Option<&str> {
        self.caller_extension_id.as_deref()
    }

    pub fn working_dir(&self) -> Option<&Path> {
        self.call.working_dir()
    }

    pub fn paths(&self) -> &ExtensionPaths {
        self.call.paths()
    }

    pub fn host(&self) -> &ExtensionHost {
        self.call.host()
    }

    pub fn events(&self) -> &ExtensionEventEmitter {
        self.call.events()
    }

    pub fn tasks(&self) -> &ExtensionTasks {
        self.call.tasks()
    }

    pub fn cancellation(&self) -> &CancellationToken {
        self.call.cancellation()
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

pub fn match_extension_http_route(pattern: &str, path: &str) -> Option<BTreeMap<String, String>> {
    let pattern_segments = extension_http_path_segments(pattern);
    let path_segments = extension_http_path_segments(path);
    if pattern_segments.len() != path_segments.len() {
        return None;
    }
    let mut params = BTreeMap::new();
    for (pattern_segment, path_segment) in pattern_segments.iter().zip(path_segments) {
        if let Some(name) = extension_http_param_name(pattern_segment) {
            params.insert(name.to_string(), path_segment.to_string());
        } else if pattern_segment != &path_segment {
            return None;
        }
    }
    Some(params)
}

pub fn extension_http_route_patterns_conflict(left: &str, right: &str) -> bool {
    let left_segments = extension_http_path_segments(left);
    let right_segments = extension_http_path_segments(right);
    left_segments.len() == right_segments.len()
        && left_segments
            .iter()
            .zip(right_segments)
            .all(|(left, right)| {
                left == &right
                    || extension_http_param_name(left).is_some()
                    || extension_http_param_name(right).is_some()
            })
}

fn valid_extension_http_route_path(path: &str) -> bool {
    if !path.starts_with('/') || path.ends_with('/') || path.contains("//") || path.contains("..") {
        return false;
    }
    let mut params = BTreeSet::new();
    path.split('/').skip(1).all(|segment| {
        if segment.is_empty() {
            return false;
        }
        let starts_param = segment.starts_with('{');
        let ends_param = segment.ends_with('}');
        match (starts_param, ends_param) {
            (false, false) => !segment.contains('{') && !segment.contains('}'),
            (true, true) => {
                let name = &segment[1..segment.len() - 1];
                !name.is_empty()
                    && name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
                    && params.insert(name)
            },
            _ => false,
        }
    })
}

fn extension_http_path_segments(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn extension_http_param_name(segment: &str) -> Option<&str> {
    segment
        .strip_prefix('{')
        .and_then(|segment| segment.strip_suffix('}'))
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod extension_http_tests {
    use super::*;

    fn assert_strict_wire<T>(valid: Value, schema: Value)
    where
        T: Serialize + DeserializeOwned,
    {
        let decoded: T = serde_json::from_value(valid).unwrap();
        let mut wire = serde_json::to_value(decoded).unwrap();
        wire.as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<T>(wire).is_err());
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn public_dispatch_contracts_reject_unknown_fields() {
        assert_strict_wire::<ExtensionHttpRequest>(
            json!({ "method": "GET", "path": "/health" }),
            ExtensionHttpRequest::wire_schema(),
        );
        assert_strict_wire::<ExtensionHttpResponse>(
            json!({ "status": 200, "body": { "ok": true } }),
            ExtensionHttpResponse::wire_schema(),
        );
    }

    #[test]
    fn package_manifest_matches_the_disk_discovery_contract() {
        let manifest: ExtensionPackageManifest = serde_json::from_value(serde_json::json!({
            "extension_id": "review-extension",
            "protocol": { "s5r": "2.0" },
            "command": ["./review-extension", "serve"],
            "env": { "LOG_LEVEL": "info" }
        }))
        .expect("valid extension package manifest");
        assert_eq!(manifest.extension_id, "review-extension");
        assert_eq!(manifest.protocol.s5r.as_deref(), Some("2.0"));
        assert_eq!(manifest.command, ["./review-extension", "serve"]);
        assert_eq!(manifest.env["LOG_LEVEL"], "info");

        assert!(
            serde_json::from_value::<ExtensionPackageManifest>(serde_json::json!({
                "extension_id": "review-extension",
                "protocol": { "s5r": "2.0" },
                "command": ["./review-extension"],
                "capabilities": ["session_control"]
            }))
            .is_err(),
            "runtime capabilities belong to the S5R initialize manifest"
        );

        assert!(
            serde_json::from_value::<ExtensionPackageManifest>(serde_json::json!({
                "protocol": { "s5r": "2.0" },
                "command": ["./review-extension"]
            }))
            .is_err(),
            "package identity is required before process startup"
        );
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
