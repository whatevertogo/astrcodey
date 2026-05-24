//! astrcode-extension-memory — 持久化记忆扩展。
//!
//! 提供跨会话的持久化记忆：
//! - MEMORY.md 干净 markdown 存储，人类可读可编辑
//! - PromptBuild 注入 MEMORY.md 内容到系统提示词
//! - LLM 可主动 save / delete
//! - SessionStart 时后台运行提取管线：从历史会话提取记忆到 contexts/
//! - TurnEnd 时召回历史上下文辅助增量提取记忆

mod handlers;
mod pipeline;
mod pipeline_prompts;
mod store;

use std::sync::Arc;

use astrcode_core::{
    extension::{
        Extension, ExtensionCtx, ExtensionError, ExtensionEvent, ExtensionTasks, HookMode,
        Registrar, StopReason,
    },
    llm::LlmProvider,
    storage::EventReader,
};
use handlers::{
    MemoryCommandHandler, MemoryDeleteHandler, MemoryRecallHandler, MemorySaveHandler,
    MemorySessionStartHandler, MemoryTurnEndHandler,
};
use parking_lot::Mutex;
use store::MemoryStorePool;

/// 返回记忆扩展。
///
/// 需要 `small_llm` 用于记忆提取和增量召回。
/// `small_llm` 为 None 时返回错误，提示用户配置小模型。
pub fn extension(
    small_llm: Option<Arc<dyn LlmProvider>>,
    session_read: Arc<dyn EventReader>,
) -> Result<Arc<dyn Extension>, ExtensionError> {
    let small_llm = small_llm.ok_or_else(|| {
        ExtensionError::Internal(
            "Memory extension requires a small LLM provider. Please configure a small model (e.g. \
             via runtime.small_llm) to enable memory."
                .to_string(),
        )
    })?;

    let store_pool = Arc::new(MemoryStorePool::new());
    Ok(Arc::new(MemoryExtension {
        store_pool,
        small_llm,
        session_read,
        pipeline: Arc::new(handlers::MemoryPipelineCoordinator::default()),
        tasks: Arc::new(Mutex::new(None)),
    }))
}

struct MemoryExtension {
    store_pool: Arc<MemoryStorePool>,
    small_llm: Arc<dyn LlmProvider>,
    session_read: Arc<dyn EventReader>,
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
                store_pool: self.store_pool.clone(),
            }),
        );
        reg.tool(
            handlers::memory_delete_definition(),
            Arc::new(MemoryDeleteHandler {
                store_pool: self.store_pool.clone(),
            }),
        );
        reg.on_prompt_build(
            0,
            Arc::new(MemoryRecallHandler {
                store_pool: self.store_pool.clone(),
            }),
        );
        reg.on_event(
            ExtensionEvent::SessionStart,
            HookMode::NonBlocking,
            0,
            Arc::new(MemorySessionStartHandler {
                store_pool: self.store_pool.clone(),
                session_read: self.session_read.clone(),
                small_llm: self.small_llm.clone(),
                pipeline: self.pipeline.clone(),
                tasks: self.tasks.clone(),
            }),
        );
        reg.on_event(
            ExtensionEvent::TurnEnd,
            HookMode::NonBlocking,
            0,
            Arc::new(MemoryTurnEndHandler {
                store_pool: self.store_pool.clone(),
                small_llm: self.small_llm.clone(),
                tasks: self.tasks.clone(),
                extract_state: Default::default(),
            }),
        );
        reg.command(
            handlers::memory_command_definition(),
            Arc::new(MemoryCommandHandler {
                store_pool: self.store_pool.clone(),
            }),
        );
    }
}
