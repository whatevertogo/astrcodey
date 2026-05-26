//! 扩展加载器 — 从全局和项目目录发现并加载 s5r 子进程扩展。

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use astrcode_extension_sdk::extension::{Extension, StopReason};
use astrcode_support::hostpaths;

use crate::{host_router::HostRouter, runner::ExtensionRunner};

/// 从磁盘加载所有扩展的结果。
#[derive(Default)]
pub struct LoadExtensionsResult {
    pub extensions: Vec<Arc<dyn Extension>>,
    pub errors: Vec<String>,
}

/// 扩展加载上下文。
pub struct ExtensionLoadContext {
    pub working_dir: Option<String>,
    /// 磁盘 s5r 扩展加载时必需。
    pub host_router: Option<Arc<HostRouter>>,
}

#[async_trait::async_trait]
pub trait ExtensionSource: Send + Sync {
    async fn load(&self, ctx: &ExtensionLoadContext) -> LoadExtensionsResult;
}

pub struct ExtensionRuntime {
    pub runner: Arc<ExtensionRunner>,
    pub load_errors: Vec<String>,
}

impl ExtensionRuntime {
    pub async fn load(
        ctx: ExtensionLoadContext,
        timeout: Duration,
        sources: &[&dyn ExtensionSource],
    ) -> Self {
        let runner = Arc::new(ExtensionRunner::new(timeout));
        let load_errors = Self::sync_sources(&runner, &ctx, sources).await;
        Self {
            runner,
            load_errors,
        }
    }

    pub async fn sync_sources(
        runner: &Arc<ExtensionRunner>,
        ctx: &ExtensionLoadContext,
        sources: &[&dyn ExtensionSource],
    ) -> Vec<String> {
        let mut desired_extensions = Vec::new();
        let mut load_errors = Vec::new();

        for source in sources {
            let load_result = source.load(ctx).await;
            desired_extensions.extend(load_result.extensions);
            load_errors.extend(load_result.errors);
        }

        let desired_ids: HashSet<String> = desired_extensions
            .iter()
            .map(|ext| ext.id().to_string())
            .collect();
        let current_ids = runner.registered_extension_ids().await;

        for id in current_ids.iter().filter(|id| desired_ids.contains(*id)) {
            if let Err(e) = runner.unregister(id, StopReason::Reload).await {
                load_errors.push(format!("failed to reload extension {id}: {e}"));
            }
        }
        for ext in desired_extensions {
            let id = ext.id().to_string();
            if let Err(e) = runner
                .register_with_startup_working_dir(ext, ctx.working_dir.as_deref())
                .await
            {
                load_errors.push(format!("failed to start extension {id}: {e}"));
            }
        }
        for id in current_ids.iter().filter(|id| !desired_ids.contains(*id)) {
            if let Err(e) = runner.unregister(id, StopReason::Disabled).await {
                load_errors.push(format!("failed to stop extension {id}: {e}"));
            }
        }

        load_errors
    }
}

/// 磁盘 s5r 扩展源（`~/.astrcode/extensions/` 与项目 `.astrcode/extensions/`）。
pub struct DiskExtensionSource {
    extension_states: BTreeMap<String, bool>,
}

impl DiskExtensionSource {
    pub fn new(extension_states: BTreeMap<String, bool>) -> Self {
        Self { extension_states }
    }
}

#[async_trait::async_trait]
impl ExtensionSource for DiskExtensionSource {
    async fn load(&self, ctx: &ExtensionLoadContext) -> LoadExtensionsResult {
        let mut result =
            ExtensionLoader::load_all(ctx.working_dir.as_deref(), ctx.host_router.clone()).await;
        result.extensions.retain(|extension| {
            self.extension_states
                .get(extension.id())
                .copied()
                .unwrap_or(true)
        });
        result
    }
}

pub struct ExtensionLoader;

impl ExtensionLoader {
    pub async fn load_all(
        working_dir: Option<&str>,
        host_router: Option<Arc<HostRouter>>,
    ) -> LoadExtensionsResult {
        let mut extensions: Vec<Arc<dyn Extension>> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        let global_dir = hostpaths::extensions_dir();
        if global_dir.exists() {
            let (exts, errs) = Self::load_from_dir(&global_dir, &host_router, working_dir).await;
            extensions.extend(exts);
            errors.extend(errs);
        }

        if let Some(wd) = working_dir {
            let project_dir = PathBuf::from(wd).join(".astrcode").join("extensions");
            if project_dir.exists() {
                let (project_exts, project_errs) =
                    Self::load_from_dir(&project_dir, &host_router, working_dir).await;
                extensions.splice(0..0, project_exts);
                errors.extend(project_errs);
            }
        }

        LoadExtensionsResult { extensions, errors }
    }

    #[doc(hidden)]
    pub async fn load_from_dir_for_test(
        dir: &Path,
        host_router: &Option<Arc<HostRouter>>,
        working_dir: Option<&str>,
    ) -> (Vec<Arc<dyn Extension>>, Vec<String>) {
        Self::load_from_dir(dir, host_router, working_dir).await
    }

    async fn load_from_dir(
        dir: &Path,
        host_router: &Option<Arc<HostRouter>>,
        working_dir: Option<&str>,
    ) -> (Vec<Arc<dyn Extension>>, Vec<String>) {
        let mut extensions = Vec::new();
        let mut errors = Vec::new();

        let paths = match Self::extension_dirs(dir).await {
            Ok(paths) => paths,
            Err(e) => {
                errors.push(e);
                return (extensions, errors);
            },
        };

        for path in paths {
            match Self::load_extension(&path, host_router.clone(), working_dir).await {
                Ok(ext) => extensions.push(ext),
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }

        (extensions, errors)
    }

    async fn extension_dirs(dir: &Path) -> Result<Vec<PathBuf>, String> {
        let mut entries = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| format!("Cannot read extensions dir {}: {e}", dir.display()))?;
        let mut paths = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("read dir entry: {e}"))?
        {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .map_err(|e| format!("read file type: {e}"))?;
            if file_type.is_dir()
                && tokio::fs::metadata(path.join("extension.json"))
                    .await
                    .is_ok()
            {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    async fn load_extension(
        ext_dir: &Path,
        host_router: Option<Arc<HostRouter>>,
        working_dir: Option<&str>,
    ) -> Result<Arc<dyn Extension>, String> {
        let manifest_path = ext_dir.join("extension.json");
        let manifest_bytes = tokio::fs::read(&manifest_path)
            .await
            .map_err(|e| format!("read manifest: {e}"))?;
        let entry: serde_json::Value =
            serde_json::from_slice(&manifest_bytes).map_err(|e| format!("parse manifest: {e}"))?;

        if entry
            .get("protocol")
            .and_then(|p| p.get("native"))
            .is_some()
        {
            return Err(format!(
                "{}: protocol.native is not implemented yet; use protocol.s5r",
                ext_dir.display()
            ));
        }

        let s5r_proto = entry
            .get("protocol")
            .and_then(|p| p.get("s5r"))
            .and_then(|v| v.as_str());
        if s5r_proto != Some(crate::s5r_ext::S5R_PROTOCOL_VERSION) {
            return Err(format!(
                "{}: extension.json must set protocol.s5r to \"{}\"",
                ext_dir.display(),
                crate::s5r_ext::S5R_PROTOCOL_VERSION
            ));
        }

        if entry.get("command").is_none() {
            return Err(format!(
                "{}: extension.json missing 'command' array for s5r extension",
                ext_dir.display()
            ));
        }

        let router = host_router
            .ok_or("ExtensionLoadContext.host_router is required for disk extensions")?;

        crate::s5r_ext::S5rExtension::load(ext_dir, &entry, router, working_dir)
            .await
            .map(|ext| ext as Arc<dyn Extension>)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[tokio::test]
    async fn extension_dirs_are_sorted_and_manifest_bound() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("astrcode-ext-loader-{suffix}"));
        fs::create_dir_all(root.join("zeta")).unwrap();
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("zeta").join("extension.json"), "{}").unwrap();
        fs::write(root.join("alpha").join("extension.json"), "{}").unwrap();

        let dirs = ExtensionLoader::extension_dirs(&root).await.unwrap();
        let names = dirs
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }
}
