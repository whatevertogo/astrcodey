//! astrcode-extension-web-tools — web search and URL fetch tools.
//!
//! Registers:
//! - `web-search`: search the public web for current information
//! - `fetch-url`: fetch and extract content from a public URL

mod cache;
mod config;
mod fetch_url;
mod preapproved;
mod url_guard;
mod web_search;

use std::{sync::Arc, time::Duration};

use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        Extension, ExtensionCall, ExtensionCapability, ExtensionConfig, ExtensionError,
        ExtensionManifest, ExtensionStartContext, Registrar, ToolContext, ToolHandler,
        ToolPlanContext,
    },
    host::{HOST_NETWORK_MAX_TIMEOUT_MS, ModelClient, NetworkClient},
    tool::{
        ExecutionMode, HostResource, ResourceAccess, ToolDefinition, ToolOrigin, ToolPlan,
        ToolResult, tool_metadata,
    },
};
use parking_lot::{Mutex, RwLock};
use serde_json::json;

use crate::{
    cache::FetchUrlCache,
    config::{WebToolsConfig, load_config},
    fetch_url::{
        FetchUrlArgs, FetchUrlResult, render_fetch_content, render_fetch_redirect, run_fetch_url,
    },
    web_search::{WebSearchArgs, render_search_results, run_web_search},
};

fn web_search_description() -> String {
    let current_month_year = web_search::current_month_year();
    format!(
        "Search the public web for current information.\n\nWhen NOT to use:\n- Questions \
         answerable from the local workspace\n- Known URLs you can fetch directly with \
         `fetch-url`\n\nTips:\n- Prefer specific queries with product names, versions, or \
         dates\n- Follow up interesting hits with `fetch-url` for full page content\n- Use \
         `allowedDomains` or `blockedDomains` to narrow results\n\nIMPORTANT:\n- After answering, \
         include a Sources section with markdown hyperlinks to relevant URLs\n- The current month \
         is {current_month_year}; include this year in queries about recent docs or events"
    )
}

fn host_timeout_ms(timeout: Duration) -> u64 {
    timeout
        .as_millis()
        .clamp(1, u128::from(HOST_NETWORK_MAX_TIMEOUT_MS)) as u64
}

const FETCH_URL_DESCRIPTION: &str =
    "Fetch content from a specified URL and process it for the given prompt.\n\nWhen NOT to \
     use:\n- Authenticated or private URLs (Google Docs, Confluence, Jira, internal \
     dashboards)\n- Binary files such as PDFs or images\n- Localhost or private-network \
     addresses\n\nTips:\n- Use after `web-search` when you need the full page body\n- Prefer \
     official docs and primary sources\n- For GitHub URLs, prefer the `gh` CLI when shell access \
     is available\n\nIMPORTANT:\n- This tool WILL FAIL for authenticated or private URLs";

/// Return bundled web tools extension.
pub fn extension() -> Arc<dyn Extension> {
    Arc::new(WebToolsExtension {
        shared: Arc::new(RwLock::new(WebToolsShared::default())),
    })
}

struct WebToolsExtension {
    shared: Arc<RwLock<WebToolsShared>>,
}

struct WebToolsShared {
    config: WebToolsConfig,
    small_llm: Option<ModelClient>,
    outbound_network: Option<NetworkClient>,
    fetch_cache: Arc<Mutex<FetchUrlCache>>,
}

impl Default for WebToolsShared {
    fn default() -> Self {
        let config = WebToolsConfig::default();
        Self {
            fetch_cache: Arc::new(Mutex::new(FetchUrlCache::new(
                config.fetch.cache_ttl_secs,
                config.fetch.cache_max_entries,
                config.fetch.cache_max_bytes,
            ))),
            config,
            small_llm: None,
            outbound_network: None,
        }
    }
}

impl WebToolsShared {
    fn update_config(&mut self, config: WebToolsConfig) {
        self.fetch_cache = Arc::new(Mutex::new(FetchUrlCache::new(
            config.fetch.cache_ttl_secs,
            config.fetch.cache_max_entries,
            config.fetch.cache_max_bytes,
        )));
        self.config = config;
    }
}

/// Validate a candidate configuration without constructing extension runtime state.
pub fn validate_config(config: &ExtensionConfig) -> Result<(), ExtensionError> {
    load_config(config).map(|_| ())
}

#[async_trait::async_trait]
impl Extension for WebToolsExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest(config::EXTENSION_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::NetworkClient)
            .capability(ExtensionCapability::SmallModel)
            .build()
    }

    fn validate_config(&self, config: &ExtensionConfig) -> Result<(), ExtensionError> {
        validate_config(config)
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let network = ctx
            .host()
            .network()
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;
        let models = ctx.host().models();
        let small_llm = models
            .small_available()
            .map_err(|error| ExtensionError::Internal(error.to_string()))?
            .then_some(models);
        let mut shared = self.shared.write();
        shared.update_config(load_config(ctx.config())?);
        shared.small_llm = small_llm;
        shared.outbound_network = Some(network);
        Ok(())
    }

    fn register(&self, reg: &mut Registrar) {
        let shared = Arc::clone(&self.shared);
        reg.tool(
            web_search_tool_definition(),
            Arc::new(WebSearchToolHandler {
                shared: shared.clone(),
            }),
        );
        reg.tool(
            fetch_url_tool_definition(),
            Arc::new(FetchUrlToolHandler { shared }),
        );
    }
}

struct WebSearchToolHandler {
    shared: Arc<RwLock<WebToolsShared>>,
}

#[async_trait::async_trait]
impl ToolHandler for WebSearchToolHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Network))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name();
        if tool_name != config::WEB_SEARCH_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let args = ctx.arguments::<WebSearchArgs>()?;
        let query = args.query.trim().to_string();
        let (config, network) = {
            let shared = self.shared.read();
            (
                shared.config.search.clone(),
                shared.outbound_network.clone().ok_or_else(|| {
                    ExtensionError::Internal("outbound network service is unavailable".into())
                })?,
            )
        };

        match run_web_search(&config, network, args).await {
            Ok(outcome) => {
                let content = render_search_results(&outcome);
                Ok(ToolResult::text(
                    content,
                    false,
                    tool_metadata([
                        ("query", json!(query)),
                        ("results", json!(outcome.hits)),
                        ("durationMs", json!(outcome.duration_ms)),
                    ]),
                )
                .into())
            },
            Err(error) => Ok(ToolResult::text(
                error.to_string(),
                true,
                tool_metadata([("query", json!(query)), ("error", json!(error.to_string()))]),
            )
            .into()),
        }
    }
}

struct FetchUrlToolHandler {
    shared: Arc<RwLock<WebToolsShared>>,
}

#[async_trait::async_trait]
impl ToolHandler for FetchUrlToolHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::new([
            ResourceAccess::host(HostResource::Network),
            ResourceAccess::host(HostResource::Model),
        ]))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name();
        if tool_name != config::FETCH_URL_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let args = ctx.arguments::<FetchUrlArgs>()?;
        let requested_url = args.url.trim().to_string();
        let prompt = args.prompt.trim().to_string();
        let (config, cache, network, small_llm) = {
            let shared = self.shared.read();
            (
                shared.config.fetch.clone(),
                Arc::clone(&shared.fetch_cache),
                shared.outbound_network.clone().ok_or_else(|| {
                    ExtensionError::Internal("outbound network service is unavailable".into())
                })?,
                shared.small_llm.clone(),
            )
        };

        match run_fetch_url(&config, &cache, network, small_llm, args).await {
            Ok(FetchUrlResult::Content(outcome)) => {
                let content = render_fetch_content(&outcome, config.max_output_chars);
                Ok(ToolResult::text(
                    content,
                    false,
                    tool_metadata([
                        ("url", json!(outcome.url)),
                        ("finalUrl", json!(outcome.final_url)),
                        ("statusCode", json!(outcome.status_code)),
                        ("bytes", json!(outcome.bytes)),
                        ("durationMs", json!(outcome.duration_ms)),
                        ("cached", json!(outcome.cached)),
                        ("prompt", json!(prompt)),
                    ]),
                )
                .into())
            },
            Ok(FetchUrlResult::Redirect(outcome)) => Ok(ToolResult::text(
                render_fetch_redirect(&outcome, config.max_output_chars),
                false,
                tool_metadata([
                    ("url", json!(requested_url)),
                    ("redirectUrl", json!(outcome.redirect_url)),
                    ("statusCode", json!(outcome.status_code)),
                    ("durationMs", json!(outcome.duration_ms)),
                    ("prompt", json!(prompt)),
                ]),
            )
            .into()),
            Err(error) => Ok(ToolResult::text(
                error.to_string(),
                true,
                tool_metadata([
                    ("url", json!(requested_url)),
                    ("prompt", json!(prompt)),
                    ("error", json!(error.to_string())),
                ]),
            )
            .into()),
        }
    }
}

fn web_search_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: config::WEB_SEARCH_TOOL_NAME.into(),
        description: web_search_description(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 2,
                    "description": "The search query to use."
                },
                "maxResults": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Maximum number of results to return."
                },
                "allowedDomains": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only include search results from these domains."
                },
                "blockedDomains": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Never include search results from these domains."
                }
            },
            "required": ["query"]
        }),
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Parallel,
    }
}

fn fetch_url_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: config::FETCH_URL_TOOL_NAME.into(),
        description: FETCH_URL_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Public HTTP or HTTPS URL to fetch."
                },
                "prompt": {
                    "type": "string",
                    "description": "What information to extract or summarize from the fetched page."
                }
            },
            "required": ["url", "prompt"]
        }),
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Parallel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_timeout_stays_within_the_wire_contract() {
        assert_eq!(host_timeout_ms(Duration::ZERO), 1);
        assert_eq!(host_timeout_ms(Duration::from_secs(30)), 30_000);
        assert_eq!(
            host_timeout_ms(Duration::from_secs(u64::MAX)),
            HOST_NETWORK_MAX_TIMEOUT_MS
        );
    }
}
