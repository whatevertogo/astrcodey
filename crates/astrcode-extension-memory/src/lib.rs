//! astrcode-extension-memory — 持久化记忆扩展。
//!
//! - **用户记忆**（跨项目）：`~/.astrcode/memory/`（`user_pref`）
//! - **项目记忆**：`~/.astrcode/projects/<key>/extension_data/astrcode.memory/`
//! - `memory_index.json`：结构化索引（BM25/子串搜索；相似条目 upsert）
//! - **SessionStart** / **`memory_save` 后**：从有变化的 rollout 批量提取，更新 MEMORY.md
//! - **PromptBuild**：全量用户偏好（SessionStart 预加载快照，session 内只读）
//! - **TurnEnd**：按当轮对话召回项目事实；下一 turn 首次 LLM 请求时注入
//! - LLM 工具：`memory_save` / `memory_delete`

mod config;
mod handlers;
mod index;
mod pipeline;
mod prompts;
mod scope;
mod store;
mod turn_recall;

use std::sync::{Arc, OnceLock};

use astrcode_extension_sdk::{
    extension::{
        Extension, ExtensionCapability, ExtensionConfig, ExtensionCtx, ExtensionError,
        ExtensionEvent, ExtensionTasks, HookMode, ProviderEvent, Registrar, StopReason,
    },
    trusted::ExtensionHostServices,
};
use handlers::{
    MemoryDeleteHandler, MemoryListHandler, MemoryRecallHandler, MemorySaveHandler,
    MemorySessionStartHandler,
};
use parking_lot::{Mutex, RwLock};
use store::MemoryStorePool;
use turn_recall::{
    MemoryProjectRecallDeliveryProvider, MemoryProjectRecallTurnEndHandler, ProjectRecallBuffer,
    SessionPrefsCache,
};

use crate::config::MemoryConfig;

/// 返回记忆扩展；所需宿主能力在标准 `start()` 生命周期中取得。
pub fn extension() -> Arc<dyn Extension> {
    let store_pool = Arc::new(MemoryStorePool::new());
    let pipeline = Arc::new(handlers::MemoryPipelineCoordinator::default());
    let session_prefs = Arc::new(SessionPrefsCache::default());
    let project_recall_buffer = Arc::new(ProjectRecallBuffer::default());
    Arc::new(MemoryExtension {
        store_pool,
        services: Arc::new(OnceLock::new()),
        pipeline,
        session_prefs,
        project_recall_buffer,
        tasks: Arc::new(Mutex::new(None)),
        config: Arc::new(RwLock::new(MemoryConfig::default())),
    })
}

pub(crate) type MemoryServices = Arc<OnceLock<Arc<ExtensionHostServices>>>;

struct MemoryExtension {
    store_pool: Arc<MemoryStorePool>,
    services: MemoryServices,
    pipeline: Arc<handlers::MemoryPipelineCoordinator>,
    session_prefs: Arc<SessionPrefsCache>,
    project_recall_buffer: Arc<ProjectRecallBuffer>,
    tasks: Arc<Mutex<Option<ExtensionTasks>>>,
    config: Arc<RwLock<MemoryConfig>>,
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
        *self.config.write() = MemoryConfig::from_extension_config(&ctx.config);
        Ok(())
    }

    async fn on_config_changed(&self, config: ExtensionConfig) -> Result<(), ExtensionError> {
        *self.config.write() = MemoryConfig::from_extension_config(&config);
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        *self.tasks.lock() = None;
        self.pipeline.reset();
        self.session_prefs.reset();
        self.project_recall_buffer.reset();
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
                services: self.services.clone(),
                tasks: self.tasks.clone(),
                pipeline: self.pipeline.clone(),
                config: self.config.clone(),
            }),
        );
        reg.tool(
            handlers::memory_delete_definition(),
            Arc::new(MemoryDeleteHandler {
                store_pool: self.store_pool.clone(),
            }),
        );
        reg.tool(
            handlers::memory_list_definition(),
            Arc::new(MemoryListHandler {
                store_pool: self.store_pool.clone(),
            }),
        );
        reg.on_prompt_build(
            0,
            Arc::new(MemoryRecallHandler {
                store_pool: self.store_pool.clone(),
                session_prefs: self.session_prefs.clone(),
            }),
        );
        reg.on_provider(
            ProviderEvent::BeforeRequest,
            HookMode::Blocking,
            40,
            Arc::new(MemoryProjectRecallDeliveryProvider {
                buffer: self.project_recall_buffer.clone(),
                config: self.config.clone(),
            }),
        );
        reg.on_event(
            ExtensionEvent::TurnEnd,
            HookMode::NonBlocking,
            0,
            Arc::new(MemoryProjectRecallTurnEndHandler {
                store_pool: self.store_pool.clone(),
                buffer: self.project_recall_buffer.clone(),
                config: self.config.clone(),
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
                config: self.config.clone(),
                session_prefs: self.session_prefs.clone(),
            }),
        );
    }
}
