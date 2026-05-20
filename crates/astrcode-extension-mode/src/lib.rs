//! astrcode-extension-mode — Agent running mode switching (code / plan).
//!
//! Provides a mode system that controls agent behavior at runtime:
//! - **Code mode** (default): full tool access, allows delegation. No prompt injection needed.
//! - **Plan mode**: full tool access, produces a structured plan artifact.
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
        CommandContext, CommandHandler, Extension, ExtensionCommandResult, ExtensionError,
        HookMode, PreToolUseContext, PreToolUseHandler, PreToolUseResult, ProviderContext,
        ProviderEvent, ProviderHandler, ProviderResult, Registrar, SlashCommand, ToolHandler,
    },
    llm::LlmMessage,
    tool::{ToolResult, tool_metadata},
};
use serde_json::json;

pub use crate::catalog::{ModeCatalog, ModeId as ExportedModeId, ModeSpec};
use crate::{
    catalog::{ModeId, builtin_catalog},
    tools::{
        SWITCH_MODE_TOOL_NAME, UPSERT_PLAN_TOOL_NAME, handle_switch_mode, handle_upsert_plan,
        switch_mode_tool_definition, upsert_plan_tool_definition,
    },
};

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(ModeExtension {
        catalog: Arc::new(builtin_catalog()),
    })
}

struct ModeExtension {
    catalog: Arc<ModeCatalog>,
}

#[async_trait::async_trait]
impl Extension for ModeExtension {
    fn id(&self) -> &str {
        "astrcode-mode"
    }

    fn register(&self, reg: &mut Registrar) {
        let catalog = self.catalog.clone();
        reg.tool(
            switch_mode_tool_definition(),
            Arc::new(ModeToolHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
        reg.tool(
            upsert_plan_tool_definition(),
            Arc::new(ModeToolHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
        reg.tool_metadata(mode_tool_metadata());
        reg.on_pre_tool_use(
            HookMode::Blocking,
            100,
            Arc::new(ModePreToolUseHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
        reg.on_provider(
            ProviderEvent::BeforeRequest,
            HookMode::Blocking,
            50,
            Arc::new(ModeProviderHandler),
        );
        // 注册快捷键：Shift+Tab 切换模式
        reg.keybinding(astrcode_core::extension::Keybinding {
            key: "shift+tab".into(),
            command: "mode".into(),
            arguments: String::new(),
            description: "Toggle plan/code mode".into(),
        });
        // 注册状态栏项：显示当前模式
        reg.status_item(astrcode_core::extension::StatusItem {
            id: "mode".into(),
            text: "code".into(),
            priority: 0,
            tooltip: Some("Current working mode (Shift+Tab to toggle)".into()),
        });
        // 注册 /mode 斜杠命令
        reg.command(
            SlashCommand {
                name: "mode".into(),
                description: "Toggle or set working mode (plan/code). Shift+Tab to toggle.".into(),
                args_schema: None,
            },
            Arc::new(ModeSlashCommandHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
    }
}

struct ModeToolHandler {
    catalog: Arc<ModeCatalog>,
}

#[async_trait::async_trait]
impl ToolHandler for ModeToolHandler {
    async fn execute(
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
                    Err(error) => {
                        let meta = tool_metadata([("error", json!(&error))]);
                        ToolResult::text(error, true, meta)
                    },
                },
            ),
            UPSERT_PLAN_TOOL_NAME => {
                Ok(match handle_upsert_plan(arguments, &mode_root, &plan_dir) {
                    Ok(result) => result,
                    Err(error) => {
                        let meta = tool_metadata([("error", json!(&error))]);
                        ToolResult::text(error, true, meta)
                    },
                })
            },
            _ => Err(ExtensionError::NotFound(tool_name.into())),
        }
    }
}

struct ModePreToolUseHandler {
    catalog: Arc<ModeCatalog>,
}

#[async_trait::async_trait]
impl PreToolUseHandler for ModePreToolUseHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        let mode_root = store::mode_store_root(&ctx.session_id, &ctx.working_dir);
        let state = store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;
        let mode_id = ModeId::from_raw(&state.current_mode);
        let Some(spec) = self.catalog.get(&mode_id) else {
            return Ok(PreToolUseResult::Allow);
        };

        if spec.restricted_tools.contains(&ctx.tool_name) {
            return Ok(PreToolUseResult::Block {
                reason: format!(
                    "Tool '{}' is not available in {} mode",
                    ctx.tool_name, spec.name
                ),
            });
        }

        Ok(PreToolUseResult::Allow)
    }
}

struct ModeProviderHandler;

/// /mode 斜杠命令处理器：切换或设置当前模式。
struct ModeSlashCommandHandler {
    catalog: Arc<ModeCatalog>,
}

#[async_trait::async_trait]
impl CommandHandler for ModeSlashCommandHandler {
    async fn execute(
        &self,
        _command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let mode_root = store::mode_store_root(&ctx.session_id, working_dir);
        let mut state = store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;

        let target_mode = match arguments.trim() {
            "" => {
                // 切换：code → plan, plan → code
                if state.current_mode == "plan" {
                    "code"
                } else {
                    "plan"
                }
            },
            other => other,
        };

        let mode_id = ModeId::from_raw(target_mode);
        if self.catalog.get(&mode_id).is_none() {
            return Ok(ExtensionCommandResult::display(
                format!("Unknown mode '{target_mode}'. Available: code, plan"),
                true,
            ));
        }

        if state.current_mode == target_mode {
            return Ok(ExtensionCommandResult::display(
                format!("Already in {target_mode} mode"),
                false,
            ));
        }

        state.current_mode = target_mode.to_string();
        store::save_mode_state(&mode_root, &state).map_err(ExtensionError::Internal)?;

        Ok(ExtensionCommandResult::display(
            format!("Switched to {target_mode} mode"),
            false,
        ))
    }
}

#[async_trait::async_trait]
impl ProviderHandler for ModeProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let mode_root = store::mode_store_root(&ctx.session_id, &ctx.working_dir);
        let mut state = store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;

        if let Some(context) = state.pending_transition_context.take() {
            store::save_mode_state(&mode_root, &state).map_err(ExtensionError::Internal)?;
            return Ok(ProviderResult::AppendMessages {
                messages: vec![LlmMessage::user(context)],
            });
        }

        Ok(ProviderResult::Allow)
    }
}

fn mode_tool_metadata() -> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata>
{
    use astrcode_core::tool::ToolPromptMetadata;
    let mut map = std::collections::HashMap::new();
    map.insert(
        SWITCH_MODE_TOOL_NAME.to_string(),
        ToolPromptMetadata::new(
            "Use `switchMode` to enter plan mode for read-only exploration, or return to code \
             mode for execution.",
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
