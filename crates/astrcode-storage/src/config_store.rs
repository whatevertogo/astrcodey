//! File-system config store with atomic writes.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use astrcode_core::config::{
    Config, ConfigOverlay, ConfigStore, ConfigStoreError, defaults::astrcode_dir,
};
use serde::{Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;

use crate::session_repo::sync_directory;

/// File-system implementation of ConfigStore.
///
/// Reads/writes `~/.astrcode/config.toml` with atomic write semantics
/// (write and sync a temporary file, rename, then sync the directory).
pub struct FileConfigStore {
    path: PathBuf,
}

impl FileConfigStore {
    /// Create a new store with the default config path.
    pub fn default_path() -> Self {
        Self {
            path: astrcode_dir().join("config.toml"),
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
        run_blocking_io(move || Ok(write_atomic(&path, &data)?)).await
    }

    pub async fn load_last_known_good(&self) -> Result<Option<Config>, ConfigStoreError> {
        let path = self.last_known_good_path();
        run_blocking_io(move || {
            if !path.exists() {
                return Ok(None);
            }
            read_config_value(&path).map(Some)
        })
        .await
    }
}

async fn run_blocking_io<F, T>(f: F) -> Result<T, ConfigStoreError>
where
    F: FnOnce() -> Result<T, ConfigStoreError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e)))?
}

fn read_config_value<T: DeserializeOwned>(path: &Path) -> Result<T, ConfigStoreError> {
    let data = std::fs::read_to_string(path)?;
    toml::from_str(&data).map_err(|error| friendly_deser_error(error.to_string(), path))
}

fn serialize_config_value<T: Serialize>(
    value: &T,
    path: &Path,
) -> Result<String, ConfigStoreError> {
    toml::to_string_pretty(value).map_err(|error| {
        ConfigStoreError::Invalid(format!(
            "配置文件 {} 序列化为 TOML 失败: {error}",
            path.display()
        ))
    })
}

fn write_atomic(path: &Path, data: &str) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_created = !parent.exists();
    std::fs::create_dir_all(parent)?;

    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(data.as_bytes())?;
    temporary.as_file().sync_all()?;
    let file = temporary.persist(path).map_err(|error| error.error)?;
    file.sync_all()?;
    sync_directory(Some(parent))?;
    if parent_created {
        sync_directory(parent.parent())?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl ConfigStore for FileConfigStore {
    async fn load(&self) -> Result<Config, ConfigStoreError> {
        let path = self.path.clone();
        run_blocking_io(move || {
            if !path.exists() {
                let config = Config::default();
                let data = serialize_config_value(&config, &path)?;
                write_atomic(&path, &data)?;
                return Ok(config);
            }
            read_config_value(&path)
        })
        .await
    }

    async fn save(&self, config: &Config) -> Result<(), ConfigStoreError> {
        let path = self.path.clone();
        let data = serialize_config_value(config, &path)?;
        run_blocking_io(move || Ok(write_atomic(&path, &data)?)).await
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
        if is_same_config_path(&self.path, &overlay_path) {
            return Ok(None);
        }
        run_blocking_io(move || {
            if !overlay_path.exists() {
                return Ok(None);
            }
            read_config_value(&overlay_path).map(Some)
        })
        .await
    }

    async fn save_overlay(
        &self,
        working_dir: &str,
        overlay: &ConfigOverlay,
    ) -> Result<(), ConfigStoreError> {
        let overlay_dir = PathBuf::from(working_dir).join(".astrcode");
        let overlay_path = overlay_dir.join("config.toml");
        let data = serialize_config_value(overlay, &overlay_path)?;
        run_blocking_io(move || Ok(write_atomic(&overlay_path, &data)?)).await
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
        let config_path = temp.path().join(".astrcode").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, toml_config("zhipu-coding", "glm-5.2")).unwrap();
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
    async fn load_reads_toml_config() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".astrcode").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, toml_config("configured", "configured-model")).unwrap();
        let store = FileConfigStore::new(config_path);

        let config = store.load().await.unwrap();

        assert_eq!(config.active_profile, "configured");
        assert_eq!(config.active_model, "configured-model");
    }

    #[tokio::test]
    async fn load_overlay_reads_toml_config() {
        let temp = tempfile::tempdir().unwrap();
        let global_path = temp
            .path()
            .join("home")
            .join(".astrcode")
            .join("config.toml");
        let workspace = temp.path().join("workspace");
        let overlay_path = workspace.join(".astrcode").join("config.toml");
        std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();
        std::fs::write(&overlay_path, toml_overlay("toml-overlay", "toml-model")).unwrap();
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
    async fn last_known_good_round_trips_toml() {
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

        let loaded = store.load_last_known_good().await.unwrap().unwrap();
        assert_eq!(loaded.active_profile, "snapshot");
        assert_eq!(loaded.active_model, "snapshot-model");
    }
}
