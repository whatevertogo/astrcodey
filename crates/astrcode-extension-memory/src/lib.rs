//! astrcode-extension-memory — 持久化记忆扩展。
//!
//! - **用户记忆**（跨项目）：runtime 归属的 extension data 根目录（`user_pref`）
//! - **项目记忆**：同一根目录下的 `projects/<key>/`
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

use std::sync::Arc;

use astrcode_extension_sdk::{
    builder::{custom_event, manifest},
    extension::{
        Extension, ExtensionCall, ExtensionCapability, ExtensionConfig, ExtensionError,
        ExtensionManifest, ExtensionStartContext, ExtensionStopContext, HookMode, LifecycleEvent,
        Registrar,
    },
};
use handlers::{
    MemoryDeleteHandler, MemoryListHandler, MemoryRecallHandler, MemorySaveHandler,
    MemorySessionStartHandler,
};
use parking_lot::RwLock;
use store::MemoryStorePool;
use turn_recall::{
    MemoryProjectRecallDeliveryProvider, MemoryProjectRecallTurnEndHandler, SessionPrefsCache,
};

use crate::config::MemoryConfig;

/// 返回记忆扩展；所需宿主能力在标准 `start()` 生命周期中取得。
pub fn extension() -> Arc<dyn Extension> {
    let store_pool = Arc::new(MemoryStorePool::new());
    let pipeline = Arc::new(handlers::MemoryPipelineCoordinator::default());
    let session_prefs = Arc::new(SessionPrefsCache::default());
    Arc::new(MemoryExtension {
        store_pool,
        pipeline,
        session_prefs,
        config: Arc::new(RwLock::new(MemoryConfig::default())),
    })
}

struct MemoryExtension {
    store_pool: Arc<MemoryStorePool>,
    pipeline: Arc<handlers::MemoryPipelineCoordinator>,
    session_prefs: Arc<SessionPrefsCache>,
    config: Arc<RwLock<MemoryConfig>>,
}

#[async_trait::async_trait]
impl Extension for MemoryExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("astrcode.memory")
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::SmallModel)
            .capability(ExtensionCapability::SessionInspect)
            .capability(ExtensionCapability::EmitCustomEvents)
            .capability(ExtensionCapability::ProviderRequest)
            .build()
    }

    fn validate_config(&self, config: &ExtensionConfig) -> Result<(), ExtensionError> {
        MemoryConfig::from_extension_config(config)
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let data_dir = ctx.paths().global_data_dir().ok_or_else(|| {
            ExtensionError::Internal("memory extension data directory is unavailable".into())
        })?;
        self.store_pool
            .set_root(data_dir.to_path_buf())
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;
        let small_model_available = ctx
            .host()
            .models()
            .small_available()
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;
        if !small_model_available {
            return Err(ExtensionError::Internal(
                "memory extension requires a configured small model provider".into(),
            ));
        }
        *self.config.write() = MemoryConfig::from_extension_config(ctx.config())?;
        Ok(())
    }

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        self.pipeline.reset();
        self.session_prefs.reset();
        Ok(())
    }

    fn register(&self, reg: &mut Registrar) {
        reg.declare_custom_event(custom_event(handlers::MEMORY_CREATED_EVENT_TYPE).build());
        reg.declare_custom_event(custom_event(handlers::MEMORY_DELETED_EVENT_TYPE).build());

        reg.tool(
            handlers::memory_save_definition(),
            Arc::new(MemorySaveHandler {
                store_pool: self.store_pool.clone(),
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
        reg.on_provider_contribution(
            40,
            Arc::new(MemoryProjectRecallDeliveryProvider {
                config: self.config.clone(),
            }),
        );
        reg.on_lifecycle(
            LifecycleEvent::TurnEnd,
            HookMode::NonBlocking,
            0,
            Arc::new(MemoryProjectRecallTurnEndHandler {
                store_pool: self.store_pool.clone(),
                config: self.config.clone(),
            }),
        );
        reg.on_lifecycle(
            LifecycleEvent::SessionStart,
            HookMode::NonBlocking,
            0,
            Arc::new(MemorySessionStartHandler {
                store_pool: self.store_pool.clone(),
                pipeline: self.pipeline.clone(),
                config: self.config.clone(),
                session_prefs: self.session_prefs.clone(),
            }),
        );
    }
}
