use super::*;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationReducerFixture {
    initial_blocks: Vec<ConversationBlockDto>,
    envelopes: Vec<ConversationStreamEnvelopeDto>,
    expected: serde_json::Value,
}

#[test]
fn conversation_stream_fixture_matches_wire_contract() {
    let fixture = include_str!("../../fixtures/conversation-stream.json");
    let envelopes: Vec<ConversationStreamEnvelopeDto> =
        serde_json::from_str(fixture).expect("fixture should deserialize");

    assert_eq!(envelopes.len(), 5);

    match &envelopes[0].delta {
        ConversationDeltaDto::PatchBlock {
            block_id,
            text_delta,
        } => {
            assert_eq!(block_id, "assistant-1");
            assert_eq!(text_delta, "hello");
        },
        other => panic!("unexpected fixture delta: {other:?}"),
    }

    match &envelopes[1].delta {
        ConversationDeltaDto::FinalizeBlock {
            block:
                ConversationBlockDto::Assistant {
                    id,
                    text,
                    reasoning_content: _,
                    storage_seq,
                    status,
                },
        } => {
            assert_eq!(id, "assistant-1");
            assert_eq!(text, "complete answer");
            assert_eq!(*storage_seq, Some(3));
            assert!(matches!(status, ConversationBlockStatusDto::Complete));
        },
        other => panic!("unexpected fixture delta: {other:?}"),
    }

    match &envelopes[4].delta {
        ConversationDeltaDto::PatchArguments {
            block_id,
            arguments,
            arguments_json,
        } => {
            assert_eq!(block_id, "tool-1");
            assert_eq!(arguments, "Cargo.toml");
            assert!(arguments_json.is_none());
        },
        other => panic!("unexpected fixture delta: {other:?}"),
    }

    let encoded = serde_json::to_string(&envelopes[0]).expect("fixture should serialize");
    assert!(encoded.contains("\"blockId\""));
    assert!(encoded.contains("\"textDelta\""));
    assert!(!encoded.contains("block_id"));
    assert!(!encoded.contains("text_delta"));
}

#[test]
fn conversation_reducer_fixture_matches_wire_contract() {
    let fixture = include_str!("../../fixtures/conversation-reducer.json");
    let fixture: ConversationReducerFixture =
        serde_json::from_str(fixture).expect("reducer fixture should deserialize");

    assert_eq!(fixture.initial_blocks.len(), 2);
    assert_eq!(fixture.envelopes.len(), 13);
    assert_eq!(
        fixture
            .envelopes
            .last()
            .map(|envelope| envelope.cursor.value.as_str()),
        Some("14")
    );
    assert!(fixture.expected.get("blocks").is_some());

    let encoded =
        serde_json::to_value(&fixture.envelopes).expect("fixture envelopes should serialize");
    assert_eq!(
        encoded[0]["delta"]["kind"],
        serde_json::Value::String("patchBlock".into())
    );
    assert_eq!(
        encoded[8]["delta"]["block"]["argumentsJson"]["path"],
        serde_json::Value::String("Cargo.toml".into())
    );
}

#[test]
fn thinking_delta_uses_block_id_wire_name() {
    let delta = ConversationDeltaDto::ThinkingDelta {
        block_id: "assistant-1".into(),
        delta: "reasoning".into(),
    };

    let encoded = serde_json::to_string(&delta).unwrap();
    assert!(encoded.contains("\"blockId\""));
    assert!(!encoded.contains("block_id"));

    let decoded: ConversationDeltaDto = serde_json::from_str(&encoded).unwrap();
    match decoded {
        ConversationDeltaDto::ThinkingDelta { block_id, delta } => {
            assert_eq!(block_id, "assistant-1");
            assert_eq!(delta, "reasoning");
        },
        other => panic!("unexpected delta: {other:?}"),
    }
}

#[test]
fn tool_definition_dto_round_trips_and_requires_execution_contract() {
    let dto: ToolDefinitionDto = astrcode_core::tool::ToolDefinition {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object"}),
        strict: false,
        origin: astrcode_core::tool::ToolOrigin::Bundled,
    }
    .into();

    assert_eq!(
        serde_json::to_value(dto).unwrap(),
        serde_json::json!({
            "name": "read",
            "description": "Read a file",
            "parameters": {"type": "object"},
            "strict": false,
            "origin": "bundled"
        })
    );

    let incomplete = serde_json::json!({
        "name": "incomplete",
        "description": "",
        "parameters": {"type": "object"},
        "origin": "extension"
    });
    assert!(serde_json::from_value::<ToolDefinitionDto>(incomplete).is_err());

    let strict: ToolDefinitionDto = serde_json::from_value(serde_json::json!({
        "name": "strict",
        "description": "",
        "parameters": {"type": "object"},
        "strict": true,
        "origin": "extension"
    }))
    .unwrap();
    assert!(strict.strict);
}
