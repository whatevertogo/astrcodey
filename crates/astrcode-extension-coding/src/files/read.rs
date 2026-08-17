use std::collections::BTreeMap;

use astrcode_extension_sdk::{
    WireErrorCode,
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{HOST_WORKSPACE_MAX_FILE_BYTES, HostWorkspaceReadOutput, HostWorkspaceReadRequest},
    tool::{
        ReadToolInlinePayload, ResourceAccess, ToolDefinition, ToolExecutionResult, ToolOrigin,
        ToolPlan, ToolResult,
    },
};
use serde::Deserialize;

use super::absolute_path;

const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_RETURNED_CHARS: usize = 30_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    char_offset: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

pub(super) struct ReadHandler;

#[async_trait::async_trait]
impl ToolHandler for ReadHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: ReadArgs = context.arguments()?;
        validate(&args)?;
        Ok(ToolPlan::new([ResourceAccess::read_file(absolute_path(
            context.working_dir(),
            &args.path,
        ))]))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let args: ReadArgs = context.arguments()?;
        validate(&args)?;
        let result = read_workspace(&context, &args).await?;
        Ok(result.into())
    }
}

async fn read_workspace(
    context: &ToolContext,
    args: &ReadArgs,
) -> Result<ToolResult, ExtensionError> {
    let output = match context
        .host()
        .workspace()?
        .read(HostWorkspaceReadRequest {
            path: args.path.clone(),
            max_bytes: Some(HOST_WORKSPACE_MAX_FILE_BYTES as u64),
            line_offset: args.offset.unwrap_or(0),
            line_limit: args.limit,
        })
        .await
    {
        Ok(output) => output,
        Err(error) if error.code_enum() == Some(WireErrorCode::InvalidInput) => {
            return Ok(
                ToolResult::error(error.message).with_metadata(BTreeMap::from([(
                    "path".into(),
                    serde_json::json!(args.path),
                )])),
            );
        },
        Err(error) => return Err(error.into()),
    };
    match output {
        HostWorkspaceReadOutput::Text {
            content,
            bytes,
            total_lines,
            line_offset,
            returned_lines,
            has_more_lines,
        } => Ok(render_text(
            args,
            content,
            bytes,
            total_lines,
            line_offset,
            returned_lines,
            has_more_lines,
        )),
        HostWorkspaceReadOutput::Image {
            media_type,
            data_base64,
            bytes,
        } => {
            let content = ReadToolInlinePayload::image(&media_type, data_base64)
                .to_content_string()
                .map_err(|error| ExtensionError::Internal(error.to_string()))?;
            Ok(ToolResult::success(content).with_metadata(BTreeMap::from([
                ("path".into(), serde_json::json!(args.path)),
                ("bytes".into(), serde_json::json!(bytes)),
                ("fileType".into(), serde_json::json!("image")),
                ("mediaType".into(), serde_json::json!(media_type)),
            ])))
        },
        HostWorkspaceReadOutput::Binary { bytes } => Ok(ToolResult::success(format!(
            "Binary file: {}",
            args.path
        ))
        .with_metadata(BTreeMap::from([
            ("path".into(), serde_json::json!(args.path)),
            ("bytes".into(), serde_json::json!(bytes)),
            ("binary".into(), serde_json::json!(true)),
        ]))),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_text(
    args: &ReadArgs,
    content: String,
    bytes: usize,
    total_lines: usize,
    line_offset: usize,
    returned_lines: usize,
    has_more_lines: bool,
) -> ToolResult {
    let lines = content.lines().collect::<Vec<_>>();
    let rendered = lines
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{:>6}\t{line}", line_offset + index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let max_chars = args.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
    let char_offset = args.char_offset.unwrap_or(0);
    let (mut content, returned_chars, char_more) = slice_chars(&rendered, char_offset, max_chars);
    let next_char_offset = char_more.then_some(char_offset.saturating_add(returned_chars));
    let next_line_offset = has_more_lines.then_some(line_offset.saturating_add(returned_lines));
    if let Some(next_char_offset) = next_char_offset {
        content.push_str(&format!(
            "\n\n[Output truncated. Continue with read using charOffset={next_char_offset}.]"
        ));
    } else if let Some(next_line_offset) = next_line_offset {
        content.push_str(&format!(
            "\n\n[More lines are available. Continue with read using offset={next_line_offset} \
             and charOffset=0.]"
        ));
    }
    ToolResult::success(content).with_metadata(BTreeMap::from([
        ("path".into(), serde_json::json!(args.path)),
        ("bytes".into(), serde_json::json!(bytes)),
        ("totalLines".into(), serde_json::json!(total_lines)),
        ("shownLines".into(), serde_json::json!(returned_lines)),
        ("offset".into(), serde_json::json!(line_offset)),
        ("nextOffset".into(), serde_json::json!(next_line_offset)),
        ("charOffset".into(), serde_json::json!(char_offset)),
        ("maxChars".into(), serde_json::json!(max_chars)),
        ("returnedChars".into(), serde_json::json!(returned_chars)),
        ("nextCharOffset".into(), serde_json::json!(next_char_offset)),
        (
            "hasMore".into(),
            serde_json::json!(char_more || has_more_lines),
        ),
        (
            "truncated".into(),
            serde_json::json!(char_more || has_more_lines),
        ),
        ("lineTruncated".into(), serde_json::json!(has_more_lines)),
        ("charTruncated".into(), serde_json::json!(char_more)),
    ]))
}

fn slice_chars(value: &str, offset: usize, max: usize) -> (String, usize, bool) {
    let mut chars = value.chars().skip(offset);
    let content = chars.by_ref().take(max).collect::<String>();
    let returned = content.chars().count();
    (content, returned, chars.next().is_some())
}

fn validate(args: &ReadArgs) -> Result<(), ExtensionError> {
    if args.path.trim().is_empty() {
        return Err(invalid("path cannot be empty"));
    }
    if args
        .max_chars
        .is_some_and(|value| value == 0 || value > MAX_RETURNED_CHARS)
    {
        return Err(invalid("maxChars must be between 1 and 30000"));
    }
    if args.limit == Some(0) {
        return Err(invalid("limit must be greater than zero"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ExtensionError {
    ExtensionError::invalid_input(
        message,
        Some("follow the read tool parameter schema".to_string()),
    )
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "read".into(),
        description: concat!(
            "Read a file with line numbers. MUST `read` before `edit`.\n\n",
            "When NOT to use:\n- Repo-wide content search → `grep` first\n\n",
            "Tips:\n- Use `read_tool_result` for a persisted result artifact ID\n",
            "- Copy text without line-number prefixes; paginate large files via parameters."
        )
        .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace file path." },
                "maxChars": { "type": "integer", "minimum": 1, "maximum": 30000, "description": "Maximum returned characters." },
                "charOffset": { "type": "integer", "minimum": 0, "description": "Character offset for pagination." },
                "offset": { "type": "integer", "minimum": 0, "description": "Start line, zero based." },
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum lines from offset." }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}
