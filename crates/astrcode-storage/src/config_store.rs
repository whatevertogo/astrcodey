//! File-system config store with atomic writes.

use std::path::{Path, PathBuf};

use astrcode_core::config::{Config, ConfigOverlay, ConfigStore, ConfigStoreError};
use astrcode_support::hostpaths;
use serde::{Serialize, de::DeserializeOwned};

/// File-system implementation of ConfigStore.
///
/// Reads/writes `~/.astrcode/config.toml` with atomic write semantics
/// (write to `.tmp`, then rename). Legacy `config.json` files are loaded as a
/// fallback when the TOML file is absent.
pub struct FileConfigStore {
    path: PathBuf,
}

impl FileConfigStore {
    /// Create a new store with the default config path.
    pub fn default_path() -> Self {
        Self {
            path: hostpaths::astrcode_dir().join("config.toml"),
        }
    }

    /// Create a store with a custom path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn last_known_good_path(&self) -> PathBuf {
        self.path
            .parent()
            .map(|dir| dir.join(".last-known-good.toml"))
            .unwrap_or_else(|| self.path.with_file_name(".last-known-good.toml"))
    }

    pub async fn save_last_known_good(&self, config: &Config) -> Result<(), ConfigStoreError> {
        let path = self.last_known_good_path();
        let data = serialize_config_value(config, &path)?;
        tokio::task::spawn_blocking(move || {
            write_atomic(&path, &data)?;
            Ok(())
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }

    pub async fn load_last_known_good(&self) -> Result<Option<Config>, ConfigStoreError> {
        let path = self.last_known_good_path();
        tokio::task::spawn_blocking(move || {
            let Some(loaded_path) = first_existing_path(&path) else {
                return Ok(None);
            };
            let config = read_config_value(&loaded_path)?;
            backfill_primary_config(&config, &loaded_path, &path);
            Ok(Some(config))
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }
}

#[derive(Clone, Copy)]
enum ConfigFileFormat {
    Json,
    Toml,
}

impl ConfigFileFormat {
    fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => Self::Json,
            _ => Self::Toml,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Toml => "toml",
        }
    }
}

fn first_existing_path(primary_path: &Path) -> Option<PathBuf> {
    if primary_path.exists() {
        return Some(primary_path.to_path_buf());
    }
    let fallback_path = legacy_json_path(primary_path);
    fallback_path.exists().then_some(fallback_path)
}

fn legacy_json_path(path: &Path) -> PathBuf {
    path.with_extension("json")
}

fn read_config_value<T: DeserializeOwned>(path: &Path) -> Result<T, ConfigStoreError> {
    let data = std::fs::read_to_string(path)?;
    match ConfigFileFormat::from_path(path) {
        ConfigFileFormat::Json => {
            serde_json::from_str(&data).map_err(|e| friendly_deser_error(e.to_string(), path))
        },
        ConfigFileFormat::Toml => {
            toml::from_str(&data).map_err(|e| friendly_deser_error(e.to_string(), path))
        },
    }
}

fn serialize_config_value<T: Serialize>(
    value: &T,
    path: &Path,
) -> Result<String, ConfigStoreError> {
    match ConfigFileFormat::from_path(path) {
        ConfigFileFormat::Json => Ok(serde_json::to_string_pretty(value)?),
        ConfigFileFormat::Toml => toml::to_string_pretty(value).map_err(|e| {
            ConfigStoreError::Invalid(format!(
                "配置文件 {} 序列化为 TOML 失败: {e}",
                path.display()
            ))
        }),
    }
}

fn write_atomic(path: &Path, data: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension(format!(
        "{}.tmp",
        ConfigFileFormat::from_path(path).extension()
    ));
    std::fs::write(&tmp_path, data)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn backfill_primary_config<T: Serialize>(value: &T, loaded_path: &Path, primary_path: &Path) {
    if is_same_config_path(loaded_path, primary_path) {
        return;
    }
    match serialize_config_value(value, primary_path) {
        Ok(data) => {
            if let Err(error) = write_atomic(primary_path, &data) {
                tracing::warn!(
                    path = %primary_path.display(),
                    %error,
                    "failed to migrate legacy JSON config to TOML"
                );
            }
        },
        Err(error) => {
            tracing::warn!(
                path = %primary_path.display(),
                %error,
                "failed to serialize legacy JSON config as TOML"
            );
        },
    }
}

#[async_trait::async_trait]
impl ConfigStore for FileConfigStore {
    async fn load(&self) -> Result<Config, ConfigStoreError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let Some(loaded_path) = first_existing_path(&path) else {
                let config = Config::default();
                if let Ok(data) = serialize_config_value(&config, &path) {
                    let _ = write_atomic(&path, &data);
                }
                return Ok(config);
            };
            let config: Config = read_config_value(&loaded_path)?;
            // Re-serialize to backfill any new fields added since the file was written.
            backfill_primary_config(&config, &loaded_path, &path);
            Ok(config)
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }

    async fn save(&self, config: &Config) -> Result<(), ConfigStoreError> {
        let path = self.path.clone();
        let data = serialize_config_value(config, &path)?;
        tokio::task::spawn_blocking(move || {
            write_atomic(&path, &data)?;
            Ok(())
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }

    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    async fn load_overlay(
        &self,
        working_dir: &str,
    ) -> Result<Option<ConfigOverlay>, ConfigStoreError> {
        let overlay_path = PathBuf::from(working_dir)
            .join(".astrcode")
            .join("config.toml");
        if is_same_config_path(&self.path, &overlay_path)
            || is_same_config_path(&self.path, &legacy_json_path(&overlay_path))
        {
            return Ok(None);
        }
        tokio::task::spawn_blocking(move || {
            let Some(loaded_path) = first_existing_path(&overlay_path) else {
                return Ok(None);
            };
            let overlay: ConfigOverlay = read_config_value(&loaded_path)?;
            backfill_primary_config(&overlay, &loaded_path, &overlay_path);
            Ok(Some(overlay))
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }

    async fn save_overlay(
        &self,
        working_dir: &str,
        overlay: &ConfigOverlay,
    ) -> Result<(), ConfigStoreError> {
        let overlay_dir = PathBuf::from(working_dir).join(".astrcode");
        let overlay_path = overlay_dir.join("config.toml");
        let data = serialize_config_value(overlay, &overlay_path)?;
        tokio::task::spawn_blocking(move || {
            write_atomic(&overlay_path, &data)?;
            Ok(())
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }
}

/// 将 serde 反序列化错误转换为更友好的提示。
///
/// 针对 "unknown field" 错误，提示 camelCase 命名约定并建议可能的正确字段名。
fn friendly_deser_error(msg: String, path: &Path) -> ConfigStoreError {
    if msg.contains("unknown field") {
        let hint = msg
            .split('`')
            .nth(1)
            .and_then(|field| {
                let camel = to_camel_case(field);
                if camel != field {
                    Some(format!("，是否应为 `{camel}`？"))
                } else {
                    None
                }
            })
            .unwrap_or_default();

        ConfigStoreError::Invalid(format!(
            "配置文件 {} 解析失败: {msg}\n提示: 字段名使用 camelCase 命名约定（如 maxTokens 而非 \
             max_tokens）{hint}",
            path.display(),
        ))
    } else {
        ConfigStoreError::Invalid(format!("配置文件 {} 解析失败: {msg}", path.display(),))
    }
}

/// snake_case → camelCase 转换，用于猜测用户意图。
fn to_camel_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut upper = false;
    for c in s.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            result.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn is_same_config_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::config::ConfigStore;

    use super::*;

    fn json_config(active_profile: &str, active_model: &str) -> String {
        format!(
            r#"{{
  "version": "1",
  "activeProfile": "{active_profile}",
  "activeModel": "{active_model}",
  "profiles": [
    {{
      "name": "{active_profile}",
      "providerKind": "openai",
      "wireFormat": "openai_chat_completions",
      "authScheme": "bearer",
      "baseUrl": "https://example.com",
      "apiKey": "test-key",
      "models": [{{ "id": "{active_model}" }}]
    }}
  ]
}}"#
        )
    }

    fn toml_config(active_profile: &str, active_model: &str) -> String {
        format!(
            r#"version = "1"
activeProfile = "{active_profile}"
activeModel = "{active_model}"

[[profiles]]
name = "{active_profile}"
providerKind = "openai"
wireFormat = "openai_chat_completions"
authScheme = "bearer"
baseUrl = "https://example.com"
apiKey = "test-key"

[[profiles.models]]
id = "{active_model}"
"#
        )
    }

    fn toml_overlay(active_profile: &str, active_model: &str) -> String {
        format!(
            r#"activeProfile = "{active_profile}"
activeModel = "{active_model}"
"#
        )
    }

    #[tokio::test]
    async fn load_overlay_skips_global_config_when_working_dir_is_home() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".astrcode").join("config.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            r#"{
  "version": "1",
  "activeProfile": "zhipu-coding",
  "activeModel": "glm-5.2"
}"#,
        )
        .unwrap();
        let store = FileConfigStore::new(config_path);

        let overlay = store
            .load_overlay(temp.path().to_str().unwrap())
            .await
            .unwrap();

        assert!(overlay.is_none());
    }

    #[tokio::test]
    async fn load_creates_default_toml_config_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".astrcode").join("config.toml");
        let store = FileConfigStore::new(config_path.clone());

        let config = store.load().await.unwrap();

        assert_eq!(config.version, "1");
        assert!(config_path.exists());
        assert!(!config_path.with_extension("json").exists());
    }

    #[tokio::test]
    async fn load_reads_legacy_json_when_toml_is_missing_and_backfills_toml() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".astrcode").join("config.toml");
        let legacy_path = config_path.with_extension("json");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, json_config("legacy", "legacy-model")).unwrap();
        let store = FileConfigStore::new(config_path.clone());

        let config = store.load().await.unwrap();

        assert_eq!(config.active_profile, "legacy");
        assert_eq!(config.active_model, "legacy-model");
        assert!(config_path.exists());
    }

    #[tokio::test]
    async fn load_prefers_toml_over_legacy_json() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".astrcode").join("config.toml");
        let legacy_path = config_path.with_extension("json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, toml_config("toml", "toml-model")).unwrap();
        std::fs::write(&legacy_path, json_config("legacy", "legacy-model")).unwrap();
        let store = FileConfigStore::new(config_path);

        let config = store.load().await.unwrap();

        assert_eq!(config.active_profile, "toml");
        assert_eq!(config.active_model, "toml-model");
    }

    #[tokio::test]
    async fn load_overlay_prefers_toml_over_legacy_json() {
        let temp = tempfile::tempdir().unwrap();
        let global_path = temp
            .path()
            .join("home")
            .join(".astrcode")
            .join("config.toml");
        let workspace = temp.path().join("workspace");
        let overlay_path = workspace.join(".astrcode").join("config.toml");
        let legacy_overlay_path = overlay_path.with_extension("json");
        std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();
        std::fs::write(&overlay_path, toml_overlay("toml-overlay", "toml-model")).unwrap();
        std::fs::write(
            &legacy_overlay_path,
            r#"{
  "activeProfile": "legacy-overlay",
  "activeModel": "legacy-model"
}"#,
        )
        .unwrap();
        let store = FileConfigStore::new(global_path);

        let overlay = store
            .load_overlay(workspace.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(overlay.active_profile.as_deref(), Some("toml-overlay"));
        assert_eq!(overlay.active_model.as_deref(), Some("toml-model"));
    }

    #[tokio::test]
    async fn load_overlay_reads_legacy_json_and_backfills_toml() {
        let temp = tempfile::tempdir().unwrap();
        let global_path = temp
            .path()
            .join("home")
            .join(".astrcode")
            .join("config.toml");
        let workspace = temp.path().join("workspace");
        let overlay_path = workspace.join(".astrcode").join("config.toml");
        let legacy_overlay_path = overlay_path.with_extension("json");
        std::fs::create_dir_all(legacy_overlay_path.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_overlay_path,
            r#"{
  "activeProfile": "legacy-overlay",
  "activeModel": "legacy-model"
}"#,
        )
        .unwrap();
        let store = FileConfigStore::new(global_path);

        let overlay = store
            .load_overlay(workspace.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(overlay.active_profile.as_deref(), Some("legacy-overlay"));
        assert_eq!(overlay.active_model.as_deref(), Some("legacy-model"));
        assert!(overlay_path.exists());
        assert!(legacy_overlay_path.exists());
        let migrated = std::fs::read_to_string(overlay_path).unwrap();
        assert!(migrated.contains("activeProfile = \"legacy-overlay\""));
    }

    #[tokio::test]
    async fn save_overlay_writes_toml_by_default() {
        let temp = tempfile::tempdir().unwrap();
        let global_path = temp
            .path()
            .join("home")
            .join(".astrcode")
            .join("config.toml");
        let workspace = temp.path().join("workspace");
        let store = FileConfigStore::new(global_path);
        let overlay = ConfigOverlay {
            active_profile: Some("openai".into()),
            active_model: Some("gpt-4.1".into()),
            ..ConfigOverlay::default()
        };

        store
            .save_overlay(workspace.to_str().unwrap(), &overlay)
            .await
            .unwrap();

        let overlay_path = workspace.join(".astrcode").join("config.toml");
        assert!(overlay_path.exists());
        assert!(!overlay_path.with_extension("json").exists());
        let saved = std::fs::read_to_string(overlay_path).unwrap();
        assert!(saved.contains("activeProfile = \"openai\""));
    }

    #[tokio::test]
    async fn last_known_good_saves_toml_and_loads_legacy_json_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".astrcode").join("config.toml");
        let store = FileConfigStore::new(config_path);
        let config = Config {
            active_profile: "snapshot".into(),
            active_model: "snapshot-model".into(),
            ..Config::default()
        };

        store.save_last_known_good(&config).await.unwrap();

        let snapshot_path = store.last_known_good_path();
        assert!(snapshot_path.exists());
        assert!(!snapshot_path.with_extension("json").exists());

        std::fs::remove_file(&snapshot_path).unwrap();
        std::fs::write(
            snapshot_path.with_extension("json"),
            json_config("legacy-snapshot", "legacy-snapshot-model"),
        )
        .unwrap();

        let loaded = store.load_last_known_good().await.unwrap().unwrap();
        assert_eq!(loaded.active_profile, "legacy-snapshot");
        assert_eq!(loaded.active_model, "legacy-snapshot-model");
        assert!(snapshot_path.exists());
    }
}
