//! 协议消息序列化测试
//!
//! 验证各类消息（初始化、调用、事件、结果等）的序列化/反序列化
//! 是否正确，确保 JSON 格式与协议版本兼容。

use astrcode_runtime_contract::governance::{
    ActionPolicies, ChildPolicySpec, GovernanceModeSpec, ModeArtifactDef, ModeExecutionPolicySpec,
    ModeExitGateDef, ModeId, ModePromptHooks, TransitionPolicySpec,
};
use serde_json::json;

use super::v1::{
    CancelMessage, CapabilityKind, CapabilityWireDescriptor, CapabilityWireDescriptorBuildError,
    ErrorPayload, EventMessage, EventPhase, FilterDescriptor, HandlerDescriptor,
    HookDiagnosticWire, HookDispatchMessage, HookEffectWire, HookResultMessage, InitializeMessage,
    InitializeResultData, InvocationContext, InvocationMode, PLUGIN_PROTOCOL_VERSION,
    PeerDescriptor, PeerRole, PermissionSpec, PluginMessage, ProfileDescriptor, ResultMessage,
    SideEffect, Stability, TriggerDescriptor, WorkspaceRef,
};

fn sample_peer() -> PeerDescriptor {
    PeerDescriptor {
        id: "peer-1".to_string(),
        name: "sample".to_string(),
        role: PeerRole::Worker,
        version: "0.1.0".to_string(),
        supported_profiles: vec!["coding".to_string()],
        metadata: json!({ "region": "local" }),
    }
}

fn sample_capability() -> CapabilityWireDescriptor {
    CapabilityWireDescriptor::builder("tool.echo", CapabilityKind::tool())
        .description("Echo the input")
        .schema(json!({ "type": "object" }), json!({ "type": "object" }))
        .invocation_mode(InvocationMode::Streaming)
        .profile("coding")
        .tag("test")
        .permissions(vec![PermissionSpec {
            name: "filesystem.read".to_string(),
            rationale: Some("reads fixtures".to_string()),
        }])
        .side_effect(SideEffect::Local)
        .stability(Stability::Stable)
        .build()
        .expect("sample capability should build")
}

#[test]
fn plugin_messages_roundtrip_as_v1_json() {
    let init = PluginMessage::Initialize(InitializeMessage {
        id: "init-1".to_string(),
        protocol_version: PLUGIN_PROTOCOL_VERSION.to_string(),
        supported_protocol_versions: vec![PLUGIN_PROTOCOL_VERSION.to_string()],
        peer: sample_peer(),
        capabilities: vec![sample_capability()],
        handlers: vec![HandlerDescriptor {
            id: "handler-1".to_string(),
            trigger: TriggerDescriptor {
                kind: "command".to_string(),
                value: "/echo".to_string(),
                metadata: json!({}),
            },
            input_schema: json!({ "type": "object" }),
            profiles: vec!["coding".to_string()],
            filters: vec![FilterDescriptor {
                field: "profile".to_string(),
                op: "eq".to_string(),
                value: "coding".to_string(),
            }],
            permissions: vec![],
        }],
        profiles: vec![ProfileDescriptor {
            name: "coding".to_string(),
            version: "1".to_string(),
            description: "Coding workflow".to_string(),
            context_schema: json!({ "type": "object" }),
            metadata: json!({}),
        }],
        metadata: json!({ "bootstrap": true }),
    });

    let invoke = PluginMessage::Invoke(super::InvokeMessage {
        id: "req-1".to_string(),
        capability: "tool.echo".to_string(),
        input: json!({ "message": "hi" }),
        context: InvocationContext {
            request_id: "req-1".to_string(),
            trace_id: Some("trace-1".to_string()),
            session_id: Some("session-1".to_string()),
            caller: None,
            workspace: Some(WorkspaceRef {
                working_dir: Some("/tmp/project".to_string()),
                repo_root: Some("/tmp/project".to_string()),
                branch: Some("main".to_string()),
                metadata: json!({}),
            }),
            deadline_ms: Some(5_000),
            budget: None,
            profile: "coding".to_string(),
            profile_context: json!({
                "workingDir": "/tmp/project",
                "repoRoot": "/tmp/project",
                "openFiles": ["/tmp/project/src/main.rs"],
                "activeFile": "/tmp/project/src/main.rs",
                "selection": { "startLine": 1, "endLine": 3 },
                "approvalMode": "on-request"
            }),
            metadata: json!({}),
        },
        stream: true,
    });

    let result = PluginMessage::Result(ResultMessage {
        id: "init-1".to_string(),
        kind: Some("initialize".to_string()),
        success: true,
        output: serde_json::to_value(InitializeResultData {
            protocol_version: PLUGIN_PROTOCOL_VERSION.to_string(),
            peer: sample_peer(),
            capabilities: vec![sample_capability()],
            handlers: vec![],
            profiles: vec![],
            skills: vec![],
            modes: vec![],
            metadata: json!({}),
        })
        .expect("serialize initialize result"),
        error: None,
        metadata: json!({ "acceptedVersion": PLUGIN_PROTOCOL_VERSION }),
    });

    let event = PluginMessage::Event(EventMessage {
        id: "req-1".to_string(),
        phase: EventPhase::Delta,
        event: "artifact.patch".to_string(),
        payload: json!({ "path": "src/main.rs", "patch": "@@ ..." }),
        seq: 2,
        error: None,
    });

    let cancel = PluginMessage::Cancel(CancelMessage {
        id: "req-1".to_string(),
        reason: Some("user interrupted".to_string()),
    });

    let hook_dispatch = PluginMessage::HookDispatch(HookDispatchMessage {
        correlation_id: "hook-1".to_string(),
        snapshot_id: "snapshot-1".to_string(),
        plugin_id: "plugin-1".to_string(),
        hook_id: "tool-policy".to_string(),
        event: "tool_call".to_string(),
        payload: json!({ "toolCallId": "call-1" }),
    });

    let hook_result = PluginMessage::HookResult(HookResultMessage {
        correlation_id: "hook-1".to_string(),
        effects: vec![HookEffectWire {
            kind: "BlockToolResult".to_string(),
            payload: json!({
                "toolCallId": "call-1",
                "reason": "policy denied"
            }),
        }],
        diagnostics: vec![HookDiagnosticWire {
            message: "checked policy".to_string(),
            severity: Some("info".to_string()),
        }],
    });

    for message in [
        init,
        invoke,
        result,
        event,
        cancel,
        hook_dispatch,
        hook_result,
    ] {
        let json = serde_json::to_string(&message).expect("serialize message");
        let decoded: PluginMessage = serde_json::from_str(&json).expect("deserialize message");
        assert_eq!(decoded, message);
    }
}

#[test]
fn initialize_result_uses_result_kind_payload() {
    let result = ResultMessage {
        id: "init-1".to_string(),
        kind: Some("initialize".to_string()),
        success: true,
        output: serde_json::to_value(InitializeResultData {
            protocol_version: PLUGIN_PROTOCOL_VERSION.to_string(),
            peer: sample_peer(),
            capabilities: vec![sample_capability()],
            handlers: vec![],
            profiles: vec![],
            skills: vec![],
            modes: vec![],
            metadata: json!({ "mode": "stdio" }),
        })
        .expect("serialize initialize result"),
        error: None,
        metadata: json!({}),
    };

    let decoded: InitializeResultData = result.parse_output().expect("parse output");
    assert_eq!(decoded.protocol_version, PLUGIN_PROTOCOL_VERSION);
    assert_eq!(decoded.peer.role, PeerRole::Worker);
    assert_eq!(decoded.capabilities[0].name.as_str(), "tool.echo");
}

#[test]
fn initialize_result_serializes_declared_modes() {
    let mode = GovernanceModeSpec {
        id: ModeId::from("plugin.plan-lite"),
        name: "Plan Lite".to_string(),
        description: "Plugin-provided planning mode.".to_string(),
        action_policies: ActionPolicies::default(),
        child_policy: ChildPolicySpec::default(),
        execution_policy: ModeExecutionPolicySpec::default(),
        prompt_program: vec![],
        artifact: Some(ModeArtifactDef {
            artifact_type: "canonical-plan".to_string(),
            file_template: Some("# Plan".to_string()),
            schema_template: None,
            required_headings: vec!["Implementation Steps".to_string()],
            actionable_sections: vec!["Implementation Steps".to_string()],
        }),
        exit_gate: Some(ModeExitGateDef {
            review_passes: 1,
            review_checklist: vec!["检查假设".to_string()],
        }),
        prompt_hooks: Some(ModePromptHooks {
            reentry_prompt: Some("read the plan".to_string()),
            initial_template: None,
            exit_prompt: Some("approved plan".to_string()),
            facts_template: None,
        }),
        transition_policy: TransitionPolicySpec {
            allowed_targets: vec![ModeId::code()],
        },
    };
    let result = InitializeResultData {
        protocol_version: PLUGIN_PROTOCOL_VERSION.to_string(),
        peer: sample_peer(),
        capabilities: vec![],
        handlers: vec![],
        profiles: vec![],
        skills: vec![],
        modes: vec![mode.clone()],
        metadata: json!({}),
    };

    let encoded = serde_json::to_value(&result).expect("initialize result should serialize");
    assert_eq!(encoded["modes"][0]["id"], "plugin.plan-lite");

    let decoded: InitializeResultData =
        serde_json::from_value(encoded).expect("initialize result should deserialize");
    assert_eq!(decoded.modes, vec![mode]);
}

#[test]
fn initialize_result_deserializes_legacy_mode_shape_without_new_contract_fields() {
    let raw = json!({
        "protocolVersion": PLUGIN_PROTOCOL_VERSION,
        "peer": sample_peer(),
        "capabilities": [],
        "handlers": [],
        "profiles": [],
        "skills": [],
        "modes": [{
            "id": "plugin.legacy",
            "name": "Legacy",
            "description": "legacy mode",
            "capabilitySelector": { "tag": "read-only" },
            "actionPolicies": {},
            "childPolicy": {},
            "executionPolicy": {},
            "promptProgram": [],
            "transitionPolicy": {}
        }],
        "metadata": {}
    });

    let decoded: InitializeResultData =
        serde_json::from_value(raw).expect("legacy shape should deserialize");
    assert_eq!(decoded.modes.len(), 1);
    assert_eq!(decoded.modes[0].artifact, None);
    assert_eq!(decoded.modes[0].exit_gate, None);
    assert_eq!(decoded.modes[0].prompt_hooks, None);
}

#[test]
fn invocation_context_supports_coding_profile_shape() {
    let context = InvocationContext {
        request_id: "req-1".to_string(),
        trace_id: None,
        session_id: Some("session-1".to_string()),
        caller: None,
        workspace: Some(WorkspaceRef {
            working_dir: Some("/repo".to_string()),
            repo_root: Some("/repo".to_string()),
            branch: None,
            metadata: json!({}),
        }),
        deadline_ms: None,
        budget: None,
        profile: "coding".to_string(),
        profile_context: json!({
            "workingDir": "/repo",
            "repoRoot": "/repo",
            "openFiles": ["/repo/src/lib.rs"],
            "activeFile": "/repo/src/lib.rs",
            "selection": {
                "startLine": 10,
                "startColumn": 1,
                "endLine": 12,
                "endColumn": 4
            },
            "approvalMode": "never"
        }),
        metadata: json!({}),
    };

    let value = serde_json::to_value(&context).expect("serialize context");
    assert_eq!(value["profile"], "coding");
    assert_eq!(value["profileContext"]["activeFile"], "/repo/src/lib.rs");
    assert_eq!(value["profileContext"]["approvalMode"], "never");
}

#[test]
fn result_message_preserves_error_payload_details() {
    let message = ResultMessage {
        id: "req-1".to_string(),
        kind: None,
        success: false,
        output: json!(null),
        error: Some(ErrorPayload {
            code: "permission_denied".to_string(),
            message: "filesystem.write requires approval".to_string(),
            details: json!({ "permission": "filesystem.write" }),
            retriable: false,
        }),
        metadata: json!({ "source": "policy" }),
    };

    let encoded = serde_json::to_value(&message).expect("serialize result");
    assert_eq!(
        encoded["error"]["details"]["permission"],
        "filesystem.write"
    );
    assert_eq!(encoded["metadata"]["source"], "policy");
}

#[test]
fn capability_builder_rejects_invalid_fields() {
    let error = CapabilityWireDescriptor::builder("tool.echo", CapabilityKind::tool())
        .description("Echo the input")
        .schema(json!({ "type": "object" }), json!("not-a-schema"))
        .profile("coding")
        .build()
        .expect_err("invalid output schema should fail");

    assert_eq!(
        error,
        CapabilityWireDescriptorBuildError::InvalidSchema("output_schema")
    );
}

#[test]
fn capability_builder_accepts_custom_kind_strings() {
    let descriptor = CapabilityWireDescriptor::builder("workspace.index", "lsp.indexer")
        .description("Indexes workspace symbols")
        .schema(json!({ "type": "object" }), json!({ "type": "object" }))
        .build()
        .expect("custom kind should build");

    assert_eq!(descriptor.kind.as_str(), "lsp.indexer");
    assert_eq!(
        serde_json::to_value(&descriptor).expect("serialize descriptor")["kind"],
        "lsp.indexer"
    );
}

#[test]
fn capability_builder_rejects_blank_custom_kind() {
    let error = CapabilityWireDescriptor::builder("workspace.index", CapabilityKind::new("  "))
        .description("Indexes workspace symbols")
        .schema(json!({ "type": "object" }), json!({ "type": "object" }))
        .build()
        .expect_err("blank kind should fail");

    assert_eq!(
        error,
        CapabilityWireDescriptorBuildError::EmptyField("kind")
    );
}

#[test]
fn capability_kind_deserialization_trims_whitespace() {
    let kind: CapabilityKind =
        serde_json::from_value(json!("  lsp.indexer  ")).expect("kind should deserialize");

    assert_eq!(kind.as_str(), "lsp.indexer");
}

#[test]
fn capability_validate_rejects_direct_blank_kind() {
    let descriptor = CapabilityWireDescriptor {
        name: "workspace.index".into(),
        kind: CapabilityKind::new("  "),
        description: "Indexes workspace symbols".to_string(),
        input_schema: json!({ "type": "object" }),
        output_schema: json!({ "type": "object" }),
        invocation_mode: InvocationMode::Unary,
        concurrency_safe: false,
        compact_clearable: false,
        profiles: vec![],
        tags: vec![],
        permissions: vec![],
        side_effect: SideEffect::None,
        stability: Stability::Stable,
        metadata: json!({}),
        max_result_inline_size: None,
    };

    assert_eq!(
        descriptor
            .validate()
            .expect_err("direct descriptor validation should reject blank kind"),
        CapabilityWireDescriptorBuildError::EmptyField("kind")
    );
}
