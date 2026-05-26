//! `astrcode.workspace.read` 路径穿越与符号链接防御。

use std::path::PathBuf;

use astrcode_core::extension::ExtensionCapability;
use astrcode_extensions::host_router::{HostBackends, HostRouter, InvokeContext};
use serde_json::json;

fn temp_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("secret.txt"), "inside").unwrap();
    (dir, root)
}

#[test]
fn workspace_read_rejects_parent_traversal() {
    let (_dir, root) = temp_workspace();
    let router = HostRouter::from_backends(HostBackends::default());
    let ctx = InvokeContext {
        working_dir: Some(root.to_string_lossy().into_owned()),
        declared_capabilities: vec![ExtensionCapability::WorkspaceRead],
        ..Default::default()
    };
    let err = router
        .invoke_sync(
            "astrcode.workspace.read",
            &json!({ "path": "../secret.txt" }).to_string(),
            &ctx,
        )
        .unwrap_err();
    assert_eq!(err.code, "permission_denied");
}

#[test]
fn workspace_read_rejects_symlink_escape() {
    let (_dir, root) = temp_workspace();
    let outside = _dir.path().join("outside.txt");
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

    let router = HostRouter::from_backends(HostBackends::default());
    let ctx = InvokeContext {
        working_dir: Some(root.to_string_lossy().into_owned()),
        declared_capabilities: vec![ExtensionCapability::WorkspaceRead],
        ..Default::default()
    };
    let err = router
        .invoke_sync(
            "astrcode.workspace.read",
            &json!({ "path": "link.txt" }).to_string(),
            &ctx,
        )
        .unwrap_err();
    assert_eq!(err.code, "permission_denied");
}

#[test]
fn workspace_read_allows_file_under_root() {
    let (_dir, root) = temp_workspace();
    let router = HostRouter::from_backends(HostBackends::default());
    let ctx = InvokeContext {
        working_dir: Some(root.to_string_lossy().into_owned()),
        declared_capabilities: vec![ExtensionCapability::WorkspaceRead],
        ..Default::default()
    };
    let out = router
        .invoke_sync(
            "astrcode.workspace.read",
            &json!({ "path": "secret.txt" }).to_string(),
            &ctx,
        )
        .unwrap();
    assert_eq!(out["content"], "inside");
}

#[test]
fn workspace_read_rejects_oversize_file() {
    let (_dir, root) = temp_workspace();
    let big = root.join("huge.bin");
    let data = vec![b'x'; 1024 * 1024 + 1];
    std::fs::write(&big, &data).unwrap();

    let router = HostRouter::from_backends(HostBackends::default());
    let ctx = InvokeContext {
        working_dir: Some(root.to_string_lossy().into_owned()),
        declared_capabilities: vec![ExtensionCapability::WorkspaceRead],
        ..Default::default()
    };
    let err = router
        .invoke_sync(
            "astrcode.workspace.read",
            &json!({ "path": "huge.bin" }).to_string(),
            &ctx,
        )
        .unwrap_err();
    assert_eq!(err.code, "file_too_large");
}
