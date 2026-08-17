use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use astrcode_core::{
    config::{
        Config, ConfigOverlay, ConfigStore, ConfigStoreError, ModelConfig, Profile,
        ProviderAuthScheme, ProviderCapabilities, ProviderWireFormat,
    },
    tool::{ToolCapabilities, ToolExecutionContext, ToolPlanningContext, access::ResourceLease},
    types::SessionId,
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_storage::config_store::FileConfigStore;
use serde_json::json;
use tokio::sync::Notify;

use super::{ConfigManager, ConfigUpdateError, PreparedConfigError};

const CODING_EXTENSION_ID: &str = "astrcode-coding";

struct BlockingConfigStore {
    inner: FileConfigStore,
    save_entered: Notify,
    save_release: Notify,
}

impl BlockingConfigStore {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            inner: FileConfigStore::new(path),
            save_entered: Notify::new(),
            save_release: Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl ConfigStore for BlockingConfigStore {
    async fn load(&self) -> Result<Config, ConfigStoreError> {
        self.inner.load().await
    }

    async fn save(&self, config: &Config) -> Result<(), ConfigStoreError> {
        self.save_entered.notify_one();
        self.save_release.notified().await;
        self.inner.save(config).await
    }

    fn path(&self) -> std::path::PathBuf {
        self.inner.path()
    }

    async fn load_overlay(
        &self,
        working_dir: &str,
    ) -> Result<Option<ConfigOverlay>, ConfigStoreError> {
        self.inner.load_overlay(working_dir).await
    }

    async fn save_overlay(
        &self,
        working_dir: &str,
        overlay: &ConfigOverlay,
    ) -> Result<(), ConfigStoreError> {
        self.inner.save_overlay(working_dir, overlay).await
    }
}

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

fn enable_only_coding(config: &mut Config) {
    config.runtime.extension_states = Some(
        astrcode_bundled_extensions::bundled_extension_ids()
            .into_iter()
            .map(|id| (id.to_owned(), id == CODING_EXTENSION_ID))
            .collect(),
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
async fn extension_config_candidate_is_published_only_after_validation_and_save() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    let config_store: Arc<dyn ConfigStore> = Arc::new(FileConfigStore::new(config_path.clone()));
    let mut raw = test_config();
    enable_only_coding(&mut raw);
    let effective = raw.clone().into_effective().unwrap();
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let (manager, runtime_services) = ConfigManager::from_loaded_config(
        config_store,
        raw,
        effective,
        Arc::clone(&runner),
        temp_dir.path().to_path_buf(),
        Default::default(),
    )
    .unwrap();
    manager.initialize_extensions().await.unwrap();

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

    let mut invalid_reload = test_config();
    set_coding_timeout(&mut invalid_reload, 0);
    let invalid_reload: Result<(), PreparedConfigError> =
        manager.apply_loaded_config(invalid_reload).await;
    assert!(matches!(
        invalid_reload,
        Err(ConfigUpdateError::ExtensionValidation(_))
    ));
    assert!(manager.raw_config_snapshot().extensions.is_none());
    assert_eq!(shell_timeout_secs(&runner, temp_dir.path()).await, 120);

    let valid: Result<(), ConfigUpdateError<()>> = manager
        .update_and_save(|candidate| {
            set_coding_timeout(candidate, 180);
            candidate.runtime.compact_threshold_percent = Some(42.0);
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
    assert_eq!(
        runtime_services
            .read_effective()
            .context
            .compact_threshold_percent,
        42.0,
        "context settings must publish with the same runtime generation"
    );

    assert!(runner.shutdown().await.is_empty());
}

#[tokio::test]
async fn cancelled_config_request_is_owned_through_publication_and_shutdown() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    let store = Arc::new(BlockingConfigStore::new(config_path));
    let config_store: Arc<dyn ConfigStore> = store.clone();
    let mut raw = test_config();
    enable_only_coding(&mut raw);
    let effective = raw.clone().into_effective().unwrap();
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let (manager, runtime_services) = ConfigManager::from_loaded_config(
        config_store,
        raw,
        effective,
        Arc::clone(&runner),
        temp_dir.path().to_path_buf(),
        Default::default(),
    )
    .unwrap();
    let manager = Arc::new(manager);
    manager.initialize_extensions().await.unwrap();

    let save_entered = store.save_entered.notified();
    let update = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move {
            let result: Result<(), ConfigUpdateError<()>> = manager
                .update_and_save(|candidate| {
                    set_coding_timeout(candidate, 180);
                    candidate.runtime.compact_threshold_percent = Some(42.0);
                    Ok(())
                })
                .await;
            result
        }
    });
    save_entered.await;
    update.abort();
    assert!(update.await.unwrap_err().is_cancelled());

    let shutdown = tokio::spawn({
        let manager = Arc::clone(&manager);
        let runner = Arc::clone(&runner);
        async move {
            manager.drain_transactions().await;
            runner.shutdown().await
        }
    });
    assert!(
        !shutdown.is_finished(),
        "shutdown must wait for the owned config transaction"
    );
    store.save_release.notify_one();
    assert!(shutdown.await.unwrap().is_empty());

    assert_eq!(
        manager.raw_config_snapshot().extensions,
        Some(BTreeMap::from([(
            CODING_EXTENSION_ID.into(),
            json!({ "shellTimeoutSecs": 180 }),
        )]))
    );
    assert_eq!(
        runtime_services
            .read_effective()
            .context
            .compact_threshold_percent,
        42.0
    );
    let persisted = store.load().await.unwrap();
    assert_eq!(persisted.runtime.compact_threshold_percent, Some(42.0));
    assert_eq!(
        persisted.extensions,
        Some(BTreeMap::from([(
            CODING_EXTENSION_ID.into(),
            json!({ "shellTimeoutSecs": 180 }),
        )]))
    );
}

#[tokio::test]
async fn disabled_bundled_extension_configs_validate_before_commit() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    let config_store: Arc<dyn ConfigStore> = Arc::new(FileConfigStore::new(config_path.clone()));
    let raw = test_config();
    let effective = raw.clone().into_effective().unwrap();
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let (manager, _) = ConfigManager::from_loaded_config(
        config_store,
        raw,
        effective,
        Arc::clone(&runner),
        temp_dir.path().to_path_buf(),
        Default::default(),
    )
    .unwrap();
    let initial_effective = manager.read_effective();

    for (extension_id, invalid_config) in [
        ("astrcode.memory", json!({ "maxContexts": "many" })),
        ("astrcode-channels", json!({ "unexpected": true })),
    ] {
        let result: Result<(), ConfigUpdateError<()>> = manager
            .update_and_save(|candidate| {
                candidate
                    .runtime
                    .extension_states
                    .get_or_insert_with(BTreeMap::new)
                    .insert(extension_id.into(), false);
                candidate
                    .extensions
                    .get_or_insert_with(BTreeMap::new)
                    .insert(extension_id.into(), invalid_config);
                Ok(())
            })
            .await;
        let Err(ConfigUpdateError::ExtensionValidation(error)) = result else {
            panic!("disabled {extension_id} config must fail before commit");
        };
        assert!(error.to_string().contains(extension_id), "{error}");
        assert!(manager.raw_config_snapshot().extensions.is_none());
        assert!(Arc::ptr_eq(&initial_effective, &manager.read_effective()));
        assert!(!config_path.exists());
    }

    assert!(runner.shutdown().await.is_empty());
}
