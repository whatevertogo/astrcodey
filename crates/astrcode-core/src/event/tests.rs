use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::{envelope::EVENT_ENVELOPE_KEYS, *};
use crate::{
    llm::{LlmMessage, LlmTokenUsage},
    tool::ToolResult,
};


#[test]
fn event_serializes_payload_under_nested_namespace() {
    let event = Event {
        seq: Some(1),
        id: "event-1".into(),
        session_id: "parent-session".into(),
        turn_id: None,
        timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        payload: EventPayload::AgentSessionCompleted {
            child_session_id: "child-a".into(),
            final_session_id: "child-a".into(),
            summary: "ok".into(),
        },
    };
    let json = serde_json::to_value(&event).unwrap();
    for key in ["seq", "id", "session_id", "timestamp"] {
        assert!(
            json.get(key).is_some(),
            "event must include envelope field `{key}`"
        );
    }
    assert!(json.get("turn_id").is_none());
    assert_eq!(json["session_id"], "parent-session");
    assert_eq!(json["payload"]["type"], "agent_session_completed");
    assert_eq!(json["payload"]["child_session_id"], "child-a");
    assert_eq!(json["id"], "event-1");
    assert!(json.get("child_session_id").is_none());
}

#[test]
fn event_rejects_reserved_keys_in_extension_payload() {
    let event = Event {
        seq: Some(0),
        id: "event-1".into(),
        session_id: "session-1".into(),
        turn_id: None,
        timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        payload: EventPayload::ExtensionEvent {
            extension_id: "memory".into(),
            event_type: "memory.accepted".into(),
            schema_version: 1,
            payload: serde_json::json!({ "session_id": "fake" }),
        },
    };

    let err = serde_json::to_string(&event).unwrap_err();

    assert!(
        err.to_string()
            .contains("ExtensionEvent payload contains reserved Event envelope key")
    );
}

#[test]
fn event_rejects_reserved_keys_in_custom_data() {
    let event = Event {
        seq: Some(0),
        id: "event-1".into(),
        session_id: "session-1".into(),
        turn_id: None,
        timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        payload: EventPayload::Custom {
            name: "external".into(),
            data: serde_json::json!({ "payload": "nested collision" }),
        },
    };

    let err = serde_json::to_string(&event).unwrap_err();

    assert!(
        err.to_string()
            .contains("Custom data contains reserved Event envelope key")
    );
}

#[test]
fn durable_classification_matches_event_log_policy() {
    assert!(
        !EventPayload::AssistantTextDelta {
            message_id: "m1".into(),
            delta: "hi".into(),
        }
        .is_durable()
    );
    assert!(
        !EventPayload::ToolCallArgumentsDelta {
            call_id: "c1".into(),
            delta: "{}".into(),
        }
        .is_durable()
    );
    assert!(
        EventPayload::ToolCallRequested {
            call_id: "c1".into(),
            tool_name: "shell".into(),
            arguments: serde_json::json!({"cmd": "pwd"}),
            raw_arguments: None,
        }
        .is_durable()
    );
    assert!(
        EventPayload::ToolCallCompleted {
            call_id: "c1".into(),
            tool_name: "shell".into(),
            result: ToolResult {
                content: "ok".into(),
                is_error: false,
                error: None,
                metadata: BTreeMap::new(),
                duration_ms: Some(10),
            },
            arguments: String::new(),
            arguments_json: None,
        }
        .is_durable()
    );
    assert!(
        !EventPayload::ThinkingDelta {
            message_id: "m1".into(),
            delta: "thinking".into(),
        }
        .is_durable(),
        "ThinkingDelta is live UI state only"
    );
    assert!(
        !EventPayload::ToolCallStarted {
            call_id: "c1".into(),
            tool_name: "shell".into(),
        }
        .is_durable(),
        "ToolCallStarted is live UI state only"
    );
    assert!(
        EventPayload::CompactBoundaryCreated {
            trigger: "manual_command".into(),
            pre_tokens: 10,
            post_tokens: 3,
            summary: "summary".into(),
            transcript_path: None,
            continued_session_id: "child".into(),
            base_event_seq: 0,
            strategy: crate::extension::CompactStrategy::Manual {
                keep_recent_turns: None,
            },
        }
        .is_durable(),
        "CompactBoundaryCreated is the durable parent-session audit fact"
    );
    assert!(
        EventPayload::SessionContinuedFromCompaction {
            parent_session_id: "parent".into(),
            parent_cursor: "2".into(),
            summary: "summary".into(),
            transcript_path: None,
            context_messages: vec![LlmMessage::system("summary")],
            retained_messages: vec![LlmMessage::user("recent")],
        }
        .is_durable(),
        "SessionContinuedFromCompaction is the durable child-session projection fact"
    );
    assert!(
        EventPayload::TurnAbortedContext.is_durable(),
        "TurnAbortedContext must be durably visible to the next provider request"
    );
}

#[test]
fn compact_boundary_created_serializes_continuation_target() {
    let payload = EventPayload::CompactBoundaryCreated {
        trigger: "manual_command".into(),
        pre_tokens: 100,
        post_tokens: 20,
        summary: "summary".into(),
        transcript_path: Some("compact.jsonl".into()),
        continued_session_id: "child-session".into(),
        base_event_seq: 42,
        strategy: crate::extension::CompactStrategy::Manual {
            keep_recent_turns: None,
        },
    };

    let value = serde_json::to_value(&payload).unwrap();
    let round_trip: EventPayload = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(value["type"], "compact_boundary_created");
    assert_eq!(value["continued_session_id"], "child-session");
    assert_eq!(round_trip, payload);
}

#[test]
fn session_continued_from_compaction_serializes_context() {
    let payload = EventPayload::SessionContinuedFromCompaction {
        parent_session_id: "parent-session".into(),
        parent_cursor: "7".into(),
        summary: "summary".into(),
        transcript_path: Some("compact.jsonl".into()),
        context_messages: vec![LlmMessage::system("hidden summary")],
        retained_messages: vec![LlmMessage::user("recent")],
    };

    let value = serde_json::to_value(&payload).unwrap();
    let round_trip: EventPayload = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(value["type"], "session_continued_from_compaction");
    assert_eq!(value["parent_session_id"], "parent-session");
    assert_eq!(value["parent_cursor"], "7");
    assert_eq!(round_trip, payload);
}

#[test]
fn thinking_delta_serializes_message_owner() {
    let payload = EventPayload::ThinkingDelta {
        message_id: "assistant-1".into(),
        delta: "reasoning".into(),
    };

    let value = serde_json::to_value(&payload).unwrap();
    let round_trip: EventPayload = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(value["type"], "thinking_delta");
    assert_eq!(value["message_id"], "assistant-1");
    assert_eq!(value["delta"], "reasoning");
    assert_eq!(round_trip, payload);
}

#[test]
fn agent_session_spawned_serializes_and_is_durable() {
    let payload = EventPayload::AgentSessionSpawned {
        child_session_id: "child-1".into(),
        agent_name: "reviewer".into(),
        task: "review current diff".into(),
        tool_selection: None,
        tool_call_id: "call-42".into(),
    };

    assert!(payload.is_durable());

    let value = serde_json::to_value(&payload).unwrap();
    assert_eq!(value["type"], "agent_session_spawned");
    assert_eq!(value["child_session_id"], "child-1");
    assert_eq!(value["agent_name"], "reviewer");
    assert_eq!(value["task"], "review current diff");
    assert!(value["tool_selection"].is_null());
    assert_eq!(value["tool_call_id"], "call-42");

    let round_trip: EventPayload = serde_json::from_value(value).unwrap();
    assert_eq!(round_trip, payload);
}

#[test]
fn legacy_tool_policy_is_rejected_instead_of_widening_session_access() {
    let legacy = serde_json::json!({
        "type": "session_started",
        "working_dir": ".",
        "model_id": "mock",
        "parent_session_id": "parent",
        "tool_policy": {
            "mode": "deny",
            "tools": ["agent"]
        }
    });

    let error = serde_json::from_value::<EventPayload>(legacy).unwrap_err();
    assert!(error.to_string().contains("unknown field `tool_policy`"));
}

#[test]
fn token_usage_recorded_serializes_snake_case_usage_fields() {
    let payload = EventPayload::TokenUsageRecorded {
        usage: LlmTokenUsage {
            input_tokens: Some(100),
            cached_input_tokens: Some(64),
            cache_creation_input_tokens: None,
            output_tokens: Some(20),
            reasoning_output_tokens: Some(5),
            total_tokens: Some(120),
            source: None,
        },
        model_context_window: 8192,
    };

    let value = serde_json::to_value(&payload).unwrap();
    assert_eq!(value["type"], "token_usage_recorded");
    assert_eq!(value["usage"]["input_tokens"], 100);
    assert_eq!(value["usage"]["cached_input_tokens"], 64);
    assert_eq!(value["usage"]["output_tokens"], 20);
    assert_eq!(value["usage"]["reasoning_output_tokens"], 5);
    assert_eq!(value["usage"]["total_tokens"], 120);
    assert_eq!(value["model_context_window"], 8192);

    let round_trip: EventPayload = serde_json::from_value(value).unwrap();
    assert_eq!(round_trip, payload);
}

/// 扫描所有现有 EventPayload 变体，断言事件写出时 payload 字段不泄漏到顶层。
#[test]
fn event_payload_variants_stay_nested() {
    let samples: Vec<EventPayload> = vec![
        EventPayload::SessionStarted {
            working_dir: ".".into(),
            model_id: "m".into(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
        },
        EventPayload::ModelIdChanged {
            model_id: "m".into(),
        },
        EventPayload::SystemPromptConfigured {
            text: "t".into(),
            fingerprint: "f".into(),
            extra_system_prompt: None,
        },
        EventPayload::SessionDeleted,
        EventPayload::AgentSessionSpawned {
            child_session_id: "c".into(),
            agent_name: "a".into(),
            task: "t".into(),
            tool_selection: None,
            tool_call_id: "tc".into(),
        },
        EventPayload::AgentRunStarted,
        EventPayload::AgentRunCompleted { reason: "r".into() },
        EventPayload::AgentSessionCompleted {
            child_session_id: "c".into(),
            final_session_id: "c".into(),
            summary: "s".into(),
        },
        EventPayload::AgentSessionFailed {
            child_session_id: "c".into(),
            final_session_id: "c".into(),
            error: "e".into(),
        },
        EventPayload::AgentSessionRecycled {
            child_session_id: "c".into(),
        },
        EventPayload::TurnStarted,
        EventPayload::TurnCompleted {
            finish_reason: "stop".into(),
        },
        EventPayload::TurnAbortedContext,
        EventPayload::UserMessage {
            message_id: "m".into(),
            text: "t".into(),
            attachments: vec![],
        },
        EventPayload::RecapGenerated {
            text: "t".into(),
            source: "manual".into(),
        },
        EventPayload::AssistantMessageStarted {
            message_id: "m".into(),
        },
        EventPayload::AssistantTextDelta {
            message_id: "m".into(),
            delta: "d".into(),
        },
        EventPayload::AssistantMessageCompleted {
            message_id: "m".into(),
            text: "t".into(),
            reasoning_content: None,
        },
        EventPayload::TokenUsageRecorded {
            usage: LlmTokenUsage {
                input_tokens: Some(1),
                cached_input_tokens: Some(1),
                cache_creation_input_tokens: None,
                output_tokens: Some(1),
                reasoning_output_tokens: Some(1),
                total_tokens: Some(2),
                source: None,
            },
            model_context_window: 1024,
        },
        EventPayload::ThinkingDelta {
            message_id: "m".into(),
            delta: "d".into(),
        },
        EventPayload::ToolCallStarted {
            call_id: "c".into(),
            tool_name: "t".into(),
        },
        EventPayload::ToolCallArgumentsDelta {
            call_id: "c".into(),
            delta: "{}".into(),
        },
        EventPayload::ToolCallRequested {
            call_id: "c".into(),
            tool_name: "t".into(),
            arguments: serde_json::json!({}),
            raw_arguments: None,
        },
        EventPayload::ToolOutputDelta {
            call_id: "c".into(),
            stream: ToolOutputStream::Stdout,
            delta: "d".into(),
        },
        EventPayload::ToolApprovalRequested {
            call_id: "c".into(),
            tool_name: "t".into(),
            prompt: "p".into(),
            rule_key: None,
            source: crate::permission::ApprovalSource::Core,
            arguments: serde_json::json!({}),
        },
        EventPayload::ToolApprovalResolved {
            call_id: "c".into(),
            decision: crate::permission::ApprovalDecision::AllowOnce,
            detail: None,
        },
        EventPayload::ToolCallInteractionPending {
            call_id: "c".into(),
            content: "ok".into(),
            metadata: BTreeMap::new(),
        },
        EventPayload::ToolCallCompleted {
            call_id: "c".into(),
            tool_name: "t".into(),
            result: ToolResult {
                content: "ok".into(),
                is_error: false,
                error: None,
                metadata: BTreeMap::new(),
                duration_ms: Some(10),
            },
            arguments: String::new(),
            arguments_json: None,
        },
        EventPayload::ToolCallFailed {
            call_id: "c".into(),
            tool_name: "t".into(),
            error: "failed".into(),
            metadata: BTreeMap::new(),
            duration_ms: Some(10),
            arguments: String::new(),
            arguments_json: None,
        },
        EventPayload::ToolCallCancelled {
            call_id: "c".into(),
            tool_name: "t".into(),
            reason: "cancelled".into(),
            duration_ms: Some(10),
            arguments: String::new(),
            arguments_json: None,
        },
        EventPayload::CompactionStarted,
        EventPayload::CompactionCompleted {
            messages_removed: 5,
        },
        EventPayload::CompactionSkipped { reason: "r".into() },
        EventPayload::CompactionFailed { reason: "r".into() },
        EventPayload::CompactBoundaryCreated {
            trigger: "manual".into(),
            pre_tokens: 100,
            post_tokens: 20,
            summary: "s".into(),
            transcript_path: None,
            continued_session_id: "c".into(),
            base_event_seq: 0,
            strategy: crate::extension::CompactStrategy::Manual {
                keep_recent_turns: None,
            },
        },
        EventPayload::SessionContinuedFromCompaction {
            parent_session_id: "p".into(),
            parent_cursor: "0".into(),
            summary: "s".into(),
            transcript_path: None,
            context_messages: vec![],
            retained_messages: vec![],
        },
        EventPayload::SessionForked {
            source_session_id: "s".into(),
            source_cursor: "0".into(),
            context_messages: vec![],
            retained_messages: vec![],
        },
        EventPayload::ErrorOccurred {
            code: 1,
            message: "m".into(),
            recoverable: false,
        },
        EventPayload::Custom {
            name: "n".into(),
            data: serde_json::json!({}),
        },
        EventPayload::ExtensionEvent {
            extension_id: "e".into(),
            event_type: "t".into(),
            schema_version: 1,
            payload: serde_json::json!({}),
        },
    ];

    for payload in samples {
        let event = Event {
            seq: Some(1),
            id: "event-1".into(),
            session_id: "session-1".into(),
            turn_id: None,
            timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            payload,
        };
        let value = serde_json::to_value(event).unwrap();
        let obj = value.as_object().expect("Event -> JSON object");
        for key in obj.keys() {
            assert!(
                EVENT_ENVELOPE_KEYS.contains(&key.as_str()),
                "EventPayload field `{key}` leaked into the Event envelope namespace"
            );
        }
        assert!(value["payload"].get("type").is_some());
    }
}

#[test]
fn event_round_trips_nested_layout() {
    let event = Event {
        seq: Some(7),
        id: "event-1".into(),
        session_id: "session-1".into(),
        turn_id: Some("turn-1".into()),
        timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        payload: EventPayload::UserMessage {
            message_id: "m1".into(),
            text: "hello".into(),
            attachments: vec![],
        },
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["seq"], 7);
    assert_eq!(value["id"], "event-1");
    assert_eq!(value["session_id"], "session-1");
    assert_eq!(value["turn_id"], "turn-1");
    assert!(value.get("timestamp").is_some());
    assert_eq!(value["payload"]["type"], "user_message");
    assert_eq!(value["payload"]["message_id"], "m1");
    assert_eq!(value["payload"]["text"], "hello");

    let round_trip: Event = serde_json::from_value(value).unwrap();
    assert_eq!(round_trip, event);
}

#[test]
fn event_serialize_omits_none_optional_envelope_fields() {
    let event = Event {
        seq: None,
        id: "event-1".into(),
        session_id: "session-1".into(),
        turn_id: None,
        timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        payload: EventPayload::SessionDeleted,
    };
    let value = serde_json::to_value(&event).unwrap();
    assert!(value.get("seq").is_none());
    assert!(value.get("turn_id").is_none());
    assert_eq!(value["payload"]["type"], "session_deleted");
    assert_eq!(value["id"], "event-1");
    assert!(value.get("type").is_none());
}
