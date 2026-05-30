use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::OnceLock,
    time::{Instant, SystemTime},
};

use astrcode_core::tool::*;
use serde::Deserialize;

use super::shared::{
    FileCollectOptions, collect_candidate_files, resolve_sandboxed_path, run_blocking,
    sandbox_escape_result, tool_call_id,
};

const DEFAULT_GLOB_MAX_RESULTS: usize = 100;

/// 按 glob 模式匹配文件路径（不搜索文件内容）。
///
/// 结果按修改时间倒序排列，支持 gitignore 过滤和隐藏文件控制。
pub struct GlobTool {
    /// 工具的工作目录
    pub working_dir: PathBuf,
}

/// `glob` 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GlobArgs {
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
    /// 是否在结果中包含目录（默认 true）
    #[serde(default = "default_true")]
    include_dirs: bool,
}

fn default_true() -> bool {
    true
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        glob_tool_definition().clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
        let args: GlobArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid glob args: {e}")))?;
        let call_id = tool_call_id(ctx);
        let working_dir = self.working_dir.clone();
        run_blocking(move || execute_glob_sync(working_dir, args, call_id, started_at)).await
    }

    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        Some(ToolPromptMetadata::new("").prompt_tag(ToolPromptTag::Filesystem))
    }
}

fn execute_glob_sync(
    working_dir: PathBuf,
    args: GlobArgs,
    call_id: String,
    started_at: Instant,
) -> Result<ToolResult, ToolError> {
    let root = match args.root {
        Some(ref raw) => match resolve_sandboxed_path(&working_dir, raw) {
            Ok(path) => path,
            Err(escaped) => return Ok(sandbox_escape_result(call_id, started_at, &escaped)),
        },
        None => working_dir.clone(),
    };
    let max_results = args.max_results.unwrap_or(DEFAULT_GLOB_MAX_RESULTS);
    let mut results = collect_candidate_files(
        &working_dir,
        &root,
        Some(&args.pattern),
        FileCollectOptions {
            recursive: true,
            include_hidden: args.include_hidden,
            respect_gitignore: args.respect_gitignore,
            skip_vcs_dirs: true,
            skip_build_output: true,
            include_dirs: args.include_dirs,
        },
    )
    .map_err(|e| ToolError::Execution(format!("glob: {e}")))?;
    results.sort_by_key(|(_, modified, _)| std::cmp::Reverse(*modified));
    let total = results.len();
    let offset = args.offset.unwrap_or(0).min(total);
    let out: Vec<_> = results.into_iter().skip(offset).take(max_results).collect();
    let next_offset = offset.saturating_add(out.len());
    let truncated = next_offset < total;
    let files = out
        .iter()
        .map(|(path, modified, is_dir)| {
            serde_json::json!({
                "path": path,
                "isDir": is_dir,
                "modifiedUnixMs": modified_unix_ms(*modified)
            })
        })
        .collect::<Vec<_>>();
    let paths: Vec<String> = out
        .into_iter()
        .map(
            |(path, _, is_dir)| {
                if is_dir { format!("{path}/") } else { path }
            },
        )
        .collect();
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
    meta.insert("pattern".into(), serde_json::json!(args.pattern.clone()));
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
        call_id,
        content,
        is_error: false,
        error: None,
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    })
}

fn glob_tool_definition() -> &'static ToolDefinition {
    static DEFINITION: OnceLock<ToolDefinition> = OnceLock::new();
    DEFINITION.get_or_init(|| ToolDefinition {
        name: "glob".into(),
        description: concat!(
            "Match file and directory paths by glob pattern (not file contents).\n\n",
            "When NOT to use:\n",
            "- Searching inside file contents → `grep`\n\n",
            "Tips:\n",
            "- Unknown path locations (e.g. `**/*.rs`, `src/**/*.ts`)\n",
            "- Multiple patterns may run together when helpful",
        )
        .into(),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Parallel,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern for paths, e.g. '*.rs', '**/*.ts', '*.{json,toml}'. Directories end with `/`."
                },
                "root": {
                    "type": "string",
                    "description": "Search root. Defaults to working directory. Results newest first."
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
                    "description": "Honor .gitignore (default true). Skips .git/target/node_modules unless false."
                },
                "includeHidden": {
                    "type": "boolean",
                    "description": "Include hidden files (default true)."
                },
                "includeDirs": {
                    "type": "boolean",
                    "description": "Include directories in results (default true). Set false for files only."
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
