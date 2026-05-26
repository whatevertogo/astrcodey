//! Memory handlers — Save, Delete, Recall (PromptBuild), TurnEnd, SessionStart, Command。

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use astrcode_extension_sdk::{
    extension::{
        CommandContext, ExtensionCommandResult, ExtensionError, ExtensionTasks, HookResult,
        LifecycleContext, LifecycleHandler, PromptBuildContext, PromptBuildHandler,
        PromptContributions, SlashCommand, ToolHandler,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;

use crate::{MemoryServices, store::MemoryStorePool};

// ─── 常量 ────────────────────────────────────────────────────────────

const MEMORY_SAVE_TOOL: &str = "memory_save";
const MEMORY_DELETE_TOOL: &str = "memory_delete";
const MEMORY_CMD: &str = "memory";
const MAX_LIST_ENTRIES: usize = 50;

// ─── Tool Definitions ────────────────────────────────────────────────

pub(crate) fn memory_save_definition() -> ToolDefinition {
    ToolDefinition {
        name: MEMORY_SAVE_TOOL.to_string(),
        description: "Save a piece of information to long-term memory. Use this to remember user \
                      preferences, project decisions, coding patterns, or any fact worth \
                      recalling in future sessions."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The information to save" },
                "category": { "type": "string", "enum": ["user_pref", "project_ctx", "decision", "general"], "description": "Category tag. Default: general" }
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
        description: "Delete entries from long-term memory by content match.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "match": { "type": "string", "description": "Substring to match (case-insensitive). All matching entries will be deleted." }
            },
            "required": ["match"]
        }),
        execution_mode: ExecutionMode::Sequential,
        origin: ToolOrigin::Extension,
    }
}

pub(crate) fn memory_command_definition() -> SlashCommand {
    SlashCommand {
        name: MEMORY_CMD.to_string(),
        description: "Manage long-term memory (list, search, delete)".to_string(),
        args_schema: None,
    }
}

fn ok_text(content: String) -> ToolResult {
    ToolResult::text(content, false, BTreeMap::new())
}

// ─── Save Handler ────────────────────────────────────────────────────

pub(crate) struct MemorySaveHandler {
    pub store_pool: Arc<MemoryStorePool>,
}

#[derive(Deserialize)]
struct SaveArgs {
    content: String,
    #[serde(default = "default_category")]
    category: String,
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
        _ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let args: SaveArgs = serde_json::from_value(arguments)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let store = self
            .store_pool
            .get(working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let content = args.content;
        let category = args.category;
        tokio::task::spawn_blocking(move || store.append(&category, &content))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        Ok(ok_text("Memory saved.".to_string()))
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
        let store = self
            .store_pool
            .get(working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let pattern = args.match_pattern;
        let pattern_for_emit = pattern.clone();
        let removed = tokio::task::spawn_blocking(move || store.delete_by_content(&pattern))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        if !removed.is_empty() {
            if let Some(ref sink) = ctx.capabilities.extension_event_sink {
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

// ─── Recall Handler (PromptBuild) ────────────────────────────────────

pub(crate) struct MemoryRecallHandler {
    pub store_pool: Arc<MemoryStorePool>,
}

#[async_trait::async_trait]
impl PromptBuildHandler for MemoryRecallHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let store = self
            .store_pool
            .get(&ctx.working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let content = match tokio::task::spawn_blocking(move || store.read_memory()).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "memory recall: failed to read");
                return Ok(PromptContributions::default());
            },
            Err(e) => {
                tracing::warn!(error = %e, "memory recall: spawn_blocking failed");
                return Ok(PromptContributions::default());
            },
        };

        // 空文件或只有 header → 不注入 (header + 4 section headers = 5 lines)
        if content.trim().lines().count() <= 5 {
            return Ok(PromptContributions::default());
        }

        // 截断防止 token 爆炸
        const MAX_MEMORY_CHARS: usize = 4000;
        let content = if content.len() > MAX_MEMORY_CHARS {
            let truncated = crate::store::truncate_to_char_boundary(&content, MAX_MEMORY_CHARS);
            format!(
                "{truncated}…\n({} bytes truncated)",
                content.len() - truncated.len()
            )
        } else {
            content
        };

        Ok(PromptContributions {
            additional_instructions: vec![format!(
                "<memory>\nYou have a persistent memory system. Use `{MEMORY_SAVE_TOOL}` to store \
                 important information and `{MEMORY_DELETE_TOOL}` to remove \
                 entries.\n\n{content}\n</memory>"
            )],
            ..Default::default()
        })
    }
}

// ─── TurnEnd Handler — recall + incremental extraction ─────────────────
//
// TurnEnd 后召回历史上下文辅助提取记忆。
// 1. 从 last_exchange 提取关键词，搜索 contexts/ 召回相关历史
// 2. 将召回内容 + 当前 turn 发给小模型提取记忆
// 3. 去重后写入 contexts/

const RECALL_MAX_RESULTS: usize = 3;
const RECALL_MAX_CHARS_PER_FILE: usize = 800;

/// TurnEnd 提取最小间隔（秒），防止每轮都调小模型。
const MIN_EXTRACT_INTERVAL_SECS: u64 = 180;

#[derive(Default)]
pub(crate) struct TurnExtractState {
    last_extract: Option<Instant>,
}

pub(crate) struct MemoryTurnEndHandler {
    pub store_pool: Arc<MemoryStorePool>,
    pub services: MemoryServices,
    pub tasks: Arc<Mutex<Option<ExtensionTasks>>>,
    pub(crate) extract_state: Mutex<TurnExtractState>,
}

#[async_trait::async_trait]
impl LifecycleHandler for MemoryTurnEndHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let Some(tasks) = self.tasks.lock().clone() else {
            return Ok(HookResult::Allow);
        };
        if tasks.shutdown().is_cancelled() {
            return Ok(HookResult::Allow);
        }

        let Some(exchange) = &ctx.last_exchange else {
            return Ok(HookResult::Allow);
        };

        // 跳过内容太短的 turn
        let query = exchange.user_message.clone();
        if query.len() < 20 {
            return Ok(HookResult::Allow);
        }

        // 冷却：距离上次提取不足 180 秒则跳过
        {
            let mut state = self.extract_state.lock();
            let now = Instant::now();
            if let Some(last) = state.last_extract {
                if now.duration_since(last).as_secs() < MIN_EXTRACT_INTERVAL_SECS {
                    return Ok(HookResult::Allow);
                }
            }
            state.last_extract = Some(now);
        }

        let store = self
            .store_pool
            .get(&ctx.working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let small_llm = self
            .services
            .get()
            .and_then(|services| services.small_llm.clone())
            .ok_or_else(|| ExtensionError::Internal("small model capability unavailable".into()))?;
        let session_id = ctx.session_id.clone();
        let assistant_message = exchange.assistant_message.clone();

        tasks.spawn("memory-turn-extract", async move {
            // 1. 召回相关历史上下文
            let store_for_recall = store.clone();
            let query_for_recall = query.clone();
            let recalled = match tokio::task::spawn_blocking(move || {
                store_for_recall.search_contexts(
                    &query_for_recall,
                    RECALL_MAX_RESULTS,
                    RECALL_MAX_CHARS_PER_FILE,
                )
            })
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "turn-end recall failed");
                    Vec::new()
                },
                Err(e) => {
                    tracing::warn!(error = %e, "turn-end recall spawn failed");
                    Vec::new()
                },
            };

            // 2. 小模型提取记忆
            if let Err(e) = crate::pipeline::extract_turn(
                store,
                small_llm.as_ref(),
                &session_id,
                &query,
                &assistant_message,
                &recalled,
            )
            .await
            {
                tracing::warn!(error = %e, session_id = %session_id, "turn-end extraction failed");
            }
        });

        Ok(HookResult::Allow)
    }
}

// ─── SessionStart Handler (Lifecycle, NonBlocking) ───────────────────

#[derive(Default)]
pub(crate) struct MemoryPipelineCoordinator {
    state: Mutex<PipelineState>,
}

#[derive(Default)]
struct PipelineState {
    running: bool,
    pending: bool,
    latest_session_id: Option<String>,
}

impl MemoryPipelineCoordinator {
    fn request_run(&self, session_id: String) -> Option<String> {
        let mut state = self.state.lock();
        state.latest_session_id = Some(session_id);
        if state.running {
            state.pending = true;
            None
        } else {
            state.running = true;
            state.latest_session_id.clone()
        }
    }

    fn complete_run(&self) -> Option<String> {
        let mut state = self.state.lock();
        if state.pending {
            state.pending = false;
            state.latest_session_id.clone()
        } else {
            state.running = false;
            None
        }
    }

    pub(crate) fn reset(&self) {
        *self.state.lock() = PipelineState::default();
    }
}

pub(crate) struct MemorySessionStartHandler {
    pub store_pool: Arc<MemoryStorePool>,
    pub services: MemoryServices,
    pub pipeline: Arc<MemoryPipelineCoordinator>,
    pub tasks: Arc<Mutex<Option<ExtensionTasks>>>,
}

#[async_trait::async_trait]
impl LifecycleHandler for MemorySessionStartHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let Some(tasks) = self.tasks.lock().clone() else {
            tracing::debug!(session_id = %ctx.session_id, "memory extension not started");
            return Ok(HookResult::Allow);
        };
        let shutdown = tasks.shutdown();
        if shutdown.is_cancelled() {
            tracing::debug!(session_id = %ctx.session_id, "memory extension is stopping");
            return Ok(HookResult::Allow);
        }

        let store = self
            .store_pool
            .get(&ctx.working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let services = self
            .services
            .get()
            .ok_or_else(|| ExtensionError::Internal("memory host services unavailable".into()))?;
        let session_read = services.session_read.clone().ok_or_else(|| {
            ExtensionError::Internal("session history capability unavailable".into())
        })?;
        let small_llm = services
            .small_llm
            .clone()
            .ok_or_else(|| ExtensionError::Internal("small model capability unavailable".into()))?;
        let Some(mut current_session_id) = self.pipeline.request_run(ctx.session_id.to_string())
        else {
            tracing::debug!(session_id = %ctx.session_id, "memory pipeline queued");
            return Ok(HookResult::Allow);
        };
        let pipeline = self.pipeline.clone();

        tasks.spawn("memory-pipeline", async move {
            loop {
                let run = crate::pipeline::run(
                    &store,
                    Arc::clone(&session_read),
                    &*small_llm,
                    &current_session_id,
                );

                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::debug!("memory pipeline stopped");
                        break;
                    },
                    result = run => {
                        if let Err(e) = result {
                            tracing::warn!(error = %e, session_id = %current_session_id, "memory pipeline failed");
                        }
                    },
                }

                if shutdown.is_cancelled() {
                    break;
                }
                let Some(next_session_id) = pipeline.complete_run() else {
                    break;
                };
                tracing::debug!(
                    session_id = %next_session_id,
                    "memory pipeline replaying queued session start trigger"
                );
                current_session_id = next_session_id;
            }
        });

        Ok(HookResult::Allow)
    }
}

// ─── Command Handler (/memory) ───────────────────────────────────────

pub(crate) struct MemoryCommandHandler {
    pub store_pool: Arc<MemoryStorePool>,
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::CommandHandler for MemoryCommandHandler {
    async fn execute(
        &self,
        _command_name: &str,
        args: &str,
        working_dir: &str,
        _ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let store = self
            .store_pool
            .get(working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let args = args.trim().to_string();

        let result = tokio::task::spawn_blocking(move || -> Result<String, std::io::Error> {
            match args.as_str() {
                "" | "list" => {
                    let entries = store.list_entries(MAX_LIST_ENTRIES)?;
                    if entries.is_empty() {
                        Ok("No memories saved yet.".to_string())
                    } else {
                        Ok(entries.join("\n"))
                    }
                },
                rest if rest.starts_with("search ") => {
                    let query = &rest[7..];
                    let results = store.search(query, 10)?;
                    if results.is_empty() {
                        Ok("No matching memories found.".to_string())
                    } else {
                        Ok(results.join("\n"))
                    }
                },
                rest if rest.starts_with("delete ") => {
                    let pattern = rest[7..].trim();
                    if pattern.is_empty() {
                        return Ok("No pattern provided. Nothing deleted.".to_string());
                    }
                    let removed = store.delete_by_content(pattern)?;
                    if removed.is_empty() {
                        Ok("No matching memories found to delete.".to_string())
                    } else {
                        Ok(format!(
                            "Deleted {} entries:\n{}",
                            removed.len(),
                            removed.join("\n")
                        ))
                    }
                },
                _ => Ok("Usage: /memory [list|search <query>|delete <pattern>]".to_string()),
            }
        })
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        Ok(ExtensionCommandResult::Display {
            content: result,
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryPipelineCoordinator;

    #[test]
    fn pipeline_coordinator_coalesces_pending_runs() {
        let coordinator = MemoryPipelineCoordinator::default();

        assert_eq!(
            coordinator.request_run("session-a".to_string()),
            Some("session-a".to_string())
        );
        assert_eq!(coordinator.request_run("session-b".to_string()), None);
        assert_eq!(coordinator.request_run("session-c".to_string()), None);
        assert_eq!(coordinator.complete_run(), Some("session-c".to_string()));
        assert_eq!(coordinator.complete_run(), None);
        assert_eq!(
            coordinator.request_run("session-d".to_string()),
            Some("session-d".to_string())
        );
    }
}
