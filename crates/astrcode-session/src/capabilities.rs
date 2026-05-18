//! 跨 session 共享的运行时能力。
//!
//! `Capabilities` 聚合所有 session 都需要的基础设施引用：LLM、扩展、上下文组装器
//! 以及当前生效的配置。Session 创建时持有 `Arc<Capabilities>`，运行 turn 时按需读取。
//!
//! `llm` 与 `effective_config` 支持热替换：server 端配置变更时通过 `swap_llm` /
//! `update_effective` 原子更新，正在运行的 turn 在下一轮 LLM 调用前看到新值。

use std::sync::Arc;

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{config::EffectiveConfig, llm::LlmProvider};
use astrcode_extensions::runner::ExtensionRunner;
use parking_lot::{RwLock, RwLockReadGuard};

pub struct Capabilities {
    llm: RwLock<Arc<dyn LlmProvider>>,
    extension_runner: Arc<ExtensionRunner>,
    context_assembler: Arc<LlmContextAssembler>,
    effective_config: RwLock<EffectiveConfig>,
}

impl Capabilities {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        extension_runner: Arc<ExtensionRunner>,
        context_assembler: Arc<LlmContextAssembler>,
        effective_config: EffectiveConfig,
    ) -> Self {
        Self {
            llm: RwLock::new(llm),
            extension_runner,
            context_assembler,
            effective_config: RwLock::new(effective_config),
        }
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        self.llm.read().clone()
    }

    pub fn swap_llm(&self, new: Arc<dyn LlmProvider>) {
        *self.llm.write() = new;
    }

    pub fn extension_runner(&self) -> &Arc<ExtensionRunner> {
        &self.extension_runner
    }

    pub fn context_assembler(&self) -> &Arc<LlmContextAssembler> {
        &self.context_assembler
    }

    pub fn read_effective(&self) -> RwLockReadGuard<'_, EffectiveConfig> {
        self.effective_config.read()
    }

    pub fn update_effective(&self, new: EffectiveConfig) {
        *self.effective_config.write() = new;
    }
}
