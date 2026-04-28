//! 扩展加载器 — 从全局和项目目录发现并加载扩展。
//!
//! 灵感来源于 pi-mono 的 `discoverAndLoadExtensions()`。
//! 支持从 `~/.astrcode/extensions/`（全局）和 `.astrcode/extensions/`（项目级）
//! 两个位置发现并加载原生扩展。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::extension::Extension;
use astrcode_support::hostpaths;

use crate::{native_ext::NativeExtension, runtime::ExtensionRuntime};

/// 从磁盘加载所有扩展的结果。
pub struct LoadExtensionsResult {
    /// 成功加载的扩展列表
    pub extensions: Vec<Arc<dyn Extension>>,
    /// 加载过程中的错误信息列表
    pub errors: Vec<String>,
    /// 共享的扩展运行时
    pub runtime: Arc<ExtensionRuntime>,
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
    ///
    /// # 返回
    /// 包含已加载扩展、错误信息和运行时的结果
    pub async fn load_all(working_dir: Option<&str>) -> LoadExtensionsResult {
        let runtime = Arc::new(ExtensionRuntime::new());
        let mut extensions: Vec<Arc<dyn Extension>> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 全局扩展: ~/.astrcode/extensions/
        let global_dir = hostpaths::extensions_dir();
        if global_dir.exists() {
            let (exts, errs) = Self::load_from_dir(&global_dir).await;
            extensions.extend(exts);
            errors.extend(errs);
        }

        // 项目扩展（优先级更高）: .astrcode/extensions/
        if let Some(wd) = working_dir {
            let project_dir = PathBuf::from(wd).join(".astrcode").join("extensions");
            if project_dir.exists() {
                let (project_exts, project_errs) = Self::load_from_dir(&project_dir).await;
                // 项目扩展在分发顺序中排在前面
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

    /// 从指定目录加载所有扩展。
    ///
    /// 遍历目录中的每个子目录，查找包含 `extension.json` 清单文件的扩展。
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
            // 跳过非目录项
            if !path.is_dir() {
                continue;
            }

            // 必须包含 extension.json 清单文件
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

    /// 加载单个扩展：读取并验证清单，然后加载原生库。
    async fn load_extension(ext_dir: &Path) -> Result<Arc<dyn Extension>, String> {
        let manifest_path = ext_dir.join("extension.json");
        let manifest_bytes =
            std::fs::read(&manifest_path).map_err(|e| format!("read manifest: {e}"))?;
        let manifest: astrcode_core::extension::ExtensionManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|e| format!("parse manifest: {e}"))?;
        Self::validate_manifest(&manifest)?;
        // 检查原生扩展功能是否启用
        if !native_extensions_enabled() {
            return Err(
                "native extensions are disabled; set ASTRCODE_ENABLE_NATIVE_EXTENSIONS=1 to load"
                    .into(),
            );
        }

        let lib_path = ext_dir.join(&manifest.library);
        // SAFETY: 库文件必须导出符合 FFI 契约的 extension_factory 符号
        let ext = unsafe {
            NativeExtension::load(&lib_path, manifest.id.clone())
                .map_err(|e| format!("load {}: {e}", lib_path.display()))?
        };
        Ok(Arc::new(ext))
    }

    /// 验证扩展清单的必填字段。
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

/// 检查原生扩展功能是否启用。
///
/// 通过环境变量 `ASTRCODE_ENABLE_NATIVE_EXTENSIONS` 控制，
/// 默认启用（未设置时返回 true）。
fn native_extensions_enabled() -> bool {
    std::env::var("ASTRCODE_ENABLE_NATIVE_EXTENSIONS")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true) // 默认: 启用
}
