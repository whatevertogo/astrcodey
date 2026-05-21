//! astrcode-extension-memory — Codex-style Markdown 记忆插件。
//!
//! 提供跨会话的持久化记忆，借鉴 Codex 的设计：
//! - Markdown 文件存储，人类可读可编辑
//! - PromptBuild 注入 memory_summary.md（精简摘要）
//! - LLM 可主动 save/search
//! - SessionStart 时后台运行两阶段管线： Phase1 从历史会话提取记忆，Phase2 整合去重到 MEMORY.md

mod handlers;
mod pipeline;
mod pipeline_prompts;
mod store;

use std::sync::Arc;

use astrcode_core::{
    extension::{
        Extension, ExtensionCtx, ExtensionError, ExtensionEvent, ExtensionTasks, HookMode,
        Registrar, SessionReadSource, StopReason,
    },
    llm::LlmProvider,
};
use handlers::{
    MemoryCommandHandler, MemoryRecallHandler, MemorySaveHandler, MemorySearchHandler,
    MemorySessionStartHandler,
};
use parking_lot::Mutex;
use store::MemoryStore;

/// 返回记忆扩展。
///
/// `small_llm` 为 None 时 Phase1 自动提取跳过，Phase2 简单合并。
/// `session_read` 用于查询历史会话。
pub fn extension(
    small_llm: Option<Arc<dyn LlmProvider>>,
    session_read: Arc<dyn SessionReadSource>,
) -> Result<Arc<dyn Extension>, ExtensionError> {
    let store = MemoryStore::new().map_err(|e| ExtensionError::Internal(e.to_string()))?;
    let store = Arc::new(store);
    Ok(Arc::new(MemoryExtension {
        store,
        small_llm,
        session_read,
        pipeline: Arc::new(handlers::MemoryPipelineCoordinator::default()),
        tasks: Arc::new(Mutex::new(None)),
    }))
}

struct MemoryExtension {
    store: Arc<MemoryStore>,
    small_llm: Option<Arc<dyn LlmProvider>>,
    session_read: Arc<dyn SessionReadSource>,
    /// 进程级管线调度器：串行执行，忙时合并触发，避免丢 SessionStart。
    pipeline: Arc<handlers::MemoryPipelineCoordinator>,
    tasks: Arc<Mutex<Option<ExtensionTasks>>>,
}

#[async_trait::async_trait]
impl Extension for MemoryExtension {
    fn id(&self) -> &str {
        "astrcode.memory"
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        *self.tasks.lock() = Some(ctx.tasks().clone());
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        *self.tasks.lock() = None;
        self.pipeline.reset();
        Ok(())
    }

    fn register(&self, reg: &mut Registrar) {
        reg.extension_data_dir();
        reg.extension_event("memory.created").register();
        reg.extension_event("memory.deleted").register();

        reg.tool(
            handlers::memory_save_definition(),
            Arc::new(MemorySaveHandler {
                store: self.store.clone(),
            }),
        );
        reg.tool(
            handlers::memory_search_definition(),
            Arc::new(MemorySearchHandler {
                store: self.store.clone(),
            }),
        );
        reg.on_prompt_build(
            0,
            Arc::new(MemoryRecallHandler {
                store: self.store.clone(),
            }),
        );
        reg.on_event(
            ExtensionEvent::SessionStart,
            HookMode::NonBlocking,
            0,
            Arc::new(MemorySessionStartHandler {
                store: self.store.clone(),
                session_read: self.session_read.clone(),
                small_llm: self.small_llm.clone(),
                pipeline: self.pipeline.clone(),
                tasks: self.tasks.clone(),
            }),
        );
        reg.command(
            handlers::memory_command_definition(),
            Arc::new(MemoryCommandHandler {
                store: self.store.clone(),
            }),
        );
    }
}
