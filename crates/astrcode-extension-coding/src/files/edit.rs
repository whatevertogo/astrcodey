use std::{collections::BTreeMap, time::Instant};

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{HostWorkspaceEditRequest, HostWorkspaceTextEdit},
    tool::{
        ExecutionMode, ResourceAccess, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan,
    },
};
use serde::Deserialize;

use super::{absolute_path, text_change_metadata};
use crate::result::success;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditArgs {
    path: String,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    edits: Vec<TextEdit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TextEdit {
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

pub(super) struct EditHandler;

#[async_trait::async_trait]
impl ToolHandler for EditHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: EditArgs = context.arguments()?;
        validate(&args)?;
        Ok(ToolPlan::from_resources([ResourceAccess::read_write_file(
            absolute_path(context.working_dir(), &args.path),
        )]))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let started_at = Instant::now();
        let args: EditArgs = context.arguments()?;
        validate(&args)?;
        let edits = args
            .edits
            .into_iter()
            .map(|edit| HostWorkspaceTextEdit {
                old_text: clean_quotes(&edit.old_text),
                new_text: clean_quotes(&edit.new_text),
                replace_all: edit.replace_all,
            })
            .collect();
        let output = context
            .host()
            .workspace()?
            .edit(HostWorkspaceEditRequest {
                path: args.path,
                old_text: args.old_text.map(|value| clean_quotes(&value)),
                new_text: args.new_text.map(|value| clean_quotes(&value)),
                replace_all: args.replace_all,
                edits,
            })
            .await?;
        let mut metadata = text_change_metadata(&output.change);
        metadata.extend(BTreeMap::from([
            ("path".into(), serde_json::json!(output.path)),
            (
                "operationCount".into(),
                serde_json::json!(output.operation_count),
            ),
            (
                "replacements".into(),
                serde_json::json!(output.replacements),
            ),
        ]));
        Ok(success(started_at, format!("Edited {}", output.path), metadata).into())
    }
}

fn validate(args: &EditArgs) -> Result<(), ExtensionError> {
    let has_top_level = args.old_text.is_some() || args.new_text.is_some();
    if has_top_level && !args.edits.is_empty() {
        return Err(invalid("use either oldText/newText or edits, not both"));
    }
    if args.edits.is_empty() && (args.old_text.is_none() || args.new_text.is_none()) {
        return Err(invalid(
            "oldText and newText are required when edits is empty",
        ));
    }
    if args.old_text.as_deref() == Some("")
        || args.edits.iter().any(|edit| edit.old_text.is_empty())
    {
        return Err(invalid("oldText cannot be empty"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ExtensionError {
    ExtensionError::InvalidInput {
        code: astrcode_extension_sdk::WireErrorCode::InvalidInput
            .as_str()
            .into(),
        message: message.into(),
        hint: Some("provide one or more exact replacements copied from `read` output".into()),
    }
}

fn clean_quotes(value: &str) -> String {
    value
        .replace(['\u{201C}', '\u{201D}'], "\"")
        .replace(['\u{2018}', '\u{2019}'], "'")
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "edit".into(),
        description: concat!(
            "Exact string replacement in an existing file.\n\n",
            "When NOT to use:\n- New files → `write`\n- Multi-file changes → `patch`\n",
            "- Large rewrites of an existing file\n\n",
            "Tips:\n- Single-file, small, precise edits after `read`"
        )
        .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Existing UTF-8 file to edit." },
                "oldText": { "type": "string", "description": "Exact text copied from read output, without line numbers." },
                "newText": { "type": "string", "description": "Replacement text." },
                "replaceAll": { "type": "boolean", "description": "Replace every occurrence." },
                "edits": {
                    "type": "array",
                    "description": "Multiple atomic replacements in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string" },
                            "newText": { "type": "string" },
                            "replaceAll": { "type": "boolean" }
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path"],
            "anyOf": [
                { "required": ["oldText", "newText"] },
                { "required": ["edits"] }
            ],
            "additionalProperties": false
        }),
    }
}
