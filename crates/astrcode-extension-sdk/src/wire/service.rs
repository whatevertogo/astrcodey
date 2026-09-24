//! S5R service boundary values. Author/runtime types are mapped explicitly.
use serde::{Deserialize, Serialize};
use serde_json::Value;


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceInvokeRequest {
    pub service: String,
    pub input: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceInvocation {
    pub caller_extension_id: String,
    pub working_dir: Option<String>,
    pub session_id: Option<String>,
    pub input: Value,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKindDto {
    Required,
    Optional,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDependencyDto {
    pub service: String,
    pub kind: DependencyKindDto,
}
