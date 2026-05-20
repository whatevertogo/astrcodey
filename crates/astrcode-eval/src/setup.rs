//! 工作目录准备策略。

use std::path::{Path, PathBuf};

use crate::{EvalError, case::Setup};

/// 根据 setup 配置创建临时工作目录，返回路径。
pub async fn setup_workspace(setup: &Setup, cases_base_dir: &Path) -> Result<PathBuf, EvalError> {
    match setup {
        Setup::Empty => {
            let dir = tempfile::tempdir()
                .map_err(|e| EvalError::Setup(format!("create tempdir: {e}")))?;
            let path = dir.path().to_path_buf();
            // 阻止 TempDir 析构时删除目录（由 runner 根据 keep_workdir 决定清理）
            std::mem::forget(dir);
            Ok(path)
        },
        Setup::Template { path } => {
            let src = cases_base_dir.join(path);
            if !src.exists() {
                return Err(EvalError::Setup(format!(
                    "template not found: {}",
                    src.display()
                )));
            }
            let dir = tempfile::tempdir()
                .map_err(|e| EvalError::Setup(format!("create tempdir: {e}")))?;
            let dest = dir.path().to_path_buf();
            std::mem::forget(dir);
            copy_dir_recursive(&src, &dest)?;
            Ok(dest)
        },
        Setup::Git { repo, commit } => {
            let dir = tempfile::tempdir()
                .map_err(|e| EvalError::Setup(format!("create tempdir: {e}")))?;
            let dest = dir.path().to_path_buf();
            std::mem::forget(dir);
            let status = tokio::process::Command::new("git")
                .args(["clone", "--no-checkout", repo, &dest.display().to_string()])
                .status()
                .await
                .map_err(|e| EvalError::Setup(format!("git clone: {e}")))?;
            if !status.success() {
                return Err(EvalError::Setup(format!("git clone failed: {repo}")));
            }
            let status = tokio::process::Command::new("git")
                .args(["checkout", commit])
                .current_dir(&dest)
                .status()
                .await
                .map_err(|e| EvalError::Setup(format!("git checkout: {e}")))?;
            if !status.success() {
                return Err(EvalError::Setup(format!("git checkout failed: {commit}")));
            }
            Ok(dest)
        },
    }
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), EvalError> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.map_err(|e| EvalError::Setup(e.to_string()))?;
        let relative = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| EvalError::Setup(e.to_string()))?;
        let target = dest.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| EvalError::Setup(format!("mkdir {}: {e}", target.display())))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::copy(entry.path(), &target)
                .map_err(|e| EvalError::Setup(format!("copy: {e}")))?;
        }
    }
    Ok(())
}
