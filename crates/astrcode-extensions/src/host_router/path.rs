use std::path::{Component, Path, PathBuf};

use astrcode_extension_sdk::{self, s5r::ErrorPayload, wire::WireErrorCode};

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

/// 校验绝对路径的组件：只拒绝父目录穿越；RootDir/Prefix 是绝对路径的固有组件，放行。
pub(super) fn validate_absolute_path_components(absolute: &Path) -> Result<(), ErrorPayload> {
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                "parent directory traversal is not allowed",
            ));
        }
    }
    Ok(())
}

/// 解析宿主文件工具的路径：绝对路径由作者显式命名，按文件系统原样解析（无工作区约束）；
/// 相对路径维持工作区沙箱（拒绝绝对组件与 `..` 穿越，必须落在 canonical root 内）。
pub(super) fn canonicalize_host_path(
    root: impl AsRef<Path>,
    path: &str,
) -> Result<PathBuf, ErrorPayload> {
    if path.contains('\0') {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "path contains NUL",
        ));
    }
    let absolute = Path::new(path);
    if absolute.is_absolute() {
        validate_absolute_path_components(absolute)?;
        return absolute.canonicalize().map_err(io_error);
    }
    canonicalize_workspace_path(root, path)
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
            ("../outside.txt", WireErrorCode::PermissionDenied.as_str()),
            ("/etc/passwd", WireErrorCode::InvalidInput.as_str()),
            ("", WireErrorCode::InvalidInput.as_str()),
        ] {
            let error = canonicalize_workspace_path(&root, relative).unwrap_err();
            assert_eq!(error.code, code, "relative path: {relative:?}");
        }
    }

    #[test]
    fn host_path_resolution_accepts_absolute_paths_and_rejects_traversal() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "ok").unwrap();
        std::fs::create_dir_all(temp.path().join("sub")).unwrap();

        let absolute = temp.path().join("note.txt");
        let resolved = canonicalize_host_path("unused-root", absolute.to_str().unwrap()).unwrap();
        assert_eq!(resolved, absolute.canonicalize().unwrap());

        let directory = temp.path().join("sub");
        let resolved = canonicalize_host_path("unused-root", directory.to_str().unwrap()).unwrap();
        assert_eq!(resolved, directory.canonicalize().unwrap());

        let traversal = temp.path().join("sub").join("..").join("note.txt");
        let error = canonicalize_host_path("unused-root", traversal.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, WireErrorCode::InvalidInput.as_str());

        let nul = format!("{}\0tail", absolute.to_str().unwrap());
        let error = canonicalize_host_path("unused-root", &nul).unwrap_err();
        assert_eq!(error.code, WireErrorCode::InvalidInput.as_str());
    }
}
