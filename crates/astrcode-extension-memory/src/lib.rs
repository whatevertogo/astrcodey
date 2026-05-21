//! astrcode-extension-memory — Codex-style Markdown 记忆插件。
//!
//! 提供跨会话的持久化记忆，借鉴 Codex 的设计：
//! - Markdown 文件存储，人类可读可编辑
//! - PromptBuild 注入相关记忆
//! - LLM 可主动 save/search
//! - small_model 异步观察对话提取记忆

mod handlers;
mod store;

use std::sync::Arc;

use astrcode_core::{
    extension::{Extension, ExtensionEvent, HookMode, Registrar},
    llm::LlmProvider,
};
use handlers::{
    MemoryCommandHandler, MemoryObserveHandler, MemoryRecallHandler, MemorySaveHandler,
    MemorySearchHandler,
};
use store::MemoryStore;

/// 返回记忆扩展。
///
/// `small_llm` 为 None 时，TurnEnd 自动观察功能降级（不调用小模型提取），
/// 其余功能正常。
pub fn extension(small_llm: Option<Arc<dyn LlmProvider>>) -> Arc<dyn Extension> {
    let store = Arc::new(MemoryStore::new());
    Arc::new(MemoryExtension { store, small_llm })
}

struct MemoryExtension {
    store: Arc<MemoryStore>,
    small_llm: Option<Arc<dyn LlmProvider>>,
}

#[async_trait::async_trait]
impl Extension for MemoryExtension {
    fn id(&self) -> &str {
        "astrcode.memory"
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
            ExtensionEvent::TurnEnd,
            HookMode::NonBlocking,
            0,
            Arc::new(MemoryObserveHandler {
                store: self.store.clone(),
                small_llm: self.small_llm.clone(),
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
