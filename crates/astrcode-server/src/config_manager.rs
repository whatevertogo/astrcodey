//! 配置与 LLM 提供者的联合管理器。
//!
//! 封装 `config_store` / `raw_config` / `effective` / `llm_provider` 的联动更新：
//! 每次配置变更都会原子地更新三者并重建 provider。

use std::sync::Arc;

use astrcode_ai::create_provider;
use astrcode_core::{
    config::{Config, ConfigStore, EffectiveConfig},
    llm::{LlmClientConfig, LlmProvider},
};
use astrcode_session::SessionRuntimeServices;
use parking_lot::RwLock;

pub struct ConfigManager {
    config_store: Arc<dyn ConfigStore>,
    raw_config: RwLock<Config>,
    effective: RwLock<EffectiveConfig>,
    llm_provider: RwLock<Arc<dyn LlmProvider>>,
    /// 共享给所有 session 的能力快照。
    ///
    /// `apply_raw_config_and_rebuild` / `rebuild_provider_from_effective` / `set_llm_provider`
    /// 会同步把新的 LLM provider 与 EffectiveConfig 推到这里，让正在运行的 session
    /// 在下一轮 LLM 调用前看到新值。`Bootstrap` 之后通过 `attach_capabilities` 注入。
    capabilities: RwLock<Option<Arc<SessionRuntimeServices>>>,
}

fn build_provider_from_effective(effective: &EffectiveConfig) -> Arc<dyn LlmProvider> {
    let llm_config = LlmClientConfig {
        base_url: effective.llm.base_url.clone(),
        api_key: effective.llm.api_key.clone(),
        connect_timeout_secs: effective.llm.connect_timeout_secs,
        read_timeout_secs: effective.llm.read_timeout_secs,
        max_retries: effective.llm.max_retries,
        retry_base_delay_ms: effective.llm.retry_base_delay_ms,
        temperature: effective.llm.temperature,
        reasoning: effective.llm.reasoning,
        reasoning_split: effective.llm.reasoning_split,
        supports_prompt_cache_key: effective.llm.supports_prompt_cache_key,
        prompt_cache_retention: effective.llm.prompt_cache_retention,
        extra_headers: Default::default(),
    };
    create_provider(
        &effective.llm.provider_kind,
        llm_config,
        effective.llm.api_mode,
        effective.llm.model_id.clone(),
        Some(effective.llm.max_tokens),
        Some(effective.llm.context_limit),
    )
}

impl ConfigManager {
    pub(crate) fn from_loaded_config(
        config_store: Arc<dyn ConfigStore>,
        raw_config: Config,
        effective: EffectiveConfig,
    ) -> Self {
        let llm_provider = build_provider_from_effective(&effective);
        Self::new(config_store, raw_config, effective, llm_provider)
    }

    pub fn new(
        config_store: Arc<dyn ConfigStore>,
        raw_config: Config,
        effective: EffectiveConfig,
        llm_provider: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            config_store,
            raw_config: RwLock::new(raw_config),
            effective: RwLock::new(effective),
            llm_provider: RwLock::new(llm_provider),
            capabilities: RwLock::new(None),
        }
    }

    /// 注入 session 共享的 `Capabilities`，建立配置→运行时的同步桥。
    ///
    /// `bootstrap` 在构造完 `Capabilities` 后调用此方法。后续 ConfigManager 写入
    /// 都会顺带推到 Capabilities，保证正在运行的 session 不读到陈旧的 LLM provider 与配置。
    pub fn attach_capabilities(&self, capabilities: Arc<SessionRuntimeServices>) {
        *self.capabilities.write() = Some(capabilities);
    }

    fn sync_to_capabilities(&self) {
        let caps = self.capabilities.read();
        let Some(caps) = caps.as_ref() else {
            // 正常的早期窗口：bootstrap 构造 ConfigManager 之后、attach_capabilities
            // 之前发生的写入会走到这里。debug 构建里把这条路径做断言以便 catch
            // 「先开始热更新配置才挂 capabilities」的反模式；release 下静默吞掉。
            debug_assert!(
                false,
                "sync_to_capabilities called before attach_capabilities; a config write happened \
                 before the runtime was wired up",
            );
            return;
        };
        caps.swap_llm(self.llm_provider.read().clone());
        caps.update_effective(self.effective.read().clone());
    }

    pub fn read_effective(&self) -> parking_lot::RwLockReadGuard<'_, EffectiveConfig> {
        self.effective.read()
    }

    pub fn read_raw_config(&self) -> parking_lot::RwLockReadGuard<'_, Config> {
        self.raw_config.read()
    }

    pub fn read_llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.llm_provider.read().clone()
    }

    pub fn config_store(&self) -> &Arc<dyn ConfigStore> {
        &self.config_store
    }

    #[cfg(test)]
    pub fn set_llm_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.llm_provider.write() = provider;
        self.sync_to_capabilities();
    }

    pub fn rebuild_provider_from_effective(&self) -> Result<(), String> {
        let new_provider = {
            let effective = self.read_effective();
            build_provider_from_effective(&effective)
        };
        {
            let mut guard = self.llm_provider.write();
            *guard = new_provider;
        }
        self.sync_to_capabilities();
        Ok(())
    }

    pub fn apply_raw_config_and_rebuild(
        &self,
        config: Config,
    ) -> Result<(), astrcode_core::config::ResolveError> {
        let new_effective = config.clone().into_effective()?;
        {
            let mut guard = self.raw_config.write();
            *guard = config;
        }
        {
            let mut guard = self.effective.write();
            *guard = new_effective;
        }
        if let Err(e) = self.rebuild_provider_from_effective() {
            tracing::warn!("provider rebuild after config update failed: {e}");
        }
        // `rebuild_provider_from_effective` 已 sync_to_capabilities，无需重复
        Ok(())
    }
}
