//! Bundled MCP extension.
//!
//! MCP servers are discovered from Astrcode-owned config files and exposed as
//! ordinary bundled extension tools. The server composition root only registers
//! this extension; stdio process handling and MCP protocol details stay here.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};

use astrcode_core::{
    extension::{
        DiscoveredTool, Extension, ExtensionError, PromptBuildContext, PromptBuildHandler,
        PromptContributions, Registrar, ToolDiscoveryHandler, ToolHandler,
    },
    tool::{
        DEFERRED_TOOLS_METADATA_KEY, ExecutionMode, ToolDefinition, ToolExecutionContext,
        ToolOrigin, ToolPromptMetadata, ToolPromptTag, ToolResult, tool_metadata,
    },
};
use serde_json::{Value, json};

use crate::{
    client::StdioMcpClient,
    config::McpServerConfig,
    names::build_tool_name,
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
const MCP_DEFERRED_GROUP: &str = "mcp";

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(McpExtension)
}

struct McpExtension;

#[async_trait::async_trait]
impl Extension for McpExtension {
    fn id(&self) -> &str {
        EXTENSION_ID
    }

    fn register(&self, reg: &mut Registrar) {
        let shared = Arc::new(McpShared::new());
        reg.tool_discovery(Arc::new(McpToolDiscovery {
            shared: shared.clone(),
        }));
        reg.tool_metadata(mcp_tool_metadata());
        reg.on_prompt_build(0, Arc::new(McpPromptBuildHandler));
    }
}

// ─── Shared Cache ───────────────────────────────────────────────────────

/// MCP 发现结果缓存，在 tool discovery 和 tool execution 之间共享。
///
/// Keyed by working_dir. Populated during `McpToolDiscovery::discover`,
/// consumed during `McpToolHandler::execute` and `tool_search_tool` calls.
struct McpShared {
    cache: Mutex<HashMap<String, Arc<McpCacheEntry>>>,
}

struct McpCacheEntry {
    /// normalized tool name -> (server config, original tool name)
    tool_lookup: HashMap<String, (McpServerConfig, String)>,
    /// search candidates for tool_search_tool
    candidates: Vec<SearchCandidate>,
    diagnostics: Vec<String>,
}

impl McpShared {
    fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn get_entry(&self, working_dir: &str) -> Option<Arc<McpCacheEntry>> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(working_dir)
            .cloned()
    }

    fn store(&self, working_dir: &str, entry: McpCacheEntry) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(working_dir.to_string(), Arc::new(entry));
    }
}

// ─── Discovery ──────────────────────────────────────────────────────────

struct McpToolDiscovery {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl ToolDiscoveryHandler for McpToolDiscovery {
    async fn discover(&self, working_dir: &str) -> Vec<DiscoveredTool> {
        let discovered = discover_mcp_tools(working_dir).await;
        warn_diagnostics(&discovered.diagnostics);
        if discovered.tools.is_empty() {
            return Vec::new();
        }

        // Build tool lookup: normalized name -> (server config, original tool name)
        let server_map: HashMap<&str, &McpServerConfig> = discovered
            .servers
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        let mut tool_lookup = HashMap::new();
        for candidate in &discovered.tools {
            if let Some(server) = server_map.get(candidate.server.as_str()) {
                tool_lookup.insert(
                    candidate.definition.name.clone(),
                    ((*server).clone(), candidate.tool.clone()),
                );
            }
        }

        self.shared.store(
            working_dir,
            McpCacheEntry {
                tool_lookup,
                candidates: discovered.tools.clone(),
                diagnostics: discovered.diagnostics,
            },
        );

        let handler = Arc::new(McpToolHandler {
            shared: self.shared.clone(),
        });
        let mut result = vec![DiscoveredTool {
            definition: tool_search_tool_definition(),
            handler: handler.clone() as Arc<dyn ToolHandler>,
            prompt_metadata: Some(tool_search_metadata()),
        }];
        for candidate in discovered.tools {
            result.push(DiscoveredTool {
                definition: candidate.definition,
                handler: handler.clone() as Arc<dyn ToolHandler>,
                prompt_metadata: Some(mcp_concrete_tool_metadata()),
            });
        }
        result
    }
}

// ─── Tool Handler ───────────────────────────────────────────────────────

struct McpToolHandler {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl ToolHandler for McpToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        working_dir: &str,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name == TOOL_SEARCH_TOOL_NAME {
            return Ok(self.handle_tool_search(arguments, working_dir).await);
        }

        let entry = self.shared.get_entry(working_dir);
        let Some(cached) = entry.as_ref().and_then(|e| e.tool_lookup.get(tool_name)) else {
            return Err(ExtensionError::NotFound(tool_name.into()));
        };

        let (server, original_tool) = cached;
        match StdioMcpClient::new(server.clone())
            .call_tool(original_tool, arguments)
            .await
        {
            Ok(result) => Ok(call_result(&server.name, original_tool, result)),
            Err(error) => Ok(error_result(
                format!("failed to call MCP tool '{original_tool}': {error}"),
                tool_metadata([
                    ("server", json!(server.name)),
                    ("tool", json!(original_tool)),
                ]),
            )),
        }
    }
}

impl McpToolHandler {
    async fn handle_tool_search(&self, arguments: Value, working_dir: &str) -> ToolResult {
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

        // Use cached candidates when available
        let (candidates, diagnostics) = if let Some(entry) = self.shared.get_entry(working_dir) {
            (entry.candidates.clone(), entry.diagnostics.clone())
        } else {
            let discovered = discover_mcp_tools(working_dir).await;
            if !discovered.tools.is_empty() {
                self.shared.store(
                    working_dir,
                    McpCacheEntry {
                        tool_lookup: HashMap::new(),
                        candidates: discovered.tools.clone(),
                        diagnostics: discovered.diagnostics.clone(),
                    },
                );
            }
            (discovered.tools, discovered.diagnostics)
        };

        warn_diagnostics(&diagnostics);
        let output = search_mcp_tools(&candidates, args);
        let mut metadata = BTreeMap::new();
        metadata.insert(
            DEFERRED_TOOLS_METADATA_KEY.into(),
            json!({
                "group": MCP_DEFERRED_GROUP,
                "matches": output
                    .matches
                    .iter()
                    .map(|candidate| candidate.definition.name.clone())
                    .collect::<Vec<_>>(),
            }),
        );
        if !diagnostics.is_empty() {
            metadata.insert("diagnostics".into(), json!(diagnostics));
        }
        text_result(search::render_search_output(&output), false, None, metadata)
    }
}

// ─── PromptBuild ────────────────────────────────────────────────────────

struct McpPromptBuildHandler;

#[async_trait::async_trait]
impl PromptBuildHandler for McpPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let has_tool_search = ctx.tools.iter().any(|t| t.name == TOOL_SEARCH_TOOL_NAME);
        if has_tool_search {
            Ok(PromptContributions {
                additional_instructions: vec![mcp_discovery_instructions().into()],
                ..Default::default()
            })
        } else {
            Ok(PromptContributions::default())
        }
    }
}

fn mcp_tool_metadata() -> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata>
{
    let mut map = std::collections::HashMap::new();
    map.insert(TOOL_SEARCH_TOOL_NAME.to_string(), tool_search_metadata());
    map
}

fn tool_search_metadata() -> ToolPromptMetadata {
    ToolPromptMetadata::new(
        "Builtin tools do not need discovery. Call `tool_search_tool` only when builtin tools are \
         insufficient and you need an external MCP tool's schema.",
    )
    .caveat(
        "After `tool_search_tool` returns, call the concrete `mcp__...` tool directly with the \
         shown schema. Do not call `tool_search_tool` again with the same query.",
    )
    .caveat(
        "If the result reports zero matches, broaden the query or accept that no MCP tool fits — \
         do not retry the same query.",
    )
    .prompt_tag(ToolPromptTag::Discovery)
    .deferred_discovery_gate(MCP_DEFERRED_GROUP)
}

fn mcp_concrete_tool_metadata() -> ToolPromptMetadata {
    ToolPromptMetadata::default().deferred_discovery_group(MCP_DEFERRED_GROUP)
}

// ─── Discovery helpers ──────────────────────────────────────────────────

struct DiscoveredMcpTools {
    tools: Vec<SearchCandidate>,
    servers: Vec<McpServerConfig>,
    diagnostics: Vec<String>,
}

async fn discover_mcp_tools(working_dir: &str) -> DiscoveredMcpTools {
    let config = config::load_config(working_dir);
    let mut diagnostics = config.diagnostics;
    let servers = config.servers;

    let results: Vec<(String, Result<Vec<McpTool>, _>)> =
        futures::future::join_all(servers.iter().map(|server| async {
            let name = server.name.clone();
            let result = StdioMcpClient::new(server.clone()).list_tools().await;
            (name, result)
        }))
        .await;

    let mut emitted = BTreeSet::new();
    let mut candidates = Vec::new();
    for (server_name, list_result) in results {
        match list_result {
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
        servers,
        diagnostics,
    }
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
        description: "Find an external MCP tool by name or keyword and return its schema. Call \
                      this only when builtin tools \
                      (`read`/`grep`/`find`/`edit`/`patch`/`write`/`shell`) cannot accomplish the \
                      task. After it returns, call the matching `mcp__...` tool directly using \
                      the schema shown."
            .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword(s), partial tool name, or `select:mcp__server__tool` for exact pick. Prefix `+term` to require a term."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 5,
                    "description": "Max matches to return."
                }
            },
            "required": ["query"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Parallel,
    }
}

fn mcp_discovery_instructions() -> &'static str {
    "MCP discovery workflow:\n1. Check whether builtin tools already solve the task.\n2. If an \
     external MCP tool is needed or a visible `mcp__...` tool has unclear parameters, call \
     `tool_search_tool` first with part of the tool name or task purpose, for example `{ \
     \"query\": \"webReader\" }` or `{ \"query\": \"github repo structure\" }`.\n3. Read the \
     returned input schema before making the external tool call.\n4. Pick the matching concrete \
     `mcp__...` tool and call it directly. Do not guess argument names when schema is available."
}

fn call_result(server: &str, tool: &str, result: CallToolResult) -> ToolResult {
    let content = protocol::render_call_content(&result);
    let mut metadata = tool_metadata([("server", json!(server)), ("tool", json!(tool))]);
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

    #[test]
    fn mcp_discovery_instruction_is_additional_instruction_content() {
        let instruction = mcp_discovery_instructions();

        assert!(instruction.starts_with("MCP discovery workflow:"));
        assert!(instruction.contains("`tool_search_tool`"));
        assert!(!instruction.contains("[Example Workflow]"));
    }
}
