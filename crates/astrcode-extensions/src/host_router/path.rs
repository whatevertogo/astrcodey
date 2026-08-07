use std::path::{Component, Path, PathBuf};

use astrcode_extension_sdk::{
    host::{HOST_ERROR_CODE_INVALID_INPUT, HOST_ERROR_CODE_PERMISSION_DENIED},
    s5r::ErrorPayload,
};

pub(super) fn canonicalize_workspace_path(
    root: impl AsRef<Path>,
    relative: &str,
) -> Result<PathBuf, ErrorPayload> {
    if relative.is_empty() {
        return Err(ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            "empty path",
        ));
    }
    if relative.contains('\0') {
        return Err(ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            "path contains NUL",
        ));
    }

    let relative = Path::new(relative);
    if relative.is_absolute() {
        return Err(ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            "absolute paths are not allowed",
        ));
    }
    for component in relative.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(ErrorPayload::new(
                    HOST_ERROR_CODE_INVALID_INPUT,
                    "absolute path components are not allowed",
                ));
            },
            Component::ParentDir => {
                return Err(ErrorPayload::new(
                    HOST_ERROR_CODE_PERMISSION_DENIED,
                    "path escapes workspace root",
                ));
            },
            Component::CurDir | Component::Normal(_) => {},
        }
    }

    let root = root
        .as_ref()
        .canonicalize()
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    if !path.starts_with(&root) {
        return Err(ErrorPayload::new(
            HOST_ERROR_CODE_PERMISSION_DENIED,
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
