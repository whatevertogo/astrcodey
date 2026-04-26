use std::path::{Path, PathBuf};

use astrcode_core::CancelToken;
use astrcode_runtime_contract::tool::ToolContext;

pub fn test_tool_context_for(path: impl Into<PathBuf>) -> ToolContext {
    let cwd = path.into();
    // 工具测试里凡是会把中间结果持久化到 session 目录的实现，都应留在当前 tempdir
    // 内部，避免污染开发者真实的 `~/.astrcode/projects/...`。
    let session_storage_root = cwd.join(".astrcode-test-state");
    ToolContext::new("session-test".to_string().into(), cwd, CancelToken::new())
        .with_session_storage_root(session_storage_root)
        // 测试上下文把 readFile 的最终内联阈值抬高到 1MB。
        // grep 等工具仍使用各自固定的持久化阈值，所以大结果会按预期先落盘；
        // 但后续 readFile 读取这些持久化文件时，不应因为临时目录路径更长等环境差异
        // 再次被持久化，导致测试对输出形态出现非确定性。
        .with_resolved_inline_limit(1_000_000)
}

pub fn canonical_tool_path(path: impl AsRef<Path>) -> PathBuf {
    let canonical =
        std::fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf());

    // Tests should compare against the same path spelling that tools expose in metadata.
    // Windows may surface either 8.3 short names or long names depending on how TempDir was
    // created, so we normalize away verbatim prefixes and trust canonicalize's stable spelling.
    #[cfg(windows)]
    {
        if let Some(rendered) = canonical.to_str() {
            if let Some(stripped) = rendered.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{}", stripped));
            }
            if let Some(stripped) = rendered.strip_prefix(r"\\?\") {
                return PathBuf::from(stripped);
            }
        }
    }

    canonical
}
