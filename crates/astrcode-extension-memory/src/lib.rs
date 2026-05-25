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
    capability::{EventQueryCap, LlmInvokerCap},
    extension::{
        Extension, ExtensionCtx, ExtensionError, ExtensionEvent, ExtensionTasks, HookMode,
        Registrar, StopReason,
    },
};
use handlers::{
    MemoryCommandHandler, MemoryDeleteHandler, MemoryRecallHandler, MemorySaveHandler,
    MemorySessionStartHandler, MemoryTurnEndHandler,
};
use parking_lot::{Mutex, RwLock};
use store::MemoryStorePool;

/// 返回记忆扩展。
///
/// 能力（LlmInvokerCap、EventQueryCap）在 `start()` 中从 `ExtensionCtx` 获取，
/// 未配置时返回错误。
pub fn extension() -> Arc<dyn Extension> {
    let store_pool = Arc::new(MemoryStorePool::new());
    let llm_invoker: Arc<RwLock<Option<Arc<LlmInvokerCap>>>> = Arc::new(RwLock::new(None));
    let event_query: Arc<RwLock<Option<Arc<EventQueryCap>>>> = Arc::new(RwLock::new(None));
    Arc::new(MemoryExtension {
        store_pool,
        llm_invoker,
        event_query,
        pipeline: Arc::new(handlers::MemoryPipelineCoordinator::default()),
        tasks: Arc::new(Mutex::new(None)),
    })
}

struct MemoryExtension {
    store_pool: Arc<MemoryStorePool>,
    llm_invoker: Arc<RwLock<Option<Arc<LlmInvokerCap>>>>,
    event_query: Arc<RwLock<Option<Arc<EventQueryCap>>>>,
    pipeline: Arc<handlers::MemoryPipelineCoordinator>,
    tasks: Arc<Mutex<Option<ExtensionTasks>>>,
}

#[async_trait::async_trait]
impl Extension for MemoryExtension {
    fn id(&self) -> &str {
        "astrcode.memory"
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        let llm = ctx.get_capability::<LlmInvokerCap>().ok_or_else(|| {
            ExtensionError::Internal(
                "Memory extension requires LlmInvokerCap. Please configure a small model \
                 (e.g. via runtime.small_llm) to enable memory."
                    .to_string(),
            )
        })?;
        let eq = ctx.get_capability::<EventQueryCap>().ok_or_else(|| {
            ExtensionError::Internal(
                "Memory extension requires EventQueryCap.".to_string(),
            )
        })?;
        *self.llm_invoker.write() = Some(llm);
        *self.event_query.write() = Some(eq);
        *self.tasks.lock() = Some(ctx.tasks().clone());
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        *self.llm_invoker.write() = None;
        *self.event_query.write() = None;
        *self.tasks.lock() = None;
        self.pipeline.reset();
        Ok(())
    }

    fn register(&self, reg: &mut Registrar) {
        reg.require_capability::<LlmInvokerCap>();
        reg.require_capability::<EventQueryCap>();

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
                event_query: self.event_query.clone(),
                llm_invoker: self.llm_invoker.clone(),
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
                llm_invoker: self.llm_invoker.clone(),
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
