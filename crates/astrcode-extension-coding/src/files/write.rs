use std::path::Path;

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::HostWorkspaceWriteRequest,
    hostpaths::resolve_path,
    tool::{ResourceAccess, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan, ToolResult},
};
use serde::Deserialize;

use super::text_change_metadata;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    create_dirs: bool,
}

pub(super) struct WriteHandler;

#[async_trait::async_trait]
impl ToolHandler for WriteHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: WriteArgs = context.arguments()?;
        Ok(ToolPlan::new([ResourceAccess::write_file(resolve_path(
            context.working_dir(),
            Path::new(&args.path),
        ))]))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let args: WriteArgs = context.arguments()?;
        let output = context
            .host()
            .workspace()?
            .write(HostWorkspaceWriteRequest {
                path: args.path.clone(),
                content: args.content,
                create_dirs: args.create_dirs,
            })
            .await?;
        let created = output.created;
        let content = if created {
            format!(
                "Created {} ({} bytes)",
                output.path, output.change.new_bytes
            )
        } else {
            format!(
                "Updated {} ({} bytes)",
                output.path, output.change.new_bytes
            )
        };
        let mut metadata = text_change_metadata(&output.change);
        metadata.insert("path".into(), serde_json::json!(output.path));
        metadata.insert("created".into(), serde_json::json!(created));
        Ok(ToolResult::success(content).with_metadata(metadata).into())
    }
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "write".into(),
        description: concat!(
            "Create or completely overwrite a file.\n\n",
            "When NOT to use:\n- Incremental edits to an existing file → `edit`\n\n",
            "Tips:\n- New files\n- Full-file rewrite after `read`"
        )
        .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Target path." },
                "content": { "type": "string", "description": "Complete UTF-8 content. Replaces the whole file. MUST read existing files first." },
                "createDirs": { "type": "boolean", "description": "Create missing parent directories." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    }
}
