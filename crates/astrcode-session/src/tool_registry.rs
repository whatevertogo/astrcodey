//! Immutable tool registry snapshots owned by the session layer.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::Arc,
};

use astrcode_core::{
    tool::{
        ExecutionMode, SessionToolSelection, Tool, ToolDefinition, ToolError, ToolExecutionContext,
        ToolPromptMetadata, ToolResult,
    },
    tool_access::ResourceAccess,
};
use serde_json::Value;

/// Registered tool plus the metadata cached from its implementation.
#[derive(Clone)]
struct RegisteredTool {
    tool: Arc<dyn Tool>,
    definition: ToolDefinition,
    prompt_metadata: Option<ToolPromptMetadata>,
}

/// Registry of tools available to a session runtime.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let mut definition = tool.definition();
        definition.execution_mode = tool.execution_mode();
        let name = definition.name.clone();
        let prompt_metadata = tool.prompt_metadata();
        if self.tools.contains_key(&name) {
            tracing::warn!("Tool '{}' already registered, overwriting", name);
        }
        self.tools.insert(
            name,
            RegisteredTool {
                tool,
                definition,
                prompt_metadata,
            },
        );
    }

    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    pub fn list_definitions_with_prompt_metadata(
        &self,
    ) -> Vec<(ToolDefinition, Option<ToolPromptMetadata>)> {
        self.tools
            .values()
            .map(|entry| (entry.definition.clone(), entry.prompt_metadata.clone()))
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        mut args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        match self.tools.get(name) {
            Some(entry) => {
                if entry.definition.strict {
                    normalize_strict_arguments(
                        &mut args,
                        &entry.definition.parameters,
                        &entry.definition.parameters,
                    );
                }
                entry.tool.execute(args, ctx).await
            },
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    pub fn execution_mode(&self, name: &str) -> ExecutionMode {
        self.tools
            .get(name)
            .map(|entry| entry.definition.execution_mode)
            .unwrap_or(ExecutionMode::Sequential)
    }

    pub fn resource_accesses(
        &self,
        name: &str,
        args: &serde_json::Value,
        working_dir: &Path,
    ) -> Result<Vec<ResourceAccess>, ToolError> {
        match self.tools.get(name) {
            Some(entry) => entry.tool.resource_accesses(args, working_dir),
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    pub fn find_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.get(name).map(|entry| entry.definition.clone())
    }

    pub(crate) fn filtered(&self, selection: &SessionToolSelection) -> Self {
        let tools = match selection {
            SessionToolSelection::All { except } => {
                let excluded = except.iter().map(String::as_str).collect::<HashSet<_>>();
                self.tools
                    .iter()
                    .filter(|(name, _)| !excluded.contains(name.as_str()))
                    .map(|(name, tool)| (name.clone(), tool.clone()))
                    .collect()
            },
            SessionToolSelection::Only { names } => names
                .iter()
                .filter_map(|name| self.tools.get_key_value(name))
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
        };
        Self { tools }
    }
}

fn normalize_strict_arguments(value: &mut Value, schema: &Value, root_schema: &Value) {
    let schema = resolve_local_schema(schema, root_schema).unwrap_or(schema);
    match (value, schema) {
        (Value::Object(arguments), Value::Object(schema)) => {
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<HashSet<_>>();
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            let names = arguments.keys().cloned().collect::<Vec<_>>();
            for name in names {
                let Some(property_schema) = properties.get(&name) else {
                    continue;
                };
                let remove_provider_null = arguments.get(&name).is_some_and(|argument| {
                    argument.is_null()
                        && !required.contains(name.as_str())
                        && !schema_allows_null(property_schema, root_schema, &mut Vec::new())
                });
                if remove_provider_null {
                    arguments.remove(&name);
                } else if let Some(argument) = arguments.get_mut(&name) {
                    normalize_strict_arguments(argument, property_schema, root_schema);
                }
            }
        },
        (Value::Array(arguments), Value::Object(schema)) => {
            if let Some(item_schema) = schema.get("items") {
                for argument in arguments {
                    normalize_strict_arguments(argument, item_schema, root_schema);
                }
            }
        },
        _ => {},
    }
}

fn resolve_local_schema<'a>(schema: &'a Value, root_schema: &'a Value) -> Option<&'a Value> {
    let reference = schema.get("$ref")?.as_str()?;
    let pointer = reference.strip_prefix('#')?;
    root_schema.pointer(pointer)
}

fn schema_allows_null(schema: &Value, root_schema: &Value, visited_refs: &mut Vec<String>) -> bool {
    if schema.is_null()
        || schema.get("const").is_some_and(Value::is_null)
        || schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(Value::is_null))
    {
        return true;
    }
    if schema
        .get("type")
        .is_some_and(|schema_type| match schema_type {
            Value::String(kind) => kind == "null",
            Value::Array(kinds) => kinds.iter().any(|kind| kind == "null"),
            _ => false,
        })
    {
        return true;
    }
    if ["anyOf", "oneOf"].iter().any(|keyword| {
        schema
            .get(keyword)
            .and_then(Value::as_array)
            .is_some_and(|branches| {
                branches
                    .iter()
                    .any(|branch| schema_allows_null(branch, root_schema, visited_refs))
            })
    }) {
        return true;
    }

    let Some(reference) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .filter(|reference| !visited_refs.iter().any(|visited| visited == reference))
    else {
        return false;
    };
    visited_refs.push(reference.to_string());
    resolve_local_schema(schema, root_schema)
        .is_some_and(|resolved| schema_allows_null(resolved, root_schema, visited_refs))
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NamedTool(&'static str, ExecutionMode);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.to_string(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
                origin: astrcode_core::tool::ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            }
        }

        fn execution_mode(&self) -> ExecutionMode {
            self.1
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolResult, ToolError> {
            unreachable!("registry tests do not execute tools")
        }
    }

    #[test]
    fn list_definitions_is_sorted_by_tool_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("zeta", ExecutionMode::Sequential)));
        registry.register(Arc::new(NamedTool("alpha", ExecutionMode::Sequential)));
        registry.register(Arc::new(NamedTool("middle", ExecutionMode::Sequential)));

        let names = registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["alpha", "middle", "zeta"]);
    }

    #[test]
    fn list_definitions_carries_tool_execution_mode() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("parallel", ExecutionMode::Parallel)));

        let definition = registry.find_definition("parallel").unwrap();
        assert_eq!(definition.execution_mode, ExecutionMode::Parallel);
    }

    #[test]
    fn session_tool_selection_derives_filtered_registry() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("read", ExecutionMode::Parallel)));
        registry.register(Arc::new(NamedTool("shell", ExecutionMode::Sequential)));

        let without_shell = registry.filtered(&SessionToolSelection::All {
            except: vec!["shell".into()],
        });
        assert!(without_shell.find_definition("shell").is_none());
        assert!(without_shell.find_definition("read").is_some());

        let only_shell = registry.filtered(&SessionToolSelection::Only {
            names: vec!["missing".into(), "shell".into(), "shell".into()],
        });
        assert!(only_shell.find_definition("read").is_none());
        assert!(only_shell.find_definition("shell").is_some());
    }

    #[test]
    fn strict_argument_normalization_only_removes_synthetic_optional_nulls() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "required": {"type": "string"},
                "optional": {"type": "string"},
                "nullable": {"type": ["string", "null"]},
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "flag": {"type": "boolean"}
                        }
                    }
                }
            },
            "required": ["required"]
        });
        let mut arguments = serde_json::json!({
            "required": "value",
            "optional": null,
            "nullable": null,
            "items": [{"flag": null}]
        });

        normalize_strict_arguments(&mut arguments, &schema, &schema);

        assert_eq!(
            arguments,
            serde_json::json!({
                "required": "value",
                "nullable": null,
                "items": [{}]
            })
        );
    }
}
