//! Bundled MCP extension.
//!
//! MCP servers are discovered from Astrcode-owned config files and exposed as
//! ordinary bundled extension tools. The server composition root only registers
//! this extension; stdio process handling and MCP protocol details stay here.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect, HookSubscription,
    },
    tool::{ExecutionMode, ToolDefinition, ToolExecutionContext, ToolOrigin, ToolResult},
};
use serde_json::{Value, json};

use crate::{
    client::StdioMcpClient,
    config::McpServerConfig,
    names::{build_tool_name, normalized_name_matches, parse_tool_name},
    protocol::{CallToolResult, McpTool},
    search::{SearchCandidate, ToolSearchArgs, search_mcp_tools},
};

mod client;
mod config;
mod names;
mod protocol;
mod search;

const EXTENSION_ID: &str = "astrcode-mcp";
const TOOL_SEARCH_TOOL_NAME: &str = "tool_search_tool";

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(McpExtension)
}

struct McpExtension;

#[async_trait::async_trait]
impl Extension for McpExtension {
    fn id(&self) -> &str {
        EXTENSION_ID
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        Vec::new()
    }

    async fn on_event(
        &self,
        _event: ExtensionEvent,
        _ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        Ok(HookEffect::Allow)
    }

    async fn tools_for(&self, working_dir: &str) -> Vec<ToolDefinition> {
        let discovered = discover_mcp_tools(working_dir).await;
        warn_diagnostics(&discovered.diagnostics);
        if discovered.tools.is_empty() {
            return Vec::new();
        }

        let mut definitions = vec![tool_search_tool_definition()];
        definitions.extend(
            discovered
                .tools
                .into_iter()
                .map(|candidate| candidate.definition),
        );
        definitions
    }

    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        working_dir: &str,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name == TOOL_SEARCH_TOOL_NAME {
            return Ok(handle_tool_search(arguments, working_dir).await);
        }

        let Some(parsed) = parse_tool_name(tool_name) else {
            return Err(ExtensionError::NotFound(tool_name.into()));
        };

        let config = config::load_config(working_dir);
        warn_diagnostics(&config.diagnostics);

        let Some(server) = config
            .servers
            .into_iter()
            .find(|server| normalized_name_matches(&server.name, &parsed.server))
        else {
            return Ok(error_result(
                format!("MCP server '{}' is not configured", parsed.server),
                metadata([("server", json!(parsed.server))]),
            ));
        };

        let original_tool = match resolve_original_tool_name(&server, tool_name).await {
            Ok(Some(name)) => name,
            Ok(None) => {
                return Ok(error_result(
                    format!(
                        "MCP tool '{tool_name}' is no longer exposed by server '{}'",
                        server.name
                    ),
                    metadata([("server", json!(server.name))]),
                ));
            },
            Err(error) => {
                return Ok(error_result(
                    format!(
                        "failed to refresh MCP tool list for server '{}': {error}",
                        server.name
                    ),
                    metadata([("server", json!(server.name))]),
                ));
            },
        };

        match StdioMcpClient::new(server.clone())
            .call_tool(&original_tool, arguments)
            .await
        {
            Ok(result) => Ok(call_result(&server.name, &original_tool, result)),
            Err(error) => Ok(error_result(
                format!("failed to call MCP tool '{}': {error}", original_tool),
                metadata([
                    ("server", json!(server.name)),
                    ("tool", json!(original_tool)),
                ]),
            )),
        }
    }
}

struct DiscoveredMcpTools {
    tools: Vec<SearchCandidate>,
    diagnostics: Vec<String>,
}

async fn discover_mcp_tools(working_dir: &str) -> DiscoveredMcpTools {
    let config = config::load_config(working_dir);
    let mut diagnostics = config.diagnostics;

    let mut emitted = BTreeSet::new();
    let mut candidates = Vec::new();
    for server in config.servers {
        let server_name = server.name.clone();
        match StdioMcpClient::new(server.clone()).list_tools().await {
            Ok(tools) => {
                for tool in tools {
                    let Some(definition) = tool_definition(&server_name, &tool) else {
                        let diagnostic = format!(
                            "skip MCP tool with empty normalized name: server={}, tool={}",
                            server_name, tool.name
                        );
                        tracing::warn!("{diagnostic}");
                        diagnostics.push(diagnostic);
                        continue;
                    };
                    if emitted.insert(definition.name.clone()) {
                        candidates.push(SearchCandidate {
                            definition,
                            server: server_name.clone(),
                            tool: tool.name,
                        });
                    } else {
                        let diagnostic = format!(
                            "skip duplicate MCP tool name after normalization: {}",
                            definition.name
                        );
                        tracing::warn!("{diagnostic}");
                        diagnostics.push(diagnostic);
                    }
                }
            },
            Err(error) => {
                let diagnostic = format!("discover MCP tools from server {server_name}: {error}");
                tracing::warn!("{diagnostic}");
                diagnostics.push(diagnostic);
            },
        }
    }

    DiscoveredMcpTools {
        tools: candidates,
        diagnostics,
    }
}

async fn handle_tool_search(arguments: Value, working_dir: &str) -> ToolResult {
    let args = match serde_json::from_value::<ToolSearchArgs>(arguments) {
        Ok(args) if !args.query.trim().is_empty() => args,
        Ok(_) => {
            return error_result(
                "invalid tool_search_tool input: query must not be empty".into(),
                BTreeMap::new(),
            );
        },
        Err(error) => {
            return error_result(
                format!("invalid tool_search_tool input: {error}"),
                BTreeMap::new(),
            );
        },
    };

    let discovered = discover_mcp_tools(working_dir).await;
    warn_diagnostics(&discovered.diagnostics);
    let output = search_mcp_tools(&discovered.tools, args);
    let mut metadata = BTreeMap::new();
    metadata.insert("toolSearch".into(), search::output_metadata(&output));
    if !discovered.diagnostics.is_empty() {
        metadata.insert("diagnostics".into(), json!(discovered.diagnostics));
    }
    text_result(search::render_search_output(&output), false, None, metadata)
}

async fn resolve_original_tool_name(
    server: &McpServerConfig,
    emitted_name: &str,
) -> Result<Option<String>, client::McpClientError> {
    let tools = StdioMcpClient::new(server.clone()).list_tools().await?;
    Ok(tools
        .into_iter()
        .find(|tool| build_tool_name(&server.name, &tool.name).as_deref() == Some(emitted_name))
        .map(|tool| tool.name))
}

fn tool_definition(server_name: &str, tool: &McpTool) -> Option<ToolDefinition> {
    Some(ToolDefinition {
        name: build_tool_name(server_name, &tool.name)?,
        description: match tool
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            Some(description) => format!("MCP tool from server '{server_name}': {description}"),
            None => format!("MCP tool from server '{server_name}'."),
        },
        parameters: tool
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    })
}

fn tool_search_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_SEARCH_TOOL_NAME.into(),
        description: "Search configured MCP tools by exact name or keywords and return matching \
                      tool schemas."
            .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "MCP tool search query. Use \"select:mcp__server__tool\" for exact selection, keywords for search, or +term to require a term."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 5,
                    "description": "Maximum number of matching MCP tools to return."
                }
            },
            "required": ["query"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Parallel,
    }
}

fn call_result(server: &str, tool: &str, result: CallToolResult) -> ToolResult {
    let content = protocol::render_call_content(&result);
    let mut metadata = metadata([("server", json!(server)), ("tool", json!(tool))]);
    if let Some(structured) = result.structured_content {
        metadata.insert("structuredContent".into(), structured);
    }
    if let Some(meta) = result.meta {
        metadata.insert("mcpMeta".into(), meta);
    }
    text_result(
        content.clone(),
        result.is_error,
        result.is_error.then_some(content),
        metadata,
    )
}

fn error_result(content: String, metadata: BTreeMap<String, Value>) -> ToolResult {
    text_result(content.clone(), true, Some(content), metadata)
}

fn text_result(
    content: String,
    is_error: bool,
    error: Option<String>,
    metadata: BTreeMap<String, Value>,
) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content,
        is_error,
        error,
        metadata,
        duration_ms: None,
    }
}

fn metadata<const N: usize>(entries: [(&str, Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn warn_diagnostics(diagnostics: &[String]) {
    for diagnostic in diagnostics {
        tracing::warn!("{diagnostic}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_mcp_tool_to_bundled_tool_definition() {
        let def = tool_definition(
            "File System",
            &McpTool {
                name: "Read File".into(),
                description: Some("Read a file".into()),
                input_schema: Some(json!({"type": "object"})),
            },
        )
        .unwrap();

        assert_eq!(def.name, "mcp__file_system__read_file");
        assert_eq!(def.origin, ToolOrigin::Bundled);
        assert_eq!(def.parameters, json!({"type": "object"}));
    }

    #[test]
    fn tool_search_tool_is_bundled_and_read_only_parallel() {
        let def = tool_search_tool_definition();

        assert_eq!(def.name, TOOL_SEARCH_TOOL_NAME);
        assert_eq!(def.origin, ToolOrigin::Bundled);
        assert_eq!(def.execution_mode, ExecutionMode::Parallel);
    }
}
