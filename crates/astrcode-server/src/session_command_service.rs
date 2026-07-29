//! 并发安全的 session-scoped 命令服务。
//!
//! 显式携带 [`SessionId`] 的传输层请求直接调用本服务，不经过保存交互式
//! focus/model-selection 状态的全局 command actor。

use std::sync::Arc;

use astrcode_core::{
    config::ModelSelection,
    event::LiveEventPayload,
    tool::SessionToolSelection,
    types::{SessionId, TurnId},
    user_input::UserInput,
};
use astrcode_extension_sdk::extension::{
    CommandCompletions, ExtensionCommandResult, ExtensionError,
};
use astrcode_extensions::runner::CommandSource;
use astrcode_protocol::{
    events::{ClientNotification, ExtensionCommandInfoDto},
    wire::CommandSourceDto,
};
use astrcode_session::compaction::{
    IdleCompactionError, IdleCompactionOutcome, compact_idle_session,
};

use crate::{
    bootstrap::ServerRuntime,
    delivery_gates::SessionOperationGuard,
    handler::{
        CommandInvocation, HandlerError, ManualCompactOutcome, PromptSubmission,
        session_command::CommandList,
        slash::{self, ParsedSlashCommand},
        snapshot::session_snapshot,
    },
    server_event_bus::ServerEventBus,
    turn_scheduler::{DeliveryOutcome, InputDelivery, TurnCompletion, TurnScheduler},
};

#[derive(Clone)]
pub(crate) struct SessionCommandService {
    runtime: Arc<ServerRuntime>,
    scheduler: Arc<TurnScheduler>,
    event_bus: Arc<ServerEventBus>,
}

enum CommandOperation {
    Complete(CommandInvocation),
    StartTurn(UserInput),
}

impl SessionCommandService {
    pub(crate) fn new(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<ServerEventBus>,
    ) -> Self {
        Self {
            runtime,
            scheduler,
            event_bus,
        }
    }

    pub(crate) async fn create_session(
        &self,
        working_dir: String,
        tool_selection: Option<SessionToolSelection>,
    ) -> Result<SessionId, HandlerError> {
        tracing::info!(%working_dir, "creating session");
        let created = self
            .runtime
            .session_manager()
            .create_with_tool_selection(&working_dir, tool_selection.as_ref())
            .await
            .map_err(HandlerError::SessionManager)?;
        let session_id = created.session.id().clone();
        self.event_bus
            .send_notification(ClientNotification::Event(created.start_event));
        tracing::info!(%session_id, "session fully initialized");
        Ok(session_id)
    }

    pub(crate) async fn submit_input(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<PromptSubmission, HandlerError> {
        astrcode_core::message_attachment::validate_attachments(&input.attachments)
            .map_err(|error| HandlerError::InvalidRequest(error.to_string()))?;

        let operation = self.scheduler.begin_session_operation(&session_id).await?;
        if !self.scheduler.registry().has_active(&session_id) {
            if let Some(command) =
                slash::parse_slash_command(&input.text).filter(ParsedSlashCommand::has_name)
            {
                match self.prepare_command_in_operation(&operation, command).await {
                    Err(HandlerError::UnknownCommand(_)) => {},
                    other => {
                        let command = other?;
                        return self
                            .execute_command_operation(operation, command)
                            .await
                            .map(invocation_to_submission);
                    },
                }
            }
        }

        self.scheduler
            .deliver_input_in_operation(operation, input, InputDelivery::QueueIfRunningElseStart)
            .await
            .map(delivery_to_submission)
            .map_err(HandlerError::from)
    }

    pub(crate) async fn submit_input_with_completion(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<(TurnId, tokio::sync::oneshot::Receiver<TurnCompletion>), HandlerError> {
        self.scheduler
            .start_tracked_with_completion(session_id, input)
            .await
            .map_err(HandlerError::from)
    }

    pub(crate) async fn inject_input(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        let operation = self.scheduler.begin_session_operation(&session_id).await?;
        match self
            .scheduler
            .deliver_input_in_operation(
                operation,
                UserInput::text_only(text),
                InputDelivery::InjectOnly,
            )
            .await?
        {
            DeliveryOutcome::Injected { .. } => Ok(PromptSubmission::Handled {
                message: "injected into active turn".into(),
            }),
            DeliveryOutcome::Started { .. } | DeliveryOutcome::Queued { .. } => Err(
                HandlerError::InvalidRequest("inject delivery unexpectedly enqueued input".into()),
            ),
        }
    }

    pub(crate) async fn compact_session(
        &self,
        session_id: &SessionId,
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        let operation = self.scheduler.begin_session_operation(session_id).await?;
        self.compact_session_in_operation(&operation, keep_recent_turns)
            .await
    }

    async fn compact_session_in_operation(
        &self,
        operation: &SessionOperationGuard,
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        let session_id = operation.session_id();
        if self.scheduler.registry().has_active(session_id) {
            return Err(HandlerError::CompactBlocked);
        }

        let session = self
            .runtime
            .session_manager()
            .open(session_id.clone())
            .await
            .map_err(HandlerError::SessionManager)?;
        session.emit_live(None, LiveEventPayload::CompactionStarted);

        let outcome = compact_idle_session(&session, keep_recent_turns)
            .await
            .map_err(|error| match error {
                IdleCompactionError::Session(error) => HandlerError::Session(error),
                IdleCompactionError::Extension(error) => HandlerError::Extension(error),
            });

        let (result, terminal_event) = match outcome {
            Ok(IdleCompactionOutcome::Skipped { message }) => (
                Ok(ManualCompactOutcome::Skipped {
                    message: message.clone(),
                }),
                LiveEventPayload::CompactionSkipped { reason: message },
            ),
            Ok(IdleCompactionOutcome::Compacted { messages_removed }) => {
                match session.read_model().await.map_err(HandlerError::Session) {
                    Ok(state) => {
                        self.event_bus
                            .send_notification(ClientNotification::SessionResumed {
                                session_id: session_id.clone().into_string(),
                                snapshot: session_snapshot(&state),
                            });
                        (
                            Ok(ManualCompactOutcome::Compacted {
                                session_id: session_id.clone(),
                            }),
                            LiveEventPayload::CompactionCompleted { messages_removed },
                        )
                    },
                    Err(error) => {
                        let reason = error.to_string();
                        (Err(error), LiveEventPayload::CompactionFailed { reason })
                    },
                }
            },
            Err(error) => {
                let reason = error.to_string();
                (Err(error), LiveEventPayload::CompactionFailed { reason })
            },
        };
        session.emit_live(None, terminal_event);
        result
    }

    pub(crate) async fn abort_session(&self, session_id: &SessionId) -> Result<(), HandlerError> {
        self.scheduler
            .abort(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(crate) async fn repair_stale_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        self.scheduler
            .repair_stale(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(crate) async fn delete_session(&self, session_id: &SessionId) -> Result<(), HandlerError> {
        self.scheduler
            .delete_session(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(crate) async fn delete_project(&self, working_dir: &str) -> Result<usize, HandlerError> {
        let summaries = self
            .runtime
            .session_manager()
            .list_summaries()
            .await
            .map_err(HandlerError::SessionManager)?;
        let mut deleted_count = 0;
        for summary in summaries
            .into_iter()
            .filter(|summary| summary.working_dir == working_dir)
        {
            match self.delete_session(&summary.session_id).await {
                Ok(()) => deleted_count += 1,
                Err(error) => tracing::warn!(
                    session_id = %summary.session_id,
                    %error,
                    "failed to delete session while deleting project"
                ),
            }
        }
        Ok(deleted_count)
    }

    pub(crate) async fn fork_session(
        &self,
        source_id: SessionId,
        at_cursor: Option<String>,
    ) -> Result<SessionId, HandlerError> {
        let session = self
            .runtime
            .session_manager()
            .fork(&source_id, at_cursor.as_ref())
            .await
            .map_err(HandlerError::SessionManager)?;
        let new_session_id = session.id().clone();
        let state = self
            .runtime
            .session_manager()
            .read_model(&new_session_id)
            .await
            .map_err(HandlerError::SessionManager)?;
        self.event_bus
            .send_notification(ClientNotification::SessionResumed {
                session_id: new_session_id.clone().into_string(),
                snapshot: session_snapshot(&state),
            });
        tracing::info!(
            source_session_id = %source_id,
            %new_session_id,
            "session forked"
        );
        Ok(new_session_id)
    }

    pub(crate) async fn invoke_command(
        &self,
        session_id: SessionId,
        command: ParsedSlashCommand,
    ) -> Result<CommandInvocation, HandlerError> {
        let operation = self.scheduler.begin_session_operation(&session_id).await?;
        let command = self
            .prepare_command_in_operation(&operation, command)
            .await?;
        self.execute_command_operation(operation, command).await
    }

    async fn prepare_command_in_operation(
        &self,
        operation: &SessionOperationGuard,
        command: ParsedSlashCommand,
    ) -> Result<CommandOperation, HandlerError> {
        let command = normalize_command(command)?;
        let session_id = operation.session_id().clone();

        if command.name == "compact" {
            if self.session_is_busy(&session_id).await {
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
            return match self
                .compact_session_in_operation(operation, keep_recent_turns)
                .await?
            {
                ManualCompactOutcome::Compacted { .. } => {
                    Ok(CommandOperation::Complete(CommandInvocation::Handled {
                        message: "compact accepted".into(),
                    }))
                },
                ManualCompactOutcome::Skipped { message } => {
                    Ok(CommandOperation::Complete(CommandInvocation::Handled {
                        message,
                    }))
                },
            };
        }

        if command.name == "model" {
            return Err(HandlerError::InvalidRequest(
                "interactive model selection is only available on interactive transports".into(),
            ));
        }

        let (working_dir, context) = self.command_context(&session_id).await?;
        let resolved = self
            .runtime
            .extension_runner()
            .resolve_commands_for_typed(&working_dir)
            .await
            .into_iter()
            .find(|resolved| resolved.command.name == command.name)
            .ok_or_else(|| HandlerError::UnknownCommand(command.name.clone()))?;

        if resolved.command.requires_idle && self.session_is_busy(&session_id).await {
            return Err(HandlerError::TurnAlreadyRunning);
        }

        match self
            .runtime
            .extension_runner()
            .invoke_resolved_command_typed(&resolved, &command.arguments, &working_dir, &context)
            .await
        {
            Ok(ExtensionCommandResult::Display {
                content,
                is_error,
                status_update,
            }) => {
                if let Some(update) = status_update {
                    self.event_bus
                        .send_notification(ClientNotification::StatusItemUpdate {
                            id: update.id,
                            text: update.text,
                        });
                }
                self.event_bus
                    .send_notification(ClientNotification::ExtensionCommandResult {
                        command_name: command.name,
                        content: content.clone(),
                        is_error,
                    });
                Ok(CommandOperation::Complete(CommandInvocation::Display {
                    content,
                    is_error,
                }))
            },
            Ok(ExtensionCommandResult::Handled { message }) => {
                Ok(CommandOperation::Complete(CommandInvocation::Handled {
                    message,
                }))
            },
            Ok(ExtensionCommandResult::StartTurn { instructions }) => {
                let user_text = if instructions.trim().is_empty() {
                    visible_command_text(&command)
                } else {
                    instructions
                };
                Ok(CommandOperation::StartTurn(UserInput::text_only(user_text)))
            },
            Err(ExtensionError::NotFound(name)) => Err(HandlerError::UnknownCommand(
                name.trim_start_matches('/').to_string(),
            )),
            Err(error) => Err(HandlerError::Extension(error)),
        }
    }

    async fn execute_command_operation(
        &self,
        operation: SessionOperationGuard,
        command: CommandOperation,
    ) -> Result<CommandInvocation, HandlerError> {
        let input = match command {
            CommandOperation::Complete(invocation) => return Ok(invocation),
            CommandOperation::StartTurn(input) => input,
        };
        match self
            .scheduler
            .deliver_input_in_operation(operation, input, InputDelivery::StartNew)
            .await?
        {
            DeliveryOutcome::Started { turn_id } => Ok(CommandInvocation::Started { turn_id }),
            DeliveryOutcome::Injected { .. } | DeliveryOutcome::Queued { .. } => Err(
                HandlerError::InvalidRequest("command start returned a non-started outcome".into()),
            ),
        }
    }

    pub(crate) async fn invoke_named_command(
        &self,
        session_id: SessionId,
        command_name: String,
        arguments: String,
    ) -> Result<CommandInvocation, HandlerError> {
        self.invoke_command(
            session_id,
            ParsedSlashCommand {
                name: command_name,
                arguments,
            },
        )
        .await
    }

    pub(crate) async fn complete_command(
        &self,
        session_id: SessionId,
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

        let (working_dir, context) = self.command_context(&session_id).await?;
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
            .complete_resolved_command_typed(&resolved, &argument, cursor, &working_dir, &context)
            .await
            .map_err(HandlerError::Extension)
    }

    pub(crate) async fn command_list(
        &self,
        session_id: &SessionId,
        include_interactive: bool,
    ) -> Result<CommandList, HandlerError> {
        let state = self
            .runtime
            .session_manager()
            .read_model(session_id)
            .await
            .map_err(HandlerError::SessionManager)?;
        Ok(self
            .command_list_for_working_dir(&state.identity.working_dir, include_interactive)
            .await)
    }

    pub(crate) async fn command_list_for_working_dir(
        &self,
        working_dir: &str,
        include_interactive: bool,
    ) -> CommandList {
        let mut commands = builtin_commands(include_interactive);
        for resolved in self
            .runtime
            .extension_runner()
            .resolve_commands_for_typed(working_dir)
            .await
        {
            if commands
                .iter()
                .any(|command| command.name == resolved.command.name)
            {
                continue;
            }
            commands.push(ExtensionCommandInfoDto {
                name: resolved.command.name,
                description: resolved.command.description,
                needs_argument: resolved.command.args_schema.is_some(),
                requires_idle: resolved.command.requires_idle,
                argument_completions: resolved.command.argument_completions,
                priority: resolved.command.priority,
                source: command_source_dto(resolved.source),
            });
        }
        CommandList { commands }
    }

    async fn command_context(
        &self,
        session_id: &SessionId,
    ) -> Result<(String, astrcode_extension_sdk::extension::CommandContext), HandlerError> {
        let state = self
            .runtime
            .session_manager()
            .read_model(session_id)
            .await
            .map_err(HandlerError::SessionManager)?;
        let working_dir = state.identity.working_dir.clone();
        let context = astrcode_extension_sdk::extension::CommandContext {
            session_id: session_id.to_string(),
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
                .session_store_dir(session_id)
                .await
                .ok()
                .flatten(),
        };
        Ok((working_dir, context))
    }

    async fn session_is_busy(&self, session_id: &SessionId) -> bool {
        self.scheduler
            .execution_view(session_id)
            .await
            .map(|view| view.active_turn_id.is_some() || view.queued_inputs > 0)
            .unwrap_or(true)
    }
}

fn delivery_to_submission(outcome: DeliveryOutcome) -> PromptSubmission {
    match outcome {
        DeliveryOutcome::Queued { .. } => PromptSubmission::Handled {
            message: "queued for next turn".into(),
        },
        DeliveryOutcome::Started { turn_id } => PromptSubmission::Accepted { turn_id },
        DeliveryOutcome::Injected { .. } => PromptSubmission::Handled {
            message: "injected into active turn".into(),
        },
    }
}

fn invocation_to_submission(invocation: CommandInvocation) -> PromptSubmission {
    match invocation {
        CommandInvocation::Display { content, is_error } => PromptSubmission::Handled {
            message: if is_error {
                format!("Error: {content}")
            } else {
                content
            },
        },
        CommandInvocation::Handled { message } => PromptSubmission::Handled { message },
        CommandInvocation::Started { turn_id } => PromptSubmission::Accepted { turn_id },
    }
}

fn normalize_command(mut command: ParsedSlashCommand) -> Result<ParsedSlashCommand, HandlerError> {
    command.name = normalize_command_name(&command.name);
    if command.name.is_empty() {
        return Err(HandlerError::InvalidRequest(
            "command must not be empty".into(),
        ));
    }
    Ok(command)
}

fn normalize_command_name(name: &str) -> String {
    name.trim().trim_start_matches('/').to_ascii_lowercase()
}

fn visible_command_text(command: &ParsedSlashCommand) -> String {
    if command.arguments.trim().is_empty() {
        format!("/{}", command.name)
    } else {
        format!("/{} {}", command.name, command.arguments.trim())
    }
}

fn command_source_dto(source: CommandSource) -> CommandSourceDto {
    match source {
        CommandSource::Extension => CommandSourceDto::Extension,
        CommandSource::Skill => CommandSourceDto::Skill,
    }
}

fn builtin_commands(include_interactive: bool) -> Vec<ExtensionCommandInfoDto> {
    let mut commands = vec![ExtensionCommandInfoDto {
        name: "compact".into(),
        description: "Compact the current session context".into(),
        needs_argument: false,
        requires_idle: true,
        argument_completions: false,
        priority: 0,
        source: CommandSourceDto::Builtin,
    }];
    if include_interactive {
        commands.push(ExtensionCommandInfoDto {
            name: "model".into(),
            description: "Select the active AI model".into(),
            needs_argument: false,
            requires_idle: false,
            argument_completions: false,
            priority: 0,
            source: CommandSourceDto::Builtin,
        });
    }
    commands
}
