//! Immutable tool registry snapshots owned by the session layer.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use astrcode_core::tool::{
    ExecutionMode, SessionToolSelection, Tool, ToolDefinition, ToolError, ToolExecutionContext,
    ToolExecutionResult, ToolPlanningContext, ToolPromptMetadata, access::ToolPlan,
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

/// 工具定义与其 prompt 元数据的配对，供 prompt 构建与 turn state 共享同一数据形状。
pub(crate) struct DefinitionWithPromptMetadata {
    pub(crate) definition: ToolDefinition,
    pub(crate) prompt_metadata: Option<ToolPromptMetadata>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolRegistryError {
    #[error("duplicate tool registered: {0}")]
    DuplicateTool(String),
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let mut definition = tool.definition();
        definition.execution_mode = tool.execution_mode();
        let name = definition.name.clone();
        let prompt_metadata = tool.prompt_metadata();
        if self.tools.contains_key(&name) {
            return Err(ToolRegistryError::DuplicateTool(name));
        }
        self.tools.insert(
            name,
            RegisteredTool {
                tool,
                definition,
                prompt_metadata,
            },
        );
        Ok(())
    }

    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    pub(crate) fn list_definitions_with_prompt_metadata(
        &self,
    ) -> Vec<DefinitionWithPromptMetadata> {
        self.tools
            .values()
            .map(|entry| DefinitionWithPromptMetadata {
                definition: entry.definition.clone(),
                prompt_metadata: entry.prompt_metadata.clone(),
            })
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolExecutionResult, ToolError> {
        match self.tools.get(name) {
            Some(entry) => entry.tool.execute(args, ctx).await,
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    /// Normalize provider quirks once, after input transforms and before admission/planning.
    pub(crate) fn normalize_final_arguments(
        &self,
        name: &str,
        args: &mut Value,
    ) -> Result<(), ToolError> {
        let entry = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.into()))?;
        normalize_stringified_booleans(args, &entry.definition.parameters);
        if entry.definition.strict {
            normalize_strict_arguments(
                args,
                &entry.definition.parameters,
                &entry.definition.parameters,
            );
        }
        Ok(())
    }

    pub async fn plan(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolPlanningContext,
    ) -> Result<ToolPlan, ToolError> {
        match self.tools.get(name) {
            Some(entry) => entry.tool.plan(args, ctx).await,
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    pub fn execution_mode(&self, name: &str) -> ExecutionMode {
        self.tools
            .get(name)
            .map(|entry| entry.definition.execution_mode)
            .unwrap_or(ExecutionMode::Sequential)
    }

    pub fn find_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.get(name).map(|entry| entry.definition.clone())
    }

    pub(crate) fn find_prompt_metadata(&self, name: &str) -> Option<ToolPromptMetadata> {
        self.tools
            .get(name)
            .and_then(|entry| entry.prompt_metadata.clone())
    }

    pub(crate) fn validate_discovered_tools(
        &self,
        source_tool: &str,
        gate: Option<&str>,
        tool_names: &[String],
    ) -> Result<(), String> {
        if tool_names.is_empty() {
            return Ok(());
        }
        let Some(gate) = gate else {
            return Err(format!(
                "tool `{source_tool}` returned discovered tools without declaring a discovery gate"
            ));
        };

        let mut seen = HashSet::new();
        for name in tool_names {
            let group = self
                .find_prompt_metadata(name)
                .and_then(|metadata| metadata.deferred_discovery_group);
            if group.as_deref() != Some(gate) {
                return Err(format!(
                    "tool `{source_tool}` cannot activate unknown or unauthorized deferred tool \
                     `{name}`"
                ));
            }
            if !seen.insert(name) {
                return Err(format!(
                    "tool `{source_tool}` returned duplicate deferred tool `{name}`"
                ));
            }
        }
        Ok(())
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

fn normalize_stringified_booleans(arguments: &mut Value, schema: &Value) -> usize {
    match arguments {
        Value::String(raw) if schema["type"] == "boolean" => {
            let normalized = match raw.trim().to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            if let Some(normalized) = normalized {
                *arguments = Value::Bool(normalized);
                1
            } else {
                0
            }
        },
        Value::Object(values) => schema["properties"]
            .as_object()
            .map(|properties| {
                values
                    .iter_mut()
                    .filter_map(|(name, value)| {
                        properties
                            .get(name)
                            .map(|field_schema| normalize_stringified_booleans(value, field_schema))
                    })
                    .sum()
            })
            .unwrap_or_default(),
        Value::Array(values) => match &schema["items"] {
            Value::Array(item_schemas) => values
                .iter_mut()
                .zip(item_schemas)
                .map(|(value, item_schema)| normalize_stringified_booleans(value, item_schema))
                .sum(),
            Value::Object(_) => values
                .iter_mut()
                .map(|value| normalize_stringified_booleans(value, &schema["items"]))
                .sum(),
            _ => 0,
        },
        _ => 0,
    }
}

fn normalize_strict_arguments(value: &mut Value, schema: &Value, root_schema: &Value) {
    // 属性 schema 常以局部 $ref 指向根 schema 中的定义；解析失败（外部引用或指针
    // 缺失）时回退到原 schema——只影响 null 归一化的覆盖范围，不改变参数本身。
    let schema = match resolve_local_schema(schema, root_schema) {
        Some(resolved) => resolved,
        None => {
            if schema.get("$ref").is_some() {
                tracing::warn!(
                    reference = %schema["$ref"],
                    "failed to resolve local $ref in strict tool schema; falling back to raw schema"
                );
            }
            schema
        },
    };
    match (value, schema) {
        (Value::Object(arguments), Value::Object(schema)) => {
            // strict 工具通常拒绝 schema 未允许的 null；主流 provider 对省略的可选参数
            // 常输出显式 null。仅当属性非必填（不在 required 内）且 schema 不允许 null
            // 时移除该 null，其余情况保留原值并递归归一化。
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
            // 数组元素按 items schema 递归归一化（元素内的 null 规则与对象属性一致）。
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
    struct DeferredTool(&'static str, &'static str);

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
                timeout_ms: None,
            }
        }

        fn execution_mode(&self) -> ExecutionMode {
            self.1
        }

        async fn plan(
            &self,
            _arguments: &serde_json::Value,
            _ctx: &ToolPlanningContext,
        ) -> Result<ToolPlan, ToolError> {
            Ok(ToolPlan::default())
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolExecutionResult, ToolError> {
            unreachable!("registry tests do not execute tools")
        }
    }

    #[async_trait::async_trait]
    impl Tool for DeferredTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.into(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
                origin: astrcode_core::tool::ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
                timeout_ms: None,
            }
        }

        fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
            Some(ToolPromptMetadata::default().deferred_discovery_group(self.1))
        }

        async fn plan(
            &self,
            _arguments: &serde_json::Value,
            _ctx: &ToolPlanningContext,
        ) -> Result<ToolPlan, ToolError> {
            Ok(ToolPlan::default())
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolExecutionResult, ToolError> {
            unreachable!("registry tests do not execute tools")
        }
    }

    #[test]
    fn list_definitions_is_sorted_by_tool_name() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(NamedTool("zeta", ExecutionMode::Sequential)))
            .unwrap();
        registry
            .register(Arc::new(NamedTool("alpha", ExecutionMode::Sequential)))
            .unwrap();
        registry
            .register(Arc::new(NamedTool("middle", ExecutionMode::Sequential)))
            .unwrap();

        let names = registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["alpha", "middle", "zeta"]);
    }

    #[test]
    fn duplicate_tool_registration_is_rejected_without_replacing_the_original() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(NamedTool("shared", ExecutionMode::Parallel)))
            .unwrap();

        let error = registry
            .register(Arc::new(NamedTool("shared", ExecutionMode::Sequential)))
            .unwrap_err();

        assert_eq!(error, ToolRegistryError::DuplicateTool("shared".to_owned()));
        assert_eq!(registry.execution_mode("shared"), ExecutionMode::Parallel);
    }

    #[test]
    fn discovered_tools_must_be_unique_known_members_of_the_declared_gate() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(DeferredTool("mcp_a", "mcp")))
            .unwrap();
        registry
            .register(Arc::new(DeferredTool("other_a", "other")))
            .unwrap();

        let cases = [
            (Some("mcp"), vec!["mcp_a".into()], true),
            (None, vec!["mcp_a".into()], false),
            (Some("mcp"), vec!["missing".into()], false),
            (Some("mcp"), vec!["other_a".into()], false),
            (Some("mcp"), vec!["mcp_a".into(), "mcp_a".into()], false),
        ];
        for (gate, names, expected) in cases {
            assert_eq!(
                registry
                    .validate_discovered_tools("discover", gate, &names)
                    .is_ok(),
                expected,
                "gate={gate:?}, names={names:?}"
            );
        }
    }

    #[test]
    fn list_definitions_carries_tool_execution_mode() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(NamedTool("parallel", ExecutionMode::Parallel)))
            .unwrap();

        let definition = registry.find_definition("parallel").unwrap();
        assert_eq!(definition.execution_mode, ExecutionMode::Parallel);
    }

    #[test]
    fn session_tool_selection_derives_filtered_registry() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(NamedTool("read", ExecutionMode::Parallel)))
            .unwrap();
        registry
            .register(Arc::new(NamedTool("shell", ExecutionMode::Sequential)))
            .unwrap();

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
    fn final_argument_normalization_handles_provider_booleans_and_optional_nulls() {
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
            "items": [{"flag": "TRUE"}, {"flag": null}]
        });

        assert_eq!(normalize_stringified_booleans(&mut arguments, &schema), 1);
        normalize_strict_arguments(&mut arguments, &schema, &schema);

        assert_eq!(
            arguments,
            serde_json::json!({
                "required": "value",
                "nullable": null,
                "items": [{"flag": true}, {}]
            })
        );
    }
}
