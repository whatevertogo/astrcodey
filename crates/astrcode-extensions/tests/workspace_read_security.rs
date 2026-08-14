//! `astrcode.workspace.read` 路径穿越与符号链接防御。

use std::path::{Path, PathBuf};

use astrcode_extension_sdk::{
    extension::ExtensionCapability, host::HOST_WORKSPACE_MAX_FILE_BYTES, s5r::ErrorPayload,
    wire::WireErrorCode,
};
use astrcode_extensions::host_router::{HostBackends, HostRouter, InvokeContext};
use serde_json::{Value, json};

fn temp_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("note.txt"), "inside").unwrap();
    (dir, root)
}

async fn read_workspace(root: &Path, path: &str) -> Result<Value, ErrorPayload> {
    HostRouter::from_backends(HostBackends::default())
        .invoke(
            "astrcode.workspace.read",
            json!({ "path": path }),
            &InvokeContext {
                working_dir: Some(root.to_string_lossy().into_owned()),
                declared_capabilities: vec![ExtensionCapability::WorkspaceRead],
                ..Default::default()
            },
        )
        .await
}

#[tokio::test]
async fn workspace_read_rejects_parent_traversal() {
    let (_dir, root) = temp_workspace();
    let err = read_workspace(&root, "../secret.txt").await.unwrap_err();
    assert_eq!(err.code_enum(), Some(WireErrorCode::PermissionDenied));
}

#[tokio::test]
async fn workspace_read_rejects_symlink_escape() {
    let (dir, root) = temp_workspace();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "leak").unwrap();
    let link = root.join("link.txt");
    let linked = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, &link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(&outside, &link)
        }
    };
    if linked.is_err() {
        // Windows 未开启开发者模式或缺少 symlink 特权时跳过。
        return;
    }

    let err = read_workspace(&root, "link.txt").await.unwrap_err();
    assert_eq!(err.code_enum(), Some(WireErrorCode::PermissionDenied));
}

#[tokio::test]
async fn workspace_read_allows_file_under_root() {
    let (_dir, root) = temp_workspace();
    let out = read_workspace(&root, "note.txt").await.unwrap();
    assert_eq!(out["content"], "inside");
}

#[tokio::test]
async fn workspace_read_rejects_oversize_file() {
    let (_dir, root) = temp_workspace();
    let big = root.join("huge.bin");
    let data = vec![b'x'; HOST_WORKSPACE_MAX_FILE_BYTES + 1];
    std::fs::write(&big, &data).unwrap();

    let err = read_workspace(&root, "huge.bin").await.unwrap_err();
    assert_eq!(err.code_enum(), Some(WireErrorCode::FileTooLarge));
}
