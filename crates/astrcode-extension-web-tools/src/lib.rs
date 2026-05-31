//! astrcode-extension-web-tools — web search and URL fetch tools.
//!
//! Registers:
//! - `web-search`: search the public web for current information
//! - `fetch-url`: fetch and extract content from a public URL

mod cache;
mod config;
mod fetch_url;
mod http;
mod preapproved;
mod url_guard;
mod web_search;

use std::sync::Arc;

use astrcode_extension_sdk::{
    extension::{
        Extension, ExtensionCapability, ExtensionConfig, ExtensionCtx, ExtensionError, Registrar,
        ToolHandler,
    },
    llm::LlmProvider,
    render::{
        RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY, UI_SUMMARY_METADATA_KEY,
    },
    tool::{
        ExecutionMode, ToolDefinition, ToolExecutionContext, ToolOrigin, ToolResult, tool_metadata,
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
         is {current_month_year}; include this year in queries about recent docs or \
         events\n\nConfig:\n- Default provider is DuckDuckGo HTML (no API key)\n- Set \
         `extensions.astrcode-web-tools.search.provider` to `brave` or `serper` with an API key \
         for higher-quality results"
    )
}

const FETCH_URL_DESCRIPTION: &str =
    "Fetch content from a specified URL and process it for the given prompt.\n\nWhen NOT to \
     use:\n- Authenticated or private URLs (Google Docs, Confluence, Jira, internal \
     dashboards)\n- Binary files such as PDFs or images\n- Localhost or private-network \
     addresses\n\nTips:\n- Use after `web-search` when you need the full page body\n- Prefer \
     official docs and primary sources\n- For GitHub URLs, prefer the `gh` CLI when shell access \
     is available\n\nIMPORTANT:\n- This tool WILL FAIL for authenticated or private URLs\n- HTTP \
     URLs are upgraded to HTTPS automatically\n- Cross-host redirects are not followed \
     automatically; retry with the redirect URL when instructed\n- Repeated fetches of the same \
     URL are cached for 15 minutes";

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
    small_llm: Option<Arc<dyn LlmProvider>>,
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

#[async_trait::async_trait]
impl Extension for WebToolsExtension {
    fn id(&self) -> &str {
        config::EXTENSION_ID
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &[
            ExtensionCapability::NetworkClient,
            ExtensionCapability::SmallModel,
        ]
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        let mut shared = self.shared.write();
        shared.update_config(load_config(&ctx.config));
        shared.small_llm = ctx
            .host_services()
            .and_then(|services| services.small_llm.clone());
        Ok(())
    }

    async fn on_config_changed(&self, config: ExtensionConfig) -> Result<(), ExtensionError> {
        self.shared.write().update_config(load_config(&config));
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
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        _working_dir: &str,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != config::WEB_SEARCH_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let args = serde_json::from_value::<WebSearchArgs>(arguments).map_err(|error| {
            ExtensionError::Internal(format!(
                "invalid args for {}: {error}",
                config::WEB_SEARCH_TOOL_NAME
            ))
        })?;
        let query = args.query.trim().to_string();
        let config = self.shared.read().config.search.clone();

        match run_web_search(&config, args).await {
            Ok(outcome) => {
                let content = render_search_results(&outcome);
                let ui_render = build_search_render_spec(&outcome);
                let ui_summary = build_search_summary(&outcome);
                Ok(ToolResult::text(
                    content,
                    false,
                    tool_metadata([
                        ("query", json!(query)),
                        ("results", json!(outcome.hits)),
                        ("durationMs", json!(outcome.duration_ms)),
                        (UI_RENDER_METADATA_KEY, json!(ui_render)),
                        (UI_SUMMARY_METADATA_KEY, json!(ui_summary)),
                    ]),
                ))
            },
            Err(error) => Ok(ToolResult::text(
                error.to_string(),
                true,
                tool_metadata([("query", json!(query)), ("error", json!(error.to_string()))]),
            )),
        }
    }
}

struct FetchUrlToolHandler {
    shared: Arc<RwLock<WebToolsShared>>,
}

#[async_trait::async_trait]
impl ToolHandler for FetchUrlToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        _working_dir: &str,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != config::FETCH_URL_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let args = serde_json::from_value::<FetchUrlArgs>(arguments).map_err(|error| {
            ExtensionError::Internal(format!(
                "invalid args for {}: {error}",
                config::FETCH_URL_TOOL_NAME
            ))
        })?;
        let requested_url = args.url.trim().to_string();
        let prompt = args.prompt.trim().to_string();
        let (config, cache, small_llm) = {
            let shared = self.shared.read();
            (
                shared.config.fetch.clone(),
                Arc::clone(&shared.fetch_cache),
                shared.small_llm.clone(),
            )
        };

        match run_fetch_url(&config, &cache, small_llm, args).await {
            Ok(FetchUrlResult::Content(outcome)) => {
                let content = render_fetch_content(&outcome);
                let ui_render = build_fetch_render_spec(&outcome);
                let ui_summary = build_fetch_summary(&outcome);
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
                        (UI_RENDER_METADATA_KEY, json!(ui_render)),
                        (UI_SUMMARY_METADATA_KEY, json!(ui_summary)),
                    ]),
                ))
            },
            Ok(FetchUrlResult::Redirect(outcome)) => Ok(ToolResult::text(
                render_fetch_redirect(&outcome),
                false,
                tool_metadata([
                    ("url", json!(requested_url)),
                    ("redirectUrl", json!(outcome.redirect_url)),
                    ("statusCode", json!(outcome.status_code)),
                    ("durationMs", json!(outcome.duration_ms)),
                    ("prompt", json!(prompt)),
                ]),
            )),
            Err(error) => Ok(ToolResult::text(
                error.to_string(),
                true,
                tool_metadata([
                    ("url", json!(requested_url)),
                    ("prompt", json!(prompt)),
                    ("error", json!(error.to_string())),
                ]),
            )),
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
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Parallel,
    }
}

fn build_search_render_spec(outcome: &web_search::WebSearchOutcome) -> RenderSpec {
    RenderSpec::Box {
        title: Some("Web Search".into()),
        tone: RenderTone::Default,
        children: vec![
            RenderSpec::KeyValue {
                entries: vec![
                    RenderKeyValue {
                        key: "Query".into(),
                        value: outcome.query.clone(),
                        tone: RenderTone::Default,
                    },
                    RenderKeyValue {
                        key: "Results".into(),
                        value: outcome.hits.len().to_string(),
                        tone: RenderTone::Default,
                    },
                    RenderKeyValue {
                        key: "Duration".into(),
                        value: format!("{}ms", outcome.duration_ms),
                        tone: RenderTone::Default,
                    },
                ],
                tone: RenderTone::Default,
            },
            RenderSpec::List {
                ordered: true,
                items: outcome
                    .hits
                    .iter()
                    .map(|hit| RenderSpec::Text {
                        text: format!("{}\n{}", hit.title, hit.url),
                        tone: RenderTone::Default,
                    })
                    .collect(),
                tone: RenderTone::Default,
            },
        ],
    }
}

fn build_search_summary(outcome: &web_search::WebSearchOutcome) -> String {
    format!(
        "web-search · {} results · {}ms · {}",
        outcome.hits.len(),
        outcome.duration_ms,
        outcome.query
    )
}

fn build_fetch_render_spec(outcome: &fetch_url::FetchUrlOutcome) -> RenderSpec {
    RenderSpec::Box {
        title: Some("Fetch URL".into()),
        tone: RenderTone::Default,
        children: vec![
            RenderSpec::KeyValue {
                entries: vec![
                    RenderKeyValue {
                        key: "URL".into(),
                        value: outcome.final_url.clone(),
                        tone: RenderTone::Default,
                    },
                    RenderKeyValue {
                        key: "Status".into(),
                        value: outcome.status_code.to_string(),
                        tone: if outcome.status_code >= 400 {
                            RenderTone::Error
                        } else {
                            RenderTone::Success
                        },
                    },
                    RenderKeyValue {
                        key: "Bytes".into(),
                        value: outcome.bytes.to_string(),
                        tone: RenderTone::Default,
                    },
                    RenderKeyValue {
                        key: "Duration".into(),
                        value: format!("{}ms", outcome.duration_ms),
                        tone: RenderTone::Default,
                    },
                ],
                tone: RenderTone::Default,
            },
            RenderSpec::Text {
                text: outcome.result.clone(),
                tone: RenderTone::Muted,
            },
        ],
    }
}

fn build_fetch_summary(outcome: &fetch_url::FetchUrlOutcome) -> String {
    format!(
        "fetch-url · HTTP {} · {} bytes · {}ms{}",
        outcome.status_code,
        outcome.bytes,
        outcome.duration_ms,
        if outcome.cached { " · cache" } else { "" }
    )
}
