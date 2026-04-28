use std::{collections::BTreeMap, path::PathBuf, time::Instant};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::{is_path_within, resolve_path};
use serde::Deserialize;

use super::shared::{FileCollectOptions, collect_candidate_files, error_result, tool_call_id};
// ─── findFiles ───────────────────────────────────────────────────────────

/// 文件查找工具，按 glob 模式搜索文件路径（不搜索内容）。
///
/// 结果按修改时间倒序排列，支持 gitignore 过滤和隐藏文件控制。
pub struct FindFilesTool {
    /// 工具的工作目录
    pub working_dir: PathBuf,
}

/// findFiles 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindFilesArgs {
    /// glob 匹配模式，如 `*.rs`、`**/*.ts`、`*.{json,toml}`
    pattern: String,
    /// 搜索的根目录（默认为工作目录）
    #[serde(default)]
    root: Option<PathBuf>,
    /// 返回结果的最大数量（默认 500）
    #[serde(default)]
    max_results: Option<usize>,
    /// 是否遵循 .gitignore 排除规则（默认 true）
    #[serde(default = "default_true")]
    respect_gitignore: bool,
    /// 是否包含隐藏文件和目录（默认 true）
    #[serde(default = "default_true")]
    include_hidden: bool,
}

/// serde 默认值函数：返回 true。
fn default_true() -> bool {
    true
}

#[async_trait::async_trait]
impl Tool for FindFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "findFiles".into(),
            description: "Find candidate file paths by glob pattern. This searches file paths \
                          only, not file contents. Use grep for content search and readFile to \
                          inspect a known result."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern for paths, e.g. '*.rs', '**/*.ts', '*.{json,toml}'. Does not search file contents."
                    },
                    "root": {
                        "type": "string",
                        "description": "Directory to search from. Defaults to the working directory."
                    },
                    "maxResults": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of paths to return (default 500)."
                    },
                    "respectGitignore": {
                        "type": "boolean",
                        "description": "Respect .gitignore-style exclusions (default true)."
                    },
                    "includeHidden": {
                        "type": "boolean",
                        "description": "Include hidden files and directories (default true)."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// 执行文件查找：解析 glob 模式 → 遍历匹配 → 过滤隐藏/gitignore → 按时间排序。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
        let args: FindFilesArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid findFiles args: {e}")))?;
        let root = args
            .root
            .as_deref()
            .map(|root| resolve_path(&self.working_dir, root))
            .unwrap_or_else(|| self.working_dir.clone());
        if !is_path_within(&root, &self.working_dir) {
            return Ok(error_result(
                ctx,
                started_at,
                format!("root escapes working directory: {}", root.display()),
                BTreeMap::from([
                    ("root".into(), serde_json::json!(root.display().to_string())),
                    ("pathEscapesWorkingDir".into(), serde_json::json!(true)),
                ]),
            ));
        }
        let max_results = args.max_results.unwrap_or(500);
        let mut results = collect_candidate_files(
            &self.working_dir,
            &root,
            Some(&args.pattern),
            FileCollectOptions {
                recursive: true,
                include_hidden: args.include_hidden,
                respect_gitignore: args.respect_gitignore,
                skip_git_dir: true,
            },
        )
        .map_err(|e| ToolError::Execution(format!("findFiles: {e}")))?;
        results.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        let total = results.len();
        let truncated = total > max_results;
        let out: Vec<_> = results
            .into_iter()
            .take(max_results)
            .map(|(s, _)| s)
            .collect();
        let mut meta = BTreeMap::new();
        meta.insert("count".into(), serde_json::json!(out.len()));
        meta.insert("maxResults".into(), serde_json::json!(max_results));
        meta.insert("truncated".into(), serde_json::json!(truncated));
        meta.insert("root".into(), serde_json::json!(root.display().to_string()));
        meta.insert("pattern".into(), serde_json::json!(args.pattern));
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content: out.join("\n"),
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}
