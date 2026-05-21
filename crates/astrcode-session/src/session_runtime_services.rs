//! 跨 session 共享的运行时能力。
//!
//! `SessionRuntimeServices` 聚合所有 session 都需要的基础设施引用：LLM、扩展、上下文组装器
//! 以及当前生效的配置。Session 创建时持有 `Arc<SessionRuntimeServices>`，运行 turn 时按需读取。
//!
//! `llm` 与 `effective_config` 支持热替换：server 端配置变更时通过 `swap_llm` /
//! `update_effective` 原子更新，正在运行的 turn 在下一轮 LLM 调用前看到新值。
//! 快路径读取使用 `ArcSwap`，避免每个 turn 为获取 provider / config 快照进入读锁。

use std::sync::Arc;

use arc_swap::ArcSwap;
use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{config::EffectiveConfig, llm::LlmProvider};
use astrcode_extensions::runner::ExtensionRunner;

pub struct SessionRuntimeServices {
    llm: ArcSwap<ProviderSlot>,
    /// 小模型 provider slot。未配置小模型时与主模型相同。
    small_llm: ArcSwap<ProviderSlot>,
    extension_runner: Arc<ExtensionRunner>,
    context_assembler: Arc<LlmContextAssembler>,
    effective_config: ArcSwap<EffectiveConfig>,
}

struct ProviderSlot {
    provider: Arc<dyn LlmProvider>,
}

impl SessionRuntimeServices {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        small_llm: Arc<dyn LlmProvider>,
        extension_runner: Arc<ExtensionRunner>,
        context_assembler: Arc<LlmContextAssembler>,
        effective_config: EffectiveConfig,
    ) -> Self {
        Self {
            llm: ArcSwap::from_pointee(ProviderSlot { provider: llm }),
            small_llm: ArcSwap::from_pointee(ProviderSlot {
                provider: small_llm,
            }),
            extension_runner,
            context_assembler,
            effective_config: ArcSwap::from_pointee(effective_config),
        }
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.llm.load_full().provider)
    }

    pub fn swap_llm(&self, new: Arc<dyn LlmProvider>) {
        self.llm.store(Arc::new(ProviderSlot { provider: new }));
    }

    /// 返回小模型 provider。
    ///
    /// 未配置小模型时返回的与主模型相同。
    pub fn small_llm(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.small_llm.load_full().provider)
    }

    /// 热替换小模型 provider。
    pub fn swap_small_llm(&self, new: Arc<dyn LlmProvider>) {
        self.small_llm
            .store(Arc::new(ProviderSlot { provider: new }));
    }

    pub fn extension_runner(&self) -> &Arc<ExtensionRunner> {
        &self.extension_runner
    }

    pub fn context_assembler(&self) -> &Arc<LlmContextAssembler> {
        &self.context_assembler
    }

    pub fn read_effective(&self) -> Arc<EffectiveConfig> {
        self.effective_config.load_full()
    }

    pub fn update_effective(&self, new: EffectiveConfig) {
        self.effective_config.store(Arc::new(new));
    }

    /// 获取 session_ops 能力引用（从 extension_runner 读取）。
    pub fn session_ops(&self) -> Option<Arc<dyn astrcode_core::tool::SessionOperations>> {
        let ops_ref = self.extension_runner.session_ops_ref();
        let guard = ops_ref.read().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    }
}
