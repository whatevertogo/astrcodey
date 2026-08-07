//! schemars 派生的共享辅助:派生入口,以及 derive 无法表达的字段级 schema 片段
//! (常量边界、base64 编码标记、`not` 约束、可空且必填等)。

use schemars::{JsonSchema, gen::SchemaGenerator, schema::Schema};
use serde_json::{Value, json};

use super::{
    HOST_NETWORK_MAX_BYTES, HOST_NETWORK_MAX_TIMEOUT_MS, HOST_PROCESS_MAX_STDIN_BYTES,
    HOST_PROCESS_MAX_TIMEOUT_MS, HOST_SESSION_STATE_KEY_MAX_LENGTH,
    HOST_SESSION_STATE_VALUE_MAX_BYTES, HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES,
    HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES, HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS,
    HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES, HOST_WORKSPACE_LIST_DEFAULT_DEPTH,
    HOST_WORKSPACE_LIST_DEFAULT_LIMIT, HOST_WORKSPACE_LIST_MAX_DEPTH,
    HOST_WORKSPACE_LIST_MAX_ENTRIES, HOST_WORKSPACE_MAX_FILE_BYTES,
    HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS, HOST_WORKSPACE_SEARCH_MAX_MATCHES,
    HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES, contracts::HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS,
};
use crate::session::SessionToolSelectionDto;

pub(crate) fn derived_wire_schema<T: JsonSchema>() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .expect("derived JSON schema is always serializable");
    normalize_numbers(&mut schema);
    schema
}

// schemars 0.8 的数值校验内部用 f64,序列化成 120000.0 这样的浮点字面量;线缆契约
// 沿用整数字面量(如 "maximum": 120000),整值浮点在边界统一收敛为整数。
fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let integral = match number.as_f64() {
                Some(float) if float.fract() == 0.0 && float >= 0.0 && float <= u64::MAX as f64 => {
                    Some(float as u64)
                },
                _ => None,
            };
            if let Some(integer) = integral {
                *value = Value::from(integer);
            }
        },
        Value::Array(items) => items.iter_mut().for_each(normalize_numbers),
        Value::Object(map) => map.values_mut().for_each(normalize_numbers),
        _ => {},
    }
}

pub(crate) fn schema_from_value(value: Value) -> Schema {
    serde_json::from_value(value).expect("static wire schema fragment must be a valid schema")
}

macro_rules! static_schemas {
    ($($name:ident => $schema:expr),+ $(,)?) => {
        $(
            pub(crate) fn $name(_: &mut SchemaGenerator) -> Schema {
                schema_from_value($schema)
            }
        )+
    };
}

static_schemas! {
    const_true_schema => json!({ "const": true }),
    // 可空且必填(宿主输出契约要求显式 null)的 string 字段。
    nullable_string_schema => json!({ "type": ["string", "null"] }),
    // 可空且必填的非负 integer 字段。
    nullable_nonnegative_integer_schema => json!({ "type": ["integer", "null"], "minimum": 0 }),
    // 可空且必填的有符号 integer 字段。
    nullable_integer_schema => json!({ "type": ["integer", "null"] }),
    event_schema_version_schema => json!({
        "type": "integer",
        "minimum": 1,
        "maximum": u32::MAX
    }),
    session_state_key_schema => json!({
        "type": "string",
        "minLength": 1,
        "maxLength": HOST_SESSION_STATE_KEY_MAX_LENGTH,
        "pattern": "^[A-Za-z0-9_.-]+$",
        "not": { "enum": [".", ".."] }
    }),
    session_state_content_schema => json!({
        "type": "string",
        "maxLength": HOST_SESSION_STATE_VALUE_MAX_BYTES,
        "description": "Limited to this many UTF-8 bytes; the host byte limit is authoritative."
    }),
    process_stdin_schema => json!({
        "type": ["string", "null"],
        "maxLength": HOST_PROCESS_MAX_STDIN_BYTES,
        "description": "Limited to this many UTF-8 bytes; the host byte limit is authoritative."
    }),
    process_timeout_schema => json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "maximum": HOST_PROCESS_MAX_TIMEOUT_MS
    }),
    network_request_body_schema => json!({
        "type": "string",
        "contentEncoding": "base64",
        "maxLength": HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS,
        "description": "Decoded body is byte-limited; the host decoded-byte limit is authoritative."
    }),
    network_response_body_schema => json!({ "type": "string", "contentEncoding": "base64" }),
    http_status_schema => json!({ "type": "integer", "minimum": 0, "maximum": u16::MAX }),
    network_max_bytes_schema => json!({
        "type": "integer",
        "minimum": 0,
        "maximum": HOST_NETWORK_MAX_BYTES
    }),
    network_timeout_schema => json!({
        "type": "integer",
        "minimum": 1,
        "maximum": HOST_NETWORK_MAX_TIMEOUT_MS
    }),
    workspace_read_max_bytes_schema => json!({
        "type": ["integer", "null"],
        "minimum": 0,
        "maximum": HOST_WORKSPACE_MAX_FILE_BYTES,
        "default": HOST_WORKSPACE_MAX_FILE_BYTES
    }),
    workspace_list_depth_schema => json!({
        "type": "integer",
        "minimum": 1,
        "maximum": HOST_WORKSPACE_LIST_MAX_DEPTH,
        "default": HOST_WORKSPACE_LIST_DEFAULT_DEPTH
    }),
    workspace_list_limit_schema => json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "maximum": HOST_WORKSPACE_LIST_MAX_ENTRIES,
        "default": HOST_WORKSPACE_LIST_DEFAULT_LIMIT
    }),
    grep_max_matches_schema => json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "maximum": HOST_WORKSPACE_SEARCH_MAX_MATCHES,
        "default": HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES
    }),
    grep_max_bytes_schema => json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "maximum": HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES,
        "default": HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES
    }),
    grep_max_line_chars_schema => json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "maximum": HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS,
        "default": HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS
    }),
    glob_max_matches_schema => json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "maximum": HOST_WORKSPACE_SEARCH_MAX_MATCHES,
        "default": HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES
    }),
    // 线缆上承诺为 JSON object 的 Value 字段(比 serde 的 Value 更严)。
    json_object_schema => json!({ "type": "object" }),
}

/// 把可序列化为 schema 的 Value 包装为「可空且必填」的 anyOf 组合。
fn nullable_schema_of(value: Value) -> Schema {
    schema_from_value(json!({ "anyOf": [value, { "type": "null" }] }))
}

/// 派生类型对应的「可空且必填」schema。
pub(crate) fn nullable_subschema<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    let subschema = serde_json::to_value(generator.subschema_for::<T>())
        .expect("derived JSON schema is always serializable");
    nullable_schema_of(subschema)
}

/// `toolSelection` DTO schema 随调用点语境变化：description 不同、可空性不同。
fn tool_selection_schema(description: &str, nullable: bool) -> Schema {
    let selection = SessionToolSelectionDto::wire_schema(description);
    if nullable {
        nullable_schema_of(selection)
    } else {
        schema_from_value(selection)
    }
}

/// `toolSelection` 可空且不必填，描述为子会话语义。
pub(crate) fn create_session_tool_selection_schema(_: &mut SchemaGenerator) -> Schema {
    tool_selection_schema("Child session tool visibility for subsequent turns.", true)
}

pub(crate) fn configure_tools_selection_schema(_: &mut SchemaGenerator) -> Schema {
    tool_selection_schema("Session tool visibility for subsequent turns.", false)
}

pub(crate) fn configure_tools_output_selection_schema(_: &mut SchemaGenerator) -> Schema {
    tool_selection_schema(
        "Effective session tool visibility for subsequent turns.",
        false,
    )
}

/// `toolSelection` 可空且必填，且 DTO schema 来自手写的 session 侧契约。
pub(crate) fn read_model_tool_selection_schema(_: &mut SchemaGenerator) -> Schema {
    tool_selection_schema("Effective session tool visibility.", true)
}
