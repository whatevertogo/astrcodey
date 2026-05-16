//! 配置与 LLM 提供者的联合管理器。
//!
//! 封装 `config_store` / `raw_config` / `effective` / `llm_provider` 的联动更新：
//! 每次配置变更都会原子地更新三者并重建 provider。

use std::sync::Arc;

use astrcode_core::{
    config::{Config, ConfigStore, EffectiveConfig},
    llm::LlmProvider,
};
use parking_lot::RwLock;

use crate::bootstrap::build_provider_from_effective;

pub struct ConfigManager {
    config_store: Arc<dyn ConfigStore>,
    raw_config: RwLock<Config>,
    effective: RwLock<EffectiveConfig>,
    llm_provider: RwLock<Arc<dyn LlmProvider>>,
}

impl ConfigManager {
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
        }
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
    }

    pub fn rebuild_provider_from_effective(&self) -> Result<(), String> {
        let new_provider = {
            let effective = self.read_effective();
            build_provider_from_effective(&effective)
        };
        let mut guard = self.llm_provider.write();
        *guard = new_provider;
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
        Ok(())
    }
}
