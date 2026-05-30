//! Bundled MCP extension.
//!
//! MCP servers are discovered from Astrcode-owned config files and exposed as
//! ordinary bundled extension tools. The extension owns a persistent process
//! pool and initializes servers for the startup workspace with the extension.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use astrcode_extension_sdk::{
    extension::{
        DiscoveredTool, Extension, ExtensionCapability, ExtensionCtx, ExtensionError,
        ExtensionEvent, HookMode, HookResult, LifecycleContext, LifecycleHandler,
        PromptBuildContext, PromptBuildHandler, PromptContributions, Registrar, StopReason,
        ToolDiscoveryHandler, ToolHandler,
    },
    tool::{
        DEFERRED_TOOLS_METADATA_KEY, ExecutionMode, ToolDefinition, ToolExecutionContext,
        ToolOrigin, ToolPromptMetadata, ToolPromptTag, ToolResult, tool_metadata,
    },
};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    config::{McpConfig, McpServerConfig},
    names::build_tool_name,
    pool::McpProcessPool,
    protocol::{McpTool, render_call_content},
    search::{SearchCandidate, ToolSearchArgs, search_mcp_tools},
};

mod config;
mod http_client;
mod names;
mod pool;
mod protocol;
mod search;

const EXTENSION_ID: &str = "astrcode-mcp";
const TOOL_SEARCH_TOOL_NAME: &str = "tool_search_tool";
const MCP_DEFERRED_GROUP: &str = "mcp";
const POOL_TIMEOUT: Duration = Duration::from_secs(20);

// ─── Extension entry point ────────────────────────────────────────────────

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(McpExtension {
        shared: Arc::new(McpShared::new(McpProcessPool::new(POOL_TIMEOUT))),
    })
}

struct McpExtension {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl Extension for McpExtension {
    fn id(&self) -> &str {
        EXTENSION_ID
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &[
            ExtensionCapability::WorkspaceRead,
            ExtensionCapability::ProcessSpawn,
            ExtensionCapability::NetworkClient,
        ]
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        let shared = Arc::clone(&self.shared);
        let startup_working_dir = ctx.startup_working_dir().map(str::to_owned);
        ctx.tasks().spawn("mcp-warm", async move {
            match startup_working_dir {
                Some(working_dir) => shared.refresh_workspace(&working_dir).await,
                None => shared.refresh_global().await,
            }
        });
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        self.shared.pool.shutdown().await;
        self.shared.clear();
        Ok(())
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        self.shared
            .pool
            .health()
            .await
            .map_err(|error| ExtensionError::Internal(error.to_string()))
    }

    fn register(&self, reg: &mut Registrar) {
        let lifecycle_handler = Arc::new(McpWorkspaceLifecycleHandler {
            shared: Arc::clone(&self.shared),
        });
        reg.on_event(
            ExtensionEvent::SessionStart,
            HookMode::NonBlocking,
            0,
            lifecycle_handler.clone(),
        );
        reg.on_event(
            ExtensionEvent::SessionResume,
            HookMode::NonBlocking,
            0,
            lifecycle_handler,
        );
        reg.tool_discovery(Arc::new(McpToolDiscovery {
            shared: self.shared.clone(),
        }));
        reg.tool_metadata(mcp_tool_metadata());
        reg.on_prompt_build(0, Arc::new(McpPromptBuildHandler));
    }
}

struct McpWorkspaceLifecycleHandler {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl LifecycleHandler for McpWorkspaceLifecycleHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        self.shared.refresh_workspace(&ctx.working_dir).await;
        Ok(HookResult::Allow)
    }
}

// ─── Shared Cache ───────────────────────────────────────────────────────

/// MCP discovery result cache + process pool, shared between tool discovery
/// and tool execution.
///
/// Cache is keyed by working_dir. Startup/session hooks prefill entries; tool
/// discovery synchronously fills only a cache miss.
struct McpShared {
    cache: Mutex<HashMap<String, Arc<McpCacheEntry>>>,
    refresh_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    pool: McpProcessPool,
}

struct McpCacheEntry {
    /// normalized tool name -> (server config, original tool name)
    tool_lookup: HashMap<String, (McpServerConfig, String)>,
    /// search candidates for tool_search_tool
    candidates: Vec<SearchCandidate>,
    diagnostics: Vec<String>,
}

impl McpShared {
    fn new(pool: McpProcessPool) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            refresh_locks: AsyncMutex::new(HashMap::new()),
            pool,
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

    fn clear(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    async fn refresh_global(&self) {
        self.refresh_if_missing("", config::load_global_only).await;
    }

    async fn refresh_workspace(&self, working_dir: &str) {
        self.refresh_if_missing(working_dir, || config::load_config(working_dir))
            .await;
    }

    async fn refresh_if_missing<F>(&self, working_dir: &str, load_config: F)
    where
        F: FnOnce() -> McpConfig + Send,
    {
        if self.get_entry(working_dir).is_some() {
            return;
        }
        let refresh_lock = {
            let mut refresh_locks = self.refresh_locks.lock().await;
            refresh_locks
                .entry(working_dir.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _refresh = refresh_lock.lock().await;
        if self.get_entry(working_dir).is_none() {
            self.refresh(working_dir, load_config()).await;
        }
    }

    async fn refresh(&self, working_dir: &str, config: McpConfig) {
        let results = self.pool.pre_warm(&config.servers).await;
        for (name, result) in &results {
            if let Err(e) = result {
                tracing::warn!("MCP server '{name}' pre-warm failed: {e}");
            }
        }
        let discovered = discover_from_pool(&self.pool, &config).await;
        self.store(working_dir, discovered.build_cache_entry());
    }
}

// ─── Discovery ──────────────────────────────────────────────────────────

struct McpToolDiscovery {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl ToolDiscoveryHandler for McpToolDiscovery {
    async fn discover(&self, working_dir: &str) -> Vec<DiscoveredTool> {
        if let Some(entry) = self.shared.get_entry(working_dir) {
            return self.build_discovered_tools(&entry);
        }
        // 后台预热若尚未完成，则首个 turn 在此同步等待同一次加载以保证工具完整。
        self.shared.refresh_workspace(working_dir).await;
        match self.shared.get_entry(working_dir) {
            Some(entry) => self.build_discovered_tools(&entry),
            None => Vec::new(),
        }
    }
}

impl McpToolDiscovery {
    fn build_discovered_tools(&self, entry: &McpCacheEntry) -> Vec<DiscoveredTool> {
        warn_diagnostics(&entry.diagnostics);
        if entry.tool_lookup.is_empty() && entry.candidates.is_empty() {
            return Vec::new();
        }

        let handler = Arc::new(McpToolHandler {
            shared: self.shared.clone(),
        });
        let mut result = vec![DiscoveredTool {
            definition: tool_search_tool_definition(),
            handler: handler.clone() as Arc<dyn ToolHandler>,
            prompt_metadata: Some(tool_search_metadata()),
        }];
        for candidate in &entry.candidates {
            result.push(DiscoveredTool {
                definition: candidate.definition.clone(),
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
        match self
            .shared
            .pool
            .call_tool(server, original_tool, arguments)
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

        let (candidates, diagnostics) = if let Some(entry) = self.shared.get_entry(working_dir) {
            (entry.candidates.clone(), entry.diagnostics.clone())
        } else {
            (Vec::new(), Vec::new())
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

fn mcp_tool_metadata()
-> std::collections::HashMap<String, astrcode_extension_sdk::tool::ToolPromptMetadata> {
    let mut map = std::collections::HashMap::new();
    map.insert(TOOL_SEARCH_TOOL_NAME.to_string(), tool_search_metadata());
    map
}

fn tool_search_metadata() -> ToolPromptMetadata {
    ToolPromptMetadata::new(String::new())
        .caveat(
            "After `tool_search_tool` returns, call the concrete `mcp__...` tool directly with \
             the shown schema. Do not call `tool_search_tool` again with the same query.",
        )
        .caveat(
            "If the result reports zero matches, broaden the query or accept that no MCP tool \
             fits — do not retry the same query.",
        )
        .prompt_tag(ToolPromptTag::Discovery)
        .deferred_discovery_gate(MCP_DEFERRED_GROUP)
}

fn mcp_concrete_tool_metadata() -> ToolPromptMetadata {
    ToolPromptMetadata::default().deferred_discovery_group(MCP_DEFERRED_GROUP)
}

// ─── Pool-based discovery ───────────────────────────────────────────────

struct DiscoveredMcpTools {
    tools: Vec<SearchCandidate>,
    servers: Vec<McpServerConfig>,
    diagnostics: Vec<String>,
}

impl DiscoveredMcpTools {
    fn build_cache_entry(self) -> McpCacheEntry {
        let server_map: HashMap<&str, &McpServerConfig> =
            self.servers.iter().map(|s| (s.name.as_str(), s)).collect();
        let mut tool_lookup = HashMap::new();
        for candidate in &self.tools {
            if let Some(server) = server_map.get(candidate.server.as_str()) {
                tool_lookup.insert(
                    candidate.definition.name.clone(),
                    ((*server).clone(), candidate.tool.clone()),
                );
            }
        }
        McpCacheEntry {
            tool_lookup,
            candidates: self.tools,
            diagnostics: self.diagnostics,
        }
    }
}

async fn discover_from_pool(pool: &McpProcessPool, config: &McpConfig) -> DiscoveredMcpTools {
    let mut diagnostics = config.diagnostics.clone();
    let servers = config.servers.clone();

    let results: Vec<(String, Result<Vec<McpTool>, _>)> =
        futures_util::future::join_all(servers.iter().map(|server| async {
            let name = server.name.clone();
            let result = pool.list_tools(server).await;
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
        description: "Find an external MCP tool by name or keyword and return its input schema \
                      (not execute it).\n\nWhen NOT to use:\n- Builtin tools suffice: \
                      `read`/`grep`/`glob`/`edit`/`patch`/`write`/`shell`\n- Guessing `mcp__...` \
                      argument names without a schema\n\nTips:\n- Task needs an external MCP \
                      capability\n- A visible `mcp__...` tool has unclear \
                      parameters\n\nWorkflow:\n1. Call `tool_search_tool` with tool name or task \
                      keywords (e.g. `\"webReader\"`, `\"github repo structure\"`; \
                      `select:mcp__server__tool` for exact pick).\n2. Read the returned input \
                      schema.\n3. Call the matching `mcp__...` tool directly — do not guess \
                      argument names."
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
    "MCP discovery workflow: covered by `tool_search_tool` — discover schema first, then call the \
     concrete `mcp__...` tool with returned arguments (never guess names)."
}

fn call_result(server: &str, tool: &str, result: crate::protocol::CallToolResult) -> ToolResult {
    let content = render_call_content(&result);
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
