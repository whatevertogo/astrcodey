//! Memory handlers — Recall, Save, Search, SessionStart, Command。

use std::{collections::BTreeMap, sync::Arc};

use astrcode_core::{
    extension::{
        CommandContext, ExtensionCommandResult, ExtensionError, ExtensionTasks, HookResult,
        LifecycleContext, LifecycleHandler, PromptBuildContext, PromptBuildHandler,
        PromptContributions, SessionReadSource, SlashCommand, ToolHandler,
    },
    llm::LlmProvider,
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;

use crate::store::MemoryStore;

// ─── 常量 ────────────────────────────────────────────────────────────

const MEMORY_SAVE_TOOL: &str = "memory_save";
const MEMORY_SEARCH_TOOL: &str = "memory_search";
const MEMORY_CMD: &str = "memory";
const MAX_RECALL_FALLBACK_CHARS: usize = 1_200;
const MAX_SEARCH_RESULTS: usize = 20;
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

pub(crate) fn memory_search_definition() -> ToolDefinition {
    ToolDefinition {
        name: MEMORY_SEARCH_TOOL.to_string(),
        description: "Search long-term memory for previously saved information.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search keywords" },
                "limit": { "type": "integer", "description": "Max results. Default: 10" }
            },
            "required": ["query"]
        }),
        execution_mode: ExecutionMode::Sequential,
        origin: ToolOrigin::Extension,
    }
}

pub(crate) fn memory_command_definition() -> SlashCommand {
    SlashCommand {
        name: MEMORY_CMD.to_string(),
        description: "Manage long-term memory (list, search, consolidate)".to_string(),
        args_schema: None,
    }
}

fn ok_text(content: String) -> ToolResult {
    ToolResult::text(content, false, BTreeMap::new())
}

// ─── Save Handler ────────────────────────────────────────────────────

pub(crate) struct MemorySaveHandler {
    pub store: Arc<MemoryStore>,
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
        _working_dir: &str,
        _ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let args: SaveArgs = serde_json::from_value(arguments)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let store = self.store.clone();
        let content = args.content;
        let category = args.category;
        tokio::task::spawn_blocking(move || store.append(&category, &content))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        Ok(ok_text("Memory saved.".to_string()))
    }
}

// ─── Search Handler ──────────────────────────────────────────────────

pub(crate) struct MemorySearchHandler {
    pub store: Arc<MemoryStore>,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[async_trait::async_trait]
impl ToolHandler for MemorySearchHandler {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: serde_json::Value,
        _working_dir: &str,
        _ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let args: SearchArgs = serde_json::from_value(arguments)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let store = self.store.clone();
        let query = args.query;
        let limit = args.limit.min(MAX_SEARCH_RESULTS);
        let results = tokio::task::spawn_blocking(move || store.search(&query, limit))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        if results.is_empty() {
            Ok(ok_text("No matching memories found.".to_string()))
        } else {
            Ok(ok_text(results.join("\n")))
        }
    }
}

// ─── Recall Handler (PromptBuild) ────────────────────────────────────

pub(crate) struct MemoryRecallHandler {
    pub store: Arc<MemoryStore>,
}

const HEADER: &str = "# Memory\n\n";

#[async_trait::async_trait]
impl PromptBuildHandler for MemoryRecallHandler {
    async fn handle(
        &self,
        _ctx: PromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        let store = self.store.clone();
        let content = match tokio::task::spawn_blocking(move || {
            let summary = store.read_summary()?;
            if !summary.trim().is_empty() {
                return Ok(summary);
            }
            store
                .read_memory()
                .map(|content| truncate_to_chars(&content, MAX_RECALL_FALLBACK_CHARS))
        })
        .await
        {
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

        if content.trim().len() <= HEADER.trim().len() {
            return Ok(PromptContributions::default());
        }

        Ok(PromptContributions {
            additional_instructions: vec![format!(
                "<memory>\nYou have a persistent memory system. Use `{MEMORY_SAVE_TOOL}` to store \
                 important information and `{MEMORY_SEARCH_TOOL}` to recall past \
                 memories.\n\n{content}\n</memory>"
            )],
            ..Default::default()
        })
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
    pub store: Arc<MemoryStore>,
    pub session_read: Arc<dyn SessionReadSource>,
    pub small_llm: Option<Arc<dyn LlmProvider>>,
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

        let store = self.store.clone();
        let session_read = self.session_read.clone();
        let small_llm = self.small_llm.clone();
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
                    session_read.as_ref(),
                    small_llm.as_deref(),
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

use crate::pipeline::truncate_to_chars;

// ─── Command Handler (/memory) ───────────────────────────────────────

pub(crate) struct MemoryCommandHandler {
    pub store: Arc<MemoryStore>,
}

#[async_trait::async_trait]
impl astrcode_core::extension::CommandHandler for MemoryCommandHandler {
    async fn execute(
        &self,
        _command_name: &str,
        args: &str,
        _working_dir: &str,
        _ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let store = self.store.clone();
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
                _ => Ok("Usage: /memory [list|search <query>]".to_string()),
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
