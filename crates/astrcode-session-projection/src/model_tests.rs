use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};

use super::*;

#[test]
fn session_read_model_serializes_round_trip() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.working_dir = "D:/work/project".into();
    model.model_id = "mock-model".into();
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("hello"),
        updated_seq: 1,
        source: None,
    });
    model.context_messages.push(SequencedLlmMessage {
        message: LlmMessage::system("system"),
        updated_seq: 1,
        source: None,
    });
    model.latest_seq = Some(7);

    let encoded = serde_json::to_string(&model).unwrap();
    let decoded: SessionReadModel = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, model);
}

#[test]
fn session_read_model_cursor_defaults_to_zero() {
    let model = SessionReadModel::empty("session-test".into());

    assert_eq!(model.cursor(), "0");
}

#[test]
fn first_user_message_ignores_source_marked_context() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: astrcode_core::llm::turn_aborted_context_message(),
        updated_seq: 1,
        source: Some(astrcode_core::llm::TURN_ABORTED_SOURCE.into()),
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("hello"),
        updated_seq: 2,
        source: None,
    });

    assert_eq!(model.first_user_message().as_deref(), Some("hello"));
}

#[test]
fn session_summary_from_ref_matches_consuming_conversion() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.working_dir = "D:/work/project".into();
    model.model_id = "mock".into();
    model.latest_seq = Some(7);
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("hello"),
        updated_seq: 1,
        source: None,
    });

    assert_eq!(model.to_summary(), SessionSummary::from(model.clone()));
}

#[test]
fn tool_calls_needing_interruption_uses_pending_and_tail_fallback() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look"),
        updated_seq: 1,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 2,
        source: None,
    });

    assert_eq!(
        model.tool_calls_needing_interruption(),
        vec![UnansweredToolCall {
            call_id: "call_1".into(),
            tool_name: "read".into(),
        }]
    );

    model.pending_tool_calls.insert("call_1".into());
    assert_eq!(model.tool_calls_needing_interruption().len(), 1);
}

#[test]
fn provider_messages_merges_consecutive_tool_call_assistant_messages() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look at these files"),
        updated_seq: 1,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 2,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_2".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "b.rs"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 3,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: "call_1".into(),
                content: "file a".into(),
                is_error: false,
            }],
            name: Some("read".into()),
            reasoning_content: None,
        },
        updated_seq: 4,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: "call_2".into(),
                content: "file b".into(),
                is_error: false,
            }],
            name: Some("read".into()),
            reasoning_content: None,
        },
        updated_seq: 5,
        source: None,
    });

    let messages = model.provider_messages();

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, LlmRole::User);
    assert_eq!(messages[1].role, LlmRole::Assistant);
    let tool_calls: Vec<_> = messages[1]
        .content
        .iter()
        .filter(|c| matches!(c, LlmContent::ToolCall { .. }))
        .collect();
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(messages[2].role, LlmRole::Tool);
    assert_eq!(messages[3].role, LlmRole::Tool);
}

#[test]
fn provider_messages_merges_reasoning_assistant_with_tool_calls() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look at this"),
        updated_seq: 1,
        source: None,
    });
    let mut thinking = LlmMessage::assistant("checking");
    thinking.reasoning_content = Some("private reasoning".into());
    model.messages.push(SequencedLlmMessage {
        message: thinking,
        updated_seq: 2,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 3,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: "call_1".into(),
                content: "file content".into(),
                is_error: false,
            }],
            name: Some("read".into()),
            reasoning_content: None,
        },
        updated_seq: 4,
        source: None,
    });

    let messages = model.provider_messages();

    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].role, LlmRole::Assistant);
    assert_eq!(
        messages[1].reasoning_content.as_deref(),
        Some("private reasoning")
    );
    assert!(matches!(
        &messages[1].content[0],
        LlmContent::Text { text } if text == "checking"
    ));
    assert!(
        messages[1]
            .content
            .iter()
            .any(|content| matches!(content, LlmContent::ToolCall { .. }))
    );
    assert_eq!(messages[2].role, LlmRole::Tool);
}

#[test]
fn provider_messages_preserve_reasoning_content() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("hello"),
        updated_seq: 1,
        source: None,
    });

    let mut reasoning_only = LlmMessage::assistant("");
    reasoning_only.reasoning_content = Some("private reasoning".into());
    model.messages.push(SequencedLlmMessage {
        message: reasoning_only,
        updated_seq: 2,
        source: None,
    });

    let mut visible_answer = LlmMessage::assistant("answer");
    visible_answer.reasoning_content = Some("more reasoning".into());
    model.messages.push(SequencedLlmMessage {
        message: visible_answer,
        updated_seq: 3,
        source: None,
    });

    let messages = model.provider_messages();

    // reasoning_content must be preserved for providers like DeepSeek
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, LlmRole::User);
    assert_eq!(messages[1].role, LlmRole::Assistant);
    assert_eq!(
        messages[1].reasoning_content,
        Some("private reasoning".into())
    );
    assert_eq!(messages[2].role, LlmRole::Assistant);
    assert_eq!(messages[2].reasoning_content, Some("more reasoning".into()));
    assert!(matches!(
        &messages[2].content[0],
        LlmContent::Text { text } if text == "answer"
    ));
}

#[test]
fn provider_messages_truncates_unanswered_tool_calls() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look at this"),
        updated_seq: 1,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 2,
        source: None,
    });
    // no tool result for call_1

    let messages = model.provider_messages();

    // The unanswered tool call round is truncated
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, LlmRole::User);
}

#[test]
fn provider_messages_truncates_partially_answered_tool_calls() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look"),
        updated_seq: 1,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![
                LlmContent::ToolCall {
                    call_id: "call_1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.rs"}),
                    raw_arguments: None,
                },
                LlmContent::ToolCall {
                    call_id: "call_2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "b.rs"}),
                    raw_arguments: None,
                },
            ],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 2,
        source: None,
    });
    // only call_1 has a result, call_2 is unanswered
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: "call_1".into(),
                content: "file a".into(),
                is_error: false,
            }],
            name: Some("read".into()),
            reasoning_content: None,
        },
        updated_seq: 3,
        source: None,
    });

    let messages = model.provider_messages();

    // The partially answered round is truncated entirely
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, LlmRole::User);
}

#[test]
fn provider_messages_truncates_orphan_tool_result() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look"),
        updated_seq: 1,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::assistant("previous complete answer"),
        updated_seq: 2,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: "call_orphan".into(),
                content: "orphan result".into(),
                is_error: false,
            }],
            name: Some("read".into()),
            reasoning_content: None,
        },
        updated_seq: 3,
        source: None,
    });

    let messages = model.provider_messages();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, LlmRole::User);
    assert_eq!(messages[1].role, LlmRole::Assistant);
}

#[test]
fn provider_messages_truncates_non_tool_after_pending_tool_calls() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look"),
        updated_seq: 1,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 2,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::assistant("late text after aborted tool call"),
        updated_seq: 3,
        source: None,
    });

    let messages = model.provider_messages();

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, LlmRole::User);
}

#[test]
fn provider_messages_keeps_fully_answered_tool_calls() {
    let mut model = SessionReadModel::empty("session-test".into());
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::user("look"),
        updated_seq: 1,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
                raw_arguments: None,
            }],
            name: None,
            reasoning_content: None,
        },
        updated_seq: 2,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: "call_1".into(),
                content: "file a".into(),
                is_error: false,
            }],
            name: Some("read".into()),
            reasoning_content: None,
        },
        updated_seq: 3,
        source: None,
    });
    model.messages.push(SequencedLlmMessage {
        message: LlmMessage::assistant("done"),
        updated_seq: 4,
        source: None,
    });

    let messages = model.provider_messages();

    // All tool calls have results, nothing truncated
    assert_eq!(messages.len(), 4);
}
