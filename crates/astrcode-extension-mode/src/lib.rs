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
//! Mode state: `<session>/extension_data/astrcode-mode/mode/mode-state.json`
//! Plan artifact: `<session>/extension_data/astrcode-mode/plan/plan.md`

mod catalog;
mod prompts;
mod store;
mod tools;

use std::{path::Path, sync::Arc};

use astrcode_extension_sdk::{
    builder::{ExtensionToolDefinition, manifest},
    extension::{
        CommandContext, CommandHandler, Extension, ExtensionCapability, ExtensionCommandResult,
        ExtensionError, ExtensionManifest, ExtensionPaths, HookMode, PreToolUseContext,
        PreToolUseHandler, PreToolUseResult, ProviderContext, ProviderHandler, ProviderResult,
        Registrar, SlashCommand, StatusItemUpdatePayload, ToolContext, ToolHandler,
    },
    llm::LlmMessage,
    permission::ApprovalMode,
    tool::{ToolPromptMetadata, ToolPromptTag, ToolResult, tool_metadata},
};

fn session_data_dir(paths: &ExtensionPaths) -> Result<&Path, ExtensionError> {
    paths
        .session_data_dir()
        .map_err(|error| ExtensionError::Internal(error.to_string()))
}
use serde_json::json;

use crate::{
    catalog::{ModeCatalog, ModeId, builtin_catalog},
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
    fn manifest(&self) -> ExtensionManifest {
        manifest("astrcode-mode")
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::ProviderRequest)
            .capability(ExtensionCapability::ToolIntercept)
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        let catalog = self.catalog.clone();
        let tool_prompt = mode_tool_prompt();
        reg.tool(
            ExtensionToolDefinition::from_definition(switch_mode_tool_definition())
                .with_prompt(tool_prompt.clone()),
            Arc::new(ModeToolHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
        reg.tool(
            ExtensionToolDefinition::from_definition(upsert_plan_tool_definition())
                .with_prompt(tool_prompt),
            Arc::new(ModeToolHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
        reg.on_pre_tool_use(
            HookMode::Blocking,
            100,
            Arc::new(ModePreToolUseHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
        reg.on_before_provider_request(HookMode::Blocking, 50, Arc::new(ModeProviderHandler));
        // 注册快捷键：Shift+Tab 切换模式
        reg.keybinding(astrcode_extension_sdk::extension::Keybinding {
            key: "shift+tab".into(),
            command: "mode".into(),
            arguments: String::new(),
            description: "Toggle plan/code mode".into(),
        });
        // 注册状态栏项：显示当前模式
        reg.status_item(astrcode_extension_sdk::extension::StatusItem {
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
                requires_idle: false,
                argument_completions: false,
                priority: 0,
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
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let extension_data_dir = session_data_dir(ctx.paths())?;
        let mode_root = store::mode_dir_from_base(extension_data_dir);
        let plan_dir = store::plan_dir_from_base(extension_data_dir);
        let tool_name = ctx.tool_name();
        let arguments = ctx.raw_arguments().clone();

        let result = match tool_name {
            SWITCH_MODE_TOOL_NAME => {
                handle_switch_mode(arguments, &mode_root, &plan_dir, &self.catalog)
            },
            UPSERT_PLAN_TOOL_NAME => handle_upsert_plan(arguments, &mode_root, &plan_dir),
            _ => return Err(ExtensionError::NotFound(tool_name.into())),
        };

        match result {
            Ok(result) => Ok(result.into()),
            Err(error) => {
                let metadata = tool_metadata([("error", json!(&error))]);
                Ok(ToolResult::text(error, true, metadata).into())
            },
        }
    }
}

struct ModePreToolUseHandler {
    catalog: Arc<ModeCatalog>,
}

#[async_trait::async_trait]
impl PreToolUseHandler for ModePreToolUseHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        if ctx.approval_mode() == ApprovalMode::Yolo {
            return Ok(PreToolUseResult::Allow);
        }

        let base = session_data_dir(ctx.paths())?;
        let mode_root = store::mode_dir_from_base(base);
        let state = store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;
        let mode_id = ModeId::from_raw(&state.current_mode);
        let Some(spec) = self.catalog.get(&mode_id) else {
            return Ok(PreToolUseResult::Allow);
        };

        if spec.restricted_tools.contains(ctx.tool_name()) {
            return Ok(PreToolUseResult::Block {
                reason: format!(
                    "Tool '{}' is not available in {} mode",
                    ctx.tool_name(),
                    spec.name
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
    async fn execute(&self, ctx: CommandContext) -> Result<ExtensionCommandResult, ExtensionError> {
        let extension_data_dir = session_data_dir(ctx.paths())?;
        let mode_root = store::mode_dir_from_base(extension_data_dir);
        let mut state = store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;

        let target_mode = match ctx.argument().trim() {
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
            return Ok(ExtensionCommandResult::display_with_status(
                format!("Already in {target_mode} mode"),
                false,
                StatusItemUpdatePayload {
                    id: "mode".into(),
                    text: target_mode.to_string(),
                },
            ));
        }

        state.current_mode = target_mode.to_string();
        if target_mode == "plan" {
            state.user_initiated = true;
        }
        store::save_mode_state(&mode_root, &state).map_err(ExtensionError::Internal)?;

        Ok(ExtensionCommandResult::display_with_status(
            format!("Switched to {target_mode} mode"),
            false,
            StatusItemUpdatePayload {
                id: "mode".into(),
                text: target_mode.to_string(),
            },
        ))
    }
}

#[async_trait::async_trait]
impl ProviderHandler for ModeProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let base = session_data_dir(ctx.paths())?;
        let mode_root = store::mode_dir_from_base(base);
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

fn mode_tool_prompt() -> ToolPromptMetadata {
    ToolPromptMetadata::new(String::new()).prompt_tag(ToolPromptTag::Planning)
}
