use std::{collections::BTreeMap, time::Instant};

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{
        HostWorkspaceApplyPatchRequest, HostWorkspacePatchChangeKind, analyze_unified_diff_paths,
    },
    tool::{
        ExecutionMode, ResourceAccess, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan,
    },
};
use serde::Deserialize;

use super::absolute_path;
use crate::result::success;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    patch: String,
}

pub(super) struct PatchHandler;

#[async_trait::async_trait]
impl ToolHandler for PatchHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: PatchArgs = context.arguments()?;
        let paths = analyze_unified_diff_paths(&args.patch).map_err(invalid_patch)?;
        let accesses = paths
            .into_iter()
            .flat_map(|paths| [paths.old_path, paths.new_path])
            .flatten()
            .map(|path| {
                ResourceAccess::read_write_file(absolute_path(context.working_dir(), &path))
            });
        Ok(ToolPlan::from_resources(accesses))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let started_at = Instant::now();
        let args: PatchArgs = context.arguments()?;
        let output = context
            .host()
            .workspace()?
            .apply_patch(HostWorkspaceApplyPatchRequest { patch: args.patch })
            .await?;
        let applied = output
            .changes
            .iter()
            .filter(|change| change.applied)
            .count();
        let failed = output.changes.len().saturating_sub(applied);
        let files = output
            .changes
            .iter()
            .map(|change| {
                serde_json::json!({
                    "path": change.path,
                    "changeType": kind_name(change.kind),
                    "applied": change.applied,
                    "summary": change.summary,
                    "error": change.error,
                })
            })
            .collect::<Vec<_>>();
        let content = if failed == 0 {
            format!(
                "patch: {applied}/{} files changed successfully",
                output.changes.len()
            )
        } else if applied == 0 {
            format!("patch: all {failed} file(s) failed to apply")
        } else {
            format!(
                "patch: {applied}/{} files changed, {failed} failed (partial changes committed)",
                output.changes.len()
            )
        };
        let metadata = BTreeMap::from([
            ("filesChanged".into(), serde_json::json!(applied)),
            ("filesApplied".into(), serde_json::json!(applied)),
            ("filesFailed".into(), serde_json::json!(failed)),
            ("files".into(), serde_json::json!(files)),
        ]);
        let mut result = success(started_at, content, metadata);
        if failed > 0 {
            result.is_error = true;
            result.error = output
                .changes
                .iter()
                .find_map(|change| change.error.clone())
                .or_else(|| Some(format!("{failed} file(s) failed to apply")));
        }
        Ok(result.into())
    }
}

fn invalid_patch(error: impl std::fmt::Display) -> ExtensionError {
    ExtensionError::InvalidInput {
        code: astrcode_extension_sdk::WireErrorCode::InvalidInput
            .as_str()
            .into(),
        message: error.to_string(),
        hint: Some("provide a complete unified diff with ---/+++ file headers".into()),
    }
}

const fn kind_name(kind: HostWorkspacePatchChangeKind) -> &'static str {
    match kind {
        HostWorkspacePatchChangeKind::Created => "created",
        HostWorkspacePatchChangeKind::Updated => "updated",
        HostWorkspacePatchChangeKind::Deleted => "deleted",
    }
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "patch".into(),
        description: concat!(
            "Apply a unified diff across one or more files.\n\n",
            "When NOT to use:\n- Single small replacement → `edit`\n- New file creation → \
             `write`\n\n",
            "Tips:\n- One diff can touch multiple files; failures roll back only that file\n",
            "- Format: `---` / `+++` headers and `@@` hunk markers\n",
            "- Context must match the current file exactly; re-read first."
        )
        .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "patch": { "type": "string", "description": "Unified diff text. New file uses --- /dev/null; deletion uses +++ /dev/null." }
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
    }
}
