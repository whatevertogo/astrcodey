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

use std::sync::{Arc, OnceLock};

use astrcode_extension_sdk::extension::{
    Extension, ExtensionCapability, ExtensionCtx, ExtensionError, ExtensionEvent,
    ExtensionHostServices, ExtensionTasks, HookMode, Registrar, StopReason,
};
use handlers::{
    MemoryCommandHandler, MemoryDeleteHandler, MemoryRecallHandler, MemorySaveHandler,
    MemorySessionStartHandler, MemoryTurnEndHandler,
};
use parking_lot::Mutex;
use store::MemoryStorePool;

/// 返回记忆扩展；所需宿主能力在标准 `start()` 生命周期中取得。
pub fn extension() -> Arc<dyn Extension> {
    let store_pool = Arc::new(MemoryStorePool::new());
    Arc::new(MemoryExtension {
        store_pool,
        services: Arc::new(OnceLock::new()),
        pipeline: Arc::new(handlers::MemoryPipelineCoordinator::default()),
        tasks: Arc::new(Mutex::new(None)),
    })
}

pub(crate) type MemoryServices = Arc<OnceLock<Arc<ExtensionHostServices>>>;

struct MemoryExtension {
    store_pool: Arc<MemoryStorePool>,
    services: MemoryServices,
    pipeline: Arc<handlers::MemoryPipelineCoordinator>,
    tasks: Arc<Mutex<Option<ExtensionTasks>>>,
}

#[async_trait::async_trait]
impl Extension for MemoryExtension {
    fn id(&self) -> &str {
        "astrcode.memory"
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &[
            ExtensionCapability::SmallModel,
            ExtensionCapability::SessionHistory,
            ExtensionCapability::EmitEvents,
        ]
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        let services = ctx.host_services().cloned().ok_or_else(|| {
            ExtensionError::Internal("memory extension requires host services".into())
        })?;
        if services.small_llm.is_none() {
            return Err(ExtensionError::Internal(
                "memory extension requires a configured small model provider".into(),
            ));
        }
        if services.session_read.is_none() {
            return Err(ExtensionError::Internal(
                "memory extension requires session history access".into(),
            ));
        }
        self.services.set(services).map_err(|_| {
            ExtensionError::Internal("memory extension services already initialized".into())
        })?;
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
                services: self.services.clone(),
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
                services: self.services.clone(),
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
