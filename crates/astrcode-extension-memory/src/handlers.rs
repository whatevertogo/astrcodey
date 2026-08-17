//! Memory handlers — Save, Delete, List, PromptBuild, SessionStart。

use std::{collections::BTreeMap, sync::Arc};

use astrcode_extension_sdk::{
    extension::{
        ExtensionCall, ExtensionError, HookResult, LifecycleContext, LifecycleHandler,
        PromptBuildContext, PromptBuildHandler, PromptContributions, ToolContext, ToolHandler,
        ToolPlanContext,
    },
    tool::{HostResource, ResourceAccess, ToolDefinition, ToolOrigin, ToolPlan, ToolResult},
};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;

use crate::{
    config::MemoryConfig,
    prompts,
    scope::ScopedMemoryStores,
    store::{AppendResult, MemoryStorePool},
    workers::MemoryWorkers,
};

// ─── 常量 ────────────────────────────────────────────────────────────

const MEMORY_SAVE_TOOL: &str = "memory_save";
const MEMORY_DELETE_TOOL: &str = "memory_delete";
const MEMORY_LIST_TOOL: &str = "memory_list";
const MAX_LIST_ENTRIES: usize = 50;
const DEFAULT_LIST_LIMIT: usize = 20;
pub(crate) const MEMORY_CREATED_EVENT_TYPE: &str = "memory.created";
pub(crate) const MEMORY_DELETED_EVENT_TYPE: &str = "memory.deleted";

// ─── Tool Definitions ────────────────────────────────────────────────

pub(crate) fn memory_save_definition() -> ToolDefinition {
    ToolDefinition {
        name: MEMORY_SAVE_TOOL.to_string(),
        description: prompts::SAVE_TOOL_DESC.to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Fact to store" },
                "category": { "type": "string", "enum": ["user_pref", "project_ctx", "decision", "general"], "description": "Category. Default: general" },
                "replace_match": { "type": "string", "description": "Substring of existing entry to update in place" }
            },
            "required": ["content"]
        }),
        strict: true,
        origin: ToolOrigin::Bundled,
    }
}

pub(crate) fn memory_delete_definition() -> ToolDefinition {
    ToolDefinition {
        name: MEMORY_DELETE_TOOL.to_string(),
        description: prompts::DELETE_TOOL_DESC.to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "match": { "type": "string", "description": "Substring to match (case-insensitive)" }
            },
            "required": ["match"]
        }),
        strict: true,
        origin: ToolOrigin::Bundled,
    }
}

pub(crate) fn memory_list_definition() -> ToolDefinition {
    ToolDefinition {
        name: MEMORY_LIST_TOOL.to_string(),
        description: prompts::LIST_TOOL_DESC.to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query; omit for recent entries" },
                "limit": { "type": "integer", "description": "Max entries (default 20, max 50)", "minimum": 1, "maximum": 50 }
            }
        }),
        strict: true,
        origin: ToolOrigin::Bundled,
    }
}

fn ok_text(content: String) -> ToolResult {
    ToolResult::text(content, false, BTreeMap::new())
}

fn tool_working_dir(ctx: &ToolContext) -> String {
    ctx.working_dir().to_string_lossy().into_owned()
}

async fn with_scoped_stores<T: Send + 'static>(
    store_pool: Arc<MemoryStorePool>,
    working_dir: String,
    operation: impl FnOnce(ScopedMemoryStores) -> std::io::Result<T> + Send + 'static,
) -> Result<T, ExtensionError> {
    tokio::task::spawn_blocking(move || {
        let stores = store_pool.get_scoped(&working_dir)?;
        operation(stores)
    })
    .await
    .map_err(|error| ExtensionError::Internal(error.to_string()))?
    .map_err(|error| ExtensionError::Internal(error.to_string()))
}

// ─── Save Handler ────────────────────────────────────────────────────

pub(crate) struct MemorySaveHandler {
    pub workers: Arc<MemoryWorkers>,
    pub config: Arc<RwLock<MemoryConfig>>,
}

#[derive(Deserialize)]
struct SaveArgs {
    content: String,
    #[serde(default = "default_category")]
    category: String,
    replace_match: Option<String>,
}

fn default_category() -> String {
    "general".to_string()
}

#[async_trait::async_trait]
impl ToolHandler for MemorySaveHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::new([
            ResourceAccess::host(HostResource::Session),
            ResourceAccess::host(HostResource::Event),
            ResourceAccess::host(HostResource::Model),
        ]))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let args: SaveArgs = ctx.arguments()?;
        let working_dir = tool_working_dir(&ctx);
        let session_id = ctx.session_id().to_string();
        let content = args.content;
        let category = args.category;
        let replace = args.replace_match.filter(|s| !s.trim().is_empty());

        // replace_match 路径：精准 upsert，不经过 delete
        if let Some(replaces) = replace {
            let changed = self
                .workers
                .upsert(working_dir, category, content, replaces)
                .await?;
            return Ok(ok_text(
                if changed {
                    "Memory updated."
                } else {
                    "Memory unchanged (content identical)."
                }
                .to_string(),
            )
            .into());
        }

        // 正常新增路径
        let category_for_emit = category.clone();
        let content_for_emit = content.clone();
        let result = self
            .workers
            .append(working_dir.clone(), category, content)
            .await?;

        match result {
            AppendResult::Saved => {
                let payload = json!({
                    "category": category_for_emit,
                    "content": content_for_emit,
                });
                if let Err(error) = ctx.events().emit(MEMORY_CREATED_EVENT_TYPE, &payload).await {
                    tracing::warn!(%error, "memory creation event was not published");
                }
                if self.config.read().auto_extract_after_save {
                    self.workers.request_pipeline(session_id, working_dir);
                }
                Ok(ok_text("Memory saved.".to_string()).into())
            },
            AppendResult::SimilarExists(similar) => Ok(ok_text(format!(
                "Similar memories exist:\n{}\n\nRetry with replace_match to update in place.",
                similar
                    .iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
            .into()),
        }
    }
}

// ─── Delete Handler ──────────────────────────────────────────────────

pub(crate) struct MemoryDeleteHandler {
    pub workers: Arc<MemoryWorkers>,
}

#[derive(Deserialize)]
struct DeleteArgs {
    #[serde(rename = "match")]
    match_pattern: String,
}

#[async_trait::async_trait]
impl ToolHandler for MemoryDeleteHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::new([
            ResourceAccess::host(HostResource::Session),
            ResourceAccess::host(HostResource::Event),
        ]))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let args: DeleteArgs = ctx.arguments()?;
        let working_dir = tool_working_dir(&ctx);
        if args.match_pattern.trim().is_empty() {
            return Ok(ok_text("No pattern provided. Nothing deleted.".to_string()).into());
        }
        let pattern = args.match_pattern;
        let pattern_for_emit = pattern.clone();
        let removed = self.workers.delete(working_dir, pattern).await?;

        if !removed.is_empty() {
            let payload = json!({
                "match": pattern_for_emit,
                "deletedCount": removed.len(),
            });
            if let Err(error) = ctx.events().emit(MEMORY_DELETED_EVENT_TYPE, &payload).await {
                tracing::warn!(%error, "memory deletion event was not published");
            }
        }

        if removed.is_empty() {
            Ok(ok_text("No matching memories found to delete.".to_string()).into())
        } else {
            Ok(ok_text(format!(
                "Deleted {} entries:\n{}",
                removed.len(),
                removed.join("\n")
            ))
            .into())
        }
    }
}

// ─── List Handler ────────────────────────────────────────────────────

pub(crate) struct MemoryListHandler {
    pub store_pool: Arc<MemoryStorePool>,
}

#[derive(Deserialize)]
struct ListArgs {
    query: Option<String>,
    #[serde(default = "default_list_limit")]
    limit: usize,
}

const fn default_list_limit() -> usize {
    DEFAULT_LIST_LIMIT
}

#[async_trait::async_trait]
impl ToolHandler for MemoryListHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Session))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let args: ListArgs = ctx.arguments()?;
        let working_dir = tool_working_dir(&ctx);
        let limit = args.limit.clamp(1, MAX_LIST_ENTRIES);
        let query = args.query.filter(|q| !q.trim().is_empty());

        let entries =
            with_scoped_stores(
                self.store_pool.clone(),
                working_dir,
                move |stores| match query {
                    Some(query) => stores.search(&query, limit),
                    None => stores.list_entries(limit),
                },
            )
            .await?;

        if entries.is_empty() {
            Ok(ok_text("No memories found.".to_string()).into())
        } else {
            Ok(ok_text(format!(
                "{} entries:\n{}",
                entries.len(),
                entries.join("\n")
            ))
            .into())
        }
    }
}

// ─── PromptBuild — 工具说明 + 全局偏好 ───────────────────────────────

pub(crate) struct MemoryRecallHandler {
    pub store_pool: Arc<MemoryStorePool>,
    pub session_prefs: Arc<crate::turn_recall::SessionPrefsCache>,
}

#[async_trait::async_trait]
impl PromptBuildHandler for MemoryRecallHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let store_pool = self.store_pool.clone();
        let working_dir = ctx.working_dir().to_string_lossy().into_owned();
        let session_id = ctx.session_id().to_string();
        let session_prefs = self.session_prefs.clone();

        let global_prefs = tokio::task::spawn_blocking(move || {
            session_prefs.lines_for_session(&session_id, || {
                let scoped = store_pool.get_scoped(&working_dir)?;
                scoped.all_user_preference_lines()
            })
        })
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
        .unwrap_or_default();

        let body = prompts::memory_tools_instruction(
            MEMORY_LIST_TOOL,
            MEMORY_SAVE_TOOL,
            MEMORY_DELETE_TOOL,
            &global_prefs,
        );

        Ok(PromptContributions {
            additional_instructions: vec![body],
            ..Default::default()
        })
    }
}

// ─── SessionStart ────────────────────────────────────────────────────

pub(crate) struct MemorySessionStartHandler {
    pub workers: Arc<MemoryWorkers>,
    pub config: Arc<RwLock<MemoryConfig>>,
}

#[async_trait::async_trait]
impl LifecycleHandler for MemorySessionStartHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let session_id = ctx.session_id().to_string();

        // 把 user_prefs 锚定在 session 边界：session 内只读，`memory_save`
        // 写入的新偏好不影响当前 session 的 system prompt（KV cache 稳定）。
        // 预加载幂等，即使赶不上首次 PromptBuild，兜底加载仍保证一致。
        // 预加载失败不阻塞——PromptBuild 的 lines_for_session 会兜底。
        let working_dir = ctx.working_dir().to_string_lossy().into_owned();
        if let Err(error) = self
            .workers
            .preload_preferences(working_dir.clone(), session_id.clone())
            .await
        {
            tracing::debug!(%error, "memory: user_prefs preload failed, will lazy-load on prompt build");
        }

        if !self.config.read().auto_extract {
            return Ok(HookResult::Allow);
        }

        self.workers.request_pipeline(session_id, working_dir);

        Ok(HookResult::Allow)
    }
}
