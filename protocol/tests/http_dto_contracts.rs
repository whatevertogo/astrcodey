use astrcode_protocol::http::{
    ForkModeDto, ResolvedSubagentContextOverridesDto, SubRunExecutionMetricsDto,
    SubRunFailureCodeDto, SubRunFailureDto, SubRunHandoffDto, SubRunResultDto,
    SubRunStorageModeDto,
};
use serde_json::json;

#[test]
fn subrun_result_dto_roundtrip_preserves_tagged_union_shape() {
    let completed = SubRunResultDto::Completed {
        handoff: SubRunHandoffDto {
            findings: vec!["checked".to_string()],
            artifacts: Vec::new(),
            delivery: None,
        },
    };
    let encoded = serde_json::to_value(&completed).expect("serialize completed result");
    assert_eq!(encoded.get("status"), Some(&json!("completed")));
    assert!(encoded.get("handoff").is_some());
    assert!(encoded.get("failure").is_none());
    let decoded: SubRunResultDto =
        serde_json::from_value(encoded).expect("deserialize completed result");
    assert_eq!(decoded, completed);

    let cancelled = SubRunResultDto::Cancelled {
        failure: SubRunFailureDto {
            code: SubRunFailureCodeDto::Interrupted,
            display_message: "cancelled by parent".to_string(),
            technical_message: "parent requested shutdown".to_string(),
            retryable: true,
        },
    };
    let encoded = serde_json::to_value(&cancelled).expect("serialize cancelled result");
    assert_eq!(encoded.get("status"), Some(&json!("cancelled")));
    assert!(encoded.get("failure").is_some());
    assert!(encoded.get("handoff").is_none());
    let decoded: SubRunResultDto =
        serde_json::from_value(encoded).expect("deserialize cancelled result");
    assert_eq!(decoded, cancelled);
}

#[test]
fn resolved_subagent_context_overrides_roundtrip_preserves_fork_mode() {
    let overrides = ResolvedSubagentContextOverridesDto {
        storage_mode: SubRunStorageModeDto::IndependentSession,
        inherit_system_instructions: true,
        inherit_project_instructions: true,
        inherit_working_dir: true,
        inherit_policy_upper_bound: true,
        inherit_cancel_token: true,
        include_compact_summary: true,
        include_recent_tail: false,
        include_recovery_refs: false,
        include_parent_findings: false,
        fork_mode: Some(ForkModeDto::LastNTurns(7)),
    };

    let encoded = serde_json::to_value(&overrides).expect("serialize overrides");
    assert_eq!(encoded.get("forkMode"), Some(&json!({ "lastNTurns": 7 })));

    let decoded: ResolvedSubagentContextOverridesDto =
        serde_json::from_value(encoded).expect("deserialize overrides");
    assert_eq!(decoded, overrides);
}

#[test]
fn subrun_execution_metrics_serialize_cancelled_field_name() {
    let metrics = SubRunExecutionMetricsDto {
        total: 12,
        failures: 2,
        completed: 7,
        cancelled: 1,
        independent_session_total: 9,
        total_duration_ms: 1200,
        last_duration_ms: 80,
        total_steps: 42,
        last_step_count: 3,
        total_estimated_tokens: 4096,
        last_estimated_tokens: 256,
    };

    let encoded = serde_json::to_value(&metrics).expect("serialize metrics");
    assert_eq!(encoded.get("cancelled"), Some(&json!(1)));
    assert!(encoded.get("aborted").is_none());

    let decoded: SubRunExecutionMetricsDto =
        serde_json::from_value(encoded).expect("deserialize metrics");
    assert_eq!(decoded, metrics);
}
