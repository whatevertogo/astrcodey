use std::{collections::BTreeMap, path::Path};

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{HostWorkspaceEditRequest, HostWorkspaceTextEdit},
    hostpaths::resolve_path,
    tool::{ResourceAccess, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan, ToolResult},
};
use serde::Deserialize;

use super::text_change_metadata;
use crate::invalid_input;

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

impl EditArgs {
    fn into_host_request(self) -> HostWorkspaceEditRequest {
        HostWorkspaceEditRequest {
            path: self.path,
            old_text: self.old_text,
            new_text: self.new_text,
            replace_all: self.replace_all,
            edits: self
                .edits
                .into_iter()
                .map(|edit| HostWorkspaceTextEdit {
                    old_text: edit.old_text,
                    new_text: edit.new_text,
                    replace_all: edit.replace_all,
                })
                .collect(),
        }
    }
}

pub(super) struct EditHandler;

#[async_trait::async_trait]
impl ToolHandler for EditHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: EditArgs = context.arguments()?;
        validate(&args)?;
        Ok(ToolPlan::new([ResourceAccess::read_write_file(
            resolve_path(context.working_dir(), Path::new(&args.path)),
        )]))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let args: EditArgs = context.arguments()?;
        validate(&args)?;
        let output = context
            .host()
            .workspace()?
            .edit(args.into_host_request())
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
        Ok(ToolResult::success(format!("Edited {}", output.path))
            .with_metadata(metadata)
            .into())
    }
}

fn validate(args: &EditArgs) -> Result<(), ExtensionError> {
    let has_top_level = args.old_text.is_some() || args.new_text.is_some();
    if has_top_level && !args.edits.is_empty() {
        return Err(invalid_input(
            "use either oldText/newText or edits, not both",
            "provide one or more exact replacements copied from `read` output",
        ));
    }
    if args.edits.is_empty() && (args.old_text.is_none() || args.new_text.is_none()) {
        return Err(invalid_input(
            "oldText and newText are required when edits is empty",
            "provide one or more exact replacements copied from `read` output",
        ));
    }
    if args.old_text.as_deref() == Some("")
        || args.edits.iter().any(|edit| edit.old_text.is_empty())
    {
        return Err(invalid_input(
            "oldText cannot be empty",
            "provide one or more exact replacements copied from `read` output",
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_edit_preserves_straight_and_curly_quotes() {
        let source = "let value = \"same\";\nlet value = “same”;\n";
        let replacement = "let value = “changed”; // keep 'straight' and ‘curly’";
        let request = EditArgs {
            path: "quotes.rs".into(),
            old_text: Some("let value = “same”;".into()),
            new_text: Some(replacement.into()),
            replace_all: false,
            edits: Vec::new(),
        }
        .into_host_request();

        let old_text = request.old_text.as_deref().unwrap();
        let new_text = request.new_text.as_deref().unwrap();
        assert_eq!(new_text, replacement);
        assert_eq!(
            source.replacen(old_text, new_text, 1),
            format!("let value = \"same\";\n{replacement}\n")
        );

        let batch = EditArgs {
            path: "quotes.rs".into(),
            old_text: None,
            new_text: None,
            replace_all: false,
            edits: vec![
                TextEdit {
                    old_text: "let straight = \"same\";".into(),
                    new_text: "let straight = “changed”;".into(),
                    replace_all: false,
                },
                TextEdit {
                    old_text: "let curly = “same”;".into(),
                    new_text: "let curly = \"changed\";".into(),
                    replace_all: true,
                },
            ],
        }
        .into_host_request();

        assert_eq!(
            batch.edits,
            [
                HostWorkspaceTextEdit {
                    old_text: "let straight = \"same\";".into(),
                    new_text: "let straight = “changed”;".into(),
                    replace_all: false,
                },
                HostWorkspaceTextEdit {
                    old_text: "let curly = “same”;".into(),
                    new_text: "let curly = \"changed\";".into(),
                    replace_all: true,
                },
            ]
        );
    }
}
