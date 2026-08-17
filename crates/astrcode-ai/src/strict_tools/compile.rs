//! 将工具 schema 编译为各 provider 的 strict JSON Schema 方言。

use std::collections::HashSet;

use astrcode_core::{
    llm::LlmError,
    tool::{ToolDefinition, ToolOrigin},
};
use serde_json::{Map, Value};

use super::{
    traverse::{
        is_object_schema_object, is_union_schema, visit_child_schemas_mut,
        visit_child_schemas_mut_count,
    },
    validate::{
        ANTHROPIC_MAX_OPTIONAL_PARAMETERS, ANTHROPIC_MAX_STRICT_TOOLS,
        ANTHROPIC_MAX_UNION_PARAMETERS, collect_anthropic_schema_stats, validate_anthropic_tool,
        validate_anthropic_tools,
    },
};

pub(super) fn compile_openai_tool_schema(schema: &mut Value) {
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

pub(super) fn prepare_anthropic_tools(tools: &mut [ToolDefinition]) -> Result<(), LlmError> {
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

fn tool_origin_priority(origin: ToolOrigin) -> u8 {
    match origin {
        ToolOrigin::Bundled => 0,
        ToolOrigin::Extension => 1,
    }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::tool::ToolOrigin;
    use astrcode_extension_sdk::extension::Registrar;
    use serde_json::json;

    use super::{
        super::{StrictToolProvider, prepare_strict_tools, tool},
        *,
    };

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
}
