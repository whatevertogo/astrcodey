//! Bundled MCP extension.
//!
//! MCP servers are discovered from Astrcode-owned config files and exposed as
//! ordinary bundled extension tools. The extension owns a persistent process
//! pool and initializes servers for the startup workspace with the extension.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        DiscoveredTool, Extension, ExtensionCapability, ExtensionError, ExtensionManifest,
        ExtensionStartContext, ExtensionStopContext, HookMode, HookResult, LifecycleContext,
        LifecycleEvent, LifecycleHandler, PromptBuildContext, PromptBuildHandler,
        PromptContributions, Registrar, ToolContext, ToolDiscovery, ToolDiscoveryContext,
        ToolDiscoveryHandler, ToolHandler, ToolPlanContext,
    },
    tool::{
        ToolDefinition, ToolExecutionPolicy, ToolExecutionResult, ToolOrigin, ToolPlan,
        ToolPromptMetadata, ToolPromptTag, ToolResult, tool_metadata,
    },
};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify};

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
const MCP_INITIAL_WARM_TIMEOUT: Duration = Duration::from_secs(90);

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
    fn manifest(&self) -> ExtensionManifest {
        manifest(EXTENSION_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::WorkspaceRead)
            .capability(ExtensionCapability::ProcessSpawn)
            .capability(ExtensionCapability::NetworkClient)
            .build()
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let shared = Arc::clone(&self.shared);
        let startup_working_dir = ctx
            .startup_working_dir()
            .map(|path| path.to_string_lossy().into_owned());
        ctx.tasks().spawn("mcp-warm", async move {
            match startup_working_dir {
                Some(working_dir) => shared.refresh_workspace(&working_dir).await,
                None => shared.refresh_global().await,
            }
        });
        Ok(())
    }

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        self.shared.pool.shutdown().await;
        self.shared.clear().await;
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
        reg.on_lifecycle(
            LifecycleEvent::SessionStart,
            HookMode::NonBlocking,
            0,
            lifecycle_handler.clone(),
        );
        reg.on_lifecycle(
            LifecycleEvent::SessionResume,
            HookMode::NonBlocking,
            0,
            lifecycle_handler,
        );
        reg.tool_discovery(Arc::new(McpToolDiscovery {
            shared: self.shared.clone(),
        }));
        reg.on_prompt_build(0, Arc::new(McpPromptBuildHandler));
    }
}

struct McpWorkspaceLifecycleHandler {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl LifecycleHandler for McpWorkspaceLifecycleHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let working_dir = ctx.working_dir().to_string_lossy();
        self.shared.refresh_workspace(&working_dir).await;
        Ok(HookResult::Allow)
    }
}

// ─── Shared Cache ───────────────────────────────────────────────────────

/// MCP discovery result cache + process pool, shared between tool discovery
/// and tool execution.
///
/// Cache is keyed by working_dir. Startup/session hooks prefill entries; tool
/// discovery synchronously fills only a cache miss.
struct WarmGate {
    done: AtomicBool,
    notify: Arc<Notify>,
}

impl WarmGate {
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            notify: Arc::new(Notify::new()),
        }
    }

    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

struct McpShared {
    cache: Mutex<HashMap<String, Arc<McpCacheEntry>>>,
    refresh_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    warm_gates: AsyncMutex<HashMap<String, Arc<WarmGate>>>,
    pool: McpProcessPool,
}

struct McpCacheEntry {
    config_fingerprint: u64,
    servers: Vec<McpServerConfig>,
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
            warm_gates: AsyncMutex::new(HashMap::new()),
            pool,
        }
    }

    async fn warm_gate(&self, working_dir: &str) -> Arc<WarmGate> {
        let mut gates = self.warm_gates.lock().await;
        gates
            .entry(working_dir.to_string())
            .or_insert_with(|| Arc::new(WarmGate::new()))
            .clone()
    }

    async fn get_warm_gate(&self, working_dir: &str) -> Option<Arc<WarmGate>> {
        self.warm_gates.lock().await.get(working_dir).cloned()
    }

    async fn mark_warm_complete(&self, working_dir: &str) {
        self.warm_gate(working_dir).await.mark_done();
    }

    /// 等待扩展启动时的后台预热完成（或超时），避免首轮 tool discovery 拿到空表。
    async fn await_initial_warm(&self, working_dir: &str) {
        if self.get_entry(working_dir).is_some() {
            return;
        }
        let Some(gate) = self.get_warm_gate(working_dir).await else {
            return;
        };
        if gate.is_done() {
            return;
        }
        let notified = gate.notify.clone();
        let wait = async {
            loop {
                if gate.is_done() || self.get_entry(working_dir).is_some() {
                    break;
                }
                notified.notified().await;
            }
        };
        if tokio::time::timeout(MCP_INITIAL_WARM_TIMEOUT, wait)
            .await
            .is_err()
        {
            tracing::warn!(
                working_dir,
                timeout_secs = MCP_INITIAL_WARM_TIMEOUT.as_secs(),
                "MCP initial warm timed out; tool discovery may proceed with partial cache"
            );
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

    async fn clear(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.refresh_locks.lock().await.clear();
        self.warm_gates.lock().await.clear();
    }

    async fn refresh_global(&self) {
        self.refresh_if_stale("", config::load_global_only).await;
    }

    async fn refresh_workspace(&self, working_dir: &str) {
        self.refresh_if_stale(working_dir, || config::load_config(working_dir))
            .await;
    }

    async fn refresh_if_stale<F>(&self, working_dir: &str, load_config: F)
    where
        F: FnOnce() -> McpConfig + Send,
    {
        // 当前磁盘读取成本可接受；若 tool discovery 频繁触达此路径，再加 mtime 缓存。
        let config = load_config();
        if self.entry_is_current(working_dir, config.fingerprint) {
            self.mark_warm_complete(working_dir).await;
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
        if !self.entry_is_current(working_dir, config.fingerprint) {
            self.refresh(working_dir, config).await;
        }
    }

    fn entry_is_current(&self, working_dir: &str, fingerprint: u64) -> bool {
        self.get_entry(working_dir)
            .is_some_and(|entry| entry.config_fingerprint == fingerprint)
    }

    async fn refresh(&self, working_dir: &str, config: McpConfig) {
        let results = self.pool.pre_warm(&config.servers).await;
        for (name, result) in &results {
            if let Err(e) = result {
                tracing::warn!("MCP server '{name}' pre-warm failed: {e}");
            }
        }
        let discovered = discover_from_pool(&self.pool, &config).await;
        let active_servers = self.active_servers_after_refresh(working_dir, &discovered.servers);
        self.pool.retain_servers(&active_servers).await;
        self.store(working_dir, discovered.build_cache_entry());
        self.mark_warm_complete(working_dir).await;
    }

    fn active_servers_after_refresh(
        &self,
        working_dir: &str,
        refreshed_servers: &[McpServerConfig],
    ) -> Vec<McpServerConfig> {
        let mut servers = refreshed_servers.to_vec();
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for (cached_working_dir, entry) in cache.iter() {
            if cached_working_dir != working_dir {
                servers.extend(entry.servers.iter().cloned());
            }
        }
        servers
    }
}

// ─── Discovery ──────────────────────────────────────────────────────────

struct McpToolDiscovery {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl ToolDiscoveryHandler for McpToolDiscovery {
    async fn discover(&self, ctx: ToolDiscoveryContext) -> Result<ToolDiscovery, ExtensionError> {
        let working_dir = ctx.working_dir().to_string_lossy();
        self.shared.await_initial_warm(&working_dir).await;
        // 后台预热若尚未完成，则首个 turn 在此同步等待同一次加载以保证工具完整；
        // 已有缓存时会按配置 fingerprint 快速判断是否仍然有效。
        self.shared.refresh_workspace(&working_dir).await;
        Ok(match self.shared.get_entry(&working_dir) {
            Some(entry) => self.build_discovered_tools(&entry),
            None => Vec::new(),
        }
        .into())
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
        let mut result = vec![
            DiscoveredTool::new(
                tool_search_tool_definition(),
                handler.clone() as Arc<dyn ToolHandler>,
            )
            .with_execution_policy(ToolExecutionPolicy::PARALLEL)
            .prompt_metadata(tool_search_metadata()),
        ];
        for candidate in &entry.candidates {
            result.push(
                DiscoveredTool::new(
                    candidate.definition.clone(),
                    handler.clone() as Arc<dyn ToolHandler>,
                )
                .prompt_metadata(mcp_concrete_tool_metadata()),
            );
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
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        // MCP tools can reach arbitrary resources declared by an external server. Until MCP
        // descriptors expose a trustworthy resource contract, approval must remain conservative.
        Ok(ToolPlan::opaque())
    }

    async fn execute(&self, ctx: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name();
        let working_dir = ctx.working_dir().to_string_lossy().into_owned();
        if tool_name == TOOL_SEARCH_TOOL_NAME {
            return Ok(self
                .handle_tool_search(ctx.arguments()?, &working_dir)
                .await);
        }

        let entry = self.shared.get_entry(&working_dir);
        let Some(cached) = entry.as_ref().and_then(|e| e.tool_lookup.get(tool_name)) else {
            return Err(ExtensionError::NotFound(tool_name.into()));
        };

        let (server, original_tool) = cached;
        match self
            .shared
            .pool
            .call_tool(server, original_tool, ctx.raw_arguments().clone())
            .await
        {
            Ok(result) => Ok(call_result(&server.name, original_tool, result).into()),
            Err(error) => Ok(error_result(
                format!("failed to call MCP tool '{original_tool}': {error}"),
                tool_metadata([
                    ("server", json!(server.name)),
                    ("tool", json!(original_tool)),
                ]),
            )
            .into()),
        }
    }
}

impl McpToolHandler {
    async fn handle_tool_search(
        &self,
        args: ToolSearchArgs,
        working_dir: &str,
    ) -> ToolExecutionResult {
        if args.query.trim().is_empty() {
            return error_result(
                "invalid tool_search_tool input: query must not be empty".into(),
                BTreeMap::new(),
            )
            .into();
        }

        let (candidates, diagnostics) = if let Some(entry) = self.shared.get_entry(working_dir) {
            (entry.candidates.clone(), entry.diagnostics.clone())
        } else {
            (Vec::new(), Vec::new())
        };

        warn_diagnostics(&diagnostics);
        let output = search_mcp_tools(&candidates, args);
        let tool_names = output
            .matches
            .iter()
            .map(|candidate| candidate.definition.name.clone())
            .collect();
        let mut metadata = BTreeMap::new();
        if !diagnostics.is_empty() {
            metadata.insert("diagnostics".into(), json!(diagnostics));
        }
        ToolExecutionResult::completed_with_discovered_tools(
            text_result(search::render_search_output(&output), false, None, metadata),
            tool_names,
        )
    }
}

// ─── PromptBuild ────────────────────────────────────────────────────────

struct McpPromptBuildHandler;

#[async_trait::async_trait]
impl PromptBuildHandler for McpPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let has_tool_search = ctx
            .tools()
            .iter()
            .any(|tool| tool.name == TOOL_SEARCH_TOOL_NAME);
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
    config_fingerprint: u64,
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
            config_fingerprint: self.config_fingerprint,
            servers: self.servers,
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
        config_fingerprint: config.fingerprint,
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
        strict: false,
        origin: ToolOrigin::Bundled,
    })
}

fn tool_search_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_SEARCH_TOOL_NAME.into(),
        description: "Find an external MCP tool by name or keyword and return its input schema \
                      (not execute it).\n\nWhen NOT to use:\n- First-party coding tools suffice: \
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
        strict: false,
        origin: ToolOrigin::Bundled,
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
    let error = result.is_error.then(|| content.clone());
    text_result(content, result.is_error, error, metadata)
}

fn error_result(content: String, metadata: BTreeMap<String, Value>) -> ToolResult {
    let error = Some(content.clone());
    text_result(content, true, error, metadata)
}

fn text_result(
    content: String,
    is_error: bool,
    error: Option<String>,
    metadata: BTreeMap<String, Value>,
) -> ToolResult {
    ToolResult {
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

    #[tokio::test]
    async fn clear_drops_cache_refresh_locks_and_warm_gates() {
        let shared = McpShared::new(McpProcessPool::new(Duration::from_secs(1)));
        let working_dir = "/workspace";

        shared.store(
            working_dir,
            McpCacheEntry {
                config_fingerprint: 1,
                servers: Vec::new(),
                tool_lookup: HashMap::new(),
                candidates: Vec::new(),
                diagnostics: Vec::new(),
            },
        );
        shared
            .refresh_locks
            .lock()
            .await
            .insert(working_dir.into(), Arc::new(AsyncMutex::new(())));
        shared
            .warm_gates
            .lock()
            .await
            .insert(working_dir.into(), Arc::new(WarmGate::new()));

        shared.clear().await;

        assert!(shared.get_entry(working_dir).is_none());
        assert!(shared.refresh_locks.lock().await.is_empty());
        assert!(shared.warm_gates.lock().await.is_empty());
    }

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
    fn discovery_tool_and_prompt_keep_the_two_step_mcp_contract() {
        let definition = tool_search_tool_definition();
        assert_eq!(definition.name, TOOL_SEARCH_TOOL_NAME);
        assert_eq!(definition.origin, ToolOrigin::Bundled);

        let instruction = mcp_discovery_instructions();
        assert!(instruction.starts_with("MCP discovery workflow:"));
        assert!(instruction.contains("`tool_search_tool`"));
        assert!(!instruction.contains("[Example Workflow]"));
    }
}
