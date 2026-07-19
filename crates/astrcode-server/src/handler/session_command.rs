//! Session-scoped slash command service.

use astrcode_core::{
    config::ModelSelection,
    extension::{CommandCompletions, ExtensionCommandResult, ExtensionError},
    types::SessionId,
};
use astrcode_extensions::runner::CommandSource;
use astrcode_protocol::{
    events::ExtensionCommandInfo, http::ShadowedSlashCommandDto, wire::CommandSourceDto,
};

use super::{CommandHandler, CommandInvocation, HandlerError, PromptSubmission, slash};

pub struct CommandList {
    pub commands: Vec<ExtensionCommandInfo>,
    pub shadowed_commands: Vec<ShadowedSlashCommandDto>,
}

impl CommandHandler {
    pub(in crate::handler) async fn invoke_command_for_session(
        &mut self,
        sid: SessionId,
        command: slash::ParsedSlashCommand,
    ) -> Result<CommandInvocation, HandlerError> {
        let command = command.normalized()?;

        if command.name == "compact" {
            if self.session_is_busy(&sid).await {
                return Err(HandlerError::TurnAlreadyRunning);
            }
            let keep_recent_turns = if command.arguments.trim().is_empty() {
                None
            } else {
                Some(command.arguments.trim().parse::<usize>().map_err(|_| {
                    HandlerError::InvalidRequest(
                        "compact expects an optional non-negative integer".into(),
                    )
                })?)
            };
            return match self.compact_session(&sid, keep_recent_turns).await? {
                super::ManualCompactOutcome::Compacted { .. } => Ok(CommandInvocation::Handled {
                    message: "compact accepted".into(),
                }),
                super::ManualCompactOutcome::Skipped { message } => {
                    Ok(CommandInvocation::Handled { message })
                },
            };
        }

        if command.name == "model" {
            self.start_model_selection().await?;
            return Ok(CommandInvocation::Handled {
                message: "model selection started".into(),
            });
        }

        let (working_dir, ctx) = self.command_context(&sid).await?;
        let resolved = self
            .runtime
            .extension_runner()
            .resolve_commands_for_typed(&working_dir)
            .await
            .into_iter()
            .find(|resolved| resolved.command.name == command.name)
            .ok_or_else(|| HandlerError::UnknownCommand(command.name.clone()))?;

        if resolved.command.requires_idle && self.session_is_busy(&sid).await {
            return Err(HandlerError::TurnAlreadyRunning);
        }

        match self
            .runtime
            .extension_runner()
            .invoke_resolved_command_typed(&resolved, &command.arguments, &working_dir, &ctx)
            .await
        {
            Ok(ExtensionCommandResult::Display {
                content,
                is_error,
                status_update,
            }) => {
                if let Some(update) = status_update {
                    self.event_bus.send_notification(
                        astrcode_protocol::events::ClientNotification::StatusItemUpdate {
                            id: update.id,
                            text: update.text,
                        },
                    );
                }
                self.event_bus.send_notification(
                    astrcode_protocol::events::ClientNotification::ExtensionCommandResult {
                        command_name: command.name.clone(),
                        content: content.clone(),
                        is_error,
                    },
                );
                Ok(CommandInvocation::Display { content, is_error })
            },
            Ok(ExtensionCommandResult::Handled { message }) => {
                Ok(CommandInvocation::Handled { message })
            },
            Ok(ExtensionCommandResult::StartTurn { instructions }) => {
                let user_text = if instructions.trim().is_empty() {
                    command.visible_text()
                } else {
                    instructions
                };
                self.start_turn_for_session(sid, user_text, vec![], None)
                    .await
                    .map(|turn_id| CommandInvocation::Started { turn_id })
            },
            Err(ExtensionError::NotFound(name)) => Err(HandlerError::UnknownCommand(
                name.trim_start_matches('/').to_string(),
            )),
            Err(error) => Err(HandlerError::Extension(error)),
        }
    }

    pub(in crate::handler) async fn execute_command_for_session(
        &mut self,
        sid: SessionId,
        command: slash::ParsedSlashCommand,
    ) -> Result<PromptSubmission, HandlerError> {
        let invocation = self.invoke_command_for_session(sid, command).await?;
        Ok(match invocation {
            CommandInvocation::Display { content, is_error } => PromptSubmission::Handled {
                message: if is_error {
                    format!("Error: {content}")
                } else {
                    content
                },
            },
            CommandInvocation::Handled { message } => PromptSubmission::Handled { message },
            CommandInvocation::Started { turn_id } => PromptSubmission::Accepted { turn_id },
        })
    }

    pub(in crate::handler) async fn complete_command_for_session(
        &self,
        sid: SessionId,
        command_name: String,
        argument: String,
        cursor: Option<usize>,
    ) -> Result<CommandCompletions, HandlerError> {
        let command_name = normalize_command_name(&command_name);
        if command_name.is_empty() {
            return Err(HandlerError::InvalidRequest(
                "command must not be empty".into(),
            ));
        }
        if matches!(command_name.as_str(), "compact" | "model") {
            return Ok(CommandCompletions::default());
        }

        let (working_dir, ctx) = self.command_context(&sid).await?;
        let resolved = self
            .runtime
            .extension_runner()
            .resolve_commands_for_typed(&working_dir)
            .await
            .into_iter()
            .find(|resolved| resolved.command.name == command_name)
            .ok_or_else(|| HandlerError::UnknownCommand(command_name.clone()))?;
        if !resolved.command.argument_completions {
            return Ok(CommandCompletions::default());
        }

        let cursor = cursor.unwrap_or_else(|| argument.chars().count());
        self.runtime
            .extension_runner()
            .complete_resolved_command_typed(&resolved, &argument, cursor, &working_dir, &ctx)
            .await
            .map_err(HandlerError::Extension)
    }

    pub(in crate::handler) async fn command_list_for_session(
        &self,
        sid: &SessionId,
    ) -> Result<CommandList, HandlerError> {
        let state = self
            .runtime
            .session_manager()
            .read_model(sid)
            .await
            .map_err(HandlerError::SessionManager)?;
        Ok(self.command_list_for_working_dir(&state.working_dir).await)
    }

    pub(in crate::handler) async fn command_list_for_working_dir(
        &self,
        working_dir: &str,
    ) -> CommandList {
        let mut commands = builtin_commands();
        let mut shadowed_commands = Vec::new();

        for resolved in self
            .runtime
            .extension_runner()
            .resolve_commands_for_typed(working_dir)
            .await
        {
            let source = command_source_dto(resolved.source);
            if let Some(active) = commands
                .iter()
                .find(|command| command.name == resolved.command.name)
            {
                shadowed_commands.push(ShadowedSlashCommandDto {
                    name: resolved.command.name,
                    active_source: active.source,
                    active_priority: active.priority,
                    shadowed_source: source,
                    shadowed_priority: resolved.command.priority,
                    shadowed_extension_id: resolved.extension_id,
                });
                continue;
            }

            for shadowed in resolved.shadowed {
                shadowed_commands.push(ShadowedSlashCommandDto {
                    name: resolved.command.name.clone(),
                    active_source: source,
                    active_priority: resolved.command.priority,
                    shadowed_source: command_source_dto(shadowed.source),
                    shadowed_priority: shadowed.priority,
                    shadowed_extension_id: shadowed.extension_id,
                });
            }

            commands.push(ExtensionCommandInfo {
                name: resolved.command.name,
                description: resolved.command.description,
                needs_argument: resolved.command.args_schema.is_some(),
                requires_idle: resolved.command.requires_idle,
                argument_completions: resolved.command.argument_completions,
                priority: resolved.command.priority,
                source,
            });
        }

        CommandList {
            commands,
            shadowed_commands,
        }
    }

    async fn command_context(
        &self,
        sid: &SessionId,
    ) -> Result<(String, astrcode_core::extension::CommandContext), HandlerError> {
        let state = self
            .runtime
            .session_manager()
            .read_model(sid)
            .await
            .map_err(HandlerError::SessionManager)?;
        let working_dir = state.working_dir;
        let ctx = astrcode_core::extension::CommandContext {
            session_id: sid.to_string(),
            working_dir: working_dir.clone(),
            model: ModelSelection::simple(
                self.runtime
                    .config_manager()
                    .read_effective()
                    .llm
                    .model_id
                    .clone(),
            ),
            session_store_dir: self
                .runtime
                .session_manager()
                .session_store_dir(sid)
                .await
                .ok()
                .flatten(),
        };
        Ok((working_dir, ctx))
    }

    async fn session_is_busy(&self, sid: &SessionId) -> bool {
        self.scheduler
            .execution_view(sid)
            .await
            .map(|view| view.active_turn_id.is_some() || view.queued_inputs > 0)
            .unwrap_or(true)
    }
}

impl slash::ParsedSlashCommand {
    fn normalized(mut self) -> Result<Self, HandlerError> {
        self.name = normalize_command_name(&self.name);
        if self.name.is_empty() {
            return Err(HandlerError::InvalidRequest(
                "command must not be empty".into(),
            ));
        }
        Ok(self)
    }

    pub(in crate::handler) fn visible_text(&self) -> String {
        if self.arguments.trim().is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.arguments.trim())
        }
    }
}

fn normalize_command_name(name: &str) -> String {
    name.trim().trim_start_matches('/').to_ascii_lowercase()
}

fn command_source_dto(source: CommandSource) -> CommandSourceDto {
    match source {
        CommandSource::Extension => CommandSourceDto::Extension,
        CommandSource::Skill => CommandSourceDto::Skill,
    }
}

fn builtin_commands() -> Vec<ExtensionCommandInfo> {
    vec![
        ExtensionCommandInfo {
            name: "compact".into(),
            description: "Compact the current session context".into(),
            needs_argument: false,
            requires_idle: true,
            argument_completions: false,
            priority: 0,
            source: CommandSourceDto::Builtin,
        },
        ExtensionCommandInfo {
            name: "model".into(),
            description: "Select the active AI model".into(),
            needs_argument: false,
            requires_idle: false,
            argument_completions: false,
            priority: 0,
            source: CommandSourceDto::Builtin,
        },
    ]
}
