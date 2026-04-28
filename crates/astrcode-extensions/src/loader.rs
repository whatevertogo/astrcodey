//! Extension loader — discovers and loads extensions from global and project dirs.
//!
//! Inspired by pi-mono's `discoverAndLoadExtensions()`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::extension::Extension;
use astrcode_support::hostpaths;

use crate::{native_ext::NativeExtension, runtime::ExtensionRuntime};

/// Result of loading all extensions from disk.
pub struct LoadExtensionsResult {
    pub extensions: Vec<Arc<dyn Extension>>,
    pub errors: Vec<String>,
    pub runtime: Arc<ExtensionRuntime>,
}

/// Loads extensions from global and project-level directories.
///
/// Project-level extensions are placed first (higher priority in dispatch order).
pub struct ExtensionLoader;

impl ExtensionLoader {
    /// Load all extensions: global first, then project-level (higher priority).
    pub async fn load_all(working_dir: Option<&str>) -> LoadExtensionsResult {
        let runtime = Arc::new(ExtensionRuntime::new());
        let mut extensions: Vec<Arc<dyn Extension>> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // Global extensions: ~/.astrcode/extensions/
        let global_dir = hostpaths::extensions_dir();
        if global_dir.exists() {
            let (exts, errs) = Self::load_from_dir(&global_dir).await;
            extensions.extend(exts);
            errors.extend(errs);
        }

        // Project extensions (higher priority): .astrcode/extensions/
        if let Some(wd) = working_dir {
            let project_dir = PathBuf::from(wd).join(".astrcode").join("extensions");
            if project_dir.exists() {
                let (project_exts, project_errs) = Self::load_from_dir(&project_dir).await;
                // Project extensions come first in dispatch order
                extensions.splice(0..0, project_exts);
                errors.extend(project_errs);
            }
        }

        LoadExtensionsResult {
            extensions,
            errors,
            runtime,
        }
    }

    async fn load_from_dir(dir: &PathBuf) -> (Vec<Arc<dyn Extension>>, Vec<String>) {
        let mut extensions = Vec::new();
        let mut errors = Vec::new();

        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                errors.push(format!("Cannot read extensions dir {}: {e}", dir.display()));
                return (extensions, errors);
            },
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let manifest_path = path.join("extension.json");
            if !manifest_path.exists() {
                continue;
            }

            match Self::load_extension(&path).await {
                Ok(ext) => extensions.push(ext),
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }

        (extensions, errors)
    }

    async fn load_extension(ext_dir: &Path) -> Result<Arc<dyn Extension>, String> {
        let manifest_path = ext_dir.join("extension.json");
        let manifest_bytes =
            std::fs::read(&manifest_path).map_err(|e| format!("read manifest: {e}"))?;
        let manifest: astrcode_core::extension::ExtensionManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|e| format!("parse manifest: {e}"))?;
        Self::validate_manifest(&manifest)?;
        if !native_extensions_enabled() {
            return Err(
                "native extensions are disabled; set ASTRCODE_ENABLE_NATIVE_EXTENSIONS=1 to load"
                    .into(),
            );
        }

        let lib_path = ext_dir.join(&manifest.library);
        let ext = unsafe {
            NativeExtension::load(&lib_path, manifest.id.clone())
                .map_err(|e| format!("load {}: {e}", lib_path.display()))?
        };
        Ok(Arc::new(ext))
    }

    fn validate_manifest(
        manifest: &astrcode_core::extension::ExtensionManifest,
    ) -> Result<(), String> {
        if manifest.id.trim().is_empty() {
            return Err("manifest id is required".into());
        }
        if manifest.name.trim().is_empty() {
            return Err(format!("manifest {} name is required", manifest.id));
        }
        if manifest.library.trim().is_empty() {
            return Err(format!("manifest {} library is required", manifest.id));
        }
        Ok(())
    }
}

fn native_extensions_enabled() -> bool {
    std::env::var("ASTRCODE_ENABLE_NATIVE_EXTENSIONS")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}
