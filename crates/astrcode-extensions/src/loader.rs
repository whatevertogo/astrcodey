//! 扩展加载器 — 从全局和项目目录发现并加载扩展。
//!
//! 灵感来源于 pi-mono 的 `discoverAndLoadExtensions()`。
//! 支持从 `~/.astrcode/extensions/`（全局）和 `.astrcode/extensions/`（项目级）
//! 两个位置发现并加载原生扩展。

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
    /// 成功加载的扩展列表
    pub extensions: Vec<Arc<dyn Extension>>,
    /// 加载过程中的错误信息列表
    pub errors: Vec<String>,
}

/// WASM 扩展资源限制，从配置系统传入。
pub struct WasmLimits {
    pub fuel: u64,
    pub memory_bytes: usize,
}

/// Extension source loading context.
pub struct ExtensionLoadContext {
    pub working_dir: Option<String>,
    pub wasm_limits: WasmLimits,
    /// s5r 宿主能力路由；WASM 扩展加载时必需。
    pub host_router: Option<Arc<HostRouter>>,
}

/// A source of extensions that can contribute to one [`ExtensionRunner`].
#[async_trait::async_trait]
pub trait ExtensionSource: Send + Sync {
    async fn load(&self, ctx: &ExtensionLoadContext) -> LoadExtensionsResult;
}

/// Runtime container for loaded extensions.
pub struct ExtensionRuntime {
    pub runner: Arc<ExtensionRunner>,
    pub load_errors: Vec<String>,
}

impl ExtensionRuntime {
    /// Load sources in order and register their extensions into one runner.
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

    /// 将 sources 的当前加载结果同步到现有 runner：新增的注册，消失的卸载。
    ///
    /// 适用于“source-managed” runner，例如 server 启动和配置热更新路径。
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

        // 先卸载同名扩展，再按 source 顺序注册，保证替换与优先级语义稳定。
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

/// Disk-backed extension source for global and project WASM extensions.
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
        let mut result = ExtensionLoader::load_all(
            ctx.working_dir.as_deref(),
            &ctx.wasm_limits,
            ctx.host_router.clone(),
        )
        .await;
        result.extensions.retain(|extension| {
            self.extension_states
                .get(extension.id())
                .copied()
                .unwrap_or(true)
        });
        result
    }
}

/// 从全局和项目级目录加载扩展。
///
/// 项目级扩展优先级更高（在分发顺序中排在前面）。
pub struct ExtensionLoader;

impl ExtensionLoader {
    /// 加载所有扩展：先加载全局扩展，再加载项目级扩展（优先级更高）。
    ///
    /// # 参数
    /// - `working_dir`: 可选的项目工作目录路径，用于发现项目级扩展
    /// - `limits`: WASM 扩展资源限制
    ///
    /// # 返回
    /// 包含已加载扩展和错误信息的结果
    pub async fn load_all(
        working_dir: Option<&str>,
        limits: &WasmLimits,
        host_router: Option<Arc<HostRouter>>,
    ) -> LoadExtensionsResult {
        let mut extensions: Vec<Arc<dyn Extension>> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 全局扩展: ~/.astrcode/extensions/
        let global_dir = hostpaths::extensions_dir();
        if global_dir.exists() {
            let (exts, errs) = Self::load_from_dir(&global_dir, limits, &host_router).await;
            extensions.extend(exts);
            errors.extend(errs);
        }

        // 项目扩展（优先级更高）: .astrcode/extensions/
        if let Some(wd) = working_dir {
            let project_dir = PathBuf::from(wd).join(".astrcode").join("extensions");
            if project_dir.exists() {
                let (project_exts, project_errs) =
                    Self::load_from_dir(&project_dir, limits, &host_router).await;
                // 项目扩展在分发顺序中排在前面
                extensions.splice(0..0, project_exts);
                errors.extend(project_errs);
            }
        }

        LoadExtensionsResult { extensions, errors }
    }

    /// 从指定目录加载所有扩展。
    ///
    /// 遍历目录中的每个子目录，查找包含 `extension.json` 清单文件的扩展。
    async fn load_from_dir(
        dir: &Path,
        limits: &WasmLimits,
        host_router: &Option<Arc<HostRouter>>,
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
            match Self::load_extension(&path, limits, host_router.clone()).await {
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

    /// 加载单个扩展：读取 extension.json 获取 library 路径，加载 WASM 模块。
    ///
    /// s5r 协议下 `id` 与能力由 `extension_init` 握手返回；
    /// `extension.json` 须声明 `protocol.s5r` 与 `library`。
    async fn load_extension(
        ext_dir: &Path,
        limits: &WasmLimits,
        host_router: Option<Arc<HostRouter>>,
    ) -> Result<Arc<dyn Extension>, String> {
        let manifest_path = ext_dir.join("extension.json");
        let manifest_bytes = tokio::fs::read(&manifest_path)
            .await
            .map_err(|e| format!("read manifest: {e}"))?;
        let entry: serde_json::Value =
            serde_json::from_slice(&manifest_bytes).map_err(|e| format!("parse manifest: {e}"))?;
        let protocol = entry
            .get("protocol")
            .and_then(|p| p.get("s5r"))
            .and_then(|v| v.as_str());
        if protocol != Some("1.0") {
            return Err(format!(
                "{}: extension.json must set protocol.s5r to \"1.0\" (s6r is no longer supported)",
                ext_dir.display()
            ));
        }

        let library = entry["library"].as_str().unwrap_or("").trim().to_string();
        if library.is_empty() {
            return Err(format!(
                "{}: extension.json missing 'library' field",
                ext_dir.display()
            ));
        }

        let router = host_router
            .ok_or("ExtensionLoadContext.host_router is required for WASM extensions")?;

        let lib_path = ext_dir.join(&library);
        crate::wasm_ext::WasmExtension::load(&lib_path, limits.fuel, limits.memory_bytes, router)
            .map(|ext| ext as Arc<dyn Extension>)
            .map_err(|e| format!("load wasm {}: {e}", lib_path.display()))
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
