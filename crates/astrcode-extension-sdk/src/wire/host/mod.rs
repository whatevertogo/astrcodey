//! Host-side wire DTOs: request/response contracts are split into submodules by domain and
//! re-exported here.

use serde::{Deserialize, Serialize};

pub(crate) fn deserialize_non_empty_string<'de, D>(
    deserializer: D,
    field: &'static str,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        Err(serde::de::Error::custom(format_args!(
            "{field} must not be empty"
        )))
    } else {
        Ok(value)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EmptyRequest {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgement {
    pub ok: bool,
}

fn serialize_bounded_utf8<S>(
    value: &str,
    max_bytes: usize,
    field: &str,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if value.len() > max_bytes {
        return Err(serde::ser::Error::custom(format_args!(
            "{field} exceeds {max_bytes} UTF-8 bytes"
        )));
    }
    serializer.serialize_str(value)
}

fn deserialize_bounded_utf8<'de, D>(
    deserializer: D,
    max_bytes: usize,
    field: &str,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() > max_bytes {
        Err(serde::de::Error::custom(format_args!(
            "{field} exceeds {max_bytes} UTF-8 bytes"
        )))
    } else {
        Ok(value)
    }
}

fn deserialize_optional_bounded_utf8<'de, D>(
    deserializer: D,
    max_bytes: usize,
    field: &str,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value {
        Some(value) if value.len() > max_bytes => Err(serde::de::Error::custom(format_args!(
            "{field} exceeds {max_bytes} UTF-8 bytes"
        ))),
        value => Ok(value),
    }
}

fn deserialize_bounded_usize<'de, D>(
    deserializer: D,
    min: usize,
    max: usize,
    field: &str,
) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(format_args!(
            "{field} must be between {min} and {max}"
        )))
    }
}

fn deserialize_optional_bounded_usize<'de, D>(
    deserializer: D,
    min: usize,
    max: usize,
    field: &str,
) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<usize>::deserialize(deserializer)?;
    match value {
        Some(value) if !(min..=max).contains(&value) => Err(serde::de::Error::custom(
            format_args!("{field} must be between {min} and {max}"),
        )),
        value => Ok(value),
    }
}

macro_rules! bounded_utf8_serde_fns {
    ($serialize:ident, $deserialize:ident,String, $max:expr, $field:literal) => {
        fn $serialize<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serialize_bounded_utf8(value, $max, $field, serializer)
        }

        fn $deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_bounded_utf8(deserializer, $max, $field)
        }
    };
    ($serialize:ident, $deserialize:ident,Option < String > , $max:expr, $field:literal) => {
        fn $serialize<S>(value: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            match value {
                Some(value) => serialize_bounded_utf8(value, $max, $field, serializer),
                None => serializer.serialize_none(),
            }
        }

        fn $deserialize<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_bounded_utf8(deserializer, $max, $field)
        }
    };
}

macro_rules! bounded_usize_deserializer {
    ($deserialize:ident,usize, $max:expr, $field:literal) => {
        fn $deserialize<'de, D>(deserializer: D) -> Result<usize, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_bounded_usize(deserializer, 1, $max, $field)
        }
    };
    ($deserialize:ident,Option < usize > , $max:expr, $field:literal) => {
        fn $deserialize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_optional_bounded_usize(deserializer, 1, $max, $field)
        }
    };
}
pub mod event;
pub mod llm;
pub mod network;
pub mod process;
pub mod session;
pub mod session_state;
pub mod tool_result;
pub mod workspace;

pub use event::{HostEventEmitOutput, HostEventEmitRequest};
pub use llm::{HostLlmChatOutput, HostLlmChatRequest, HostLlmContent, HostLlmMessage, HostLlmRole};
pub use network::{
    HOST_NETWORK_DEFAULT_TIMEOUT_MS, HOST_NETWORK_MAX_BYTES, HOST_NETWORK_MAX_REQUEST_BODY_BYTES,
    HOST_NETWORK_MAX_TIMEOUT_MS, HostNetworkRedirectPolicy, HostNetworkRequest,
    HostNetworkResponse,
};
pub use process::{
    HOST_PROCESS_DEFAULT_TIMEOUT_MS, HOST_PROCESS_MAX_STDIN_BYTES, HOST_PROCESS_MAX_TIMEOUT_MS,
    HOST_PROCESS_MAX_WAIT_MS, HostProcessHandleOutput, HostProcessInputAction,
    HostProcessInputRequest, HostProcessLifetime, HostProcessListOutput, HostProcessOutput,
    HostProcessReadOutput, HostProcessReadRequest, HostProcessRequest, HostProcessStartRequest,
    HostProcessState, HostProcessStatusOutput, HostProcessTargetRequest,
};
pub use session::{
    HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest, HostSessionCancelOutput,
    HostSessionDeliveryOutput, HostSessionExecutionView, HostSessionInputRequest,
    HostSessionProviderMessagesOutput, HostSessionSummariesOutput, HostSessionSummary,
    HostSessionTokenUsage, HostSessionTokenUsageOutput, HostSessionTranscript,
    HostSessionTranscriptMessage,
};
pub use session_state::{
    HOST_SESSION_STATE_KEY_MAX_LENGTH, HOST_SESSION_STATE_VALUE_MAX_BYTES,
    HostSessionStateReadOutput, HostSessionStateReadRequest, HostSessionStateWriteRequest,
};
pub use tool_result::{
    HOST_TOOL_RESULT_DEFAULT_MAX_BYTES, HOST_TOOL_RESULT_MAX_BYTES, HOST_TOOL_RESULT_MIN_BYTES,
    HostToolResultReadOutput, HostToolResultReadRequest,
};
pub use workspace::{
    HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES, HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES,
    HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS, HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES,
    HOST_WORKSPACE_LIST_DEFAULT_DEPTH, HOST_WORKSPACE_LIST_DEFAULT_LIMIT,
    HOST_WORKSPACE_LIST_MAX_DEPTH, HOST_WORKSPACE_LIST_MAX_ENTRIES, HOST_WORKSPACE_MAX_DIFF_BYTES,
    HOST_WORKSPACE_MAX_FILE_BYTES, HOST_WORKSPACE_MAX_IMAGE_BYTES, HOST_WORKSPACE_MAX_LINE_LIMIT,
    HOST_WORKSPACE_MAX_PATCH_BYTES, HOST_WORKSPACE_MAX_TEXT_OUTPUT_BYTES,
    HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES, HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS,
    HOST_WORKSPACE_SEARCH_MAX_MATCHES, HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES,
    HostWorkspaceApplyPatchOutput, HostWorkspaceApplyPatchRequest, HostWorkspaceEditOutput,
    HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
    HostWorkspaceGrepContextLine, HostWorkspaceGrepEntry, HostWorkspaceGrepMode,
    HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListEntry,
    HostWorkspaceListOutput, HostWorkspaceListRequest, HostWorkspacePatchChange,
    HostWorkspacePatchChangeKind, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
    HostWorkspaceTextChange, HostWorkspaceTextEdit, HostWorkspaceWriteOutput,
    HostWorkspaceWriteRequest,
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base64::engine::general_purpose::STANDARD;
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, json};

    use super::*;
    use crate::wire::session::{SessionMessageOriginDto, SessionPhaseDto, SessionToolSelectionDto};

    fn assert_strict_round_trip<T>(contract: T)
    where
        T: Serialize + DeserializeOwned,
    {
        let type_name = std::any::type_name::<T>();
        let wire = serde_json::to_value(&contract)
            .unwrap_or_else(|error| panic!("failed to serialize {type_name}: {error}"));
        let decoded = serde_json::from_value::<T>(wire.clone())
            .unwrap_or_else(|error| panic!("failed to deserialize {type_name}: {error}"));
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            wire,
            "{type_name} did not round-trip"
        );

        let mut unknown_field = wire;
        let Value::Object(fields) = &mut unknown_field else {
            panic!("{type_name} must serialize as an object");
        };
        fields.insert("unexpected".into(), json!(true));
        assert!(
            serde_json::from_value::<T>(unknown_field).is_err(),
            "{type_name} accepted an unknown top-level field"
        );
    }

    macro_rules! assert_contracts {
        ($($contract:expr),+ $(,)?) => {
            $(assert_strict_round_trip($contract);)+
        };
    }

    #[test]
    fn host_client_contracts_are_strict_and_round_trip() {
        let session_id = || "session-1".to_owned();
        let user_message = || HostLlmMessage {
            role: HostLlmRole::User,
            content: vec![HostLlmContent::Text {
                text: "hello".into(),
            }],
            name: None,
            reasoning_content: None,
        };

        assert_contracts!(
            HostEventEmitRequest {
                event_type: "review.completed".into(),
                schema_version: 1,
                payload: json!({ "status": "ok" }),
            },
            HostEventEmitOutput::Persisted {
                event_id: "event-1".into(),
                seq: 7,
            },
            HostEventEmitOutput::Accepted,
            HostSessionStateReadRequest { key: "goal".into() },
            HostSessionStateReadOutput {
                content: Some("active".into()),
            },
            HostSessionStateReadOutput { content: None },
            HostSessionStateWriteRequest {
                key: "goal".into(),
                content: "active".into(),
            },
            HostLlmChatRequest::new(vec![user_message()]).with_max_output_tokens(512),
            HostLlmChatOutput {
                content: "hello".into(),
                model: "main".into(),
            },
            HostSessionSummariesOutput {
                sessions: vec![HostSessionSummary {
                    session_id: session_id(),
                    parent_session_id: None,
                    source_extension: Some("example".into()),
                    working_dir: "/workspace".into(),
                    model_id: "main".into(),
                    created_at: "2026-08-06T00:00:00Z".into(),
                    updated_at: "2026-08-06T00:00:01Z".into(),
                    latest_cursor: "7".into(),
                }],
            },
            HostSessionTranscript {
                session_id: session_id(),
                messages: vec![HostSessionTranscriptMessage {
                    message: user_message(),
                    origin: Some(SessionMessageOriginDto::TurnAborted),
                }],
            },
            HostSessionProviderMessagesOutput {
                session_id: session_id(),
                messages: vec![user_message()],
            },
            HostSessionTokenUsageOutput {
                usage: Some(HostSessionTokenUsage {
                    total_tokens: 42,
                    model_context_window: Some(128_000),
                }),
            },
            HostProcessRequest {
                command: "printf".into(),
                args: vec!["hello".into()],
                cwd: Some(".".into()),
                stdin: Some("input".into()),
                timeout_ms: Some(1_000),
            },
            HostProcessOutput {
                status: Some(0),
                success: true,
                stdout: "hello".into(),
                stderr: String::new(),
                combined: "hello".into(),
                stdout_truncated: false,
                stderr_truncated: false,
                combined_truncated: false,
            },
            HostProcessStartRequest::new("cat"),
            HostProcessHandleOutput {
                id: "process-1".into(),
            },
            HostProcessReadRequest {
                id: "process-1".into(),
                wait_ms: Some(1_000),
            },
            HostProcessReadOutput {
                id: "process-1".into(),
                stdout: "hello".into(),
                stderr: String::new(),
                combined: "hello".into(),
                dropped_bytes: 0,
                state: HostProcessState::Exited { status: Some(0) },
            },
            HostProcessStatusOutput {
                id: "process-1".into(),
                state: HostProcessState::Running {},
            },
            HostProcessInputRequest::write("process-1", "hello"),
            HostProcessInputRequest::close("process-1"),
            HostToolResultReadRequest {
                artifact_id: format!("result-{}.txt", "a".repeat(64)),
                byte_offset: 10,
                max_bytes: 2_000,
            },
            HostToolResultReadOutput {
                artifact_id: format!("result-{}.txt", "a".repeat(64)),
                bytes: 42,
                byte_offset: 10,
                returned_bytes: 20,
                next_byte_offset: Some(30),
                has_more: true,
                content: "output".into(),
            },
            HostNetworkRequest {
                url: "https://example.com".into(),
                method: "POST".into(),
                headers: BTreeMap::from([("content-type".into(), "text/plain".into())]),
                body: b"hello".to_vec(),
                max_bytes: 4_096,
                timeout_ms: 1_000,
                redirect_policy: HostNetworkRedirectPolicy::Manual,
            },
            HostNetworkResponse {
                final_url: "https://example.com/final".into(),
                status: 200,
                headers: BTreeMap::from([("content-type".into(), "text/plain".into())]),
                body: b"hello".to_vec(),
            },
            HostWorkspaceReadRequest {
                path: "notes.txt".into(),
                max_bytes: Some(4_096),
                line_offset: 0,
                line_limit: Some(20),
            },
            HostWorkspaceReadOutput::Text {
                content: "hello".into(),
                bytes: 5,
                total_lines: 1,
                line_offset: 0,
                returned_lines: 1,
                has_more_lines: false,
            },
            HostWorkspaceWriteRequest {
                path: "notes.txt".into(),
                content: "hello".into(),
                create_dirs: false,
            },
            HostWorkspaceWriteOutput {
                path: "notes.txt".into(),
                created: false,
                change: HostWorkspaceTextChange {
                    old_bytes: Some(4),
                    new_bytes: 5,
                    unified_diff: Some("--- notes.txt\n+++ notes.txt\n".into()),
                    insertions: 1,
                    deletions: 1,
                    diff_truncated: false,
                },
            },
            HostWorkspaceEditRequest {
                path: "notes.txt".into(),
                old_text: Some("hello".into()),
                new_text: Some("hi".into()),
                replace_all: true,
                edits: Vec::new(),
            },
            HostWorkspaceEditOutput {
                path: "notes.txt".into(),
                operation_count: 1,
                replacements: 1,
                change: HostWorkspaceTextChange {
                    old_bytes: Some(5),
                    new_bytes: 2,
                    unified_diff: Some("--- notes.txt\n+++ notes.txt\n".into()),
                    insertions: 1,
                    deletions: 1,
                    diff_truncated: false,
                },
            },
            HostWorkspaceListRequest {
                path: ".".into(),
                depth: 2,
                limit: Some(20),
            },
            HostWorkspaceListOutput {
                path: ".".into(),
                entries: vec![HostWorkspaceListEntry {
                    name: "notes.txt".into(),
                    path: "notes.txt".into(),
                    kind: "file".into(),
                    bytes: Some(5),
                }],
                returned_entries: 1,
                truncated: false,
            },
            HostWorkspaceGrepRequest {
                pattern: "hello".into(),
                path: Some("notes.txt".into()),
                offset: 0,
                max_matches: Some(10),
                max_bytes: Some(4_096),
                max_line_chars: Some(200),
                recursive: true,
                multiline: false,
                path_filters: vec!["**/*.txt".into()],
                before_context: 1,
                after_context: 1,
                mode: HostWorkspaceGrepMode::Content,
            },
            HostWorkspaceGrepOutput {
                pattern: "hello".into(),
                root: ".".into(),
                entries: vec![HostWorkspaceGrepEntry::Content {
                    path: "notes.txt".into(),
                    line_number: 1,
                    line: "hello".into(),
                    line_truncated: false,
                    before_context: Vec::new(),
                    after_context: Vec::new(),
                }],
                has_more: false,
                scan_truncated: false,
                skipped_files: 2,
            },
            HostWorkspaceGlobRequest {
                pattern: "**/*.txt".into(),
                root: Some(".".into()),
                offset: 0,
                max_matches: Some(10),
                respect_gitignore: false,
                include_hidden: true,
                include_directories: true,
            },
            HostWorkspaceGlobOutput {
                pattern: "**/*.txt".into(),
                root: ".".into(),
                paths: vec!["notes.txt".into()],
                total_matches: Some(1),
                has_more: false,
                scan_truncated: false,
            },
            HostSessionInputRequest {
                target_session_id: "session-1".into(),
                content: "continue".into(),
            },
            HostSessionDeliveryOutput::Started {
                turn_id: "turn-1".into(),
            },
            HostSessionDeliveryOutput::Injected {
                turn_id: "turn-1".into(),
            },
            HostSessionDeliveryOutput::Queued { queue_len: 2 },
            HostSessionCancelOutput { cancelled: true },
            HostSessionExecutionView {
                phase: SessionPhaseDto::Streaming,
                active_turn_id: Some("turn-1".into()),
                queued_inputs: 2,
            },
            HostConfigureSessionToolsRequest {
                session_id: "session-1".into(),
                selection: SessionToolSelectionDto::no_tools(),
            },
            HostConfigureSessionToolsOutput {
                selection: SessionToolSelectionDto::no_tools(),
            },
        );
    }

    #[test]
    fn context_contracts_reject_invalid_shapes() {
        assert!(
            serde_json::from_value::<HostProcessState>(json!({
                "kind": "running",
                "status": 0
            }))
            .is_err()
        );
        for value in [
            json!({ "event_type": "review.completed", "payload": {} }),
            json!({ "event_type": "review.completed", "schema_version": 1 }),
            json!({ "event_type": "", "schema_version": 1, "payload": {} }),
            json!({ "event_type": "review.completed", "schema_version": 0, "payload": {} }),
            json!({
                "event_type": "review.completed",
                "schema_version": u64::from(u32::MAX) + 1,
                "payload": {}
            }),
            json!({
                "event_type": "review.completed",
                "schema_version": 1,
                "payload": {},
                "extra": true
            }),
        ] {
            assert!(
                serde_json::from_value::<HostEventEmitRequest>(value.clone()).is_err(),
                "accepted invalid event emit request: {value}"
            );
        }

        for value in [
            json!({}),
            json!({ "key": "goal", "content": "active", "extra": true }),
        ] {
            assert!(
                serde_json::from_value::<HostSessionStateWriteRequest>(value.clone()).is_err(),
                "accepted invalid session state write: {value}"
            );
        }

        let invalid_keys = [
            String::new(),
            ".".into(),
            "..".into(),
            "nested/key".into(),
            "x".repeat(HOST_SESSION_STATE_KEY_MAX_LENGTH + 1),
        ];
        for key in invalid_keys {
            assert!(
                serde_json::from_value::<HostSessionStateReadRequest>(json!({ "key": key }))
                    .is_err(),
                "accepted invalid session state key"
            );
        }
    }

    #[test]
    fn workspace_request_bounds_are_enforced_by_serde() {
        assert!(
            serde_json::from_value::<HostWorkspaceReadRequest>(json!({
                "path": "notes.txt",
                "max_bytes": HOST_WORKSPACE_MAX_FILE_BYTES as u64 + 1
            }))
            .is_err()
        );
        for line_limit in [0, HOST_WORKSPACE_MAX_LINE_LIMIT + 1] {
            assert!(
                serde_json::from_value::<HostWorkspaceReadRequest>(json!({
                    "path": "notes.txt",
                    "line_limit": line_limit
                }))
                .is_err()
            );
        }
        for max_bytes in [0, HOST_WORKSPACE_MAX_FILE_BYTES as u64] {
            let request: HostWorkspaceReadRequest = serde_json::from_value(json!({
                "path": "notes.txt",
                "max_bytes": max_bytes
            }))
            .unwrap();
            assert_eq!(request.max_bytes, Some(max_bytes));
        }

        for value in [
            json!({ "path": ".", "depth": 0 }),
            json!({ "path": ".", "depth": HOST_WORKSPACE_LIST_MAX_DEPTH + 1 }),
            json!({ "path": ".", "limit": 0 }),
            json!({ "path": ".", "limit": HOST_WORKSPACE_LIST_MAX_ENTRIES + 1 }),
        ] {
            assert!(
                serde_json::from_value::<HostWorkspaceListRequest>(value.clone()).is_err(),
                "accepted invalid workspace list request: {value}"
            );
        }
        for value in [
            json!({ "pattern": "x", "max_matches": 0 }),
            json!({
                "pattern": "x",
                "max_matches": HOST_WORKSPACE_SEARCH_MAX_MATCHES + 1
            }),
            json!({ "pattern": "x", "max_bytes": 0 }),
            json!({
                "pattern": "x",
                "max_bytes": HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES + 1
            }),
            json!({ "pattern": "x", "max_line_chars": 0 }),
            json!({
                "pattern": "x",
                "max_line_chars": HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS + 1
            }),
            json!({
                "pattern": "x",
                "before_context": HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES + 1
            }),
            json!({
                "pattern": "x",
                "after_context": HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES + 1
            }),
        ] {
            assert!(
                serde_json::from_value::<HostWorkspaceGrepRequest>(value.clone()).is_err(),
                "accepted invalid workspace grep request: {value}"
            );
        }
        for value in [
            json!({ "pattern": "*.rs", "max_matches": 0 }),
            json!({
                "pattern": "*.rs",
                "max_matches": HOST_WORKSPACE_SEARCH_MAX_MATCHES + 1
            }),
        ] {
            assert!(
                serde_json::from_value::<HostWorkspaceGlobRequest>(value.clone()).is_err(),
                "accepted invalid workspace glob request: {value}"
            );
        }

        let list: HostWorkspaceListRequest = serde_json::from_value(json!({
            "path": ".",
            "depth": HOST_WORKSPACE_LIST_MAX_DEPTH,
            "limit": HOST_WORKSPACE_LIST_MAX_ENTRIES
        }))
        .unwrap();
        assert_eq!(list.depth, HOST_WORKSPACE_LIST_MAX_DEPTH);
        assert_eq!(list.limit, Some(HOST_WORKSPACE_LIST_MAX_ENTRIES));
        let grep: HostWorkspaceGrepRequest = serde_json::from_value(json!({
            "pattern": "x",
            "max_matches": HOST_WORKSPACE_SEARCH_MAX_MATCHES,
            "max_bytes": HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES,
            "max_line_chars": HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS
        }))
        .unwrap();
        assert_eq!(grep.max_matches, Some(HOST_WORKSPACE_SEARCH_MAX_MATCHES));
        assert_eq!(grep.max_bytes, Some(HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES));
        assert_eq!(
            grep.max_line_chars,
            Some(HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS)
        );
        let glob: HostWorkspaceGlobRequest = serde_json::from_value(json!({
            "pattern": "*.rs",
            "max_matches": null
        }))
        .unwrap();
        assert_eq!(glob.max_matches, None);
    }

    #[test]
    fn bounded_payload_contracts_enforce_byte_limits() {
        let mut process = HostProcessRequest::new("printf");
        process.stdin = Some("x".repeat(HOST_PROCESS_MAX_STDIN_BYTES));
        assert!(serde_json::to_value(&process).is_ok());
        process.stdin = Some("x".repeat(HOST_PROCESS_MAX_STDIN_BYTES + 1));
        assert!(serde_json::to_value(&process).is_err());
        for stdin in [
            "x".repeat(HOST_PROCESS_MAX_STDIN_BYTES + 1),
            "é".repeat(HOST_PROCESS_MAX_STDIN_BYTES / 2 + 1),
        ] {
            assert!(
                serde_json::from_value::<HostProcessRequest>(json!({
                    "command": "printf",
                    "stdin": stdin
                }))
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<HostProcessStartRequest>(json!({
                "command": "cat",
                "stdin": "legacy"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<HostProcessInputRequest>(json!({
                "id": "process-1",
                "action": {
                    "kind": "write",
                    "input": "x".repeat(HOST_PROCESS_MAX_STDIN_BYTES + 1)
                }
            }))
            .is_err()
        );
        for max_bytes in [
            0,
            HOST_TOOL_RESULT_MIN_BYTES - 1,
            HOST_TOOL_RESULT_MAX_BYTES + 1,
        ] {
            assert!(
                serde_json::from_value::<HostToolResultReadRequest>(json!({
                    "artifact_id": "shell-call-1.txt",
                    "max_bytes": max_bytes
                }))
                .is_err()
            );
        }

        let max_state_content = "x".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES);
        assert!(
            serde_json::from_value::<HostSessionStateWriteRequest>(json!({
                "key": "goal",
                "content": max_state_content
            }))
            .is_ok()
        );
        for content in [
            "x".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES + 1),
            "é".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES / 2 + 1),
        ] {
            assert!(
                serde_json::from_value::<HostSessionStateWriteRequest>(json!({
                    "key": "goal",
                    "content": content
                }))
                .is_err()
            );
        }
        assert!(
            serde_json::to_value(HostSessionStateWriteRequest {
                key: "goal".into(),
                content: "x".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES + 1),
            })
            .is_err()
        );

        let mut network = HostNetworkRequest::get("https://example.com");
        network.body = vec![0; HOST_NETWORK_MAX_REQUEST_BODY_BYTES];
        assert!(serde_json::to_value(&network).is_ok());
        network.body.push(0);
        assert!(serde_json::to_value(&network).is_err());

        let encoded_over_limit =
            base64::Engine::encode(&STANDARD, vec![0; HOST_NETWORK_MAX_REQUEST_BODY_BYTES + 1]);
        assert!(encoded_over_limit.len() <= network::HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS);
        assert!(
            serde_json::from_value::<HostNetworkRequest>(json!({
                "url": "https://example.com",
                "body": encoded_over_limit
            }))
            .is_err()
        );
    }

    #[test]
    fn host_llm_wire_contract_is_nested_strict() {
        let message = HostLlmMessage {
            role: HostLlmRole::Assistant,
            content: vec![
                HostLlmContent::Text {
                    text: "hello".into(),
                },
                HostLlmContent::Image {
                    base64: "aW1hZ2U=".into(),
                    media_type: "image/png".into(),
                    filename: Some("image.png".into()),
                },
                HostLlmContent::ToolCall {
                    call_id: "call-1".into(),
                    name: "read".into(),
                    arguments: json!({ "path": "notes.txt" }),
                    raw_arguments: Some("{path:notes.txt}".into()),
                },
                HostLlmContent::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "hello".into(),
                    is_error: false,
                },
            ],
            name: Some("assistant".into()),
            reasoning_content: Some("reasoning".into()),
        };
        let valid = serde_json::to_value(message).unwrap();
        let mut explicit_nulls = valid.clone();
        explicit_nulls["name"] = Value::Null;
        explicit_nulls["reasoning_content"] = Value::Null;
        explicit_nulls["content"][1]["filename"] = Value::Null;
        explicit_nulls["content"][2]["raw_arguments"] = Value::Null;
        assert!(serde_json::from_value::<HostLlmMessage>(explicit_nulls).is_ok());

        for pointer in ["", "/content/0", "/content/1", "/content/2", "/content/3"] {
            let mut invalid = valid.clone();
            let object = if pointer.is_empty() {
                invalid.as_object_mut()
            } else {
                invalid.pointer_mut(pointer).and_then(Value::as_object_mut)
            }
            .unwrap();
            object.insert("unexpected".into(), Value::Bool(true));
            assert!(
                serde_json::from_value::<HostLlmMessage>(invalid).is_err(),
                "HostLlmMessage accepted an unknown field at {pointer}"
            );
        }
    }

    #[test]
    fn session_delivery_rejects_invalid_tag_shapes() {
        let invalid = [
            json!({ "turn_id": "turn-1" }),
            json!({ "status": "unknown", "turn_id": "turn-1" }),
            json!({ "status": "started" }),
            json!({ "status": "injected" }),
            json!({ "status": "queued" }),
            json!({ "status": "started", "turn_id": "turn-1", "queue_len": 1 }),
            json!({ "status": "injected", "turn_id": "turn-1", "queue_len": 1 }),
            json!({ "status": "queued", "queue_len": 1, "turn_id": "turn-1" }),
        ];

        for value in invalid {
            assert!(
                serde_json::from_value::<HostSessionDeliveryOutput>(value.clone()).is_err(),
                "accepted invalid session delivery output: {value}"
            );
        }
    }
}
