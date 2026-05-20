//! 跨 session 共享的运行时能力。
//!
//! `SessionRuntimeServices` 聚合所有 session 都需要的基础设施引用：LLM、扩展、上下文组装器
//! 以及当前生效的配置。Session 创建时持有 `Arc<SessionRuntimeServices>`，运行 turn 时按需读取。
//!
//! `llm` 与 `effective_config` 支持热替换：server 端配置变更时通过 `swap_llm` /
//! `update_effective` 原子更新，正在运行的 turn 在下一轮 LLM 调用前看到新值。
//!
//! TODO: 当前热替换走 `RwLock<Arc<dyn LlmProvider>>` —— 写者每次配置变更才动一次锁，
//! 读者每个 turn 拉一次快照，实践上没冲突。但读者落在快路径，未来若 provider 切换
//! 更频繁（例如多 profile 在 turn 之间动态切换），应换成 `arc_swap::ArcSwap` 实现
//! 无锁原子读，避免 `parking_lot::RwLock::read` 的原子计数。`effective_config` 若
//! 也改成 `Arc<EffectiveConfig>` + `ArcSwap` 路径，可以一并消除 `read_effective`
//! 返回 `RwLockReadGuard` 限制（持有期间不能 await）的隐式约束。

use std::sync::Arc;

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{config::EffectiveConfig, llm::LlmProvider};
use astrcode_extensions::runner::ExtensionRunner;
use parking_lot::{RwLock, RwLockReadGuard};

pub struct SessionRuntimeServices {
    llm: RwLock<Arc<dyn LlmProvider>>,
    extension_runner: Arc<ExtensionRunner>,
    context_assembler: Arc<LlmContextAssembler>,
    effective_config: RwLock<EffectiveConfig>,
}

impl SessionRuntimeServices {
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

    /// 获取 session_ops 能力引用（从 extension_runner 读取）。
    pub fn session_ops(&self) -> Option<Arc<dyn astrcode_core::tool::SessionOperations>> {
        let ops_ref = self.extension_runner.session_ops_ref();
        let guard = ops_ref.read().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    }
}
