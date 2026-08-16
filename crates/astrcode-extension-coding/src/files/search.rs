use std::{collections::BTreeMap, sync::Arc};

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{
        HostWorkspaceGlobRequest, HostWorkspaceGrepContextLine, HostWorkspaceGrepEntry,
        HostWorkspaceGrepMode, HostWorkspaceGrepRequest,
    },
    tool::{ResourceAccess, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan, ToolResult},
};
use serde::Deserialize;

use super::absolute_path;

const DEFAULT_GLOB_MAX_RESULTS: usize = 100;
const DEFAULT_GREP_MAX_MATCHES: usize = 250;
const MAX_SEARCH_PAGE_SIZE: usize = 1_000;

pub(super) fn handlers() -> Vec<(ToolDefinition, Arc<dyn ToolHandler>)> {
    vec![
        (glob_definition(), Arc::new(GlobHandler)),
        (grep_definition(), Arc::new(GrepHandler)),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_true")]
    respect_gitignore: bool,
    #[serde(default = "default_true")]
    include_hidden: bool,
    #[serde(default = "default_true")]
    include_dirs: bool,
}

const fn default_true() -> bool {
    true
}

struct GlobHandler;

#[async_trait::async_trait]
impl ToolHandler for GlobHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: GlobArgs = context.arguments()?;
        validate_glob(&args)?;
        let root = args.root.as_deref().unwrap_or(".");
        Ok(ToolPlan::new([ResourceAccess::search_file(
            absolute_path(context.working_dir(), root),
            true,
        )]))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let args: GlobArgs = context.arguments()?;
        validate_glob(&args)?;
        let offset = args.offset.unwrap_or(0);
        let max_results = args.max_results.unwrap_or(DEFAULT_GLOB_MAX_RESULTS);
        let output = context
            .host()
            .workspace()?
            .glob(HostWorkspaceGlobRequest {
                pattern: args.pattern.clone(),
                root: args.root.clone(),
                offset,
                max_matches: Some(max_results),
                respect_gitignore: args.respect_gitignore,
                include_hidden: args.include_hidden,
                include_directories: args.include_dirs,
            })
            .await?;
        let paths = output.paths;
        let next_offset = offset.saturating_add(paths.len());
        let has_more = output.has_more;
        let scan_truncated = output.scan_truncated;
        let total_matches = output.total_matches;
        let mut content = if paths.is_empty() {
            format!(
                "No files found matching pattern {:?} in {}",
                args.pattern, output.root
            )
        } else {
            paths.join("\n")
        };
        append_pagination_notice(&mut content, has_more, next_offset);
        append_scan_limit_notice(&mut content, scan_truncated);
        let mut metadata = BTreeMap::from([
            ("count".into(), serde_json::json!(paths.len())),
            ("offset".into(), serde_json::json!(offset)),
            ("maxResults".into(), serde_json::json!(max_results)),
            (
                "truncated".into(),
                serde_json::json!(has_more || scan_truncated),
            ),
            ("hasMore".into(), serde_json::json!(has_more)),
            ("scanTruncated".into(), serde_json::json!(scan_truncated)),
            (
                "nextOffset".into(),
                serde_json::json!(has_more.then_some(next_offset)),
            ),
            ("root".into(), serde_json::json!(output.root)),
            ("pattern".into(), serde_json::json!(args.pattern)),
            ("files".into(), serde_json::json!(paths)),
        ]);
        if let Some(total_matches) = total_matches {
            metadata.insert("totalMatches".into(), serde_json::json!(total_matches));
        }
        Ok(ToolResult::success(content).with_metadata(metadata).into())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GrepOutputMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

impl GrepOutputMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    multiline: bool,
    #[serde(default = "default_true")]
    recursive: bool,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    file_type: Option<String>,
    #[serde(default)]
    before_context: usize,
    #[serde(default)]
    after_context: usize,
    #[serde(default)]
    max_matches: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    output_mode: GrepOutputMode,
}

struct GrepHandler;

#[async_trait::async_trait]
impl ToolHandler for GrepHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: GrepArgs = context.arguments()?;
        validate_grep(&args)?;
        Ok(ToolPlan::new([ResourceAccess::search_file(
            absolute_path(context.working_dir(), args.path.as_deref().unwrap_or(".")),
            true,
        )]))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let args: GrepArgs = context.arguments()?;
        validate_grep(&args)?;
        let pattern = host_pattern(&args);
        let path_filters = path_filters(&args);
        let offset = args.offset.unwrap_or(0);
        let max_matches = args.max_matches.unwrap_or(DEFAULT_GREP_MAX_MATCHES);
        let output = context
            .host()
            .workspace()?
            .grep(HostWorkspaceGrepRequest {
                pattern,
                path: args.path.clone(),
                offset,
                max_matches: Some(max_matches),
                max_bytes: None,
                max_line_chars: None,
                recursive: args.recursive,
                multiline: args.multiline,
                path_filters,
                before_context: args.before_context,
                after_context: args.after_context,
                mode: args.output_mode.into(),
            })
            .await?;
        let rendered = render_entries(&output.entries);
        let next_offset = offset.saturating_add(output.entries.len());
        let has_more = output.has_more;
        let scan_truncated = output.scan_truncated;
        let mut content = if rendered.is_empty() {
            format!(
                "No matches found for pattern {:?} in {}",
                args.pattern,
                args.path.as_deref().unwrap_or(".")
            )
        } else {
            rendered.join("\n")
        };
        append_pagination_notice(&mut content, has_more, next_offset);
        append_scan_limit_notice(&mut content, scan_truncated);
        Ok(ToolResult::success(content)
            .with_metadata(BTreeMap::from([
                ("pattern".into(), serde_json::json!(args.pattern)),
                ("literal".into(), serde_json::json!(args.literal)),
                ("returned".into(), serde_json::json!(rendered.len())),
                (
                    "returnedEntries".into(),
                    serde_json::json!(output.entries.len()),
                ),
                ("offset".into(), serde_json::json!(offset)),
                ("maxMatches".into(), serde_json::json!(max_matches)),
                ("hasMore".into(), serde_json::json!(has_more)),
                (
                    "truncated".into(),
                    serde_json::json!(has_more || scan_truncated),
                ),
                ("scanTruncated".into(), serde_json::json!(scan_truncated)),
                (
                    "skippedFiles".into(),
                    serde_json::json!(output.skipped_files),
                ),
                ("multiline".into(), serde_json::json!(args.multiline)),
                (
                    "nextOffset".into(),
                    serde_json::json!(has_more.then_some(next_offset)),
                ),
                (
                    "outputMode".into(),
                    serde_json::json!(args.output_mode.as_str()),
                ),
            ]))
            .into())
    }
}

fn append_scan_limit_notice(content: &mut String, scan_truncated: bool) {
    if scan_truncated {
        content.push_str("\n\n[Search scan limit reached; narrow the root or pattern.]");
    }
}

fn append_pagination_notice(content: &mut String, has_more: bool, next_offset: usize) {
    if has_more {
        content.push_str(&format!(
            "\n\n[More results are available. Continue with offset={next_offset}.]"
        ));
    }
}

fn validate_glob(args: &GlobArgs) -> Result<(), ExtensionError> {
    if args.pattern.trim().is_empty() {
        return Err(invalid("pattern cannot be empty"));
    }
    validate_page_size(args.max_results, "maxResults")
}

fn validate_grep(args: &GrepArgs) -> Result<(), ExtensionError> {
    if args.pattern.is_empty() {
        return Err(invalid("pattern cannot be empty"));
    }
    validate_page_size(args.max_matches, "maxMatches")?;
    if args.before_context > astrcode_extension_sdk::host::HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES
        || args.after_context
            > astrcode_extension_sdk::host::HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES
    {
        return Err(invalid("beforeContext and afterContext must not exceed 20"));
    }
    if args
        .glob
        .as_deref()
        .is_some_and(|pattern| pattern.trim().is_empty())
        || args
            .file_type
            .as_deref()
            .is_some_and(|file_type| file_type.trim().is_empty())
    {
        return Err(invalid("glob and fileType must not be empty"));
    }
    Ok(())
}

fn validate_page_size(value: Option<usize>, name: &str) -> Result<(), ExtensionError> {
    if value.is_some_and(|value| value == 0 || value > MAX_SEARCH_PAGE_SIZE) {
        return Err(invalid(format!(
            "{name} must be between 1 and {MAX_SEARCH_PAGE_SIZE}"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ExtensionError {
    ExtensionError::InvalidInput {
        code: astrcode_extension_sdk::WireErrorCode::InvalidInput
            .as_str()
            .into(),
        message: message.into(),
        hint: Some("narrow the search and follow the tool parameter schema".into()),
    }
}

fn host_pattern(args: &GrepArgs) -> String {
    let pattern = if args.literal {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    };
    if args.case_insensitive {
        format!("(?i){pattern}")
    } else {
        pattern
    }
}

impl From<GrepOutputMode> for HostWorkspaceGrepMode {
    fn from(mode: GrepOutputMode) -> Self {
        match mode {
            GrepOutputMode::Content => Self::Content,
            GrepOutputMode::FilesWithMatches => Self::FilesWithMatches,
            GrepOutputMode::Count => Self::Count,
        }
    }
}

fn path_filters(args: &GrepArgs) -> Vec<String> {
    let mut filters = args.glob.iter().cloned().collect::<Vec<_>>();
    if let Some(file_type) = args.file_type.as_deref() {
        let extension = match file_type.trim_start_matches('.') {
            "rust" => "rs",
            "typescript" => "{ts,tsx}",
            "javascript" => "{js,jsx,mjs,cjs}",
            "markdown" => "md",
            extension => extension,
        };
        filters.push(format!("**/*.{extension}"));
    }
    filters
}

fn render_entries(entries: &[HostWorkspaceGrepEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| match entry {
            HostWorkspaceGrepEntry::Content {
                path,
                line_number,
                line,
                before_context,
                after_context,
                ..
            } => render_content_entry(path, *line_number, line, before_context, after_context),
            HostWorkspaceGrepEntry::File { path } => path.clone(),
            HostWorkspaceGrepEntry::Count { path, count } => format!("{path}:{count}"),
        })
        .collect()
}

fn render_content_entry(
    path: &str,
    line_number: usize,
    line: &str,
    before: &[HostWorkspaceGrepContextLine],
    after: &[HostWorkspaceGrepContextLine],
) -> String {
    before
        .iter()
        .map(|context| format!("{path}-{}-{}", context.line_number, context.line))
        .chain(std::iter::once(format!("{path}:{line_number}:{line}")))
        .chain(
            after
                .iter()
                .map(|context| format!("{path}-{}-{}", context.line_number, context.line)),
        )
        .collect::<Vec<_>>()
        .join("\n")
}

fn glob_definition() -> ToolDefinition {
    ToolDefinition {
        name: "glob".into(),
        description: "Match file and directory paths by glob pattern (not file contents). Use \
                      `grep` for content search. Paginate with offset and nextOffset."
            .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern such as '*.rs' or 'src/**/*.ts'." },
                "root": { "type": "string", "description": "Search root, default working directory." },
                "maxResults": { "type": "integer", "minimum": 1, "maximum": 1000 },
                "offset": { "type": "integer", "minimum": 0 },
                "respectGitignore": { "type": "boolean" },
                "includeHidden": { "type": "boolean" },
                "includeDirs": { "type": "boolean" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

fn grep_definition() -> ToolDefinition {
    ToolDefinition {
        name: "grep".into(),
        description: "Search file contents by regex or literal text. Use `glob` to find paths. \
                      Paginate with offset and nextOffset."
            .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex, or exact text when literal=true." },
                "literal": { "type": "boolean" },
                "path": { "type": "string" },
                "recursive": { "type": "boolean" },
                "caseInsensitive": { "type": "boolean" },
                "multiline": { "type": "boolean" },
                "maxMatches": { "type": "integer", "minimum": 1, "maximum": 1000 },
                "offset": { "type": "integer", "minimum": 0 },
                "glob": { "type": "string", "description": "Path filter within the search root." },
                "fileType": { "type": "string", "description": "File extension or common type name." },
                "beforeContext": { "type": "integer", "minimum": 0, "maximum": 20 },
                "afterContext": { "type": "integer", "minimum": 0, "maximum": 20 },
                "outputMode": { "type": "string", "enum": ["content", "files_with_matches", "count"] }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}
