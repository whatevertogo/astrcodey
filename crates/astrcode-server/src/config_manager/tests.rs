use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{
        Config, ConfigStore, ModelConfig, Profile, ProviderAuthScheme, ProviderCapabilities,
        ProviderWireFormat,
    },
    tool::{ToolCapabilities, ToolExecutionContext, ToolPlanningContext, access::ResourceLease},
    types::SessionId,
};
use astrcode_extension_sdk::{
    builder::manifest,
    extension::{Extension, ExtensionConfig, ExtensionError, ExtensionManifest},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_storage::config_store::FileConfigStore;
use serde_json::json;

use super::{ConfigManager, ConfigUpdateError};

const CODING_EXTENSION_ID: &str = "astrcode-coding";
const APPLY_FAILURE_EXTENSION_ID: &str = "config-apply-failure";

struct ApplyFailingExtension;

#[async_trait::async_trait]
impl Extension for ApplyFailingExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest(APPLY_FAILURE_EXTENSION_ID)
            .version("test")
            .description("Config apply failure probe")
            .build()
    }

    async fn on_config_changed(&self, _config: ExtensionConfig) -> Result<(), ExtensionError> {
        Err(ExtensionError::Internal(
            "injected config apply failure".into(),
        ))
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
async fn extension_config_changes_validate_before_commit_and_report_apply_failures() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    let config_store: Arc<dyn ConfigStore> = Arc::new(FileConfigStore::new(config_path.clone()));
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

    let mut invalid_reload = test_config();
    set_coding_timeout(&mut invalid_reload, 0);
    let invalid_reload: Result<(), ConfigUpdateError<()>> =
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

    runner
        .register(Arc::new(ApplyFailingExtension))
        .await
        .unwrap();
    let apply_failure: Result<(), ConfigUpdateError<()>> = manager
        .update_and_save(|candidate| {
            candidate
                .extensions
                .get_or_insert_with(BTreeMap::new)
                .insert(
                    APPLY_FAILURE_EXTENSION_ID.into(),
                    json!({ "enabled": true }),
                );
            Ok(())
        })
        .await;
    let Err(ConfigUpdateError::ExtensionApply(error)) = apply_failure else {
        panic!("on_config_changed failure must be returned to the caller");
    };
    assert!(error.contains(APPLY_FAILURE_EXTENSION_ID));
    let persisted = manager.config_store().load().await.unwrap();
    assert_eq!(
        persisted
            .extensions
            .and_then(|configs| configs.get(APPLY_FAILURE_EXTENSION_ID).cloned()),
        Some(json!({ "enabled": true })),
        "runtime apply happens after the candidate is durably committed"
    );
    assert!(runner.shutdown().await.is_empty());
}
