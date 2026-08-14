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
        CommandContext, CommandHandler, CompactContributions, CompactRetainedContext, Extension,
        ExtensionCall, ExtensionCapability, ExtensionCommandResult, ExtensionError,
        ExtensionManifest, ExtensionPaths, PreCompactContext, PreCompactHandler, PreCompactResult,
        PreToolUseContext, PreToolUseHandler, PreToolUseResult, PreparedProviderContribution,
        PreparedProviderEffect, ProviderContext, ProviderContributionHandler,
        ProviderContributionId, ProviderSettlementContext, Registrar, SlashCommand,
        StatusItemUpdatePayload, ToolContext, ToolHandler, ToolPlanContext,
    },
    llm::LlmMessage,
    permission::ApprovalMode,
    tool::{HostResource, ToolPlan, ToolPromptMetadata, ToolPromptTag, ToolResult, tool_metadata},
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
            .capability(ExtensionCapability::SessionHistory)
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
            100,
            Arc::new(ModePreToolUseHandler {
                catalog: Arc::clone(&catalog),
            }),
        );
        reg.on_provider_contribution(50, Arc::new(ModeProviderHandler));
        reg.on_pre_compact(100, Arc::new(ModePreCompactHandler));
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
                availability: astrcode_extension_sdk::extension::CommandAvailability::AllTransports,
                execution: astrcode_extension_sdk::extension::CommandExecution::Extension,
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
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Session))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let extension_data_dir = session_data_dir(ctx.paths())?;
        let mode_root = store::mode_dir_from_base(extension_data_dir);
        let plan_dir = store::plan_dir_from_base(extension_data_dir);
        let tool_name = ctx.tool_name();

        let result = match tool_name {
            SWITCH_MODE_TOOL_NAME => {
                handle_switch_mode(ctx.arguments()?, &mode_root, &plan_dir, &self.catalog)
            },
            UPSERT_PLAN_TOOL_NAME => handle_upsert_plan(ctx.arguments()?, &mode_root, &plan_dir),
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

struct ModePreCompactHandler;

#[async_trait::async_trait]
impl PreCompactHandler for ModePreCompactHandler {
    async fn handle(&self, ctx: PreCompactContext) -> Result<PreCompactResult, ExtensionError> {
        let plan_dir = store::plan_dir_from_base(session_data_dir(ctx.paths())?);
        let Some(plan) = store::load_plan(&plan_dir).map_err(ExtensionError::Internal)? else {
            return Ok(PreCompactResult::Allow);
        };
        Ok(PreCompactResult::Contributions(CompactContributions {
            instructions: Vec::new(),
            retained_context: vec![CompactRetainedContext::Note {
                title: "Session Plan".into(),
                body: plan,
            }],
        }))
    }
}

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
impl ProviderContributionHandler for ModeProviderHandler {
    async fn prepare(
        &self,
        ctx: ProviderContext,
    ) -> Result<Option<PreparedProviderContribution>, ExtensionError> {
        let base = session_data_dir(ctx.paths())?;
        let mode_root = store::mode_dir_from_base(base);
        let state = store::load_mode_state(&mode_root).map_err(ExtensionError::Internal)?;

        if let Some(pending) = state.pending_transition {
            return Ok(Some(PreparedProviderContribution::new(
                ProviderContributionId::new(pending.id),
                PreparedProviderEffect::AppendMessages(vec![LlmMessage::user(pending.context)]),
            )));
        }

        Ok(None)
    }

    async fn acknowledge(&self, ctx: ProviderSettlementContext) -> Result<(), ExtensionError> {
        let base = session_data_dir(ctx.paths())?;
        let mode_root = store::mode_dir_from_base(base);
        store::acknowledge_mode_transition(&mode_root, ctx.contribution_id().as_str())
            .map_err(ExtensionError::Internal)
    }
}

fn mode_tool_prompt() -> ToolPromptMetadata {
    ToolPromptMetadata::new(String::new()).prompt_tag(ToolPromptTag::Planning)
}
