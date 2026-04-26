//! # FindFiles 工具
//!
//! 实现 `findFiles` 工具，用于基于 glob 模式查找文件。
//!
//! ## 设计要点
//!
//! - 使用 `ignore` crate（ripgrep 同源）进行 .gitignore 感知的文件遍历
//! - 支持 glob 模式匹配，包括 `**` 递归
//! - 路径沙箱检查：glob 模式不能逃逸工作目录
//! - 默认最多返回 500 条结果，按修改时间排序（最新优先）
//! - 返回结构化 JSON 数组，便于前端渲染

use std::{
    path::{Component, Path, PathBuf},
    time::Instant,
};

use astrcode_core::{AstrError, CancelToken, Result, SideEffect};
use astrcode_runtime_contract::tool::{
    Tool, ToolCapabilityMetadata, ToolContext, ToolDefinition, ToolExecutionResult,
    ToolPromptMetadata,
};
use astrcode_support::tool_results::maybe_persist_tool_result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::builtin_tools::fs_common::{
    check_cancel, json_output, merge_persisted_tool_output_metadata, resolve_path,
    session_dir_for_tool_results,
};

/// FindFiles 工具实现。
///
/// 基于 glob 模式在工作目录内查找匹配的文件路径。
#[derive(Default)]
pub struct FindFilesTool;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindFilesArgs {
    pattern: String,
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    max_results: Option<usize>,
    /// 是否尊重 .gitignore/.ignore 文件（默认 true）
    #[serde(default = "default_true")]
    respect_gitignore: bool,
    /// 是否包含隐藏文件（默认 true，agent 需要看到 .env.example 等）
    #[serde(default = "default_true")]
    include_hidden: bool,
}

fn default_true() -> bool {
    true
}

#[async_trait]
impl Tool for FindFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "findFiles".to_string(),
            description: "Find candidate file paths matching a glob pattern. This is a Glob-style \
                          file path search, not a content search. Respects .gitignore by default \
                          and supports ** for recursive search."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match file paths, e.g. '*.rs', '**/*.ts', '*.{json,toml}'. Does not search file contents."
                    },
                    "root": {
                        "type": "string",
                        "description": "Root directory to search from (default: current working directory)"
                    },
                    "maxResults": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of results to return (default 500)"
                    },
                    "respectGitignore": {
                        "type": "boolean",
                        "description": "Respect .gitignore/.ignore files (default true)"
                    },
                    "includeHidden": {
                        "type": "boolean",
                        "description": "Include hidden files like .env.example (default true)"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn capability_metadata(&self) -> ToolCapabilityMetadata {
        ToolCapabilityMetadata::builtin()
            .tags(["filesystem", "read", "search"])
            .permission("filesystem.read")
            .side_effect(SideEffect::None)
            .concurrency_safe(true)
            .compact_clearable(true)
            .prompt(
                ToolPromptMetadata::new(
                    "Find candidate file paths by Glob-style pattern. Does not search file \
                     contents.",
                    "Use `findFiles` when you know a file name, extension, or path glob but not \
                     the exact path. Results are sorted by modification time, newest first. Use \
                     `grep` after this when the next step is content search.",
                )
                .caveat(
                    "Pattern matches paths only and is relative to `root` when provided. Narrow \
                     with `root` or a more specific glob if results are truncated.",
                )
                .prompt_tag("search")
                .always_include(true),
            )
            .max_result_inline_size(100_000)
    }

    async fn execute(
        &self,
        tool_call_id: String,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolExecutionResult> {
        check_cancel(ctx.cancel())?;

        let args: FindFilesArgs = serde_json::from_value(args)
            .map_err(|e| AstrError::parse("invalid args for findFiles", e))?;
        let started_at = Instant::now();
        validate_glob_pattern(&args.pattern)?;
        let root = match args.root {
            Some(root) => resolve_path(ctx, &root)?,
            None => resolve_path(ctx, Path::new("."))?,
        };
        // Fixme:500 可能需要调整默认值，或者完全去掉默认值，强制用户指定 maxResults
        // 来避免误用导致性能问题
        let max_results = args.max_results.unwrap_or(500);

        // 构建 globset 匹配器
        let glob_matcher = build_glob_matcher(&args.pattern)?;

        // 使用 ignore crate 进行 .gitignore 感知的文件遍历
        let mut entries = collect_files_with_ignore(
            &root,
            &glob_matcher,
            args.respect_gitignore,
            args.include_hidden,
            ctx.cancel(),
        )?;

        // 按修改时间降序排序（最新文件优先）
        entries.sort_by_key(|b| std::cmp::Reverse(b.modified));

        let mut paths = Vec::new();
        let truncated = if entries.len() > max_results {
            for entry in entries.into_iter().take(max_results) {
                paths.push(entry.path.to_string_lossy().to_string());
            }
            true
        } else {
            for entry in entries {
                paths.push(entry.path.to_string_lossy().to_string());
            }
            false
        };

        let output = json_output(&paths)?;
        let session_dir = session_dir_for_tool_results(ctx)?;
        let final_output = maybe_persist_tool_result(
            &session_dir,
            &tool_call_id,
            &output,
            ctx.resolved_inline_limit(),
        );
        let mut metadata = serde_json::Map::new();
        metadata.insert("pattern".to_string(), json!(args.pattern));
        metadata.insert("root".to_string(), json!(root.to_string_lossy()));
        metadata.insert("count".to_string(), json!(paths.len()));
        metadata.insert("truncated".to_string(), json!(truncated));
        metadata.insert(
            "respectGitignore".to_string(),
            json!(args.respect_gitignore),
        );
        merge_persisted_tool_output_metadata(&mut metadata, final_output.persisted.as_ref());

        Ok(ToolExecutionResult {
            tool_call_id,
            tool_name: "findFiles".to_string(),
            ok: true,
            output: final_output.output,
            error: None,
            metadata: Some(serde_json::Value::Object(metadata)),
            continuation: None,
            duration_ms: started_at.elapsed().as_millis() as u64,
            truncated,
        })
    }
}

/// 文件条目，包含路径和修改时间。
struct FileEntry {
    path: PathBuf,
    modified: std::time::SystemTime,
}

/// 构建 globset 匹配器。
///
/// 支持常见 glob 模式：
/// - `*.rs` - 当前目录下的 Rust 文件
/// - `**/*.ts` - 递归所有 TypeScript 文件
/// - `*.{json,toml}` - 多种扩展名
fn build_glob_matcher(pattern: &str) -> Result<globset::GlobSet> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| AstrError::Validation(format!("invalid glob pattern '{}': {}", pattern, e)))?;

    let mut builder = globset::GlobSetBuilder::new();
    builder.add(glob);
    builder.build().map_err(|e| {
        AstrError::Validation(format!("failed to build glob matcher '{}': {}", pattern, e))
    })
}

/// 使用 ignore crate 收集匹配的文件。
///
/// ## 为什么用 ignore crate
///
/// `ignore` 是 ripgrep 的底层遍历库，原生支持：
/// - `.gitignore` / `.ignore` / `.git/info/exclude`
/// - 全局 gitignore 配置
/// - 高效的并行遍历
fn collect_files_with_ignore(
    root: &Path,
    glob_matcher: &globset::GlobSet,
    respect_gitignore: bool,
    include_hidden: bool,
    cancel: &CancelToken,
) -> Result<Vec<FileEntry>> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!include_hidden)      // 隐藏文件控制
        .git_ignore(respect_gitignore) // .gitignore
        .git_global(respect_gitignore) // 全局 gitignore
        .git_exclude(respect_gitignore) // .git/info/exclude
        .ignore(respect_gitignore); // .ignore

    for result in builder.build() {
        check_cancel(cancel)?;
        let entry = result.map_err(|e| {
            AstrError::io(
                format!("failed walking '{}'", root.display()),
                std::io::Error::other(e.to_string()),
            )
        })?;

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path();
        // 匹配 glob 模式：先匹配完整路径，再匹配文件名
        if glob_matcher.is_match(path)
            || glob_matcher.is_match(path.file_name().unwrap_or_default())
        {
            let metadata = std::fs::metadata(path).map_err(|e| {
                AstrError::io(
                    format!("failed reading metadata for '{}'", path.display()),
                    e,
                )
            })?;
            let modified = metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            files.push(FileEntry {
                path: path.to_path_buf(),
                modified,
            });
        }
    }

    Ok(files)
}

fn validate_glob_pattern(pattern: &str) -> Result<()> {
    if looks_like_windows_drive_relative_path(pattern) {
        return Err(AstrError::Validation(format!(
            "glob pattern '{}' must stay relative to the search root",
            pattern
        )));
    }

    let path = Path::new(pattern);
    if path.is_absolute() {
        return Err(AstrError::Validation(format!(
            "glob pattern '{}' must stay relative to the search root",
            pattern
        )));
    }

    for component in path.components() {
        match component {
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(AstrError::Validation(format!(
                    "glob pattern '{}' must stay relative to the search root",
                    pattern
                )));
            },
            Component::CurDir | Component::Normal(_) => {},
        }
    }

    Ok(())
}

#[cfg(windows)]
fn looks_like_windows_drive_relative_path(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(not(windows))]
fn looks_like_windows_drive_relative_path(_pattern: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{canonical_tool_path, test_tool_context_for};

    #[tokio::test]
    async fn find_files_matches_direct_glob() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        tokio::fs::write(temp.path().join("a.txt"), "a")
            .await
            .expect("seed write should work");
        let tool = FindFilesTool;

        let result = tool
            .execute(
                "tc-find-direct".to_string(),
                json!({
                    "pattern": "*.txt",
                    "root": temp.path().to_string_lossy()
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("findFiles should execute");

        assert!(result.ok);
        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0],
            canonical_tool_path(temp.path().join("a.txt"))
                .to_string_lossy()
                .to_string()
        );
    }

    #[tokio::test]
    async fn find_files_matches_recursive_glob() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let nested = temp.path().join("nested");
        tokio::fs::create_dir_all(&nested)
            .await
            .expect("mkdir should work");
        tokio::fs::write(nested.join("lib.rs"), "fn main() {}")
            .await
            .expect("seed write should work");
        let tool = FindFilesTool;

        let result = tool
            .execute(
                "tc-find-recursive".to_string(),
                json!({
                    "pattern": "**/*.rs",
                    "root": temp.path().to_string_lossy()
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("findFiles should execute");

        assert!(result.ok);
        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(paths.len(), 1);
    }

    #[tokio::test]
    async fn find_files_returns_empty_list_when_no_match_exists() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let tool = FindFilesTool;

        let result = tool
            .execute(
                "tc-find-empty".to_string(),
                json!({
                    "pattern": "*.txt",
                    "root": temp.path().to_string_lossy()
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("findFiles should execute");

        assert!(result.ok);
        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn find_files_truncates_at_max_results() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        tokio::fs::write(temp.path().join("a.txt"), "a")
            .await
            .expect("seed write should work");
        tokio::fs::write(temp.path().join("b.txt"), "b")
            .await
            .expect("seed write should work");
        let tool = FindFilesTool;

        let result = tool
            .execute(
                "tc-find-truncate".to_string(),
                json!({
                    "pattern": "*.txt",
                    "root": temp.path().to_string_lossy(),
                    "maxResults": 1
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("findFiles should execute");

        assert!(result.ok);
        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(paths.len(), 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn validate_glob_pattern_allows_unix_glob_with_colon_after_drive_like_prefix() {
        validate_glob_pattern("a:*.rs").expect("unix glob should remain valid");
    }

    #[cfg(windows)]
    #[test]
    fn validate_glob_pattern_rejects_windows_drive_relative_pattern() {
        let error = validate_glob_pattern("a:*.rs").expect_err("drive-relative path must fail");
        assert!(
            error
                .to_string()
                .contains("must stay relative to the search root")
        );
    }

    #[tokio::test]
    async fn find_files_respects_gitignore() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        // 创建 .gitignore 排除 *.log
        tokio::fs::write(temp.path().join(".gitignore"), "*.log\n")
            .await
            .expect("write gitignore");
        // 创建 .git 目录使 ignore crate 认为这是 git 仓库
        tokio::fs::create_dir(temp.path().join(".git"))
            .await
            .expect("create .git dir");
        let log_file = temp.path().join("app.log");
        let rs_file = temp.path().join("main.rs");
        tokio::fs::write(&log_file, "log content")
            .await
            .expect("write log");
        tokio::fs::write(&rs_file, "fn main() {}")
            .await
            .expect("write rs");

        let tool = FindFilesTool;
        let result = tool
            .execute(
                "tc-find-gitignore".to_string(),
                json!({
                    "pattern": "*",
                    "root": temp.path().to_string_lossy(),
                    "respectGitignore": true
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("findFiles should succeed");

        assert!(result.ok);
        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        // .log 文件应被 .gitignore 排除
        assert!(paths.iter().any(|p| p.ends_with("main.rs")));
        assert!(!paths.iter().any(|p| p.ends_with("app.log")));
    }

    #[tokio::test]
    async fn find_files_can_disable_gitignore() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        tokio::fs::write(temp.path().join(".gitignore"), "*.log\n")
            .await
            .expect("write gitignore");
        tokio::fs::create_dir(temp.path().join(".git"))
            .await
            .expect("create .git dir");
        let log_file = temp.path().join("app.log");
        tokio::fs::write(&log_file, "log content")
            .await
            .expect("write log");

        let tool = FindFilesTool;
        let result = tool
            .execute(
                "tc-find-no-gitignore".to_string(),
                json!({
                    "pattern": "*.log",
                    "root": temp.path().to_string_lossy(),
                    "respectGitignore": false
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("findFiles should succeed");

        assert!(result.ok);
        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        // 禁用 gitignore 后应能找到 .log 文件
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("app.log"));
    }

    #[tokio::test]
    async fn find_files_sorts_by_modified_time() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file1 = temp.path().join("old.txt");
        let file2 = temp.path().join("new.txt");

        tokio::fs::write(&file1, "old").await.expect("write old");
        // 确保有微小的时间差
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        tokio::fs::write(&file2, "new").await.expect("write new");

        let tool = FindFilesTool;
        let result = tool
            .execute(
                "tc-find-sort".to_string(),
                json!({
                    "pattern": "*.txt",
                    "root": temp.path().to_string_lossy()
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("findFiles should succeed");

        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        // 新文件应排在前面
        assert!(paths[0].ends_with("new.txt"));
        assert!(paths[1].ends_with("old.txt"));
    }

    #[tokio::test]
    async fn find_files_allows_root_outside_working_dir() {
        let parent = tempfile::tempdir().expect("tempdir should be created");
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace should be created");
        tokio::fs::create_dir_all(&outside)
            .await
            .expect("outside dir should be created");
        tokio::fs::write(outside.join("found.txt"), "hello")
            .await
            .expect("outside file should be written");
        let tool = FindFilesTool;

        let result = tool
            .execute(
                "tc-find-outside".to_string(),
                json!({
                    "pattern": "*.txt",
                    "root": "../outside"
                }),
                &test_tool_context_for(&workspace),
            )
            .await
            .expect("findFiles should succeed");

        assert!(result.ok);
        let paths: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0],
            canonical_tool_path(outside.join("found.txt"))
                .to_string_lossy()
                .to_string()
        );
    }

    #[test]
    fn find_files_prompt_metadata_mentions_grep_hand_off() {
        let prompt = FindFilesTool
            .capability_metadata()
            .prompt
            .expect("findFiles should expose prompt metadata");

        assert!(prompt.summary.contains("Glob-style"));
        assert!(prompt.summary.contains("Does not search file contents"));
        assert!(prompt.guide.contains("Use `grep` after this"));
    }
}
