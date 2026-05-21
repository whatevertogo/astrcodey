//! Memory handlers — Recall, Save, Search, Observe, Command。

use std::{collections::BTreeMap, sync::Arc};

use astrcode_core::{
    extension::{
        CommandContext, ExchangeSummary, ExtensionCommandResult, ExtensionError, HookResult,
        LifecycleContext, PromptBuildContext, PromptBuildHandler, PromptContributions,
        SlashCommand, ToolHandler,
    },
    llm::{LlmEvent, LlmMessage, LlmRole},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use serde::Deserialize;
use serde_json::json;

use crate::store::MemoryStore;

// ─── 常量 ────────────────────────────────────────────────────────────

const MEMORY_SAVE_TOOL: &str = "memory_save";
const MEMORY_SEARCH_TOOL: &str = "memory_search";
const MEMORY_CMD: &str = "memory";
const MAX_MEMORY_CHARS: usize = 8_000;
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
        description: "Manage long-term memory (list, search)".to_string(),
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
        let content = match tokio::task::spawn_blocking(move || store.read_memory()).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "memory recall: failed to read MEMORY.md");
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

        let truncated = if content.len() > MAX_MEMORY_CHARS {
            let mut end = MAX_MEMORY_CHARS;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            &content[..end]
        } else {
            &content
        };

        Ok(PromptContributions {
            additional_instructions: vec![format!(
                "<memory>\nYou have a persistent memory system. Use `{MEMORY_SAVE_TOOL}` to store \
                 important information and `{MEMORY_SEARCH_TOOL}` to recall past \
                 memories.\n\n{truncated}\n</memory>"
            )],
            ..Default::default()
        })
    }
}

// ─── Observe Handler (Lifecycle TurnEnd, NonBlocking) ────────────────

pub(crate) struct MemoryObserveHandler {
    pub store: Arc<MemoryStore>,
    pub small_llm: Option<Arc<dyn astrcode_core::llm::LlmProvider>>,
}

#[derive(Deserialize)]
struct ExtractionResult {
    should_save: bool,
    #[serde(default)]
    memories: Vec<ExtractedMemory>,
}

#[derive(Deserialize)]
struct ExtractedMemory {
    content: String,
    #[serde(default = "default_category")]
    category: String,
}

impl MemoryObserveHandler {
    async fn extract_memories(
        &self,
        exchange: &ExchangeSummary,
    ) -> Result<Option<ExtractionResult>, ExtensionError> {
        let small_llm = match &self.small_llm {
            Some(llm) => llm,
            None => return Ok(None),
        };

        let prompt = format!(
            "Analyze this conversation exchange. Determine if any information is worth saving to \
             long-term memory.\nFocus on: user preferences, project decisions, coding patterns, \
             important facts.\nIgnore: greetings, simple Q&A, tool usage details.\n\nUser: \
             {}\nAssistant: {}\n\nRespond with JSON only: {{ \"should_save\": bool, \"memories\": \
             [{{ \"content\": \"...\", \"category\": \"user_pref|project_ctx|decision|general\" \
             }}] }}",
            exchange.user_message, exchange.assistant_message
        );

        let messages = vec![LlmMessage {
            role: LlmRole::User,
            content: vec![astrcode_core::llm::LlmContent::Text { text: prompt }],
            name: None,
            reasoning_content: None,
        }];

        let rx = small_llm
            .generate(messages, vec![])
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        let text = collect_stream_text(rx).await;
        let text = text.trim();

        let json_str = text
            .strip_prefix("```json")
            .and_then(|s| s.strip_suffix("```"))
            .map(|s| s.trim())
            .unwrap_or(text);

        serde_json::from_str(json_str)
            .map(Some)
            .map_err(|e| ExtensionError::Internal(format!("parse extraction: {e}")))
    }
}

async fn collect_stream_text(mut rx: tokio::sync::mpsc::UnboundedReceiver<LlmEvent>) -> String {
    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => text.push_str(&delta),
            LlmEvent::Done { .. } => break,
            _ => {},
        }
    }
    text
}

#[async_trait::async_trait]
impl astrcode_core::extension::LifecycleHandler for MemoryObserveHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let exchange = match &ctx.last_exchange {
            Some(e) => e,
            None => return Ok(HookResult::Allow),
        };

        if exchange.user_message.len() < 10 && exchange.assistant_message.len() < 20 {
            return Ok(HookResult::Allow);
        }

        let extraction = match self.extract_memories(exchange).await? {
            Some(r) if r.should_save => r,
            _ => return Ok(HookResult::Allow),
        };

        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            for m in &extraction.memories {
                if let Err(e) = store.append(&m.category, &m.content) {
                    tracing::warn!(error = %e, "failed to save extracted memory");
                }
            }
        })
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        Ok(HookResult::Allow)
    }
}

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
