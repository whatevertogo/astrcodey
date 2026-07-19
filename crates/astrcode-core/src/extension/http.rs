use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use super::{ExtensionCapability, ExtensionError};

// ─── Extension Manifest ──────────────────────────────────────────────────

/// 磁盘扩展目录中的 `extension.json` 契约（发现阶段元数据）。
///
/// **当前 loader 行为（s5r）**：`protocol.s5r`（须为 `"1.0"`）与 **`command`**
/// （启动子进程的 argv 数组）为必填。扩展的真实 `id`、能力、工具与 hook 均由
/// Worker 在 `Initialize.metadata` 中上报。本结构中的 `id` / `name` / `capabilities` 等字段
/// 可被 serde 解析，供 UI、诊断或未来校验使用，但**不会**替代 s5r 握手 manifest。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// 扩展唯一标识符。
    pub id: String,
    /// 扩展显示名称。
    pub name: String,
    /// 可选的扩展版本号，用于诊断/UI 展示。
    #[serde(default)]
    pub version: Option<String>,
    /// 可选的人类可读描述。
    #[serde(default)]
    pub description: Option<String>,
    /// 可选的宿主版本提示。目前仅作为元数据，不做硬性校验。
    #[serde(default)]
    pub astrcode_version: Option<String>,
    /// 宿主必须授予此扩展的能力。
    #[serde(default)]
    pub capabilities: Vec<ExtensionCapability>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionHttpRoute {
    pub method: ExtensionHttpMethod,
    pub path: String,
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
#[serde(rename_all = "camelCase")]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
}

#[async_trait::async_trait]
pub trait ExtensionHttpHandler: Send + Sync {
    async fn handle(
        &self,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError>;
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
