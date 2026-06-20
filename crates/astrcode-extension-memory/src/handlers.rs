//! Memory handlers — Save, Delete, List, PromptBuild, SessionStart。

use std::{collections::BTreeMap, sync::Arc};

use astrcode_extension_sdk::{
    extension::{
        ExtensionError, ExtensionTasks, HookResult, LifecycleContext, LifecycleHandler,
        PromptBuildContext, PromptBuildHandler, PromptContributions, ToolHandler,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use serde_json::json;

use crate::{
    MemoryServices,
    config::MemoryConfig,
    pipeline, prompts,
    store::{AppendResult, MemoryStorePool},
};

// ─── 常量 ────────────────────────────────────────────────────────────

const MEMORY_SAVE_TOOL: &str = "memory_save";
const MEMORY_DELETE_TOOL: &str = "memory_delete";
const MEMORY_LIST_TOOL: &str = "memory_list";
const MAX_LIST_ENTRIES: usize = 50;
const DEFAULT_LIST_LIMIT: usize = 20;

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
        execution_mode: ExecutionMode::Sequential,
        origin: ToolOrigin::Extension,
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
        execution_mode: ExecutionMode::Sequential,
        origin: ToolOrigin::Extension,
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
        execution_mode: ExecutionMode::Sequential,
        origin: ToolOrigin::Extension,
    }
}

fn ok_text(content: String) -> ToolResult {
    ToolResult::text(content, false, BTreeMap::new())
}

// ─── Save Handler ────────────────────────────────────────────────────

pub(crate) struct MemorySaveHandler {
    pub store_pool: Arc<MemoryStorePool>,
    pub services: MemoryServices,
    pub tasks: Arc<Mutex<Option<ExtensionTasks>>>,
    pub pipeline: Arc<MemoryPipelineCoordinator>,
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
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let args: SaveArgs = serde_json::from_value(arguments)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let scoped = self
            .store_pool
            .get_scoped(working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let content = args.content;
        let category = args.category;
        let replace = args.replace_match.filter(|s| !s.trim().is_empty());

        // replace_match 路径：精准 upsert，不经过 delete
        if let Some(ref replaces) = replace {
            let replaces = replaces.clone();
            let changed = tokio::task::spawn_blocking(move || {
                scoped.upsert(&category, &content, Some(replaces.as_str()))
            })
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
            return Ok(ok_text(
                if changed {
                    "Memory updated."
                } else {
                    "Memory unchanged (content identical)."
                }
                .to_string(),
            ));
        }

        // 正常新增路径
        let result = tokio::task::spawn_blocking(move || scoped.append(&category, &content))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        match result {
            AppendResult::Saved => {
                let cfg = self.config.read().clone();
                if cfg.auto_extract_after_save {
                    if let Some(tasks) = self.tasks.lock().clone() {
                        spawn_memory_pipeline(
                            &tasks,
                            self.pipeline.clone(),
                            self.store_pool.clone(),
                            &self.services,
                            self.config.clone(),
                            ctx.session_id.to_string(),
                            working_dir.to_string(),
                        );
                    }
                }
                Ok(ok_text("Memory saved.".to_string()))
            },
            AppendResult::SimilarExists(similar) => Ok(ok_text(format!(
                "Similar memories exist:\n{}\n\nRetry with replace_match to update in place.",
                similar
                    .iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))),
        }
    }
}

// ─── Delete Handler ──────────────────────────────────────────────────

pub(crate) struct MemoryDeleteHandler {
    pub store_pool: Arc<MemoryStorePool>,
}

#[derive(Deserialize)]
struct DeleteArgs {
    #[serde(rename = "match")]
    match_pattern: String,
}

#[async_trait::async_trait]
impl ToolHandler for MemoryDeleteHandler {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let args: DeleteArgs = serde_json::from_value(arguments)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        if args.match_pattern.trim().is_empty() {
            return Ok(ok_text("No pattern provided. Nothing deleted.".to_string()));
        }
        let scoped = self
            .store_pool
            .get_scoped(working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let pattern = args.match_pattern;
        let pattern_for_emit = pattern.clone();
        let removed = tokio::task::spawn_blocking(move || scoped.delete_by_content(&pattern))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        if !removed.is_empty() {
            if let Some(ref sink) = ctx.capabilities.host.extension_event_sink {
                let payload = json!({
                    "match": pattern_for_emit,
                    "deleted_count": removed.len(),
                });
                let _ = sink.emit("memory.deleted", 1, payload).await;
            }
        }

        if removed.is_empty() {
            Ok(ok_text("No matching memories found to delete.".to_string()))
        } else {
            Ok(ok_text(format!(
                "Deleted {} entries:\n{}",
                removed.len(),
                removed.join("\n")
            )))
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
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        _ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let args: ListArgs = serde_json::from_value(arguments)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let scoped = self
            .store_pool
            .get_scoped(working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        let limit = args.limit.clamp(1, MAX_LIST_ENTRIES);
        let query = args.query.filter(|q| !q.trim().is_empty());

        let entries = tokio::task::spawn_blocking(move || match query {
            Some(q) => scoped.search(&q, limit),
            None => scoped.list_entries(limit),
        })
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        if entries.is_empty() {
            Ok(ok_text("No memories found.".to_string()))
        } else {
            Ok(ok_text(format!(
                "{} entries:\n{}",
                entries.len(),
                entries.join("\n")
            )))
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
        let working_dir = ctx.working_dir.clone();
        let session_id = ctx.session_id.clone();
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

// ─── SessionStart + pipeline coordinator ───────────────────────────

#[derive(Default)]
pub(crate) struct MemoryPipelineCoordinator {
    state: Mutex<PipelineState>,
}

#[derive(Default)]
struct PipelineState {
    running: bool,
    pending: bool,
    latest_session_id: Option<String>,
    latest_working_dir: Option<String>,
}

impl MemoryPipelineCoordinator {
    fn request_run(&self, session_id: String, working_dir: String) -> Option<(String, String)> {
        let mut state = self.state.lock();
        state.latest_session_id = Some(session_id.clone());
        state.latest_working_dir = Some(working_dir.clone());
        if state.running {
            state.pending = true;
            None
        } else {
            state.running = true;
            Some((session_id, working_dir))
        }
    }

    fn complete_run(&self) -> Option<(String, String)> {
        let mut state = self.state.lock();
        if state.pending {
            state.pending = false;
            Some((
                state.latest_session_id.clone()?,
                state.latest_working_dir.clone()?,
            ))
        } else {
            state.running = false;
            None
        }
    }

    pub(crate) fn reset(&self) {
        *self.state.lock() = PipelineState::default();
    }
}

pub(crate) fn spawn_memory_pipeline(
    tasks: &ExtensionTasks,
    pipeline: Arc<MemoryPipelineCoordinator>,
    store_pool: Arc<MemoryStorePool>,
    services: &MemoryServices,
    config: Arc<RwLock<MemoryConfig>>,
    session_id: String,
    working_dir: String,
) {
    let Some((mut current_session_id, mut working_dir)) =
        pipeline.request_run(session_id, working_dir)
    else {
        tracing::debug!("memory pipeline queued");
        return;
    };

    let services = services.get().cloned();
    let shutdown = tasks.shutdown();
    let pipeline = pipeline.clone();

    tasks.spawn("memory-pipeline", async move {
        let Some(services) = services else {
            pipeline.reset();
            return;
        };
        let session_read = match services.session_read.clone() {
            Some(r) => r,
            None => {
                tracing::warn!("memory pipeline: session history unavailable");
                pipeline.reset();
                return;
            },
        };
        let small_llm = match services.small_llm.clone() {
            Some(llm) => llm,
            None => {
                tracing::warn!("memory pipeline: small model unavailable");
                pipeline.reset();
                return;
            },
        };

        loop {
            let scoped = match store_pool.get_scoped(&working_dir) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "memory pipeline: scoped store failed");
                    break;
                },
            };
            let cfg = config.read().clone();
            let run = pipeline::run(
                &scoped,
                session_read.clone(),
                small_llm.as_ref(),
                &current_session_id,
                &cfg,
            );

            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("memory pipeline stopped");
                    break;
                },
                result = run => {
                    if let Err(e) = result {
                        tracing::warn!(
                            error = %e,
                            session_id = %current_session_id,
                            "memory pipeline failed"
                        );
                    }
                },
            }

            if shutdown.is_cancelled() {
                break;
            }
            let Some((next_id, next_dir)) = pipeline.complete_run() else {
                break;
            };
            current_session_id = next_id;
            working_dir = next_dir;
        }

        while pipeline.complete_run().is_some() {}
    });
}

pub(crate) struct MemorySessionStartHandler {
    pub store_pool: Arc<MemoryStorePool>,
    pub services: MemoryServices,
    pub pipeline: Arc<MemoryPipelineCoordinator>,
    pub tasks: Arc<Mutex<Option<ExtensionTasks>>>,
    pub config: Arc<RwLock<MemoryConfig>>,
    pub session_prefs: Arc<crate::turn_recall::SessionPrefsCache>,
}

#[async_trait::async_trait]
impl LifecycleHandler for MemorySessionStartHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let Some(tasks) = self.tasks.lock().clone() else {
            tracing::debug!(session_id = %ctx.session_id, "memory extension not started");
            return Ok(HookResult::Allow);
        };
        if tasks.shutdown().is_cancelled() {
            return Ok(HookResult::Allow);
        }

        // 把 user_prefs 锚定在 session 边界：session 内只读，`memory_save`
        // 写入的新偏好不影响当前 session 的 system prompt（KV cache 稳定）。
        // 预加载幂等，即使赶不上首次 PromptBuild，兜底加载仍保证一致。
        // 预加载失败不阻塞——PromptBuild 的 lines_for_session 会兜底。
        let store_pool = self.store_pool.clone();
        let session_prefs = self.session_prefs.clone();
        let working_dir = ctx.working_dir.clone();
        let session_id = ctx.session_id.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            let scoped = store_pool.get_scoped(&working_dir)?;
            session_prefs.preload_for_session(&session_id, || scoped.all_user_preference_lines())
        })
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
        {
            tracing::debug!(error = %e, "memory: user_prefs preload failed, will lazy-load on prompt build");
        }

        if !self.config.read().auto_extract {
            return Ok(HookResult::Allow);
        }

        spawn_memory_pipeline(
            &tasks,
            self.pipeline.clone(),
            self.store_pool.clone(),
            &self.services,
            self.config.clone(),
            ctx.session_id.to_string(),
            ctx.working_dir.to_string(),
        );

        Ok(HookResult::Allow)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_coordinator_coalesces_pending_runs() {
        let coord = MemoryPipelineCoordinator::default();

        // First run starts immediately
        let run1 = coord.request_run("s1".into(), "/a".into());
        assert!(run1.is_some());

        // Second run is queued (coalesced)
        let run2 = coord.request_run("s2".into(), "/b".into());
        assert!(run2.is_none());

        // Completing the first run dequeues the pending one
        let next = coord.complete_run();
        assert!(next.is_some());
        assert_eq!(next.unwrap().0, "s2");

        // No more pending
        let done = coord.complete_run();
        assert!(done.is_none());
    }
}
