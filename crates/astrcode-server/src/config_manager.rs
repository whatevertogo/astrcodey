//! 配置与 LLM 提供者的联合管理器。
//!
//! 封装 `config_store` / `raw_config` 的写入路径；`effective` 和 `llm_provider`
//! 的存储位置统一在 [`SessionRuntimeServices`] 内，`ConfigManager` 只持有引用。
//! 这样消除了「ConfigManager 持一份、Capabilities 持一份再手动 sync」的双份事实。
//!
//! 写入路径（`apply_raw_config_and_rebuild` / `rebuild_provider_from_effective` /
//! `set_llm_provider`）直接更新 `Capabilities` 内的 `llm` 与 `effective_config`，
//! 正在运行的 session 在下一轮 LLM 调用前看到新值。

use std::sync::Arc;

use astrcode_ai::create_provider;
use astrcode_core::{
    config::{Config, ConfigStore, EffectiveConfig, LlmSettings},
    llm::{LlmClientConfig, LlmProvider, OpenAiProviderExtras, ProviderExtras},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::SessionRuntimeServices;
use parking_lot::RwLock;

pub struct ConfigManager {
    config_store: Arc<dyn ConfigStore>,
    raw_config: RwLock<Config>,
    /// 共享给所有 session 的运行时能力。
    ///
    /// `effective` 与 `llm_provider` 的真正存储位置在这里，避免双份事实。
    capabilities: Arc<SessionRuntimeServices>,
}

fn build_provider_from_settings(settings: &LlmSettings) -> Arc<dyn LlmProvider> {
    let llm_config = LlmClientConfig {
        base_url: settings.base_url.clone(),
        api_key: settings.api_key.clone(),
        connect_timeout_secs: settings.connect_timeout_secs,
        read_timeout_secs: settings.read_timeout_secs,
        max_retries: settings.max_retries,
        retry_base_delay_ms: settings.retry_base_delay_ms,
        reasoning: settings.reasoning,
        extras: ProviderExtras::OpenAi(OpenAiProviderExtras {
            reasoning_split: settings.reasoning_split,
            supports_prompt_cache_key: settings.supports_prompt_cache_key,
            prompt_cache_retention: settings.prompt_cache_retention,
        }),
        extra_headers: Default::default(),
    };
    create_provider(
        &settings.provider_kind,
        llm_config,
        settings.api_mode,
        settings.model_id.clone(),
        Some(settings.max_tokens),
        Some(settings.context_limit),
    )
}

impl ConfigManager {
    /// 从已解析的配置组装 `ConfigManager` 与共享的 `SessionRuntimeServices`。
    ///
    /// providers 从 `effective` 内部构建，不需要调用方传入。
    /// `extension_runner` 在调用时可以为空——后续由 bootstrap 加载扩展后填充。
    pub(crate) fn from_loaded_config(
        config_store: Arc<dyn ConfigStore>,
        raw_config: Config,
        effective: EffectiveConfig,
        extension_runner: Arc<astrcode_extensions::runner::ExtensionRunner>,
        context_assembler: Arc<astrcode_context::context_assembler::LlmContextAssembler>,
    ) -> (Self, Arc<SessionRuntimeServices>) {
        let capabilities = Arc::new(SessionRuntimeServices::new(
            build_provider_from_settings(&effective.llm),
            build_provider_from_settings(&effective.small_llm),
            extension_runner.clone(),
            context_assembler,
            effective,
        ));
        let manager = Self {
            config_store,
            raw_config: RwLock::new(raw_config),
            capabilities: Arc::clone(&capabilities),
        };
        (manager, capabilities)
    }

    /// 测试用构造：调用方负责传入预先组装好的 `Capabilities`。
    pub fn new(
        config_store: Arc<dyn ConfigStore>,
        raw_config: Config,
        capabilities: Arc<SessionRuntimeServices>,
    ) -> Self {
        Self {
            config_store,
            raw_config: RwLock::new(raw_config),
            capabilities,
        }
    }

    fn extension_runner(&self) -> &Arc<ExtensionRunner> {
        self.capabilities.extension_runner()
    }

    pub fn capabilities(&self) -> &Arc<SessionRuntimeServices> {
        &self.capabilities
    }

    pub fn read_effective(&self) -> Arc<EffectiveConfig> {
        self.capabilities.read_effective()
    }

    pub fn raw_config_snapshot(&self) -> Config {
        self.raw_config.read().clone()
    }

    pub fn read_llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.capabilities.llm()
    }

    /// 读取小模型 provider。
    pub fn read_small_llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.capabilities.small_llm()
    }

    pub fn config_store(&self) -> &Arc<dyn ConfigStore> {
        &self.config_store
    }

    #[cfg(test)]
    pub fn set_llm_provider(&self, provider: Arc<dyn LlmProvider>) {
        self.capabilities.swap_llm(provider);
    }

    pub fn rebuild_provider_from_effective(&self) {
        let (new_llm, new_small) = {
            let effective = self.read_effective();
            (
                build_provider_from_settings(&effective.llm),
                build_provider_from_settings(&effective.small_llm),
            )
        };
        self.capabilities.swap_llm(new_llm);
        self.capabilities.swap_small_llm(new_small);
    }

    pub fn apply_raw_config_and_rebuild(
        &self,
        config: Config,
    ) -> Result<(), astrcode_core::config::ResolveError> {
        let new_effective = config.clone().into_effective()?;
        let changed = {
            let old_effective = self.read_effective();
            old_effective.extensions.extension_configs != new_effective.extensions.extension_configs
        };
        {
            let mut guard = self.raw_config.write();
            *guard = config;
        }
        self.capabilities.update_effective(new_effective);
        self.rebuild_provider_from_effective();
        if changed {
            // 原子替换运行器中的配置映射（同步），后续由调用方异步通知扩展
            let effective = self.read_effective();
            let configs: std::collections::BTreeMap<_, _> = effective
                .extensions
                .extension_configs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.extension_runner().update_extension_configs(configs);
        }
        Ok(())
    }

    /// 在配置热更新后，异步通知所有受影响的扩展。
    ///
    /// 应在 `apply_raw_config_and_rebuild` 之后调用（通常在 HTTP handler 的 async 上下文中）。
    pub async fn notify_extensions_config_changed(&self) -> Vec<String> {
        if self.extension_runner().count().await == 0 {
            return Vec::new();
        }
        self.extension_runner().notify_config_changed().await
    }
}
