//! File-system config store with atomic writes.

use std::path::{Path, PathBuf};

use astrcode_core::config::{Config, ConfigOverlay, ConfigStore, ConfigStoreError};
use astrcode_support::hostpaths;

/// File-system implementation of ConfigStore.
///
/// Reads/writes `~/.astrcode/config.json` with atomic write semantics
/// (write to `.tmp`, then rename).
pub struct FileConfigStore {
    path: PathBuf,
}

impl FileConfigStore {
    /// Create a new store with the default config path.
    pub fn default_path() -> Self {
        Self {
            path: hostpaths::astrcode_dir().join("config.json"),
        }
    }

    /// Create a store with a custom path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn last_known_good_path(&self) -> PathBuf {
        self.path
            .parent()
            .map(|dir| dir.join(".last-known-good.json"))
            .unwrap_or_else(|| self.path.with_file_name(".last-known-good.json"))
    }

    pub async fn save_last_known_good(&self, config: &Config) -> Result<(), ConfigStoreError> {
        let path = self.last_known_good_path();
        let json = serde_json::to_string_pretty(config)?;
        tokio::task::spawn_blocking(move || {
            let tmp_path = path.with_extension("json.tmp");
            std::fs::write(&tmp_path, &json)?;
            std::fs::rename(&tmp_path, &path)?;
            Ok(())
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }

    pub async fn load_last_known_good(&self) -> Result<Option<Config>, ConfigStoreError> {
        let path = self.last_known_good_path();
        tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Ok(None);
            }
            let data = std::fs::read_to_string(&path)?;
            let config: Config =
                serde_json::from_str(&data).map_err(|e| friendly_deser_error(&e, &path))?;
            Ok(Some(config))
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }
}

#[async_trait::async_trait]
impl ConfigStore for FileConfigStore {
    async fn load(&self) -> Result<Config, ConfigStoreError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            if !path.exists() {
                let config = Config::default();
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(json) = serde_json::to_string_pretty(&config) {
                    let tmp = path.with_extension("json.tmp");
                    if std::fs::write(&tmp, &json).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
                return Ok(config);
            }
            let data = std::fs::read_to_string(&path)?;
            let config: Config = match serde_json::from_str(&data) {
                Ok(c) => c,
                Err(e) => {
                    // Break-glass: when config schema changes in a non-backward-compatible way
                    // (e.g. removing fields), old config.json may fail to deserialize.
                    // The user explicitly asked for "覆盖一次": back up and overwrite with defaults.
                    let msg = e.to_string();
                    if msg.contains("unknown field") {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let backup = path.with_file_name(format!(
                            "config.json.bak-{}",
                            ts
                        ));
                        let _ = std::fs::write(&backup, &data);

                        let config = Config::default();
                        if let Ok(json) = serde_json::to_string_pretty(&config) {
                            let tmp = path.with_extension("json.tmp");
                            if std::fs::write(&tmp, &json).is_ok() {
                                let _ = std::fs::rename(&tmp, &path);
                            }
                        }
                        tracing::warn!(
                            path = %path.display(),
                            backup = %backup.display(),
                            "Config file failed to deserialize due to unknown fields; backed up and rewrote defaults"
                        );
                        return Ok(config);
                    }

                    return Err(friendly_deser_error(&e, &path));
                }
            };
            // Re-serialize to backfill any new fields added since the file was written.
            if let Ok(json) = serde_json::to_string_pretty(&config) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, &json).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
            Ok(config)
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }

    async fn save(&self, config: &Config) -> Result<(), ConfigStoreError> {
        let path = self.path.clone();
        let json = serde_json::to_string_pretty(config)?;
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp_path = path.with_extension("json.tmp");
            std::fs::write(&tmp_path, &json)?;
            std::fs::rename(&tmp_path, &path)?;
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
            .join("config.json");
        tokio::task::spawn_blocking(move || {
            if !overlay_path.exists() {
                return Ok(None);
            }
            let data = std::fs::read_to_string(&overlay_path)?;
            let overlay: ConfigOverlay =
                serde_json::from_str(&data).map_err(|e| friendly_deser_error(&e, &overlay_path))?;
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
        let json = serde_json::to_string_pretty(overlay)?;
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&overlay_dir)?;
            let overlay_path = overlay_dir.join("config.json");
            let tmp_path = overlay_path.with_extension("json.tmp");
            std::fs::write(&tmp_path, &json)?;
            std::fs::rename(&tmp_path, &overlay_path)?;
            Ok(())
        })
        .await
        .map_err(|e| ConfigStoreError::Io(std::io::Error::other(e.to_string())))?
    }
}

/// 将 serde 反序列化错误转换为更友好的提示。
///
/// 针对 "unknown field" 错误，提示 camelCase 命名约定并建议可能的正确字段名。
fn friendly_deser_error(e: &serde_json::Error, path: &Path) -> ConfigStoreError {
    let msg = e.to_string();
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
