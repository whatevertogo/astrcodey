//! Provider-specific validation for strict tool schemas.
//!
//! Strict tool use is opt-in at both the tool and provider-profile levels. This module validates
//! only declarations that will actually be sent, so legacy profiles keep their previous behavior.

use std::collections::{HashMap, HashSet};

use astrcode_core::{
    llm::LlmError,
    tool::{ToolDefinition, ToolOrigin},
};
use serde_json::{Map, Value};

const OPENAI_MAX_OBJECT_PROPERTIES: usize = 5_000;
const OPENAI_MAX_NESTING_DEPTH: usize = 10;
const OPENAI_MAX_SCHEMA_STRING_CHARS: usize = 120_000;
const OPENAI_MAX_ENUM_VALUES: usize = 1_000;
const OPENAI_LARGE_ENUM_THRESHOLD: usize = 250;
const OPENAI_MAX_LARGE_ENUM_STRING_CHARS: usize = 15_000;
const ANTHROPIC_MAX_STRICT_TOOLS: usize = 20;
const ANTHROPIC_MAX_OPTIONAL_PARAMETERS: usize = 24;
const ANTHROPIC_MAX_UNION_PARAMETERS: usize = 16;

/// 三种遍历器共享的子 schema 关键字清单：数组形态与对象形态各一组。
const CHILD_SCHEMA_KEYWORDS: [&str; 4] = ["anyOf", "oneOf", "allOf", "prefixItems"];
const DEFINITION_KEYWORDS: [&str; 3] = ["$defs", "definitions", "patternProperties"];

#[derive(Debug, Clone, Copy)]
pub(crate) enum StrictToolProvider {
    OpenAi,
    Anthropic,
}

/// Compile first-party tool schemas to the strict JSON Schema dialect used by the provider.
///
/// Tool definitions keep their natural runtime contract: optional Rust fields remain optional and
/// validation constraints remain available to the executor. Provider strict dialects differ,
/// though. OpenAI requires every object property to appear in `required`, while Anthropic permits
/// optional properties but rejects several validation-only keywords. Compiling at this boundary
/// avoids duplicating provider-specific schemas in every tool implementation.
pub(crate) fn prepare_strict_tools(
    tools: &mut [ToolDefinition],
    supports_strict_tool_use: bool,
    provider: StrictToolProvider,
) -> Result<(), LlmError> {
    if !supports_strict_tool_use {
        return Ok(());
    }

    match provider {
        StrictToolProvider::OpenAi => {
            for tool in tools.iter_mut().filter(|tool| tool.strict) {
                compile_openai_tool_schema(&mut tool.parameters);
            }
        },
        StrictToolProvider::Anthropic => {
            prepare_anthropic_tools(tools)?;
        },
    }
    validate_strict_tools(tools, supports_strict_tool_use, provider)
}

fn compile_openai_tool_schema(schema: &mut Value) {
    // OpenAI forbids a root `anyOf`. First-party executors remain authoritative for cross-field
    // invariants (for example edit's single-edit versus batch-edit shape), while the compiled
    // schema still constrains every field name and value type.
    if schema.get("anyOf").is_some_and(is_required_only_root_union)
        && let Some(object) = schema.as_object_mut()
    {
        object.remove("anyOf");
    }
    compile_openai_schema(schema);
}

fn is_required_only_root_union(union: &Value) -> bool {
    union.as_array().is_some_and(|branches| {
        !branches.is_empty()
            && branches.iter().all(|branch| {
                branch.as_object().is_some_and(|object| {
                    object.len() == 1
                        && object.get("required").is_some_and(|required| {
                            required.as_array().is_some_and(|names| {
                                !names.is_empty() && names.iter().all(Value::is_string)
                            })
                        })
                })
            })
    })
}

fn prepare_anthropic_tools(tools: &mut [ToolDefinition]) -> Result<(), LlmError> {
    let mut candidates = tools
        .iter()
        .enumerate()
        .filter(|(_, tool)| tool.strict)
        .map(|(index, tool)| {
            let mut candidate = tool.clone();
            compile_anthropic_schema(
                &mut candidate.parameters,
                candidate.origin == ToolOrigin::Bundled,
            );
            (index, candidate)
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(index, tool)| (tool_origin_priority(tool.origin), *index));

    let mut accepted = Vec::new();
    let mut downgraded = Vec::new();
    for (index, mut candidate) in candidates {
        validate_anthropic_tool(&candidate)?;
        if accepted.len() < ANTHROPIC_MAX_STRICT_TOOLS {
            let stats = {
                let mut trial = accepted.iter().collect::<Vec<_>>();
                trial.push(&candidate);
                collect_anthropic_schema_stats(&trial)?
            };
            let optional_overflow = stats
                .optional_parameters
                .saturating_sub(ANTHROPIC_MAX_OPTIONAL_PARAMETERS);
            if optional_overflow > 0 {
                let promoted_unions =
                    promote_optional_parameters(&mut candidate.parameters, optional_overflow, true);
                let remaining = optional_overflow.saturating_sub(promoted_unions);
                let union_capacity =
                    ANTHROPIC_MAX_UNION_PARAMETERS.saturating_sub(stats.union_parameters);
                promote_optional_parameters(
                    &mut candidate.parameters,
                    remaining.min(union_capacity),
                    false,
                );
            }

            let mut trial = accepted.iter().collect::<Vec<_>>();
            trial.push(&candidate);
            if validate_anthropic_tools(&trial).is_ok() {
                tools[index] = candidate.clone();
                accepted.push(candidate);
                continue;
            }
        }
        tools[index].strict = false;
        downgraded.push(tools[index].name.clone());
    }

    if !downgraded.is_empty() {
        tracing::warn!(
            tool_names = ?downgraded,
            "Anthropic strict-tool request limits exceeded; sending overflow tools without \
             provider-side strict mode"
        );
    }
    Ok(())
}

fn promote_optional_parameters(
    schema: &mut Value,
    maximum: usize,
    existing_unions_only: bool,
) -> usize {
    if maximum == 0 {
        return 0;
    }
    let Some(object) = schema.as_object_mut() else {
        return 0;
    };
    // 校验（validate_anthropic_tool）已保证 `required` 为数组或缺失；此处仅防御。
    let Some(required) = required_array(object) else {
        return 0;
    };
    let promoted =
        promote_properties_to_required(object, required, maximum, |_, property_schema| {
            is_union_schema(property_schema) == existing_unions_only
        });
    if promoted == maximum {
        return promoted;
    }

    visit_child_schemas_mut_count(object, maximum - promoted, |child, remaining| {
        promote_optional_parameters(child, remaining, existing_unions_only)
    }) + promoted
}

/// 提取 `required` 数组；`None` 表示该键存在但类型错误——调用方应保留原值，
/// 让后续校验以"`required` must be an array"拒绝，而不是用默认值掩盖损坏。
fn required_array(object: &Map<String, Value>) -> Option<Vec<Value>> {
    match object.get("required") {
        None => Some(Vec::new()),
        Some(Value::Array(values)) => Some(values.clone()),
        Some(_) => None,
    }
}

/// 将对象属性提升进 `required`（对提升的属性做 nullable 包裹），返回提升数量。
///
/// `required` 缺失视为空数组；`properties` 存在时写回提升后的 `required` 数组。
fn promote_properties_to_required(
    object: &mut Map<String, Value>,
    mut required: Vec<Value>,
    maximum: usize,
    mut should_promote: impl FnMut(&str, &Value) -> bool,
) -> usize {
    let mut required_names = required
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let Some(Value::Object(properties)) = object.get_mut("properties") else {
        return 0;
    };
    let mut promoted = 0;
    for (name, property_schema) in properties {
        if promoted == maximum {
            break;
        }
        if required_names.contains(name) {
            continue;
        }
        if !should_promote(name, property_schema) {
            continue;
        }
        make_nullable(property_schema);
        required_names.insert(name.clone());
        required.push(Value::String(name.clone()));
        promoted += 1;
    }
    object.insert("required".into(), Value::Array(required));
    promoted
}

/// 遍历所有子 schema；回调返回 `false` 时提前停止。三种遍历器共享的骨架。
fn for_each_child_schema_mut(
    schema: &mut Map<String, Value>,
    mut visit: impl FnMut(&mut Value) -> bool,
) -> bool {
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        for child in properties.values_mut() {
            if !visit(child) {
                return false;
            }
        }
    }
    if let Some(items) = schema.get_mut("items")
        && !visit(items)
    {
        return false;
    }
    for keyword in CHILD_SCHEMA_KEYWORDS {
        if let Some(Value::Array(children)) = schema.get_mut(keyword) {
            for child in children {
                if !visit(child) {
                    return false;
                }
            }
        }
    }
    for keyword in DEFINITION_KEYWORDS {
        if let Some(Value::Object(definitions)) = schema.get_mut(keyword) {
            for child in definitions.values_mut() {
                if !visit(child) {
                    return false;
                }
            }
        }
    }
    true
}

fn visit_child_schemas_mut_count(
    schema: &mut Map<String, Value>,
    maximum: usize,
    mut visit: impl FnMut(&mut Value, usize) -> usize,
) -> usize {
    let mut visited = 0;
    for_each_child_schema_mut(schema, |child| {
        if visited == maximum {
            return false;
        }
        visited += visit(child, maximum - visited);
        visited < maximum
    });
    visited
}

fn tool_origin_priority(origin: ToolOrigin) -> u8 {
    match origin {
        ToolOrigin::Bundled => 0,
        ToolOrigin::Extension => 1,
    }
}

fn validate_strict_tools(
    tools: &[ToolDefinition],
    supports_strict_tool_use: bool,
    provider: StrictToolProvider,
) -> Result<(), LlmError> {
    if !supports_strict_tool_use {
        return Ok(());
    }

    let strict_tools: Vec<_> = tools.iter().filter(|tool| tool.strict).collect();
    match provider {
        StrictToolProvider::OpenAi => {
            for tool in strict_tools {
                validate_openai_tool(tool)?;
            }
        },
        StrictToolProvider::Anthropic => validate_anthropic_tools(&strict_tools)?,
    }
    Ok(())
}

fn compile_openai_schema(schema: &mut Value) {
    visit_child_schemas_mut(schema, compile_openai_schema);

    let Some(object) = schema.as_object_mut() else {
        return;
    };
    // `required` 类型损坏时保留原值，由 validate_strict_tools 以类型化错误拒绝。
    let Some(required) = required_array(object) else {
        return;
    };
    promote_properties_to_required(object, required, usize::MAX, |_, _| true);
    if is_object_schema_object(object) {
        object.insert("additionalProperties".into(), Value::Bool(false));
    }
}

fn compile_anthropic_schema(schema: &mut Value, elide_validation_constraints: bool) {
    visit_child_schemas_mut(schema, |child| {
        compile_anthropic_schema(child, elide_validation_constraints);
    });

    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if elide_validation_constraints {
        for keyword in [
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
            "minLength",
            "maxLength",
            "maxItems",
            "uniqueItems",
        ] {
            object.remove(keyword);
        }
        if object
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| minimum > 1)
        {
            object.remove("minItems");
        }
    }
    if is_object_schema_object(object) {
        object.insert("additionalProperties".into(), Value::Bool(false));
    }
}

fn is_object_schema_object(object: &Map<String, Value>) -> bool {
    object.contains_key("properties")
        || match object.get("type") {
            Some(Value::String(kind)) => kind == "object",
            Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
            _ => false,
        }
}

fn make_nullable(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        wrap_nullable(schema);
        return;
    };

    if object.contains_key("const") {
        wrap_nullable(schema);
        return;
    }
    if let Some(Value::Array(values)) = object.get_mut("enum")
        && !values.iter().any(Value::is_null)
    {
        values.push(Value::Null);
    }
    if let Some(schema_type) = object.get_mut("type") {
        match schema_type {
            Value::String(kind) if kind != "null" => {
                *schema_type = Value::Array(vec![
                    Value::String(kind.clone()),
                    Value::String("null".into()),
                ]);
            },
            Value::Array(kinds) if !kinds.iter().any(|kind| kind.as_str() == Some("null")) => {
                kinds.push(Value::String("null".into()));
            },
            _ => {},
        }
        return;
    }
    if let Some(Value::Array(branches)) = object.get_mut("anyOf") {
        if !branches.iter().any(is_null_schema) {
            branches.push(serde_json::json!({"type": "null"}));
        }
        return;
    }

    wrap_nullable(schema);
}

/// 将 schema 包裹为 `anyOf: [原值, {"type": "null"}]`。
fn wrap_nullable(schema: &mut Value) {
    let original = std::mem::take(schema);
    *schema = serde_json::json!({"anyOf": [original, {"type": "null"}]});
}

fn is_null_schema(schema: &Value) -> bool {
    schema
        .get("type")
        .is_some_and(|schema_type| match schema_type {
            Value::String(kind) => kind == "null",
            Value::Array(kinds) => kinds.iter().any(|kind| kind == "null"),
            _ => false,
        })
}

fn visit_child_schemas_mut(schema: &mut Value, mut visit: impl FnMut(&mut Value)) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for_each_child_schema_mut(object, |child| {
        visit(child);
        true
    });
}

fn validate_openai_tool(tool: &ToolDefinition) -> Result<(), LlmError> {
    if !is_tool_parameters_object(&tool.parameters) {
        return Err(schema_error(
            tool,
            "$",
            "OpenAI strict tool parameters must use an object schema",
        ));
    }
    if tool.parameters.get("anyOf").is_some() {
        return Err(schema_error(
            tool,
            "$/anyOf",
            "OpenAI strict tool parameters cannot use `anyOf` at the root",
        ));
    }
    validate_openai_schema(
        tool,
        &tool.parameters,
        "$",
        0,
        &mut OpenAiSchemaStats::default(),
    )?;
    validate_local_refs(tool, &tool.parameters, &tool.parameters, "$", "OpenAI")
}

#[derive(Debug, Default)]
struct OpenAiSchemaStats {
    object_properties: usize,
    schema_string_chars: usize,
    enum_values: usize,
}

fn validate_openai_schema(
    tool: &ToolDefinition,
    schema: &Value,
    path: &str,
    parent_object_depth: usize,
    stats: &mut OpenAiSchemaStats,
) -> Result<(), LlmError> {
    let Some(object) = schema.as_object() else {
        return Err(schema_error(
            tool,
            path,
            "OpenAI strict schema nodes must be JSON objects",
        ));
    };

    for keyword in [
        "unevaluatedProperties",
        "propertyNames",
        "dependentRequired",
        "dependentSchemas",
        "allOf",
        "oneOf",
        "if",
        "then",
        "else",
        "not",
    ] {
        if object.contains_key(keyword) {
            return Err(schema_error(
                tool,
                &child_path(path, keyword),
                &format!("OpenAI strict tool schemas do not support `{keyword}`"),
            ));
        }
    }

    validate_strict_schema_type(tool, object, path, "OpenAI")?;
    validate_schema_map_keywords(tool, object, path, "OpenAI")?;
    if let Some(any_of) = object.get("anyOf") {
        match any_of {
            Value::Array(branches) if !branches.is_empty() => {},
            Value::Array(_) => {
                return Err(schema_error(
                    tool,
                    &child_path(path, "anyOf"),
                    "OpenAI strict `anyOf` must contain at least one schema",
                ));
            },
            _ => {
                return Err(schema_error(
                    tool,
                    &child_path(path, "anyOf"),
                    "OpenAI strict `anyOf` must be an array",
                ));
            },
        }
    }

    let object_depth = if is_object_schema(schema) {
        let object_depth = parent_object_depth + 1;
        if object_depth > OPENAI_MAX_NESTING_DEPTH {
            return Err(schema_error(
                tool,
                path,
                &format!(
                    "OpenAI strict tool schemas allow at most {OPENAI_MAX_NESTING_DEPTH} levels \
                     of object nesting"
                ),
            ));
        }
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(schema_error(
                tool,
                &child_path(path, "additionalProperties"),
                "OpenAI strict object schemas must set `additionalProperties` to false",
            ));
        }

        let properties = schema_properties(tool, object, path)?;
        stats.object_properties = stats.object_properties.saturating_add(properties.len());
        if stats.object_properties > OPENAI_MAX_OBJECT_PROPERTIES {
            return Err(schema_error(
                tool,
                &child_path(path, "properties"),
                &format!(
                    "OpenAI strict tool schemas allow at most {OPENAI_MAX_OBJECT_PROPERTIES} \
                     object properties"
                ),
            ));
        }
        for name in properties.keys() {
            add_openai_schema_string_chars(
                tool,
                stats,
                name.chars().count(),
                &child_path(&child_path(path, "properties"), name),
            )?;
        }
        let required = required_property_names(tool, object, path)?;
        for name in properties.keys() {
            if !required.contains(name.as_str()) {
                return Err(schema_error(
                    tool,
                    &child_path(&child_path(path, "properties"), name),
                    "OpenAI strict object properties must all appear in `required`",
                ));
            }
        }
        for name in required {
            if !properties.contains_key(name) {
                return Err(schema_error(
                    tool,
                    &child_path(path, "required"),
                    &format!("`required` references unknown property `{name}`"),
                ));
            }
        }
        object_depth
    } else {
        parent_object_depth
    };

    for keyword in ["$defs", "definitions"] {
        if let Some(Value::Object(definitions)) = object.get(keyword) {
            for name in definitions.keys() {
                add_openai_schema_string_chars(
                    tool,
                    stats,
                    name.chars().count(),
                    &child_path(&child_path(path, keyword), name),
                )?;
            }
        }
    }

    if let Some(values) = schema_enum_values(tool, object, path, "OpenAI")? {
        stats.enum_values = stats.enum_values.saturating_add(values.len());
        if stats.enum_values > OPENAI_MAX_ENUM_VALUES {
            return Err(schema_error(
                tool,
                &child_path(path, "enum"),
                &format!(
                    "OpenAI strict tool schemas allow at most {OPENAI_MAX_ENUM_VALUES} enum values"
                ),
            ));
        }
        let enum_string_chars = values.iter().map(schema_literal_chars).sum();
        if values.len() > OPENAI_LARGE_ENUM_THRESHOLD
            && enum_string_chars > OPENAI_MAX_LARGE_ENUM_STRING_CHARS
        {
            return Err(schema_error(
                tool,
                &child_path(path, "enum"),
                &format!(
                    "OpenAI strict enum properties with more than {OPENAI_LARGE_ENUM_THRESHOLD} \
                     values allow at most {OPENAI_MAX_LARGE_ENUM_STRING_CHARS} characters across \
                     their values"
                ),
            ));
        }
        add_openai_schema_string_chars(tool, stats, enum_string_chars, &child_path(path, "enum"))?;
    }
    if let Some(value) = object.get("const") {
        add_openai_schema_string_chars(
            tool,
            stats,
            schema_literal_chars(value),
            &child_path(path, "const"),
        )?;
    }

    visit_child_schemas(object, path, |child, child_path| {
        validate_openai_schema(tool, child, child_path, object_depth, stats)
    })
}

fn add_openai_schema_string_chars(
    tool: &ToolDefinition,
    stats: &mut OpenAiSchemaStats,
    count: usize,
    path: &str,
) -> Result<(), LlmError> {
    stats.schema_string_chars = stats.schema_string_chars.saturating_add(count);
    if stats.schema_string_chars > OPENAI_MAX_SCHEMA_STRING_CHARS {
        return Err(schema_error(
            tool,
            path,
            &format!(
                "OpenAI strict tool schemas allow at most {OPENAI_MAX_SCHEMA_STRING_CHARS} \
                 characters across property names, definition names, enum values, and const values"
            ),
        ));
    }
    Ok(())
}

fn schema_literal_chars(value: &Value) -> usize {
    match value {
        Value::String(value) => value.chars().count(),
        _ => value.to_string().chars().count(),
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AnthropicSchemaStats {
    optional_parameters: usize,
    union_parameters: usize,
}

fn validate_anthropic_tools(strict_tools: &[&ToolDefinition]) -> Result<(), LlmError> {
    if strict_tools.len() > ANTHROPIC_MAX_STRICT_TOOLS {
        let tool = strict_tools[ANTHROPIC_MAX_STRICT_TOOLS];
        return Err(schema_error(
            tool,
            "$",
            &format!(
                "Anthropic allows at most {ANTHROPIC_MAX_STRICT_TOOLS} strict tools per request"
            ),
        ));
    }

    let stats = collect_anthropic_schema_stats(strict_tools)?;
    let Some(tool) = strict_tools.last() else {
        return Ok(());
    };
    if stats.optional_parameters > ANTHROPIC_MAX_OPTIONAL_PARAMETERS {
        return Err(schema_error(
            tool,
            "$",
            &format!(
                "Anthropic allows at most {ANTHROPIC_MAX_OPTIONAL_PARAMETERS} optional parameters \
                 across strict tools"
            ),
        ));
    }
    if stats.union_parameters > ANTHROPIC_MAX_UNION_PARAMETERS {
        return Err(schema_error(
            tool,
            "$",
            &format!(
                "Anthropic allows at most {ANTHROPIC_MAX_UNION_PARAMETERS} union parameters \
                 across strict tools"
            ),
        ));
    }
    Ok(())
}

fn validate_anthropic_tool(tool: &ToolDefinition) -> Result<(), LlmError> {
    collect_anthropic_schema_stats(&[tool]).map(|_| ())
}

fn collect_anthropic_schema_stats(
    strict_tools: &[&ToolDefinition],
) -> Result<AnthropicSchemaStats, LlmError> {
    let mut stats = AnthropicSchemaStats::default();
    for tool in strict_tools {
        if !is_tool_parameters_object(&tool.parameters) {
            return Err(schema_error(
                tool,
                "$",
                "Anthropic strict tool parameters must use an object schema",
            ));
        }
        validate_anthropic_schema(tool, &tool.parameters, &tool.parameters, "$", &mut stats)?;
        validate_local_refs(tool, &tool.parameters, &tool.parameters, "$", "Anthropic")?;
        detect_recursive_refs(tool, &tool.parameters)?;
    }
    Ok(stats)
}

fn validate_anthropic_schema(
    tool: &ToolDefinition,
    root: &Value,
    schema: &Value,
    path: &str,
    stats: &mut AnthropicSchemaStats,
) -> Result<(), LlmError> {
    let Some(object) = schema.as_object() else {
        return Err(schema_error(
            tool,
            path,
            "Anthropic strict schema nodes must be JSON objects",
        ));
    };

    validate_strict_schema_type(tool, object, path, "Anthropic")?;
    validate_schema_map_keywords(tool, object, path, "Anthropic")?;
    for keyword in ["anyOf", "allOf"] {
        if let Some(value) = object.get(keyword) {
            match value {
                Value::Array(branches) if !branches.is_empty() => {},
                Value::Array(_) => {
                    return Err(schema_error(
                        tool,
                        &child_path(path, keyword),
                        &format!("Anthropic strict `{keyword}` must contain at least one schema"),
                    ));
                },
                _ => {
                    return Err(schema_error(
                        tool,
                        &child_path(path, keyword),
                        &format!("Anthropic strict `{keyword}` must be an array"),
                    ));
                },
            }
        }
    }

    for keyword in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "maxItems",
        "uniqueItems",
        "contains",
        "minContains",
        "maxContains",
        "prefixItems",
        "additionalItems",
        "unevaluatedItems",
        "patternProperties",
        "propertyNames",
        "unevaluatedProperties",
        "dependentRequired",
        "dependentSchemas",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
    ] {
        if object.contains_key(keyword) {
            return Err(schema_error(
                tool,
                &child_path(path, keyword),
                &format!("Anthropic strict tool schemas do not support `{keyword}`"),
            ));
        }
    }

    if let Some(min_items) = object.get("minItems")
        && !matches!(min_items.as_u64(), Some(0 | 1))
    {
        return Err(schema_error(
            tool,
            &child_path(path, "minItems"),
            "Anthropic strict tool schemas only support `minItems` values of 0 or 1",
        ));
    }

    if let Some(values) = schema_enum_values(tool, object, path, "Anthropic")? {
        for (index, value) in values.iter().enumerate() {
            if value.is_array() || value.is_object() {
                return Err(schema_error(
                    tool,
                    &format!("{}/enum/{index}", path.trim_end_matches('/')),
                    "Anthropic strict tool enums may contain only primitive values",
                ));
            }
        }
    }

    if is_object_schema(schema) {
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(schema_error(
                tool,
                &child_path(path, "additionalProperties"),
                "Anthropic strict object schemas must set `additionalProperties` to false",
            ));
        }
        let properties = schema_properties(tool, object, path)?;
        let required = required_property_names(tool, object, path)?;
        for name in &required {
            if !properties.contains_key(*name) {
                return Err(schema_error(
                    tool,
                    &child_path(path, "required"),
                    &format!("`required` references unknown property `{name}`"),
                ));
            }
        }
        for (name, property_schema) in properties {
            if !required.contains(name.as_str()) {
                stats.optional_parameters += 1;
            }
            if is_anthropic_union_parameter(root, property_schema, &mut HashSet::new()) {
                stats.union_parameters += 1;
            }
        }
    }

    if object
        .get("allOf")
        .is_some_and(|all_of| contains_keyword(all_of, "$ref"))
    {
        return Err(schema_error(
            tool,
            &child_path(path, "allOf"),
            "Anthropic strict tool schemas do not support `allOf` combined with `$ref`",
        ));
    }

    visit_child_schemas(object, path, |child, child_path| {
        validate_anthropic_schema(tool, root, child, child_path, stats)
    })
}

fn is_anthropic_union_parameter(
    root: &Value,
    schema: &Value,
    visited_refs: &mut HashSet<String>,
) -> bool {
    if is_union_schema(schema) {
        return true;
    }
    let Some(reference) = schema
        .as_object()
        .and_then(|object| object.get("$ref"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Some(pointer) = reference.strip_prefix('#') else {
        return false;
    };
    if !visited_refs.insert(pointer.to_string()) {
        return false;
    }
    root.pointer(pointer)
        .is_some_and(|target| is_anthropic_union_parameter(root, target, visited_refs))
}

fn validate_strict_schema_type(
    tool: &ToolDefinition,
    schema: &Map<String, Value>,
    path: &str,
    provider: &str,
) -> Result<(), LlmError> {
    let Some(schema_type) = schema.get("type") else {
        return Ok(());
    };
    let supported = |value: &str| {
        matches!(
            value,
            "string" | "number" | "boolean" | "integer" | "object" | "array" | "null"
        )
    };
    let valid = match schema_type {
        Value::String(value) => supported(value),
        Value::Array(values) if !values.is_empty() => values
            .iter()
            .all(|value| value.as_str().is_some_and(supported)),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(schema_error(
            tool,
            &child_path(path, "type"),
            &format!("{provider} strict `type` must contain only supported JSON Schema types"),
        ))
    }
}

fn validate_schema_map_keywords(
    tool: &ToolDefinition,
    schema: &Map<String, Value>,
    path: &str,
    provider_name: &str,
) -> Result<(), LlmError> {
    for keyword in ["$defs", "definitions"] {
        if schema.get(keyword).is_some_and(|value| !value.is_object()) {
            return Err(schema_error(
                tool,
                &child_path(path, keyword),
                &format!("{provider_name} strict `{keyword}` must be an object"),
            ));
        }
    }
    Ok(())
}

fn schema_enum_values<'a>(
    tool: &ToolDefinition,
    schema: &'a Map<String, Value>,
    path: &str,
    provider_name: &str,
) -> Result<Option<&'a [Value]>, LlmError> {
    match schema.get("enum") {
        Some(Value::Array(values)) if !values.is_empty() => Ok(Some(values)),
        Some(Value::Array(_)) => Err(schema_error(
            tool,
            &child_path(path, "enum"),
            &format!("{provider_name} strict `enum` must contain at least one value"),
        )),
        Some(_) => Err(schema_error(
            tool,
            &child_path(path, "enum"),
            &format!("{provider_name} strict `enum` must be an array"),
        )),
        None => Ok(None),
    }
}

fn validate_local_refs(
    tool: &ToolDefinition,
    root: &Value,
    schema: &Value,
    path: &str,
    provider_name: &str,
) -> Result<(), LlmError> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if let Some(reference) = object.get("$ref") {
        let reference = reference.as_str().ok_or_else(|| {
            schema_error(
                tool,
                &child_path(path, "$ref"),
                &format!("{provider_name} strict `$ref` must be a string"),
            )
        })?;
        if reference != "#" && !reference.starts_with("#/") {
            return Err(schema_error(
                tool,
                &child_path(path, "$ref"),
                &format!("{provider_name} strict schemas do not support external `$ref` values"),
            ));
        }
        let pointer = reference.strip_prefix('#').ok_or_else(|| {
            schema_error(
                tool,
                &child_path(path, "$ref"),
                &format!("{provider_name} strict schemas require local `$ref` values"),
            )
        })?;
        let target = root.pointer(pointer).ok_or_else(|| {
            schema_error(
                tool,
                &child_path(path, "$ref"),
                &format!("local `$ref` target `{reference}` does not exist"),
            )
        })?;
        if !target.is_object() {
            return Err(schema_error(
                tool,
                &child_path(path, "$ref"),
                &format!("{provider_name} strict `$ref` targets must be schema objects"),
            ));
        }
    }
    visit_child_schemas(object, path, |child, child_path| {
        validate_local_refs(tool, root, child, child_path, provider_name)
    })
}

fn detect_recursive_refs(tool: &ToolDefinition, root: &Value) -> Result<(), LlmError> {
    detect_recursive_refs_from(tool, root, root, "$", &mut HashMap::new())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefVisitState {
    Visiting,
    Done,
}

fn detect_recursive_refs_from(
    tool: &ToolDefinition,
    root: &Value,
    schema: &Value,
    path: &str,
    ref_states: &mut HashMap<String, RefVisitState>,
) -> Result<(), LlmError> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };

    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(pointer) = reference.strip_prefix('#')
    {
        match ref_states.get(pointer) {
            Some(RefVisitState::Visiting) => {
                return Err(schema_error(
                    tool,
                    &child_path(path, "$ref"),
                    "Anthropic strict tool schemas do not support recursive schemas",
                ));
            },
            Some(RefVisitState::Done) => {},
            None => {
                let target = root.pointer(pointer).ok_or_else(|| {
                    schema_error(
                        tool,
                        &child_path(path, "$ref"),
                        &format!("local `$ref` target `{reference}` does not exist"),
                    )
                })?;
                ref_states.insert(pointer.to_string(), RefVisitState::Visiting);
                detect_recursive_refs_from(tool, root, target, reference, ref_states)?;
                ref_states.insert(pointer.to_string(), RefVisitState::Done);
            },
        }
    }

    visit_child_schemas(object, path, |child, child_path| {
        detect_recursive_refs_from(tool, root, child, child_path, ref_states)
    })
}

fn schema_properties<'a>(
    tool: &ToolDefinition,
    schema: &'a Map<String, Value>,
    path: &str,
) -> Result<&'a Map<String, Value>, LlmError> {
    match schema.get("properties") {
        Some(Value::Object(properties)) => Ok(properties),
        Some(_) => Err(schema_error(
            tool,
            &child_path(path, "properties"),
            "`properties` must be an object",
        )),
        None => Ok(empty_object()),
    }
}

fn required_property_names<'a>(
    tool: &ToolDefinition,
    schema: &'a Map<String, Value>,
    path: &str,
) -> Result<HashSet<&'a str>, LlmError> {
    match schema.get("required") {
        Some(Value::Array(required)) => required
            .iter()
            .map(|name| {
                name.as_str().ok_or_else(|| {
                    schema_error(
                        tool,
                        &child_path(path, "required"),
                        "`required` entries must be strings",
                    )
                })
            })
            .collect(),
        Some(_) => Err(schema_error(
            tool,
            &child_path(path, "required"),
            "`required` must be an array",
        )),
        None => Ok(HashSet::new()),
    }
}

fn empty_object() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn is_object_schema(schema: &Value) -> bool {
    schema.as_object().is_some_and(is_object_schema_object)
}

fn is_tool_parameters_object(schema: &Value) -> bool {
    schema
        .as_object()
        .is_some_and(|object| object.get("type").and_then(Value::as_str) == Some("object"))
}

fn is_union_schema(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    object.contains_key("anyOf")
        || object
            .get("type")
            .and_then(Value::as_array)
            .is_some_and(|types| types.len() > 1)
}

fn contains_keyword(value: &Value, keyword: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(keyword)
                || object
                    .values()
                    .any(|child| contains_keyword(child, keyword))
        },
        Value::Array(values) => values.iter().any(|child| contains_keyword(child, keyword)),
        _ => false,
    }
}

fn visit_child_schemas(
    schema: &Map<String, Value>,
    path: &str,
    mut visit: impl FnMut(&Value, &str) -> Result<(), LlmError>,
) -> Result<(), LlmError> {
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (name, child) in properties {
            let child_path = child_path(&child_path(path, "properties"), name);
            visit(child, &child_path)?;
        }
    }
    if let Some(items) = schema.get("items") {
        visit(items, &child_path(path, "items"))?;
    }
    for keyword in CHILD_SCHEMA_KEYWORDS {
        if let Some(Value::Array(children)) = schema.get(keyword) {
            for (index, child) in children.iter().enumerate() {
                visit(
                    child,
                    &format!("{}/{keyword}/{index}", path.trim_end_matches('/')),
                )?;
            }
        }
    }
    for keyword in DEFINITION_KEYWORDS {
        if let Some(Value::Object(definitions)) = schema.get(keyword) {
            for (name, child) in definitions {
                let child_path = child_path(&child_path(path, keyword), name);
                visit(child, &child_path)?;
            }
        }
    }
    Ok(())
}

fn child_path(parent: &str, segment: &str) -> String {
    format!("{parent}/{}", segment.replace('~', "~0").replace('/', "~1"))
}

fn schema_error(tool: &ToolDefinition, path: &str, message: &str) -> LlmError {
    LlmError::Unsupported {
        message: format!("strict tool `{}` schema at `{path}`: {message}", tool.name),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::tool::{ExecutionMode, ToolOrigin};
    use astrcode_extension_sdk::extension::Registrar;
    use serde_json::json;

    use super::*;

    fn tool(name: &str, parameters: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters,
            strict: true,
            origin: ToolOrigin::Bundled,
            execution_mode: ExecutionMode::Parallel,
            timeout_ms: None,
        }
    }

    #[test]
    fn validates_diverse_provider_schema_rules() {
        let cases = [
            (
                StrictToolProvider::OpenAi,
                tool(
                    "validNested",
                    json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "object",
                                "properties": {"value": {"type": ["string", "null"]}},
                                "required": ["value"],
                                "additionalProperties": false
                            }
                        },
                        "required": ["input"],
                        "additionalProperties": false
                    }),
                ),
                None,
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "unsupportedUnion",
                    json!({
                        "type": "object",
                        "properties": {
                            "value": {
                                "oneOf": [{"type": "string"}, {"type": "number"}]
                            }
                        },
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                ),
                Some("`$/properties/value/oneOf`"),
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "invalidType",
                    json!({
                        "type": "object",
                        "properties": {"value": {"type": "decimal"}},
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                ),
                Some("`$/properties/value/type`"),
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "emptyUnion",
                    json!({
                        "type": "object",
                        "properties": {"value": {"anyOf": []}},
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                ),
                Some("`$/properties/value/anyOf`"),
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "invalidReference",
                    json!({
                        "type": "object",
                        "properties": {"value": {"$ref": 1}},
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                ),
                Some("`$/properties/value/$ref`"),
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "openObject",
                    json!({
                        "type": "object",
                        "properties": {},
                        "required": []
                    }),
                ),
                Some("`$/additionalProperties`"),
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "optionalField",
                    json!({
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": [],
                        "additionalProperties": false
                    }),
                ),
                Some("`$/properties/value`"),
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "corruptedRequired",
                    json!({
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": "bogus",
                        "additionalProperties": false
                    }),
                ),
                Some("`$/required`"),
            ),
            (
                StrictToolProvider::OpenAi,
                tool(
                    "patternedObject",
                    json!({
                        "type": "object",
                        "properties": {},
                        "required": [],
                        "additionalProperties": false,
                        "patternProperties": {
                            "^x-": {"type": "string"}
                        }
                    }),
                ),
                None,
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "openAnthropicObject",
                    json!({
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {}
                    }),
                ),
                Some("`$/additionalProperties`"),
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "validOptionalNested",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "filter": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {"query": {"type": "string"}}
                            }
                        }
                    }),
                ),
                None,
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "numericConstraint",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"count": {"type": "integer", "minimum": 0}}
                    }),
                ),
                Some("`$/properties/count/minimum`"),
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "invalidAnthropicType",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"value": {"type": "decimal"}}
                    }),
                ),
                Some("`$/properties/value/type`"),
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "invalidAnthropicUnion",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"value": {"anyOf": "string"}}
                    }),
                ),
                Some("`$/properties/value/anyOf`"),
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "invalidAnthropicEnum",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"value": {"type": "string", "enum": "one"}}
                    }),
                ),
                Some("`$/properties/value/enum`"),
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "externalReference",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"value": {"$ref": "https://example.com/schema.json"}}
                    }),
                ),
                Some("`$/properties/value/$ref`"),
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "recursive",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"next": {"$ref": "#/$defs/node"}},
                        "$defs": {
                            "node": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {"next": {"$ref": "#/$defs/node"}}
                            }
                        }
                    }),
                ),
                Some("recursive schemas"),
            ),
            (
                StrictToolProvider::Anthropic,
                tool(
                    "sharedReferences",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "left": {"$ref": "#/$defs/branch"},
                            "right": {"$ref": "#/$defs/branch"}
                        },
                        "$defs": {
                            "leaf": {"type": "string"},
                            "branch": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "first": {"$ref": "#/$defs/leaf"},
                                    "second": {"$ref": "#/$defs/leaf"}
                                }
                            }
                        }
                    }),
                ),
                None,
            ),
        ];

        for (provider, tool, expected_error) in cases {
            let result = validate_strict_tools(&[tool], true, provider);
            match expected_error {
                Some(fragment) => assert!(
                    result
                        .expect_err("schema should be rejected")
                        .to_string()
                        .contains(fragment)
                ),
                None => result.expect("schema should be accepted"),
            }
        }
    }

    #[test]
    fn enforces_openai_schema_complexity_limits() {
        let property_names: Vec<_> = (0..=OPENAI_MAX_OBJECT_PROPERTIES)
            .map(|index| format!("p{index}"))
            .collect();
        let properties = property_names
            .iter()
            .map(|name| (name.clone(), json!({"type": "string"})))
            .collect::<Map<String, Value>>();
        let long_property_name = "x".repeat(OPENAI_MAX_SCHEMA_STRING_CHARS + 1);
        let too_many_enum_values = vec![Value::String("value".into()); OPENAI_MAX_ENUM_VALUES + 1];
        let large_enum_values = (0..=OPENAI_LARGE_ENUM_THRESHOLD)
            .map(|index| {
                Value::String(format!(
                    "{index:03}{}",
                    "x".repeat(
                        OPENAI_MAX_LARGE_ENUM_STRING_CHARS / (OPENAI_LARGE_ENUM_THRESHOLD + 1)
                    )
                ))
            })
            .collect::<Vec<_>>();
        let cases = [
            (
                tool("deep", nested_openai_object(OPENAI_MAX_NESTING_DEPTH + 1)),
                "at most 10 levels",
            ),
            (
                tool(
                    "manyProperties",
                    json!({
                        "type": "object",
                        "properties": properties,
                        "required": property_names,
                        "additionalProperties": false
                    }),
                ),
                "at most 5000 object properties",
            ),
            (
                tool(
                    "longName",
                    json!({
                        "type": "object",
                        "properties": {
                            (long_property_name.clone()): {"type": "string"}
                        },
                        "required": [long_property_name],
                        "additionalProperties": false
                    }),
                ),
                "at most 120000 characters",
            ),
            (
                tool(
                    "manyEnumValues",
                    json!({
                        "type": "object",
                        "properties": {
                            "value": {"type": "string", "enum": too_many_enum_values}
                        },
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                ),
                "at most 1000 enum values",
            ),
            (
                tool(
                    "largeEnum",
                    json!({
                        "type": "object",
                        "properties": {
                            "value": {"type": "string", "enum": large_enum_values}
                        },
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                ),
                "at most 15000 characters",
            ),
        ];

        for (tool, expected) in cases {
            assert!(
                validate_strict_tools(&[tool], true, StrictToolProvider::OpenAi)
                    .expect_err("OpenAI complexity limit should be rejected")
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn enforces_anthropic_request_aggregate_limits() {
        let cases = [
            (
                (0..21)
                    .map(|index| {
                        tool(
                            &format!("tool{index}"),
                            json!({
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {}
                            }),
                        )
                    })
                    .collect::<Vec<_>>(),
                "at most 20 strict tools",
            ),
            (
                vec![tool(
                    "optional",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": (0..25)
                            .map(|index| (format!("p{index}"), json!({"type": "string"})))
                            .collect::<Map<String, Value>>()
                    }),
                )],
                "at most 24 optional parameters",
            ),
            (
                vec![tool(
                    "unions",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": (0..17)
                            .map(|index| (
                                format!("p{index}"),
                                json!({"type": ["string", "null"]})
                            ))
                            .collect::<Map<String, Value>>()
                    }),
                )],
                "at most 16 union parameters",
            ),
            (
                vec![tool(
                    "referencedUnions",
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": (0..17)
                            .map(|index| (
                                format!("p{index}"),
                                json!({"$ref": "#/$defs/maybeString"})
                            ))
                            .collect::<Map<String, Value>>(),
                        "$defs": {
                            "maybeString": {"type": ["string", "null"]}
                        }
                    }),
                )],
                "at most 16 union parameters",
            ),
        ];

        for (tools, expected) in cases {
            assert!(
                validate_strict_tools(&tools, true, StrictToolProvider::Anthropic)
                    .expect_err("aggregate limit should be rejected")
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn compiles_natural_optional_schema_for_each_strict_dialect() {
        let natural_schema = json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1},
                "mode": {"type": "string", "enum": ["fast", "thorough"]},
                "options": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 1}
                    }
                }
            },
            "required": ["query"]
        });

        let mut openai_tools = vec![tool("search", natural_schema.clone())];
        prepare_strict_tools(&mut openai_tools, true, StrictToolProvider::OpenAi)
            .expect("natural schema should compile for OpenAI");
        let openai = &openai_tools[0].parameters;
        assert_eq!(openai["required"], json!(["query", "mode", "options"]));
        assert_eq!(
            openai["properties"]["mode"]["enum"],
            json!(["fast", "thorough", null])
        );
        assert_eq!(
            openai["properties"]["options"]["type"],
            json!(["object", "null"])
        );
        assert_eq!(
            openai["properties"]["options"]["required"],
            json!(["limit"])
        );
        assert_eq!(
            openai["properties"]["options"]["properties"]["limit"]["type"],
            json!(["integer", "null"])
        );
        assert_eq!(openai["additionalProperties"], false);
        assert_eq!(
            openai["properties"]["options"]["additionalProperties"],
            false
        );

        let mut anthropic_tools = vec![tool("search", natural_schema)];
        prepare_strict_tools(&mut anthropic_tools, true, StrictToolProvider::Anthropic)
            .expect("natural schema should compile for Anthropic");
        let anthropic = &anthropic_tools[0].parameters;
        assert_eq!(anthropic["required"], json!(["query"]));
        assert!(anthropic["properties"]["options"]["type"].is_string());
        assert!(anthropic["properties"]["query"].get("minLength").is_none());
        assert!(
            anthropic["properties"]["options"]["properties"]["limit"]
                .get("minimum")
                .is_none()
        );
        assert_eq!(anthropic["additionalProperties"], false);
        assert_eq!(
            anthropic["properties"]["options"]["additionalProperties"],
            false
        );

        let mut capped_tools = (0..=ANTHROPIC_MAX_STRICT_TOOLS)
            .map(|index| {
                tool(
                    &format!("tool{index}"),
                    json!({"type": "object", "properties": {}}),
                )
            })
            .collect::<Vec<_>>();
        capped_tools[0].origin = ToolOrigin::Extension;
        prepare_strict_tools(&mut capped_tools, true, StrictToolProvider::Anthropic)
            .expect("Anthropic overflow should degrade deterministically");
        assert!(!capped_tools[0].strict);
        assert!(capped_tools[1..].iter().all(|definition| definition.strict));

        let mut structural_union = vec![tool(
            "structuralUnion",
            json!({
                "type": "object",
                "properties": {"value": {}},
                "anyOf": [
                    {"properties": {"value": {"type": "string"}}},
                    {"properties": {"value": {"type": "integer"}}}
                ]
            }),
        )];
        assert!(
            prepare_strict_tools(&mut structural_union, true, StrictToolProvider::OpenAi)
                .expect_err("structural root unions must not be silently weakened")
                .to_string()
                .contains("$/anyOf")
        );

        let mut external_constraint = vec![tool(
            "externalConstraint",
            json!({
                "type": "object",
                "properties": {"count": {"type": "integer", "minimum": 1}}
            }),
        )];
        external_constraint[0].origin = ToolOrigin::Extension;
        assert!(
            prepare_strict_tools(
                &mut external_constraint,
                true,
                StrictToolProvider::Anthropic
            )
            .expect_err("third-party constraints must not be silently removed")
            .to_string()
            .contains("$/properties/count/minimum")
        );
    }

    #[test]
    fn compiles_all_first_party_non_mcp_tool_schemas() {
        let mut definitions = Vec::new();
        let states = astrcode_bundled_extensions::bundled_extension_ids()
            .into_iter()
            .map(|id| (id.to_string(), true))
            .collect::<BTreeMap<_, _>>();
        for extension in astrcode_bundled_extensions::bundled_extensions(&states) {
            let manifest = extension.manifest();
            if manifest.id() == "astrcode-mcp" {
                continue;
            }
            let mut registrar = Registrar::new();
            extension.register(&mut registrar);
            let (_, registrations) = registrar
                .finish(manifest)
                .expect("bundled extension registrations should match its manifest");
            definitions.extend(
                registrations
                    .tools()
                    .iter()
                    .map(|registration| registration.definition().clone()),
            );
        }
        let mut openai_definitions = definitions.clone();
        prepare_strict_tools(&mut openai_definitions, true, StrictToolProvider::OpenAi)
            .expect("all first-party schemas should compile for OpenAI");
        assert!(
            openai_definitions
                .iter()
                .all(|definition| definition.strict)
        );

        let mut anthropic_bundled = definitions
            .iter()
            .filter(|definition| definition.origin == ToolOrigin::Bundled)
            .cloned()
            .collect::<Vec<_>>();
        for definition in &mut anthropic_bundled {
            compile_anthropic_schema(&mut definition.parameters, true);
        }
        let bundled_refs = anthropic_bundled.iter().collect::<Vec<_>>();
        let bundled_stats = collect_anthropic_schema_stats(&bundled_refs)
            .expect("compiled bundled tools should be valid Anthropic schemas");

        prepare_strict_tools(&mut definitions, true, StrictToolProvider::Anthropic)
            .expect("all first-party schemas should compile or deterministically degrade");
        let downgraded_bundled = definitions
            .iter()
            .filter(|definition| definition.origin == ToolOrigin::Bundled && !definition.strict)
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert!(
            !downgraded_bundled.is_empty(),
            "Anthropic's strict aggregate limits should downgrade overflow from optional={}, \
             unions={}",
            bundled_stats.optional_parameters,
            bundled_stats.union_parameters,
        );
        let accepted = definitions
            .iter()
            .filter(|definition| definition.strict)
            .collect::<Vec<_>>();
        validate_anthropic_tools(&accepted)
            .expect("the accepted first-party strict subset must satisfy aggregate limits");
        for coding_tool in [
            "read",
            "read_tool_result",
            "write",
            "edit",
            "patch",
            "glob",
            "grep",
            "shell",
        ] {
            assert!(
                definitions
                    .iter()
                    .any(|definition| definition.name == coding_tool && definition.strict),
                "bundled coding tool {coding_tool} should remain in the prioritized strict subset"
            );
        }
    }

    #[test]
    fn skips_validation_when_profile_capability_is_disabled() {
        let invalid = tool("legacy", json!({"type": "string"}));
        validate_strict_tools(&[invalid], false, StrictToolProvider::OpenAi)
            .expect("disabled capability should preserve legacy behavior");
    }

    fn nested_openai_object(depth: usize) -> Value {
        let mut schema = json!({"type": "string"});
        for index in 0..depth {
            let property_name = format!("level{index}");
            let mut properties = Map::new();
            properties.insert(property_name.clone(), schema);
            schema = json!({
                "type": "object",
                "properties": properties,
                "required": [property_name],
                "additionalProperties": false
            });
        }
        schema
    }
}
