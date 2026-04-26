use astrcode_protocol::plugin::v1::{
    BudgetHint, CallerRef, CancelMessage, CapabilityKind, CapabilityWireDescriptor, ErrorPayload,
    EventMessage, EventPhase, FilterDescriptor, HandlerDescriptor, InitializeMessage,
    InitializeResultData, InvocationContext, InvocationMode, PLUGIN_PROTOCOL_VERSION,
    PeerDescriptor, PeerRole, PermissionSpec, PluginMessage, ProfileDescriptor, ResultMessage,
    SideEffect, Stability, TriggerDescriptor, WorkspaceRef,
};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = match name {
        "initialize" => include_str!("fixtures/plugin/v1/initialize.json"),
        "invoke" => include_str!("fixtures/plugin/v1/invoke.json"),
        "result_initialize" => include_str!("fixtures/plugin/v1/result_initialize.json"),
        "result_error" => include_str!("fixtures/plugin/v1/result_error.json"),
        "event_delta" => include_str!("fixtures/plugin/v1/event_delta.json"),
        "cancel" => include_str!("fixtures/plugin/v1/cancel.json"),
        other => panic!("unknown fixture {other}"),
    };
    serde_json::from_str(path).expect("fixture should be valid JSON")
}

fn sample_peer() -> PeerDescriptor {
    PeerDescriptor {
        id: "worker-1".to_string(),
        name: "repo-inspector".to_string(),
        role: PeerRole::Worker,
        version: "0.1.0".to_string(),
        supported_profiles: vec!["coding".to_string()],
        metadata: json!({ "transport": "stdio" }),
    }
}

fn sample_capability() -> CapabilityWireDescriptor {
    CapabilityWireDescriptor {
        name: "workspace.summary".into(),
        kind: CapabilityKind::tool(),
        description: "Summarize the active coding workspace.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
        output_schema: json!({ "type": "object" }),
        invocation_mode: InvocationMode::Unary,
        concurrency_safe: false,
        compact_clearable: false,
        profiles: vec!["coding".to_string()],
        tags: vec!["workspace".to_string(), "summary".to_string()],
        permissions: vec![PermissionSpec {
            name: "filesystem.read".to_string(),
            rationale: Some("Need to inspect the active repository.".to_string()),
        }],
        side_effect: SideEffect::None,
        stability: Stability::Stable,
        metadata: Value::Null,
        max_result_inline_size: None,
    }
}

fn sample_profile() -> ProfileDescriptor {
    ProfileDescriptor {
        name: "coding".to_string(),
        version: "1".to_string(),
        description: "Coding workflow profile.".to_string(),
        context_schema: json!({
            "type": "object",
            "properties": {
                "workingDir": { "type": "string" },
                "repoRoot": { "type": "string" },
                "openFiles": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "activeFile": { "type": "string" },
                "selection": { "type": "object" },
                "approvalMode": { "type": "string" }
            }
        }),
        metadata: json!({ "firstClass": true }),
    }
}

#[test]
fn initialize_fixture_matches_plugin_v1_shape() {
    let expected = PluginMessage::Initialize(InitializeMessage {
        id: "init-1".to_string(),
        protocol_version: PLUGIN_PROTOCOL_VERSION.to_string(),
        supported_protocol_versions: vec![PLUGIN_PROTOCOL_VERSION.to_string()],
        peer: sample_peer(),
        capabilities: vec![sample_capability()],
        handlers: vec![HandlerDescriptor {
            id: "command.workspace.summary".to_string(),
            trigger: TriggerDescriptor {
                kind: "command".to_string(),
                value: "/workspace-summary".to_string(),
                metadata: json!({ "aliases": ["/ws"] }),
            },
            input_schema: json!({ "type": "object" }),
            profiles: vec!["coding".to_string()],
            filters: vec![FilterDescriptor {
                field: "profile".to_string(),
                op: "eq".to_string(),
                value: "coding".to_string(),
            }],
            permissions: vec![PermissionSpec {
                name: "filesystem.read".to_string(),
                rationale: Some("Reads workspace files.".to_string()),
            }],
        }],
        profiles: vec![sample_profile()],
        metadata: json!({
            "bootstrap": true,
            "transport": "stdio"
        }),
    });

    let fixture = fixture("initialize");
    let decoded: PluginMessage =
        serde_json::from_value(fixture.clone()).expect("fixture should decode");
    assert_eq!(decoded, expected);
    assert_eq!(
        serde_json::to_value(&decoded).expect("message should encode"),
        fixture
    );
}

#[test]
fn invoke_fixture_freezes_coding_profile_context_shape() {
    let expected = PluginMessage::Invoke(astrcode_protocol::plugin::InvokeMessage {
        id: "req-1".to_string(),
        capability: "workspace.summary".to_string(),
        input: json!({ "scope": "workspace" }),
        context: InvocationContext {
            request_id: "req-1".to_string(),
            trace_id: Some("trace-1".to_string()),
            session_id: Some("session-1".to_string()),
            caller: Some(CallerRef {
                id: "astrcode-runtime".to_string(),
                role: "runtime".to_string(),
                metadata: json!({ "origin": "server" }),
            }),
            workspace: Some(WorkspaceRef {
                working_dir: Some("/repo".to_string()),
                repo_root: Some("/repo".to_string()),
                branch: Some("main".to_string()),
                metadata: json!({ "provider": "git" }),
            }),
            deadline_ms: Some(5_000),
            budget: Some(BudgetHint {
                max_duration_ms: Some(5_000),
                max_events: Some(64),
                max_bytes: Some(65_536),
            }),
            profile: "coding".to_string(),
            profile_context: json!({
                "workingDir": "/repo",
                "repoRoot": "/repo",
                "openFiles": ["/repo/src/lib.rs", "/repo/README.md"],
                "activeFile": "/repo/src/lib.rs",
                "selection": {
                    "startLine": 10,
                    "startColumn": 1,
                    "endLine": 12,
                    "endColumn": 4
                },
                "approvalMode": "on-request"
            }),
            metadata: json!({ "requestSource": "chat" }),
        },
        stream: true,
    });

    let fixture = fixture("invoke");
    let decoded: PluginMessage =
        serde_json::from_value(fixture.clone()).expect("fixture should decode");
    assert_eq!(decoded, expected);
    assert_eq!(
        serde_json::to_value(&decoded).expect("message should encode"),
        fixture
    );
}

#[test]
fn result_initialize_fixture_freezes_handshake_response_shape() {
    let expected = PluginMessage::Result(ResultMessage {
        id: "init-1".to_string(),
        kind: Some("initialize".to_string()),
        success: true,
        output: serde_json::to_value(InitializeResultData {
            protocol_version: PLUGIN_PROTOCOL_VERSION.to_string(),
            peer: sample_peer(),
            capabilities: vec![sample_capability()],
            handlers: vec![],
            profiles: vec![sample_profile()],
            skills: vec![],
            modes: vec![],
            metadata: json!({ "transport": "stdio" }),
        })
        .expect("initialize result should serialize"),
        error: None,
        metadata: json!({ "acceptedVersion": PLUGIN_PROTOCOL_VERSION }),
    });

    let fixture = fixture("result_initialize");
    let decoded: PluginMessage =
        serde_json::from_value(fixture.clone()).expect("fixture should decode");
    assert_eq!(decoded, expected);
    assert_eq!(
        serde_json::to_value(&decoded).expect("message should encode"),
        fixture
    );

    let PluginMessage::Result(result) = decoded else {
        panic!("expected result fixture");
    };
    let handshake: InitializeResultData = result.parse_output().expect("output should parse");
    assert_eq!(handshake.protocol_version, PLUGIN_PROTOCOL_VERSION);
    assert_eq!(handshake.capabilities[0].name.as_str(), "workspace.summary");
}

#[test]
fn result_error_fixture_freezes_error_payload_contract() {
    let expected = PluginMessage::Result(ResultMessage {
        id: "req-2".to_string(),
        kind: None,
        success: false,
        output: Value::Null,
        error: Some(ErrorPayload {
            code: "permission_denied".to_string(),
            message: "filesystem.write requires approval".to_string(),
            details: json!({ "permission": "filesystem.write" }),
            retriable: false,
        }),
        metadata: json!({ "source": "policy" }),
    });

    let fixture = fixture("result_error");
    let decoded: PluginMessage =
        serde_json::from_value(fixture.clone()).expect("fixture should decode");
    assert_eq!(decoded, expected);
    assert_eq!(
        serde_json::to_value(&decoded).expect("message should encode"),
        fixture
    );
}

#[test]
fn event_and_cancel_fixtures_freeze_stream_and_cancel_shapes() {
    let event_fixture = fixture("event_delta");
    let event_decoded: PluginMessage =
        serde_json::from_value(event_fixture.clone()).expect("event fixture should decode");
    assert_eq!(
        event_decoded,
        PluginMessage::Event(EventMessage {
            id: "req-1".to_string(),
            phase: EventPhase::Delta,
            event: "artifact.patch".to_string(),
            payload: json!({
                "path": "src/lib.rs",
                "patch": "@@ -1 +1 @@\n-pub fn old() {}\n+pub fn new() {}\n"
            }),
            seq: 2,
            error: None,
        })
    );
    assert_eq!(
        serde_json::to_value(&event_decoded).expect("event should encode"),
        event_fixture
    );

    let cancel_fixture = fixture("cancel");
    let cancel_decoded: PluginMessage =
        serde_json::from_value(cancel_fixture.clone()).expect("cancel fixture should decode");
    assert_eq!(
        cancel_decoded,
        PluginMessage::Cancel(CancelMessage {
            id: "req-1".to_string(),
            reason: Some("user interrupted".to_string()),
        })
    );
    assert_eq!(
        serde_json::to_value(&cancel_decoded).expect("cancel should encode"),
        cancel_fixture
    );
}
