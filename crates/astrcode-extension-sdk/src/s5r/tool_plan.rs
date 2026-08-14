use astrcode_core::tool::access::{
    FileOperation, HostResource, ResourceAccess, ResourceSet, ToolPlan,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Strict payload sent through `handler.invoke` for a tool plan or execution call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolInvocationRequest {
    pub phase: ToolInvocationPhase,
    pub arguments: Value,
    pub scope: ToolInvocationScope,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationPhase {
    Plan,
    Execute,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolInvocationScope {
    pub session_id: String,
    pub working_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Strict S5R representation of an extension tool plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolPlanDto {
    pub resources: Vec<ResourceAccessDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceAccessDto {
    File {
        operation: FileOperationDto,
        path: String,
        recursive: bool,
    },
    Host {
        resource: HostResourceDto,
    },
    Opaque,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileOperationDto {
    Read,
    Search,
    Write,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostResourceDto {
    Process,
    ToolResultArtifact,
    Session,
    Model,
    Network,
    Event,
    ExtensionHttp,
}

impl From<&ToolPlan> for ToolPlanDto {
    fn from(plan: &ToolPlan) -> Self {
        Self {
            resources: plan
                .resources()
                .iter()
                .map(ResourceAccessDto::from)
                .collect(),
        }
    }
}

impl From<&ResourceAccess> for ResourceAccessDto {
    fn from(access: &ResourceAccess) -> Self {
        match access {
            ResourceAccess::File {
                operation,
                path,
                recursive,
            } => Self::File {
                operation: (*operation).into(),
                path: path.clone(),
                recursive: *recursive,
            },
            ResourceAccess::Host(resource) => Self::Host {
                resource: (*resource).into(),
            },
            ResourceAccess::Opaque => Self::Opaque,
        }
    }
}

impl From<ToolPlanDto> for ToolPlan {
    fn from(plan: ToolPlanDto) -> Self {
        ToolPlan::new(ResourceSet::new(
            plan.resources.into_iter().map(ResourceAccess::from),
        ))
    }
}

impl From<ResourceAccessDto> for ResourceAccess {
    fn from(access: ResourceAccessDto) -> Self {
        match access {
            ResourceAccessDto::File {
                operation,
                path,
                recursive,
            } => Self::File {
                operation: operation.into(),
                path,
                recursive,
            },
            ResourceAccessDto::Host { resource } => Self::Host(resource.into()),
            ResourceAccessDto::Opaque => Self::Opaque,
        }
    }
}

impl From<FileOperation> for FileOperationDto {
    fn from(operation: FileOperation) -> Self {
        match operation {
            FileOperation::Read => Self::Read,
            FileOperation::Search => Self::Search,
            FileOperation::Write => Self::Write,
            FileOperation::ReadWrite => Self::ReadWrite,
        }
    }
}

impl From<FileOperationDto> for FileOperation {
    fn from(operation: FileOperationDto) -> Self {
        match operation {
            FileOperationDto::Read => Self::Read,
            FileOperationDto::Search => Self::Search,
            FileOperationDto::Write => Self::Write,
            FileOperationDto::ReadWrite => Self::ReadWrite,
        }
    }
}

impl From<HostResource> for HostResourceDto {
    fn from(resource: HostResource) -> Self {
        match resource {
            HostResource::Process => Self::Process,
            HostResource::ToolResultArtifact => Self::ToolResultArtifact,
            HostResource::Session => Self::Session,
            HostResource::Model => Self::Model,
            HostResource::Network => Self::Network,
            HostResource::Event => Self::Event,
            HostResource::ExtensionHttp => Self::ExtensionHttp,
        }
    }
}

impl From<HostResourceDto> for HostResource {
    fn from(resource: HostResourceDto) -> Self {
        match resource {
            HostResourceDto::Process => Self::Process,
            HostResourceDto::ToolResultArtifact => Self::ToolResultArtifact,
            HostResourceDto::Session => Self::Session,
            HostResourceDto::Model => Self::Model,
            HostResourceDto::Network => Self::Network,
            HostResourceDto::Event => Self::Event,
            HostResourceDto::ExtensionHttp => Self::ExtensionHttp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_plan_wire_round_trip_preserves_resources_and_rejects_unknown_fields() {
        let plan = ToolPlan::from_resources([
            ResourceAccess::search_file("/workspace/src", true),
            ResourceAccess::host(HostResource::Process),
            ResourceAccess::Opaque,
        ]);
        let value = serde_json::to_value(ToolPlanDto::from(&plan)).expect("serialize plan");
        let decoded: ToolPlanDto = serde_json::from_value(value).expect("decode plan");
        assert_eq!(ToolPlan::from(decoded), plan);

        assert!(
            serde_json::from_value::<ToolPlanDto>(serde_json::json!({
                "resources": [{
                    "kind": "file",
                    "operation": "read",
                    "path": "/workspace/src/lib.rs",
                    "recursive": false,
                    "unexpected": true
                }]
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<ToolInvocationRequest>(serde_json::json!({
                "phase": "plan",
                "arguments": {},
                "scope": {
                    "session_id": "session-1",
                    "working_dir": "/workspace"
                },
                "on": "tool"
            }))
            .is_err()
        );
    }
}
