//! Tool definitions and handlers for switchMode and upsertSessionPlan.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use astrcode_extension_sdk::{
    render::{RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    catalog::{ModeCatalog, ModeId, validate_transition},
    store,
};

// ─── Tool Names ──────────────────────────────────────────────────────────

pub const SWITCH_MODE_TOOL_NAME: &str = "switchMode";
pub const UPSERT_PLAN_TOOL_NAME: &str = "upsertSessionPlan";

// ─── Tool Definitions ────────────────────────────────────────────────────

pub fn switch_mode_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: SWITCH_MODE_TOOL_NAME.into(),
        description: ("Switch agent mode: \"code\" (default, full execution) or \"plan\" \
                       (read-only planning).\nWhen to switch to plan mode (proactive, not only \
                       when asked):\n- New feature or multi-file change (6+ files likely \
                       affected)\n- Ambiguous scope: unclear which modules are involved or what \
                       the right approach is\n- User says \"plan\", \"design\", \"how would you \
                       approach\", \"think about\", or describes a task without saying \"just do \
                       it\"\n- Risky changes: touching shared infrastructure, migrations, public \
                       APIs\nWhen NOT to plan: single-file fixes, bug fixes with clear cause, \
                       small config changes.\nSet `requireApproval: true` when the user \
                       explicitly asked for a plan. Default: false (proceed directly after \
                       planning).")
            .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["code", "plan"],
                    "description": "Target mode."
                },
                "requireApproval": {
                    "type": "boolean",
                    "description": "Set to true when the user explicitly asked for a plan. \
                                    When true, the plan must be presented to the user for review \
                                    before implementation begins. Default: false."
                }
            },
            "required": ["mode"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

pub fn upsert_plan_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: UPSERT_PLAN_TOOL_NAME.into(),
        description: "Create or update the session plan (plan mode only). Include all necessary \
                      headings: Context, Goal, Scope, Implementation Steps, Verification, \
                      Dependencies and Risks. Optional: Non-Goals, Existing Code to Reuse, \
                      Assumptions."
            .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Full plan markdown including all required headings."
                }
            },
            "required": ["content"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SwitchModeArgs {
    mode: String,
    #[serde(default)]
    require_approval: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpsertPlanArgs {
    content: String,
}

/// Transition context messages for mode entry/exit.
fn transition_context(from: &ModeId, to: &ModeId, user_initiated: bool) -> Option<String> {
    match (from.as_str(), to.as_str()) {
        ("code", "plan") => Some(format!(
            "{}\n\nCritical: You must ensure the plan follows the template format below \
             exactly\n\n{}",
            crate::prompts::plan_entry_prompt().trim(),
            crate::prompts::plan_template().trim(),
        )),
        ("plan", "code") if user_initiated => {
            Some(crate::prompts::plan_exit_prompt().trim().to_string())
        },
        ("plan", "code") => Some(
            "The session has exited plan mode and is now back in code mode.\n\nProceed with \
             implementing the plan directly. No user approval is needed."
                .to_string(),
        ),
        _ => None,
    }
}

pub fn handle_switch_mode(
    arguments: Value,
    mode_root: &Path,
    plan_dir: &Path,
    catalog: &ModeCatalog,
) -> Result<ToolResult, String> {
    let args = serde_json::from_value::<SwitchModeArgs>(arguments)
        .map_err(|e| format!("invalid args for {SWITCH_MODE_TOOL_NAME}: {e}"))?;
    let target_id = ModeId::from_raw(&args.mode);
    let mut state = store::load_mode_state(mode_root)?;
    let current_id = ModeId::from_raw(&state.current_mode);

    if current_id == target_id {
        let spec = catalog.get(&target_id);
        let mode_name = spec.map(|s| s.name.as_str()).unwrap_or(&args.mode);
        return Ok(ToolResult::text(
            format!("Already in {mode_name} mode."),
            false,
            tool_metadata([
                ("currentMode", json!(state.current_mode)),
                ("targetMode", json!(args.mode)),
            ]),
        ));
    }

    validate_transition(catalog, &current_id, &target_id)?;

    let current_spec = catalog
        .get(&current_id)
        .ok_or_else(|| format!("unknown current mode '{}'", current_id))?;

    // Require plan artifact before leaving plan mode.
    if current_spec.requires_plan_artifact {
        match store::load_plan(plan_dir)? {
            None => {
                return Ok(ToolResult::text(
                    format!(
                        "Cannot exit {} mode: no plan artifact found. Create one with \
                         {UPSERT_PLAN_TOOL_NAME} first.",
                        current_spec.name
                    ),
                    true,
                    tool_metadata([("gateBlocked", json!("no_plan_artifact"))]),
                ));
            },
            Some(content) => {
                let missing = store::validate_plan_headings(&content);
                if !missing.is_empty() {
                    return Ok(ToolResult::text(
                        format!(
                            "Plan artifact is incomplete. Missing headings: {}",
                            missing.join(", ")
                        ),
                        true,
                        tool_metadata([
                            ("gateBlocked", json!("incomplete_plan")),
                            ("missingHeadings", json!(missing)),
                        ]),
                    ));
                }
            },
        }
    }

    let target_spec = catalog
        .get(&target_id)
        .ok_or_else(|| format!("unknown target mode '{}'", args.mode))?;

    state.previous_mode = Some(state.current_mode.clone());
    state.current_mode = target_id.as_str().to_string();

    if target_id.as_str() == "plan" {
        state.user_initiated = args.require_approval;
    }

    let context = transition_context(&current_id, &target_id, state.user_initiated);

    state.pending_transition_context = context;
    store::save_mode_state(mode_root, &state)?;

    Ok(ToolResult::text(
        format!(
            "Switched from {} to {} mode.{}",
            current_id,
            target_spec.name,
            if target_spec.restricted_tools.is_empty() {
                String::new()
            } else {
                format!(
                    " Restricted tools: {}.",
                    target_spec
                        .restricted_tools
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ),
        false,
        tool_metadata([
            ("fromMode", json!(current_id.to_string())),
            ("toMode", json!(target_id.to_string())),
        ]),
    ))
}

pub fn handle_upsert_plan(
    arguments: Value,
    mode_root: &Path,
    plan_dir: &Path,
) -> Result<ToolResult, String> {
    let args = serde_json::from_value::<UpsertPlanArgs>(arguments)
        .map_err(|e| format!("invalid args for {UPSERT_PLAN_TOOL_NAME}: {e}"))?;

    let state = store::load_mode_state(mode_root)?;
    if state.current_mode != "plan" {
        return Ok(ToolResult::text(
            format!(
                "{UPSERT_PLAN_TOOL_NAME} is only available in plan mode. Current mode: {}.",
                state.current_mode
            ),
            true,
            tool_metadata([("currentMode", json!(state.current_mode))]),
        ));
    }

    let missing = store::validate_plan_headings(&args.content);
    if !missing.is_empty() {
        return Ok(ToolResult::text(
            format!(
                "Plan is missing required headings: {}. Use the plan template:\n{}",
                missing.join(", "),
                crate::prompts::plan_template()
            ),
            true,
            tool_metadata([("missingHeadings", json!(missing))]),
        ));
    }

    let is_create = store::load_plan(plan_dir)?.is_none();
    let path = store::save_plan(plan_dir, &args.content)?;

    let operation = if is_create { "create" } else { "update" };
    let ui_render = RenderSpec::Box {
        title: Some(format!("Plan {operation}")),
        tone: RenderTone::Success,
        children: vec![RenderSpec::Markdown {
            text: args.content.clone(),
            tone: RenderTone::Default,
        }],
    };

    Ok(ToolResult::text(
        if is_create {
            format!("Plan artifact created at {}.", path)
        } else {
            format!("Plan artifact updated at {}.", path)
        },
        false,
        tool_metadata([
            ("path", json!(path)),
            ("operation", json!(operation)),
            ("planContent", json!(args.content)),
            (
                UI_RENDER_METADATA_KEY,
                serde_json::to_value(&ui_render).unwrap_or_default(),
            ),
        ]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::builtin_catalog;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("astrcode-mode-tools-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn switch_to_plan_persists_state() {
        let root = test_root("switch-plan");
        let plan_dir = root.join("plan");
        let catalog = builtin_catalog();

        let result = handle_switch_mode(
            json!({ "mode": "plan" }),
            &root.join("mode"),
            &plan_dir,
            &catalog,
        )
        .expect("switch should succeed");

        assert!(!result.is_error);
        assert!(result.content.contains("Plan"));
    }

    #[test]
    fn switch_idempotent_returns_message() {
        let root = test_root("switch-idempotent");
        let plan_dir = root.join("plan");
        let catalog = builtin_catalog();

        let result = handle_switch_mode(
            json!({ "mode": "code" }),
            &root.join("mode"),
            &plan_dir,
            &catalog,
        )
        .expect("same mode should succeed");

        assert!(!result.is_error);
        assert!(result.content.contains("Already in Code"));
    }

    #[test]
    fn exit_requires_plan_artifact() {
        let mode_root = test_root("gate-no-plan").join("mode");
        let plan_dir = test_root("gate-no-plan").join("plan");
        let catalog = builtin_catalog();

        handle_switch_mode(json!({ "mode": "plan" }), &mode_root, &plan_dir, &catalog).unwrap();

        let result = handle_switch_mode(json!({ "mode": "code" }), &mode_root, &plan_dir, &catalog)
            .expect("should return result");
        assert!(result.is_error);
        assert!(result.content.contains("no plan artifact found"));
    }

    #[test]
    fn exit_switches_directly_when_plan_exists() {
        let mode_root = test_root("direct-exit").join("mode");
        let plan_dir = test_root("direct-exit").join("plan");
        let catalog = builtin_catalog();

        handle_switch_mode(json!({ "mode": "plan" }), &mode_root, &plan_dir, &catalog).unwrap();

        let plan = crate::prompts::plan_template().replace("<title>", "test");
        store::save_plan(&plan_dir, &plan).unwrap();

        let result = handle_switch_mode(json!({ "mode": "code" }), &mode_root, &plan_dir, &catalog)
            .expect("should succeed");
        assert!(!result.is_error);
        assert!(result.content.contains("Switched from plan to Code"));
    }

    #[test]
    fn upsert_plan_creates_artifact_in_plan_mode() {
        let mode_root = test_root("upsert-create").join("mode");
        let plan_dir = test_root("upsert-create").join("plan");
        let catalog = builtin_catalog();

        handle_switch_mode(json!({ "mode": "plan" }), &mode_root, &plan_dir, &catalog).unwrap();

        let plan = crate::prompts::plan_template().replace("<title>", "test plan");
        let result = handle_upsert_plan(json!({ "content": plan }), &mode_root, &plan_dir)
            .expect("upsert should succeed");

        assert!(!result.is_error);
        assert!(result.content.contains("created"));
        assert!(store::load_plan(&plan_dir).unwrap().is_some());
    }

    #[test]
    fn upsert_plan_rejects_in_code_mode() {
        let mode_root = test_root("upsert-code-mode").join("mode");
        let plan_dir = test_root("upsert-code-mode").join("plan");

        let result =
            handle_upsert_plan(json!({ "content": "## Goal\nTest" }), &mode_root, &plan_dir)
                .expect("should return result");

        assert!(result.is_error);
        assert!(result.content.contains("only available in plan mode"));
    }

    #[test]
    fn upsert_plan_rejects_incomplete_headings() {
        let mode_root = test_root("upsert-incomplete").join("mode");
        let plan_dir = test_root("upsert-incomplete").join("plan");
        let catalog = builtin_catalog();

        handle_switch_mode(json!({ "mode": "plan" }), &mode_root, &plan_dir, &catalog).unwrap();

        let result = handle_upsert_plan(
            json!({ "content": "# Plan: test\n\n## Goal\n\nDo something.\n" }),
            &mode_root,
            &plan_dir,
        )
        .expect("should return result");

        assert!(result.is_error);
        assert!(result.content.contains("missing required headings"));
    }

    #[test]
    fn full_round_trip() {
        let mode_root = test_root("full-round-trip").join("mode");
        let plan_dir = test_root("full-round-trip").join("plan");
        let catalog = builtin_catalog();

        handle_switch_mode(json!({ "mode": "plan" }), &mode_root, &plan_dir, &catalog).unwrap();

        let plan = crate::prompts::plan_template().replace("<title>", "full test");
        handle_upsert_plan(json!({ "content": plan }), &mode_root, &plan_dir).unwrap();

        let exit =
            handle_switch_mode(json!({ "mode": "code" }), &mode_root, &plan_dir, &catalog).unwrap();
        assert!(!exit.is_error);

        let state = store::load_mode_state(&mode_root).unwrap();
        assert_eq!(state.current_mode, "code");
    }
}
