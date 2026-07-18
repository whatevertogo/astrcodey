//! Host capability 的类型化标识与单一元数据注册表。

use astrcode_core::extension::ExtensionCapability;
use astrcode_extension_sdk::s5r::{CapabilityDescriptor, ErrorPayload};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum HostCapability {
    Llm(LlmCapability),
    Session(SessionCapability),
    Context(ContextCapability),
    Workspace(WorkspaceCapability),
    Process(ProcessCapability),
    Network(NetworkCapability),
    ExtensionHttp(ExtensionHttpCapability),
}

macro_rules! capability_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(super) enum $name {
            $($variant),+
        }

        impl $name {
            #[cfg(test)]
            const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

capability_enum!(LlmCapability {
    MainChat,
    SmallChat,
});

capability_enum!(SessionCapability {
    ReadEvents,
    Create,
    SubmitTurn,
    InterruptAndSubmit,
    Inject,
    CancelTurn,
    ExecutionView,
    Dispose,
    InspectList,
    InspectSnapshot,
    InspectReadModel,
    InspectProviderMessages,
});

capability_enum!(ContextCapability {
    StateRead,
    StateWrite,
    EmitEvent,
});

capability_enum!(WorkspaceCapability {
    Read,
    List,
    Grep,
    Glob,
    Write,
    Edit,
});

capability_enum!(ProcessCapability { Spawn });
capability_enum!(NetworkCapability { Client });
capability_enum!(ExtensionHttpCapability { PublicDispatch });

#[derive(Debug, Clone, Copy)]
enum CapabilitySchema {
    Object,
    LlmMessages,
    SessionId,
    SessionCreate,
    WorkspaceWrite,
    WorkspaceEdit,
    ProcessSpawn,
    NetworkRequest,
    NetworkResponse,
    PublicHttpDispatch,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HostCapabilitySpec {
    pub(super) capability: HostCapability,
    pub(super) name: &'static str,
    pub(super) required: Option<ExtensionCapability>,
    description: &'static str,
    input_schema: CapabilitySchema,
    output_schema: CapabilitySchema,
    pub(super) supports_stream: bool,
    cancelable: bool,
    catalog: bool,
}

macro_rules! spec {
    ($capability:expr, $name:literal, $required:expr, $description:literal) => {
        HostCapabilitySpec {
            capability: $capability,
            name: $name,
            required: $required,
            description: $description,
            input_schema: CapabilitySchema::Object,
            output_schema: CapabilitySchema::Object,
            supports_stream: false,
            cancelable: false,
            catalog: true,
        }
    };
}

pub(super) const HOST_CAPABILITY_SPECS: &[HostCapabilitySpec] = &[
    spec!(
        HostCapability::Context(ContextCapability::EmitEvent),
        "astrcode.event.emit",
        Some(ExtensionCapability::EmitEvents),
        "Emit a declared extension event"
    ),
    HostCapabilitySpec {
        capability: HostCapability::ExtensionHttp(ExtensionHttpCapability::PublicDispatch),
        name: "astrcode.extension.http.public",
        required: Some(ExtensionCapability::PublicHttpDispatch),
        description: "Dispatch a request to another extension's public HTTP route",
        input_schema: CapabilitySchema::PublicHttpDispatch,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Llm(LlmCapability::MainChat),
        name: "astrcode.llm.main_chat",
        required: Some(ExtensionCapability::MainModel),
        description: "Chat with the host-configured main LLM (session active model)",
        input_schema: CapabilitySchema::LlmMessages,
        output_schema: CapabilitySchema::Object,
        supports_stream: true,
        cancelable: true,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Llm(LlmCapability::SmallChat),
        name: "astrcode.llm.small_chat",
        required: Some(ExtensionCapability::SmallModel),
        description: "Chat with the host-configured small LLM",
        input_schema: CapabilitySchema::LlmMessages,
        output_schema: CapabilitySchema::Object,
        supports_stream: true,
        cancelable: true,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Network(NetworkCapability::Client),
        name: "astrcode.network.client",
        required: Some(ExtensionCapability::NetworkClient),
        description: "Send a bounded outbound HTTP or HTTPS request with a UTF-8 text body",
        input_schema: CapabilitySchema::NetworkRequest,
        output_schema: CapabilitySchema::NetworkResponse,
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Process(ProcessCapability::Spawn),
        name: "astrcode.process.spawn",
        required: Some(ExtensionCapability::ProcessSpawn),
        description: "Run a bounded subprocess with an optional workspace-relative cwd",
        input_schema: CapabilitySchema::ProcessSpawn,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::CancelTurn),
        name: "astrcode.session.control.cancel_turn",
        required: Some(ExtensionCapability::SessionControl),
        description: "Cancel the active turn",
        input_schema: CapabilitySchema::Object,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::Create),
        name: "astrcode.session.control.create",
        required: Some(ExtensionCapability::SessionControl),
        description: "Create a child session",
        input_schema: CapabilitySchema::SessionCreate,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: true,
    },
    spec!(
        HostCapability::Session(SessionCapability::Dispose),
        "astrcode.session.control.dispose",
        Some(ExtensionCapability::SessionControl),
        "Dispose a session"
    ),
    spec!(
        HostCapability::Session(SessionCapability::ExecutionView),
        "astrcode.session.control.execution_view",
        Some(ExtensionCapability::SessionControl),
        "Read active turn and queued-input state"
    ),
    spec!(
        HostCapability::Session(SessionCapability::Inject),
        "astrcode.session.control.inject_input",
        Some(ExtensionCapability::SessionControl),
        "Inject input into a running turn or start when idle"
    ),
    spec!(
        HostCapability::Session(SessionCapability::Inject),
        "astrcode.session.control.inject_or_start",
        Some(ExtensionCapability::SessionControl),
        "Inject input into a running turn or start when idle"
    ),
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InterruptAndSubmit),
        name: "astrcode.session.control.interrupt_and_submit",
        required: Some(ExtensionCapability::SessionControl),
        description: "Interrupt the active turn and submit new input",
        input_schema: CapabilitySchema::Object,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    spec!(
        HostCapability::Session(SessionCapability::SubmitTurn),
        "astrcode.session.control.submit_turn",
        Some(ExtensionCapability::SessionControl),
        "Submit a turn to a session"
    ),
    spec!(
        HostCapability::Session(SessionCapability::InspectList),
        "astrcode.session.inspect.list",
        Some(ExtensionCapability::SessionInspect),
        "List all sessions visible to the host (global privileged access)"
    ),
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InspectProviderMessages),
        name: "astrcode.session.inspect.provider_messages",
        required: Some(ExtensionCapability::SessionInspect),
        description: "Read provider-visible messages for any host-visible session",
        input_schema: CapabilitySchema::SessionId,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InspectReadModel),
        name: "astrcode.session.inspect.read_model",
        required: Some(ExtensionCapability::SessionInspect),
        description: "Read any host-visible projected session model through a stable wire DTO",
        input_schema: CapabilitySchema::SessionId,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: true,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InspectSnapshot),
        name: "astrcode.session.inspect.snapshot",
        required: Some(ExtensionCapability::SessionInspect),
        description: "Read any host-visible session snapshot (global privileged access)",
        input_schema: CapabilitySchema::SessionId,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: true,
    },
    spec!(
        HostCapability::Session(SessionCapability::ReadEvents),
        "astrcode.session.read_events",
        Some(ExtensionCapability::SessionHistory),
        "Read session event log"
    ),
    HostCapabilitySpec {
        capability: HostCapability::Context(ContextCapability::StateRead),
        name: "astrcode.session.state.read",
        required: None,
        description: "Read extension-namespaced session state",
        input_schema: CapabilitySchema::Object,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: false,
    },
    HostCapabilitySpec {
        capability: HostCapability::Context(ContextCapability::StateWrite),
        name: "astrcode.session.state.write",
        required: None,
        description: "Write extension-namespaced session state",
        input_schema: CapabilitySchema::Object,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: false,
    },
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::Edit),
        name: "astrcode.workspace.edit",
        required: Some(ExtensionCapability::WorkspaceWrite),
        description: "Replace an exact text fragment in a non-sensitive workspace file",
        input_schema: CapabilitySchema::WorkspaceEdit,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: true,
    },
    spec!(
        HostCapability::Workspace(WorkspaceCapability::Glob),
        "astrcode.workspace.glob",
        Some(ExtensionCapability::WorkspaceRead),
        "Match bounded workspace paths by glob"
    ),
    spec!(
        HostCapability::Workspace(WorkspaceCapability::Grep),
        "astrcode.workspace.grep",
        Some(ExtensionCapability::WorkspaceRead),
        "Regex-search bounded UTF-8 workspace files"
    ),
    spec!(
        HostCapability::Workspace(WorkspaceCapability::List),
        "astrcode.workspace.list",
        Some(ExtensionCapability::WorkspaceRead),
        "List a bounded workspace directory tree"
    ),
    spec!(
        HostCapability::Workspace(WorkspaceCapability::Read),
        "astrcode.workspace.read",
        Some(ExtensionCapability::WorkspaceRead),
        "Read a bounded UTF-8 workspace file"
    ),
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::Write),
        name: "astrcode.workspace.write",
        required: Some(ExtensionCapability::WorkspaceWrite),
        description: "Create or replace a non-sensitive file under the working directory",
        input_schema: CapabilitySchema::WorkspaceWrite,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: true,
    },
];

pub(super) fn lookup(name: &str) -> Result<&'static HostCapabilitySpec, ErrorPayload> {
    HOST_CAPABILITY_SPECS
        .binary_search_by(|spec| spec.name.cmp(name))
        .map(|index| &HOST_CAPABILITY_SPECS[index])
        .map_err(|_| {
            ErrorPayload::new(
                "unknown_capability",
                format!("unknown astrcode capability: {name}"),
            )
        })
}

pub(super) fn authorize(
    spec: &HostCapabilitySpec,
    declared: &[ExtensionCapability],
) -> Result<(), ErrorPayload> {
    let Some(required) = spec.required else {
        return Ok(());
    };
    if declared.contains(&required) {
        return Ok(());
    }
    Err(ErrorPayload::new(
        "permission_denied",
        format!(
            "{} requires declared capability {}",
            spec.name,
            astrcode_extension_sdk::s5r::capability_to_wire(required)
        ),
    ))
}

pub(super) fn catalog_for_grants(
    capabilities: &[ExtensionCapability],
) -> Vec<CapabilityDescriptor> {
    HOST_CAPABILITY_SPECS
        .iter()
        .filter(|spec| spec.catalog)
        .filter(|spec| match spec.required {
            Some(required) => capabilities.contains(&required),
            None => true,
        })
        .map(capability_descriptor)
        .collect()
}

fn capability_descriptor(spec: &HostCapabilitySpec) -> CapabilityDescriptor {
    CapabilityDescriptor {
        name: spec.name.into(),
        description: spec.description.into(),
        input_schema: capability_schema(spec.input_schema),
        output_schema: capability_schema(spec.output_schema),
        supports_stream: spec.supports_stream,
        cancelable: spec.cancelable,
    }
}

fn capability_schema(schema: CapabilitySchema) -> Value {
    match schema {
        CapabilitySchema::Object => json!({ "type": "object" }),
        CapabilitySchema::LlmMessages => json!({
            "type": "object",
            "properties": { "messages": { "type": "array" } }
        }),
        CapabilitySchema::SessionId => json!({
            "type": "object",
            "properties": { "session_id": { "type": "string" } },
            "required": ["session_id"]
        }),
        CapabilitySchema::SessionCreate => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "working_dir": { "type": "string" },
                "system_prompt": { "type": "string" },
                "model_preference": { "type": "string" },
                "ephemeral": { "type": "boolean" },
                "tool_call_id": { "type": "string" },
                "tool_policy": {
                    "type": "object",
                    "description": "Child session tool visibility policy.",
                    "oneOf": [
                        {
                            "properties": {
                                "mode": { "const": "deny" },
                                "tools": { "type": "array", "items": { "type": "string" } }
                            },
                            "required": ["mode", "tools"]
                        },
                        {
                            "properties": {
                                "mode": { "const": "allow" },
                                "tools": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "minItems": 1
                                }
                            },
                            "required": ["mode", "tools"]
                        }
                    ]
                }
            }
        }),
        CapabilitySchema::WorkspaceWrite => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
        CapabilitySchema::WorkspaceEdit => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" },
                "replace_all": { "type": "boolean" }
            },
            "required": ["path", "old_text", "new_text"]
        }),
        CapabilitySchema::ProcessSpawn => json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": "string" },
                "stdin": { "type": "string" },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            },
            "required": ["command"]
        }),
        CapabilitySchema::NetworkRequest => json!({
            "type": "object",
            "properties": {
                "method": { "type": "string" },
                "url": { "type": "string" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "type": "string", "description": "UTF-8 request body" },
                "max_bytes": { "type": "integer", "minimum": 0 },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            },
            "required": ["url"]
        }),
        CapabilitySchema::NetworkResponse => json!({
            "type": "object",
            "properties": {
                "final_url": { "type": "string" },
                "status": { "type": "integer" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "type": "string", "description": "UTF-8 response body" }
            },
            "required": ["final_url", "status", "headers", "body"]
        }),
        CapabilitySchema::PublicHttpDispatch => json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                "path": { "type": "string" },
                "query": { "type": "string" },
                "body": {}
            },
            "required": ["method", "path"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn registry_names_are_unique_and_round_trip() {
        let mut names = HashSet::new();
        let mut capabilities = HashSet::new();

        assert!(
            HOST_CAPABILITY_SPECS
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name),
            "registry must remain sorted by wire name"
        );

        for spec in HOST_CAPABILITY_SPECS {
            assert!(names.insert(spec.name), "duplicate name: {}", spec.name);
            capabilities.insert(spec.capability);
            assert_eq!(
                lookup(spec.name).expect("registered capability").capability,
                spec.capability
            );

            let granted_catalog = match spec.required {
                Some(required) => catalog_for_grants(&[required]),
                None => catalog_for_grants(&[]),
            };
            assert_eq!(
                granted_catalog
                    .iter()
                    .any(|descriptor| descriptor.name == spec.name),
                spec.catalog,
                "catalog visibility mismatch: {}",
                spec.name
            );

            if let Some(required) = spec.required {
                assert!(authorize(spec, &[required]).is_ok());
                assert_eq!(
                    authorize(spec, &[]).expect_err("missing grant").code,
                    "permission_denied"
                );
                assert!(
                    !catalog_for_grants(&[])
                        .iter()
                        .any(|descriptor| descriptor.name == spec.name),
                    "capability visible without grant: {}",
                    spec.name
                );
            } else {
                assert!(authorize(spec, &[]).is_ok());
            }

            assert_eq!(
                spec.supports_stream,
                matches!(spec.capability, HostCapability::Llm(_)),
                "stream handler mismatch: {}",
                spec.name
            );
        }

        let expected_capabilities = LlmCapability::ALL
            .iter()
            .copied()
            .map(HostCapability::Llm)
            .chain(
                SessionCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Session),
            )
            .chain(
                ContextCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Context),
            )
            .chain(
                WorkspaceCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Workspace),
            )
            .chain(
                ProcessCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Process),
            )
            .chain(
                NetworkCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Network),
            )
            .chain(
                ExtensionHttpCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::ExtensionHttp),
            )
            .collect::<HashSet<_>>();
        assert_eq!(capabilities, expected_capabilities);
    }
}
