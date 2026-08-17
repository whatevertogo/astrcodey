use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
    Public,
    #[default]
    Authenticated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ExtensionHttpDispatchRequest {
    pub method: ExtensionHttpMethod,
    pub path: String,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub body: serde_json::Value,
}

impl ExtensionHttpDispatchRequest {
    pub fn new(method: ExtensionHttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
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

impl From<ExtensionHttpDispatchRequest> for ExtensionHttpRequest {
    fn from(request: ExtensionHttpDispatchRequest) -> Self {
        Self {
            method: request.method,
            path: request.path,
            path_params: BTreeMap::new(),
            query: request.query,
            body: request.body,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionHttpResponse {
    #[serde(
        serialize_with = "serialize_extension_http_status",
        deserialize_with = "deserialize_extension_http_status"
    )]
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

fn serialize_extension_http_status<S>(status: &u16, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if (100..=599).contains(status) {
        serializer.serialize_u16(*status)
    } else {
        Err(serde::ser::Error::custom(
            "extension HTTP status must be between 100 and 599",
        ))
    }
}

fn deserialize_extension_http_status<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let status = u16::deserialize(deserializer)?;
    if (100..=599).contains(&status) {
        Ok(status)
    } else {
        Err(serde::de::Error::custom(
            "extension HTTP status must be between 100 and 599",
        ))
    }
}
