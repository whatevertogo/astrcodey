//! Schema 遍历原语：三种遍历器（可变/计数/只读带路径）共享的子 schema 关键字清单与骨架。

use astrcode_core::llm::LlmError;
use serde_json::{Map, Value};

/// 三种遍历器共享的子 schema 关键字清单：数组形态与对象形态各一组。
const CHILD_SCHEMA_KEYWORDS: [&str; 4] = ["anyOf", "oneOf", "allOf", "prefixItems"];
const DEFINITION_KEYWORDS: [&str; 3] = ["$defs", "definitions", "patternProperties"];

/// 遍历所有子 schema；回调返回 `false` 时提前停止。三种遍历器共享的骨架。
pub(super) fn for_each_child_schema_mut(
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

pub(super) fn visit_child_schemas_mut_count(
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

pub(super) fn visit_child_schemas_mut(schema: &mut Value, mut visit: impl FnMut(&mut Value)) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for_each_child_schema_mut(object, |child| {
        visit(child);
        true
    });
}

pub(super) fn visit_child_schemas(
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

pub(super) fn child_path(parent: &str, segment: &str) -> String {
    format!("{parent}/{}", segment.replace('~', "~0").replace('/', "~1"))
}

pub(super) fn is_object_schema_object(object: &Map<String, Value>) -> bool {
    object.contains_key("properties")
        || match object.get("type") {
            Some(Value::String(kind)) => kind == "object",
            Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
            _ => false,
        }
}

pub(super) fn is_union_schema(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    object.contains_key("anyOf")
        || object
            .get("type")
            .and_then(Value::as_array)
            .is_some_and(|types| types.len() > 1)
}
