//! 配置与 LLM 提供者的联合管理器。
//!
//! 封装 `config_store` / `raw_config` 的唯一写入路径；`effective` 和 `llm_provider`
//! 的存储位置统一在 [`SessionRuntimeServices`] 内。更新会串行执行并在落盘前完成解析与
//! provider 构建，避免并发 snapshot-modify-save 丢更新，也避免磁盘与运行态各成功一半。

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use astrcode_ai::create_provider;
use astrcode_context::{
    context_assembler::LlmContextAssembler, post_compact_enricher::DefaultPostCompactEnricher,
};
use astrcode_core::{
    config::{Config, ConfigStore, ConfigStoreError, EffectiveConfig, LlmSettings, ResolveError},
    llm::{LlmClientConfig, LlmProvider},
};
use astrcode_extension_sdk::runtime_ports::{CompositeToolCatalogProvider, ToolCatalogProvider};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::{SessionExtensionPorts, SessionRuntimeServices};
use parking_lot::RwLock;

pub struct ConfigManager {
    config_store: Arc<dyn ConfigStore>,
    raw_config: RwLock<Config>,
    extension_runner: Arc<ExtensionRunner>,
    /// 共享给所有 session 的运行时能力。
    ///
    /// `effective` 与 `llm_provider` 的真正存储位置在这里，避免双份事实。
    runtime_services: Arc<SessionRuntimeServices>,
    shell_timeout_secs: Arc<AtomicU64>,
    update_lock: tokio::sync::Mutex<()>,
}

pub(crate) enum ConfigUpdateError<E> {
    Mutation(E),
    Resolve(ResolveError),
    Provider(astrcode_core::llm::LlmError),
    Store(ConfigStoreError),
}

struct PreparedConfig {
    raw: Config,
    effective: EffectiveConfig,
    llm: Arc<dyn LlmProvider>,
    small_llm: Arc<dyn LlmProvider>,
}

fn build_provider_from_settings(
    settings: &LlmSettings,
) -> Result<Arc<dyn LlmProvider>, astrcode_core::llm::LlmError> {
    let llm_config = LlmClientConfig::from_llm_settings(settings);
    create_provider(
        &settings.provider_kind,
        settings.wire_format,
        llm_config,
        settings.model_id.clone(),
        settings.max_tokens,
        settings.context_limit,
    )
}

pub(crate) fn assemble_session_runtime_services(
    llm: Arc<dyn LlmProvider>,
    small_llm: Arc<dyn LlmProvider>,
    effective: EffectiveConfig,
    extension_runner: Arc<ExtensionRunner>,
    context_assembler: Arc<LlmContextAssembler>,
    shell_timeout_secs: Arc<AtomicU64>,
) -> Arc<SessionRuntimeServices> {
    let extension_catalog: Arc<dyn ToolCatalogProvider> = extension_runner.clone();
    let builtin_catalog = astrcode_tools::registry::default_tool_catalog_with_shell_timeout_source(
        shell_timeout_secs,
    );
    let tool_catalog: Arc<dyn ToolCatalogProvider> =
        Arc::new(CompositeToolCatalogProvider::new(vec![
            ("extensions".into(), extension_catalog),
            ("builtins".into(), builtin_catalog),
        ]));

    Arc::new(SessionRuntimeServices::new(
        llm,
        small_llm,
        effective,
        SessionExtensionPorts::from_adapter(extension_runner),
        context_assembler,
        Arc::new(DefaultPostCompactEnricher),
        tool_catalog,
    ))
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
    ) -> Result<(Self, Arc<SessionRuntimeServices>), astrcode_core::llm::LlmError> {
        let shell_timeout_secs = Arc::new(AtomicU64::new(effective.agent.shell_timeout_secs));
        let runtime_services = assemble_session_runtime_services(
            build_provider_from_settings(&effective.llm)?,
            build_provider_from_settings(&effective.small_llm)?,
            effective,
            extension_runner.clone(),
            context_assembler,
            Arc::clone(&shell_timeout_secs),
        );
        let manager = Self {
            config_store,
            raw_config: RwLock::new(raw_config),
            extension_runner,
            runtime_services: Arc::clone(&runtime_services),
            shell_timeout_secs,
            update_lock: tokio::sync::Mutex::new(()),
        };
        Ok((manager, runtime_services))
    }

    /// 测试用构造：调用方负责传入预先组装好的 session runtime services。
    #[cfg(any(test, feature = "testing"))]
    pub fn new(
        config_store: Arc<dyn ConfigStore>,
        raw_config: Config,
        extension_runner: Arc<ExtensionRunner>,
        shell_timeout_secs: Arc<AtomicU64>,
        runtime_services: Arc<SessionRuntimeServices>,
    ) -> Self {
        Self {
            config_store,
            raw_config: RwLock::new(raw_config),
            extension_runner,
            shell_timeout_secs,
            runtime_services,
            update_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn extension_runner(&self) -> &ExtensionRunner {
        &self.extension_runner
    }

    pub fn runtime_services(&self) -> &Arc<SessionRuntimeServices> {
        &self.runtime_services
    }

    pub fn read_effective(&self) -> Arc<EffectiveConfig> {
        self.runtime_services.read_effective()
    }

    pub fn raw_config_snapshot(&self) -> Config {
        self.raw_config.read().clone()
    }

    /// 同时读取 raw/effective 配置时与更新事务串行，避免响应混合两个版本。
    pub(crate) async fn config_snapshot(&self) -> (Config, Arc<EffectiveConfig>) {
        let _update = self.update_lock.lock().await;
        (self.raw_config_snapshot(), self.read_effective())
    }

    pub fn read_llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.runtime_services.llm()
    }

    /// 读取小模型 provider。
    pub fn read_small_llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.runtime_services.small_llm()
    }

    pub fn config_store(&self) -> &Arc<dyn ConfigStore> {
        &self.config_store
    }

    pub(crate) async fn update_and_save<T, E>(
        &self,
        update: impl FnOnce(&mut Config) -> Result<T, E>,
    ) -> Result<T, ConfigUpdateError<E>> {
        let _update = self.update_lock.lock().await;
        let mut candidate = self.raw_config_snapshot();
        let result = update(&mut candidate).map_err(ConfigUpdateError::Mutation)?;
        let prepared = Self::prepare(candidate)?;
        self.config_store
            .save(&prepared.raw)
            .await
            .map_err(ConfigUpdateError::Store)?;
        self.publish(prepared);
        Ok(result)
    }

    pub(crate) async fn apply_loaded_config<E>(
        &self,
        config: Config,
    ) -> Result<(), ConfigUpdateError<E>> {
        let _update = self.update_lock.lock().await;
        let prepared = Self::prepare(config)?;
        self.publish(prepared);
        Ok(())
    }

    fn prepare<E>(config: Config) -> Result<PreparedConfig, ConfigUpdateError<E>> {
        let effective = config
            .clone()
            .into_effective()
            .map_err(ConfigUpdateError::Resolve)?;
        let llm =
            build_provider_from_settings(&effective.llm).map_err(ConfigUpdateError::Provider)?;
        let small_llm = build_provider_from_settings(&effective.small_llm)
            .map_err(ConfigUpdateError::Provider)?;
        Ok(PreparedConfig {
            raw: config,
            effective,
            llm,
            small_llm,
        })
    }

    fn publish(&self, prepared: PreparedConfig) {
        let changed = {
            let old_effective = self.read_effective();
            old_effective.extensions.extension_configs
                != prepared.effective.extensions.extension_configs
        };
        self.shell_timeout_secs.store(
            prepared.effective.agent.shell_timeout_secs,
            Ordering::Release,
        );
        self.runtime_services.update_effective(prepared.effective);
        self.runtime_services.swap_llm(prepared.llm);
        self.runtime_services.swap_small_llm(prepared.small_llm);
        *self.raw_config.write() = prepared.raw;
        if changed {
            let effective = self.read_effective();
            let configs: std::collections::BTreeMap<_, _> = effective
                .extensions
                .extension_configs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.extension_runner().update_extension_configs(configs);
        }
    }

    /// 在配置热更新后，异步通知所有受影响的扩展。
    ///
    /// 应在配置提交之后调用（通常在 HTTP handler 的 async 上下文中）。
    pub async fn notify_extensions_config_changed(&self) -> Vec<String> {
        if self.extension_runner().count().await == 0 {
            return Vec::new();
        }
        self.extension_runner().notify_config_changed().await
    }
}
