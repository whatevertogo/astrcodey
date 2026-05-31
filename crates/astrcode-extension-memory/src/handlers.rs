//! Memory handlers — Save, Delete, PromptBuild, SessionStart, Command。

use std::{collections::BTreeMap, sync::Arc};

use astrcode_extension_sdk::{
    extension::{
        CommandContext, ExtensionCommandResult, ExtensionError, ExtensionTasks, HookResult,
        LifecycleContext, LifecycleHandler, PromptBuildContext, PromptBuildHandler,
        PromptContributions, SlashCommand, ToolHandler,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use serde_json::json;

use crate::{
    MemoryServices,
    config::MemoryConfig,
    pipeline,
    store::{AppendResult, MemoryStorePool},
};

// ─── 常量 ────────────────────────────────────────────────────────────

const MEMORY_SAVE_TOOL: &str = "memory_save";
const MEMORY_DELETE_TOOL: &str = "memory_delete";
const MEMORY_CMD: &str = "memory";
const MAX_LIST_ENTRIES: usize = 50;

// ─── Tool Definitions ────────────────────────────────────────────────

pub(crate) fn memory_save_definition() -> ToolDefinition {
    ToolDefinition {
        name: MEMORY_SAVE_TOOL.to_string(),
        description: "Save information to long-term memory for future sessions.\n\nWhen NOT to \
                      use:\n- Secrets, tokens, credentials, or one-off debug output\n- Facts \
                      already in AGENTS.md or easy to re-read from the repo next turn\n\nTips:\n- \
                      Stable user preferences, project decisions, or recurring patterns worth \
                      recalling later."
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
        description: "Delete entries from long-term memory by content match.\n\nWhen NOT to \
                      use:\n- Clearing session-local context (use normal conversation \
                      flow)\n\nTips:\n- User asks to forget specific stored facts\n- Correcting \
                      outdated memory entries"
            .to_string(),
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
            }
            AppendResult::SimilarExists(similar) => Ok(ok_text(format!(
                "Similar memories already exist:\n{}\n\nPlease consolidate: \
                 use memory_delete to remove the old entries, then memory_save the \
                 consolidated version.",
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

// ─── PromptBuild — 工具说明 + 全局偏好 ───────────────────────────────

pub(crate) struct MemoryRecallHandler {
    pub store_pool: Arc<MemoryStorePool>,
}

#[async_trait::async_trait]
impl PromptBuildHandler for MemoryRecallHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let scoped = self
            .store_pool
            .get_scoped(&ctx.working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        let global_prefs = tokio::task::spawn_blocking(move || scoped.global_preference_lines(3))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .unwrap_or_default();

        let mut body = format!(
            "<memory-tools>\nYou have a persistent memory system.\n- Use `{MEMORY_SAVE_TOOL}` to \
             store important facts.\n- Use `{MEMORY_DELETE_TOOL}` to remove outdated entries.\n- \
             Changed past sessions are summarized into memory on session start and after you \
             save; use `memory_save` when something important should be recorded immediately.\n- \
             User preferences live in `~/.astrcode/memory/` and are shared across projects."
        );
        if !global_prefs.is_empty() {
            body.push_str("\n\nStable preferences (high confidence):\n");
            for line in global_prefs {
                body.push_str(&line);
                body.push('\n');
            }
        }
        body.push_str("\n</memory-tools>");

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
        let scoped = self
            .store_pool
            .get_scoped(working_dir)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let args = args.trim().to_string();

        let result = tokio::task::spawn_blocking(move || -> Result<String, std::io::Error> {
            match args.as_str() {
                "" | "list" => {
                    let entries = scoped.list_entries(MAX_LIST_ENTRIES)?;
                    if entries.is_empty() {
                        Ok("No memories saved yet.".to_string())
                    } else {
                        Ok(entries.join("\n"))
                    }
                },
                rest if rest.starts_with("search ") => {
                    let query = &rest[7..];
                    let results = scoped.search(query, 10)?;
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
                    let removed = scoped.delete_by_content(pattern)?;
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
            status_update: None,
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
            coordinator.request_run("session-a".to_string(), "/tmp/a".to_string()),
            Some(("session-a".to_string(), "/tmp/a".to_string()))
        );
        assert_eq!(
            coordinator.request_run("session-b".to_string(), "/tmp/b".to_string()),
            None
        );
        assert_eq!(
            coordinator.complete_run(),
            Some(("session-b".to_string(), "/tmp/b".to_string()))
        );
        assert_eq!(coordinator.complete_run(), None);
        assert_eq!(
            coordinator.request_run("session-d".to_string(), "/tmp/d".to_string()),
            Some(("session-d".to_string(), "/tmp/d".to_string()))
        );
    }
}
