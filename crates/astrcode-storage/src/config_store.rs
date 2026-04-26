//! File-system config store with atomic writes.

use std::path::PathBuf;

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
}

#[async_trait::async_trait]
impl ConfigStore for FileConfigStore {
    async fn load(&self) -> Result<Config, ConfigStoreError> {
        if !self.path.exists() {
            // Return defaults if no config file exists
            let config = Config::default();
            // Save defaults so user has a starting point
            self.save(&config).await?;
            return Ok(config);
        }
        let data = std::fs::read_to_string(&self.path)?;
        let config: Config = serde_json::from_str(&data)?;
        Ok(config)
    }

    async fn save(&self, config: &Config) -> Result<(), ConfigStoreError> {
        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic write: write to .tmp, then rename
        let tmp_path = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(config)?;
        std::fs::write(&tmp_path, &json)?;
        std::fs::rename(&tmp_path, &self.path)?;
        Ok(())
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
        if !overlay_path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&overlay_path)?;
        let overlay: ConfigOverlay = serde_json::from_str(&data)?;
        Ok(Some(overlay))
    }

    async fn save_overlay(
        &self,
        working_dir: &str,
        overlay: &ConfigOverlay,
    ) -> Result<(), ConfigStoreError> {
        let overlay_dir = PathBuf::from(working_dir).join(".astrcode");
        std::fs::create_dir_all(&overlay_dir)?;
        let overlay_path = overlay_dir.join("config.json");
        let tmp_path = overlay_path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(overlay)?;
        std::fs::write(&tmp_path, &json)?;
        std::fs::rename(&tmp_path, &overlay_path)?;
        Ok(())
    }
}
