//! 各 provider strict 工具 schema 的限额与合规校验。

use std::collections::{HashMap, HashSet};

use astrcode_core::{llm::LlmError, tool::ToolDefinition};
use serde_json::{Map, Value};

use super::{
    StrictToolProvider,
    traverse::{child_path, is_object_schema_object, is_union_schema, visit_child_schemas},
};

const OPENAI_MAX_OBJECT_PROPERTIES: usize = 5_000;
const OPENAI_MAX_NESTING_DEPTH: usize = 10;
const OPENAI_MAX_SCHEMA_STRING_CHARS: usize = 120_000;
const OPENAI_MAX_ENUM_VALUES: usize = 1_000;
const OPENAI_LARGE_ENUM_THRESHOLD: usize = 250;
const OPENAI_MAX_LARGE_ENUM_STRING_CHARS: usize = 15_000;
pub(super) const ANTHROPIC_MAX_STRICT_TOOLS: usize = 20;
pub(super) const ANTHROPIC_MAX_OPTIONAL_PARAMETERS: usize = 24;
pub(super) const ANTHROPIC_MAX_UNION_PARAMETERS: usize = 16;

pub(super) fn validate_strict_tools(
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
pub(super) struct AnthropicSchemaStats {
    pub(super) optional_parameters: usize,
    pub(super) union_parameters: usize,
}

pub(super) fn validate_anthropic_tools(strict_tools: &[&ToolDefinition]) -> Result<(), LlmError> {
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

pub(super) fn validate_anthropic_tool(tool: &ToolDefinition) -> Result<(), LlmError> {
    collect_anthropic_schema_stats(&[tool]).map(|_| ())
}

pub(super) fn collect_anthropic_schema_stats(
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

fn schema_error(tool: &ToolDefinition, path: &str, message: &str) -> LlmError {
    LlmError::Unsupported {
        message: format!("strict tool `{}` schema at `{path}`: {message}", tool.name),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{super::tool, *};

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
