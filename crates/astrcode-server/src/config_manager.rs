//! 配置与 LLM 提供者的联合管理器。
//!
//! 封装 `config_store` / `raw_config` 的唯一写入路径；`effective` 和 `llm_provider`
//! 的存储位置统一在 [`SessionRuntimeServices`] 内。更新会串行执行并在落盘前完成解析与
//! provider 构建和扩展配置验证，避免并发 snapshot-modify-save 丢更新，也避免无效配置
//! 被持久化或发布。扩展运行态应用失败会返回给调用方，不会伪装成成功。

use std::sync::Arc;

use astrcode_ai::create_provider;
use astrcode_context::{
    context_assembler::LlmContextAssembler, post_compact_enricher::DefaultPostCompactEnricher,
};
use astrcode_core::{
    config::{Config, ConfigStore, ConfigStoreError, EffectiveConfig, LlmSettings, ResolveError},
    llm::{LlmClientConfig, LlmProvider},
};
use astrcode_extensions::runner::{ExtensionConfigValidationError, ExtensionRunner};
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
    update_lock: tokio::sync::Mutex<()>,
}

pub(crate) enum ConfigUpdateError<E> {
    Mutation(E),
    Resolve(ResolveError),
    Provider(astrcode_core::llm::LlmError),
    ExtensionValidation(ExtensionConfigValidationError),
    ExtensionApply(String),
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
) -> Arc<SessionRuntimeServices> {
    Arc::new(SessionRuntimeServices::new(
        llm,
        small_llm,
        effective,
        SessionExtensionPorts::from_adapter(extension_runner),
        context_assembler,
        Arc::new(DefaultPostCompactEnricher),
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
        let runtime_services = assemble_session_runtime_services(
            build_provider_from_settings(&effective.llm)?,
            build_provider_from_settings(&effective.small_llm)?,
            effective,
            extension_runner.clone(),
            context_assembler,
        );
        let manager = Self {
            config_store,
            raw_config: RwLock::new(raw_config),
            extension_runner,
            runtime_services: Arc::clone(&runtime_services),
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
        runtime_services: Arc<SessionRuntimeServices>,
    ) -> Self {
        Self {
            config_store,
            raw_config: RwLock::new(raw_config),
            extension_runner,
            runtime_services,
            update_lock: tokio::sync::Mutex::new(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path, time::Duration};

    use astrcode_core::{
        config::{
            Config, ConfigStore, ModelConfig, Profile, ProviderAuthScheme, ProviderCapabilities,
            ProviderWireFormat,
        },
        tool::{
            ToolCapabilities, ToolExecutionContext, ToolPlanningContext, access::ResourceLease,
        },
        types::SessionId,
    };
    use astrcode_storage::config_store::FileConfigStore;
    use serde_json::json;

    use super::*;

    const CODING_EXTENSION_ID: &str = "astrcode-coding";

    fn test_config() -> Config {
        Config {
            active_profile: "test".into(),
            active_model: "test-model".into(),
            profiles: vec![Profile {
                name: "test".into(),
                provider_kind: "openai".into(),
                wire_format: ProviderWireFormat::OpenAiChatCompletions,
                auth_scheme: ProviderAuthScheme::None,
                base_url: "http://127.0.0.1:1".into(),
                api_key: Some("test-key".into()),
                models: vec![ModelConfig {
                    id: "test-model".into(),
                    max_tokens: Some(1024),
                    context_limit: Some(8192),
                    model_options: None,
                    thinking_capability: None,
                }],
                capabilities: ProviderCapabilities::default(),
            }],
            ..Config::default()
        }
    }

    fn set_coding_timeout(config: &mut Config, timeout_secs: u64) {
        config.extensions.get_or_insert_with(BTreeMap::new).insert(
            CODING_EXTENSION_ID.into(),
            json!({ "shellTimeoutSecs": timeout_secs }),
        );
    }

    async fn shell_timeout_secs(runner: &ExtensionRunner, working_dir: &Path) -> u64 {
        let working_dir = working_dir.to_string_lossy().into_owned();
        let tool = runner
            .tool_catalog_snapshot_typed(&working_dir)
            .await
            .tools
            .into_iter()
            .find(|tool| tool.definition().name == "shell")
            .expect("coding extension must publish shell");
        let arguments = json!({ "command": "echo config" });
        let planning = ToolPlanningContext::new(
            SessionId::new("config-test"),
            &working_dir,
            Some("shell-call".into()),
        );
        let plan = tool
            .plan(&arguments, &planning)
            .await
            .expect("shell arguments must plan");
        let execution = ToolExecutionContext::new(
            SessionId::new("config-test"),
            working_dir,
            Some("shell-call".into()),
            None,
            ToolCapabilities::default(),
        )
        .with_resource_lease(ResourceLease::from_plan(&plan));
        let result = tool
            .execute(arguments, &execution)
            .await
            .expect("shell must execute");
        result.metadata["timeoutSecs"]
            .as_u64()
            .expect("shell must report its effective timeout")
    }

    #[tokio::test]
    async fn extension_config_update_validates_before_commit_and_applies_after_commit() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        let config_store: Arc<dyn ConfigStore> =
            Arc::new(FileConfigStore::new(config_path.clone()));
        let raw = test_config();
        let effective = raw.clone().into_effective().unwrap();
        let context_assembler = Arc::new(LlmContextAssembler::new(effective.context.clone()));
        let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let coding = astrcode_bundled_extensions::bundled_extensions(&BTreeMap::new())
            .into_iter()
            .find(|extension| extension.manifest().id() == CODING_EXTENSION_ID)
            .expect("bundled composition must include coding");
        runner.register(coding).await.unwrap();
        let (manager, _) = ConfigManager::from_loaded_config(
            config_store,
            raw,
            effective,
            Arc::clone(&runner),
            context_assembler,
        )
        .unwrap();

        let invalid: Result<(), ConfigUpdateError<()>> = manager
            .update_and_save(|candidate| {
                set_coding_timeout(candidate, 0);
                Ok(())
            })
            .await;
        let ConfigUpdateError::ExtensionValidation(error) = invalid.unwrap_err() else {
            panic!("invalid coding config must fail during extension validation");
        };
        assert!(error.to_string().contains("shellTimeoutSecs"));
        assert!(manager.raw_config_snapshot().extensions.is_none());
        assert!(
            manager
                .read_effective()
                .extensions
                .extension_configs
                .is_empty()
        );
        assert!(!config_path.exists());
        assert_eq!(shell_timeout_secs(&runner, temp_dir.path()).await, 120);

        let valid: Result<(), ConfigUpdateError<()>> = manager
            .update_and_save(|candidate| {
                set_coding_timeout(candidate, 180);
                Ok(())
            })
            .await;
        assert!(valid.is_ok());
        assert!(config_path.exists());
        assert_eq!(
            manager.raw_config_snapshot().extensions,
            Some(BTreeMap::from([(
                CODING_EXTENSION_ID.into(),
                json!({ "shellTimeoutSecs": 180 }),
            )]))
        );
        assert_eq!(shell_timeout_secs(&runner, temp_dir.path()).await, 180);
        assert!(runner.shutdown().await.is_empty());
    }
}

impl ConfigManager {
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
        self.validate_extension_configs(&prepared).await?;
        self.config_store
            .save(&prepared.raw)
            .await
            .map_err(ConfigUpdateError::Store)?;
        self.publish(prepared);
        self.apply_extension_configs().await?;
        Ok(result)
    }

    pub(crate) async fn apply_loaded_config<E>(
        &self,
        config: Config,
    ) -> Result<(), ConfigUpdateError<E>> {
        let _update = self.update_lock.lock().await;
        let prepared = Self::prepare(config)?;
        self.validate_extension_configs(&prepared).await?;
        self.publish(prepared);
        self.apply_extension_configs().await?;
        Ok(())
    }

    async fn validate_extension_configs<E>(
        &self,
        prepared: &PreparedConfig,
    ) -> Result<(), ConfigUpdateError<E>> {
        self.extension_runner()
            .validate_extension_configs(&prepared.effective.extensions.extension_configs)
            .await
            .map_err(ConfigUpdateError::ExtensionValidation)
    }

    async fn apply_extension_configs<E>(&self) -> Result<(), ConfigUpdateError<E>> {
        let errors = self.extension_runner().notify_config_changed().await;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigUpdateError::ExtensionApply(errors.join("; ")))
        }
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
}
