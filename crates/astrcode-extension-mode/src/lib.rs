//! astrcode-extension-mode — Agent running mode switching (code / plan).
//!
//! Provides a mode system that controls agent behavior at runtime:
//! - **Code mode** (default): full tool access, allows delegation. No prompt injection needed.
//! - **Plan mode**: read-only tools, no delegation, produces a structured plan artifact.
//!
//! Mode instructions are injected via `BeforeProviderRequest` as user messages on transition,
//! keeping the system prompt stable so KV cache is preserved across mode switches.
//! Tool restrictions are enforced by `PreToolUse` blocking.
//!
//! Tools:
//! - `switchMode`: switch between code and plan modes, with exit gate in plan mode
//! - `upsertSessionPlan`: create or update the session plan artifact (plan mode only)
//!
//! Mode state: `<session>/mode/mode-state.json`
//! Plan artifact: `<session>/plan/plan.md`

mod catalog;
mod prompts;
mod store;
mod tools;

use std::sync::Arc;

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect, HookMode,
        HookSubscription,
    },
    llm::LlmMessage,
    tool::{ToolResult, tool_metadata},
};
use serde_json::json;

pub use crate::{
    catalog::{ModeCatalog, ModeId as ExportedModeId, ModeSpec},
    store::ModeState,
};
use crate::{
    catalog::{ModeId, builtin_catalog},
    tools::{
        SWITCH_MODE_TOOL_NAME, UPSERT_PLAN_TOOL_NAME, handle_switch_mode, handle_upsert_plan,
        switch_mode_tool_definition, upsert_plan_tool_definition,
    },
};

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(ModeExtension {
        catalog: builtin_catalog(),
    })
}

struct ModeExtension {
    catalog: ModeCatalog,
}

#[async_trait::async_trait]
impl Extension for ModeExtension {
    fn id(&self) -> &str {
        "astrcode-mode"
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![
            HookSubscription {
                event: ExtensionEvent::PreToolUse,
                mode: HookMode::Blocking,
                priority: 100,
            },
            HookSubscription {
                event: ExtensionEvent::BeforeProviderRequest,
                mode: HookMode::Blocking,
                priority: 50,
            },
        ]
    }

    async fn on_event(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        let session_id = ctx.session_id();
        let working_dir = ctx.working_dir();
        let mode_root = store::mode_store_root(session_id, working_dir);

        match event {
            ExtensionEvent::PreToolUse => {
                let Some(input) = ctx.pre_tool_use_input() else {
                    return Ok(HookEffect::Allow);
                };
                let state = store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;
                let mode_id = ModeId::from_raw(&state.current_mode);
                let Some(spec) = self.catalog.get(&mode_id) else {
                    return Ok(HookEffect::Allow);
                };

                if spec.restricted_tools.contains(&input.tool_name) {
                    return Ok(HookEffect::Block {
                        reason: format!(
                            "Tool '{}' is not available in {} mode",
                            input.tool_name, spec.name
                        ),
                    });
                }

                if !spec.allow_delegation && input.tool_name == "agent" {
                    return Ok(HookEffect::Block {
                        reason: format!("Agent delegation is not allowed in {} mode", spec.name),
                    });
                }

                Ok(HookEffect::Allow)
            },
            ExtensionEvent::BeforeProviderRequest => {
                let mut state =
                    store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;

                if let Some(context) = state.pending_transition_context.take() {
                    store::save_mode_state(&mode_root, &state).map_err(ExtensionError::Internal)?;
                    return Ok(HookEffect::AppendMessages {
                        messages: vec![LlmMessage::user(context)],
                    });
                }

                Ok(HookEffect::Allow)
            },
            _ => Ok(HookEffect::Allow),
        }
    }

    fn tools(&self) -> Vec<astrcode_core::tool::ToolDefinition> {
        vec![switch_mode_tool_definition(), upsert_plan_tool_definition()]
    }

    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let mode_root = store::mode_store_root(ctx.session_id.as_str(), working_dir);
        let plan_dir = store::plan_dir(ctx.session_id.as_str(), working_dir);

        match tool_name {
            SWITCH_MODE_TOOL_NAME => Ok(
                match handle_switch_mode(arguments, &mode_root, &plan_dir, &self.catalog) {
                    Ok(result) => result,
                    Err(error) => ToolResult::text(
                        error.clone(),
                        true,
                        tool_metadata([("error", json!(error))]),
                    ),
                },
            ),
            UPSERT_PLAN_TOOL_NAME => {
                Ok(match handle_upsert_plan(arguments, &mode_root, &plan_dir) {
                    Ok(result) => result,
                    Err(error) => ToolResult::text(
                        error.clone(),
                        true,
                        tool_metadata([("error", json!(error))]),
                    ),
                })
            },
            _ => Err(ExtensionError::NotFound(tool_name.into())),
        }
    }

    fn tool_prompt_metadata(&self) -> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata> {
        use astrcode_core::tool::ToolPromptMetadata;
        let mut map = std::collections::HashMap::new();
        map.insert(
            SWITCH_MODE_TOOL_NAME.to_string(),
            ToolPromptMetadata::new(
                "Use `switchMode` to enter plan mode for read-only exploration, or return to code mode for execution.",
            )
            .prompt_tag("planning"),
        );
        map.insert(
            UPSERT_PLAN_TOOL_NAME.to_string(),
            ToolPromptMetadata::new(
                "Only available in plan mode. The plan must contain all required headings.",
            )
            .prompt_tag("planning"),
        );
        map
    }
}
