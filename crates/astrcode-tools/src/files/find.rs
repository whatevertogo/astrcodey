use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::OnceLock,
    time::{Instant, SystemTime},
};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::resolve_path;
use serde::Deserialize;

use super::shared::{FileCollectOptions, collect_candidate_files, tool_call_id};
// ─── find ────────────────────────────────────────────────────────────────

const DEFAULT_FIND_FILES_MAX_RESULTS: usize = 100;

/// 文件查找工具，按 glob 模式搜索文件路径（不搜索内容）。
///
/// 结果按修改时间倒序排列，支持 gitignore 过滤和隐藏文件控制。
pub struct FindFilesTool {
    /// 工具的工作目录
    pub working_dir: PathBuf,
}

/// find 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindFilesArgs {
    /// glob 匹配模式，如 `*.rs`、`**/*.ts`、`*.{json,toml}`
    pattern: String,
    /// 搜索的根目录（默认为工作目录）
    #[serde(default)]
    root: Option<PathBuf>,
    /// 返回结果的最大数量（默认 100）
    #[serde(default)]
    max_results: Option<usize>,
    /// 跳过的结果数量（用于分页）
    #[serde(default)]
    offset: Option<usize>,
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
        find_files_tool_definition().clone()
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
            .map_err(|e| ToolError::InvalidArguments(format!("invalid find args: {e}")))?;
        let root = match args.root {
            Some(ref raw) => resolve_path(&self.working_dir, raw),
            None => self.working_dir.clone(),
        };
        let max_results = args.max_results.unwrap_or(DEFAULT_FIND_FILES_MAX_RESULTS);
        let mut results = collect_candidate_files(
            &self.working_dir,
            &root,
            Some(&args.pattern),
            FileCollectOptions {
                recursive: true,
                include_hidden: args.include_hidden,
                respect_gitignore: args.respect_gitignore,
                skip_vcs_dirs: true,
                skip_build_output: true,
            },
        )
        .map_err(|e| ToolError::Execution(format!("find: {e}")))?;
        results.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        let total = results.len();
        let offset = args.offset.unwrap_or(0).min(total);
        let out: Vec<_> = results.into_iter().skip(offset).take(max_results).collect();
        let next_offset = offset.saturating_add(out.len());
        let truncated = next_offset < total;
        let files = out
            .iter()
            .map(|(path, modified)| {
                serde_json::json!({
                    "path": path,
                    "modifiedUnixMs": modified_unix_ms(*modified)
                })
            })
            .collect::<Vec<_>>();
        let paths = out.into_iter().map(|(path, _)| path).collect::<Vec<_>>();
        let mut meta = BTreeMap::new();
        meta.insert("count".into(), serde_json::json!(paths.len()));
        meta.insert("totalMatches".into(), serde_json::json!(total));
        meta.insert("offset".into(), serde_json::json!(offset));
        meta.insert("maxResults".into(), serde_json::json!(max_results));
        meta.insert("truncated".into(), serde_json::json!(truncated));
        meta.insert("hasMore".into(), serde_json::json!(truncated));
        if truncated {
            meta.insert("nextOffset".into(), serde_json::json!(next_offset));
        }
        meta.insert("root".into(), serde_json::json!(root.display().to_string()));
        meta.insert("pattern".into(), serde_json::json!(args.pattern));
        meta.insert("files".into(), serde_json::json!(files));
        let content = if paths.is_empty() {
            format!(
                "No files found matching pattern {:?} in {}",
                args.pattern,
                root.display()
            )
        } else {
            paths.join("\n")
        };
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content,
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }

    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        Some(ToolPromptMetadata::new("").prompt_tag(ToolPromptTag::Filesystem))
    }
}

fn find_files_tool_definition() -> &'static ToolDefinition {
    static DEFINITION: OnceLock<ToolDefinition> = OnceLock::new();
    DEFINITION.get_or_init(|| ToolDefinition {
        name: "find".into(),
        description: concat!(
            "Finds files by glob pattern, sorted by modification time (newest first).\n",
            "- Supports patterns: `**/*.js`, `src/**/*.ts`, `*.{json,toml}`\n",
            "- Honors .gitignore by default; set `respectGitignore=false` to include ignored \
             files.\n",
            "- For file contents, use `grep`.",
        )
        .into(),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Parallel,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob, e.g. '*.rs', '**/*.ts', '*.{json,toml}'."
                },
                "root": {
                    "type": "string",
                    "description": "Search root. Defaults to working directory."
                },
                "maxResults": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Default 100. Paginate with offset+nextOffset."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Paths to skip (pagination)."
                },
                "respectGitignore": {
                    "type": "boolean",
                    "description": "Honor .gitignore (default true)."
                },
                "includeHidden": {
                    "type": "boolean",
                    "description": "Include hidden files (default true)."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    })
}

fn modified_unix_ms(modified: SystemTime) -> u128 {
    modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
