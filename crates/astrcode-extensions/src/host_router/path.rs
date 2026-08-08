use std::path::{Component, Path, PathBuf};

use astrcode_core::wire::WireErrorCode;
use astrcode_extension_sdk::{self, s5r::ErrorPayload};

use super::io_error;

/// 校验相对路径的组件：拒绝空路径、绝对路径组件与父目录穿越。
/// 读写两侧共用同一校验，避免错误码与消息漂移。
pub(super) fn validate_relative_path_components(relative: &Path) -> Result<(), ErrorPayload> {
    if relative.as_os_str().is_empty() {
        return Err(ErrorPayload::new(WireErrorCode::InvalidInput, "empty path"));
    }
    for component in relative.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(ErrorPayload::new(
                    WireErrorCode::InvalidInput,
                    "absolute path components are not allowed",
                ));
            },
            Component::ParentDir => {
                return Err(ErrorPayload::new(
                    WireErrorCode::PermissionDenied,
                    "path escapes workspace root",
                ));
            },
            Component::CurDir | Component::Normal(_) => {},
        }
    }
    Ok(())
}

pub(super) fn canonicalize_workspace_path(
    root: impl AsRef<Path>,
    relative: &str,
) -> Result<PathBuf, ErrorPayload> {
    if relative.contains('\0') {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "path contains NUL",
        ));
    }

    let relative = Path::new(relative);
    if relative.is_absolute() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "absolute paths are not allowed",
        ));
    }
    validate_relative_path_components(relative)?;

    let root = root.as_ref().canonicalize().map_err(io_error)?;
    let path = root.join(relative).canonicalize().map_err(io_error)?;
    if !path.starts_with(&root) {
        return Err(ErrorPayload::new(
            WireErrorCode::PermissionDenied,
            "path outside working directory",
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_enforce_the_canonical_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/file.txt"), "ok").unwrap();

        let resolved = canonicalize_workspace_path(&root, "nested/file.txt").unwrap();
        assert_eq!(
            resolved,
            root.canonicalize().unwrap().join("nested/file.txt")
        );

        for (relative, code) in [
            ("../outside.txt", "permission_denied"),
            ("/etc/passwd", "invalid_input"),
            ("", "invalid_input"),
        ] {
            let error = canonicalize_workspace_path(&root, relative).unwrap_err();
            assert_eq!(error.code, code, "relative path: {relative:?}");
        }
    }
}
