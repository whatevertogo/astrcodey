//! Extension manifest types and validation.

use std::fmt;

/// Manifest 校验错误。
#[derive(Debug)]
pub enum ManifestError {
    /// `id` 字段为空或纯空白。
    MissingId,
    /// `name` 字段为空或纯空白。
    MissingName { id: String },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::MissingId => write!(f, "manifest id is required"),
            ManifestError::MissingName { id } => write!(f, "manifest {id} name is required"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// 验证扩展清单的必填字段（`extension/initialize` 握手 manifest）。
///
/// 插件作者打包时和宿主加载时都应调用此函数。
pub fn validate_manifest(
    manifest: &crate::extension::ExtensionManifest,
) -> Result<(), ManifestError> {
    if manifest.id.trim().is_empty() {
        return Err(ManifestError::MissingId);
    }
    if manifest.name.trim().is_empty() {
        return Err(ManifestError::MissingName {
            id: manifest.id.clone(),
        });
    }
    Ok(())
}
