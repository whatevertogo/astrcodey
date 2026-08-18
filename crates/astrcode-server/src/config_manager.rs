//! 配置与 LLM 提供者的联合管理器。
//!
//! 封装 `config_store` / `raw_config` 的唯一写入路径；`effective` 和 `llm_provider`
//! 的存储位置统一在 [`SessionRuntimeServices`] 内。更新会串行执行并在落盘前完成解析与
//! provider 构建和扩展配置验证，避免并发 snapshot-modify-save 丢更新，也避免无效配置
//! 被持久化或发布。扩展使用独立候选实例完成启动验证，失败时保留上一已提交代。

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use astrcode_ai::create_provider;
use astrcode_core::{
    config::{Config, ConfigStore, ConfigStoreError, EffectiveConfig, LlmSettings},
    llm::{LlmClientConfig, LlmProvider},
};
use astrcode_extension_sdk::transport::TransportProfile;
use astrcode_extensions::{
    loader::{DiskExtensionSource, ExtensionLoadContext, prepare_extension_generation},
    runner::{ExtensionConfigValidationError, ExtensionRunner, PreparedExtensionGeneration},
};
use astrcode_session::{SessionExtensionPorts, SessionRuntimeServices};
use futures_util::FutureExt;
use parking_lot::RwLock;
use tokio::sync::{Notify, oneshot};
use tracing::Instrument;

pub(crate) mod effective;
pub(crate) mod provider_catalog;

pub(crate) use effective::{
    ConfigResolve, ResolveError, merge_overlay, profile_has_resolvable_api_key,
};
pub(crate) use provider_catalog::{
    ProviderSpec, builtin_provider_catalog, resolve_thinking_capability,
};

pub struct ConfigManager {
    config_store: Arc<dyn ConfigStore>,
    raw_config: Arc<RwLock<Config>>,
    extension_runner: Arc<ExtensionRunner>,
    extension_working_dir: PathBuf,
    transport_profile: TransportProfile,
    /// 共享给所有 session 的运行时能力。
    ///
    /// `effective` 与 `llm_provider` 的真正存储位置在这里，避免双份事实。
    runtime_services: Arc<SessionRuntimeServices>,
    update_lock: Arc<tokio::sync::Mutex<()>>,
    transactions: ConfigTransactionTasks,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigUpdateError<E> {
    #[error("config mutation: {0}")]
    Mutation(E),
    #[error("config resolution: {0}")]
    Resolve(ResolveError),
    #[error("provider construction: {0}")]
    Provider(astrcode_core::llm::LlmError),
    #[error(transparent)]
    ExtensionValidation(ExtensionConfigValidationError),
    #[error("extension candidate: {0}")]
    ExtensionCandidate(String),
    #[error("config store: {0}")]
    Store(ConfigStoreError),
    #[error(transparent)]
    Transaction(#[from] ConfigTransactionError),
}

/// 无 mutation 闭包的路径（apply/initialize/reload）使用的错误类型。
pub(crate) type PreparedConfigError = ConfigUpdateError<std::convert::Infallible>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigTransactionError {
    #[error(
        "config publication task stopped before reporting its result; runtime state is unknown"
    )]
    RuntimeStopped,
}

#[derive(Clone, Copy)]
enum ConfigPersistence {
    Save,
    AlreadyPersisted,
}

#[derive(Debug, thiserror::Error)]
enum ConfigPublicationError {
    #[error(transparent)]
    ExtensionValidation(ExtensionConfigValidationError),
    #[error("extension candidate: {0}")]
    ExtensionCandidate(String),
    #[error("config store: {0}")]
    Store(ConfigStoreError),
}

#[derive(Default)]
struct ConfigTransactionTasks {
    state: Arc<ConfigTransactionState>,
}

#[derive(Default)]
struct ConfigTransactionState {
    pending: AtomicUsize,
    completed: Notify,
}

struct ConfigTransactionCompletion {
    state: Arc<ConfigTransactionState>,
}

impl Drop for ConfigTransactionCompletion {
    fn drop(&mut self) {
        self.state.pending.fetch_sub(1, Ordering::AcqRel);
        self.state.completed.notify_waiters();
    }
}

impl ConfigTransactionTasks {
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.state.pending.fetch_add(1, Ordering::AcqRel);
        let completion = ConfigTransactionCompletion {
            state: Arc::clone(&self.state),
        };
        tokio::spawn(
            async move {
                let _completion = completion;
                task.await;
            }
            .instrument(tracing::info_span!("config.publication")),
        );
    }

    async fn drain(&self) {
        loop {
            let completed = self.state.completed.notified();
            if self.state.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            completed.await;
        }
    }
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
) -> Arc<SessionRuntimeServices> {
    Arc::new(SessionRuntimeServices::new(
        llm,
        small_llm,
        effective,
        SessionExtensionPorts::from_adapter(extension_runner),
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
        extension_working_dir: PathBuf,
        transport_profile: TransportProfile,
    ) -> Result<(Self, Arc<SessionRuntimeServices>), astrcode_core::llm::LlmError> {
        let runtime_services = assemble_session_runtime_services(
            build_provider_from_settings(&effective.llm)?,
            build_provider_from_settings(&effective.small_llm)?,
            effective,
            extension_runner.clone(),
        );
        let manager = Self {
            config_store,
            raw_config: Arc::new(RwLock::new(raw_config)),
            extension_runner,
            extension_working_dir,
            transport_profile,
            runtime_services: Arc::clone(&runtime_services),
            update_lock: Arc::new(tokio::sync::Mutex::new(())),
            transactions: ConfigTransactionTasks::default(),
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
        extension_working_dir: PathBuf,
    ) -> Self {
        Self {
            config_store,
            raw_config: Arc::new(RwLock::new(raw_config)),
            extension_runner,
            extension_working_dir,
            transport_profile: TransportProfile::default(),
            runtime_services,
            update_lock: Arc::new(tokio::sync::Mutex::new(())),
            transactions: ConfigTransactionTasks::default(),
        }
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
        let update_guard = Arc::clone(&self.update_lock).lock_owned().await;
        let mut candidate = self.raw_config_snapshot();
        let result = update(&mut candidate).map_err(ConfigUpdateError::Mutation)?;
        let prepared = Self::prepare(candidate)?;
        let publication =
            self.start_publication_transaction(update_guard, prepared, ConfigPersistence::Save);
        Self::await_publication(publication).await?;
        Ok(result)
    }

    fn start_publication_transaction(
        &self,
        update: tokio::sync::OwnedMutexGuard<()>,
        prepared: PreparedConfig,
        persistence: ConfigPersistence,
    ) -> oneshot::Receiver<Result<(), ConfigPublicationError>> {
        let config_store = Arc::clone(&self.config_store);
        let raw_config = Arc::clone(&self.raw_config);
        let runtime_services = Arc::clone(&self.runtime_services);
        let extension_runner = Arc::clone(&self.extension_runner);
        let extension_working_dir = self.extension_working_dir.clone();
        let transport_profile = self.transport_profile.clone();
        let (result_tx, result_rx) = oneshot::channel();
        self.transactions.spawn(async move {
            let _update = update;
            let extension_candidate = AssertUnwindSafe(Self::prepare_extension_candidate(
                &extension_runner,
                &extension_working_dir,
                &transport_profile,
                &prepared,
            ))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(ConfigPublicationError::ExtensionCandidate(
                    "extension candidate preparation panicked".into(),
                ))
            });
            let extension_candidate = match extension_candidate {
                Ok(candidate) => candidate,
                Err(error) => {
                    Self::report_publication_result(result_tx, Err(error));
                    return;
                },
            };
            let publication = AssertUnwindSafe(async move {
                if matches!(persistence, ConfigPersistence::Save)
                    && let Err(error) = config_store.save(&prepared.raw).await
                {
                    extension_candidate.abort().await;
                    return Err(ConfigPublicationError::Store(error));
                }
                extension_candidate
                    .commit_with(|extension_generation| {
                        Self::publish_to(
                            &raw_config,
                            &runtime_services,
                            prepared,
                            extension_generation,
                        );
                    })
                    .await;
                Ok(())
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                tracing::error!(
                    "config publication panicked after ownership transfer; aborting to avoid \
                     mixed generations"
                );
                std::process::abort();
            });
            Self::report_publication_result(result_tx, publication);
        });
        result_rx
    }

    fn report_publication_result(
        result_tx: oneshot::Sender<Result<(), ConfigPublicationError>>,
        result: Result<(), ConfigPublicationError>,
    ) {
        if let Err(result) = result_tx.send(result) {
            match result {
                Ok(()) => tracing::debug!("config publication completed after caller detached"),
                Err(error) => tracing::warn!(
                    %error,
                    "config publication failed after caller detached"
                ),
            }
        }
    }

    async fn await_publication<E>(
        result_rx: oneshot::Receiver<Result<(), ConfigPublicationError>>,
    ) -> Result<(), ConfigUpdateError<E>> {
        match result_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(ConfigPublicationError::ExtensionValidation(error))) => {
                Err(ConfigUpdateError::ExtensionValidation(error))
            },
            Ok(Err(ConfigPublicationError::ExtensionCandidate(error))) => {
                Err(ConfigUpdateError::ExtensionCandidate(error))
            },
            Ok(Err(ConfigPublicationError::Store(error))) => Err(ConfigUpdateError::Store(error)),
            Err(_) => Err(ConfigUpdateError::Transaction(
                ConfigTransactionError::RuntimeStopped,
            )),
        }
    }

    pub(crate) async fn apply_loaded_config(
        &self,
        config: Config,
    ) -> Result<(), PreparedConfigError> {
        let update = Arc::clone(&self.update_lock).lock_owned().await;
        let prepared = Self::prepare(config)?;
        let publication = self.start_publication_transaction(
            update,
            prepared,
            ConfigPersistence::AlreadyPersisted,
        );
        Self::await_publication(publication).await
    }

    pub(crate) async fn initialize_extensions(&self) -> Result<(), PreparedConfigError> {
        let update = Arc::clone(&self.update_lock).lock_owned().await;
        let prepared = Self::prepare(self.raw_config_snapshot())?;
        let publication = self.start_publication_transaction(
            update,
            prepared,
            ConfigPersistence::AlreadyPersisted,
        );
        Self::await_publication(publication).await
    }

    pub(crate) async fn reload_extensions(&self) -> Result<(), PreparedConfigError> {
        self.initialize_extensions().await
    }

    async fn prepare_extension_candidate(
        extension_runner: &Arc<ExtensionRunner>,
        extension_working_dir: &std::path::Path,
        transport_profile: &TransportProfile,
        prepared: &PreparedConfig,
    ) -> Result<PreparedExtensionGeneration, ConfigPublicationError> {
        astrcode_bundled_extensions::validate_bundled_extension_configs(
            &prepared.effective.extensions.extension_configs,
        )
        .map_err(ConfigPublicationError::ExtensionValidation)?;
        let bundled_source = astrcode_bundled_extensions::BundledExtensionSource::new(
            prepared.effective.extensions.extension_states.clone(),
        );
        let disk_source =
            DiskExtensionSource::new(prepared.effective.extensions.extension_states.clone());
        prepare_extension_generation(
            extension_runner,
            &ExtensionLoadContext {
                working_dir: Some(extension_working_dir.to_string_lossy().into_owned()),
                host_router: Some(extension_runner.host_router()),
                transport_profile: transport_profile.clone(),
            },
            &[&bundled_source, &disk_source],
            &prepared.effective.extensions.extension_configs,
        )
        .await
        .map_err(|errors| ConfigPublicationError::ExtensionCandidate(errors.join("; ")))
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

    fn publish_to(
        raw_config: &RwLock<Config>,
        runtime_services: &SessionRuntimeServices,
        prepared: PreparedConfig,
        extension_generation: u64,
    ) {
        runtime_services.publish_runtime_generation_for_extension(
            prepared.effective,
            prepared.llm,
            prepared.small_llm,
            extension_generation,
        );
        *raw_config.write() = prepared.raw;
    }

    pub(crate) async fn drain_transactions(&self) {
        let _update = Arc::clone(&self.update_lock).lock_owned().await;
        self.transactions.drain().await;
    }
}

#[cfg(test)]
mod tests;
