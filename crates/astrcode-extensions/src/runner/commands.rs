use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Weak},
};

use astrcode_extension_sdk::extension::{
    internal::{
        RuntimeHookCallContext, command_completion_context, command_context,
        command_discovery_context,
    },
    *,
};
use tokio_util::sync::CancellationToken;

use super::{
    ExtensionCallContextInput, ExtensionRunner, ExtensionUiContributions, ExtensionView,
    HandlerIndex,
};

#[derive(Clone)]
pub struct ResolvedSlashCommand {
    pub extension_id: String,
    pub command: astrcode_extension_sdk::extension::SlashCommand,
    pub shadowed: Vec<ShadowedSlashCommand>,
    handler: Arc<dyn CommandHandler>,
    index: Weak<HandlerIndex>,
}

pub struct ResolvedCommandSurface {
    pub commands: Vec<ResolvedSlashCommand>,
    pub ui: ExtensionUiContributions,
}

impl fmt::Debug for ResolvedSlashCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedSlashCommand")
            .field("extension_id", &self.extension_id)
            .field("command", &self.command)
            .field("shadowed", &self.shadowed)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct ShadowedSlashCommand {
    pub extension_id: String,
    pub priority: i32,
}

impl ExtensionView {
    fn extension_command_context(
        &self,
        index: &Arc<HandlerIndex>,
        extension_id: &str,
        command_name: &str,
        argument: &str,
        runtime: &RuntimeHookCallContext,
    ) -> Result<(CommandContext, CancellationToken), ExtensionError> {
        let cancellation = runtime.cancellation().child_token();
        let call = self.make_registered_extension_call_context_from_index(
            index,
            extension_id,
            ExtensionCallContextInput::from_hook(runtime, cancellation.clone()),
        )?;
        let cancellation = call.cancellation().clone();
        Ok((
            command_context(
                call,
                runtime.session_id().clone(),
                runtime.turn_id().map(str::to_owned),
                runtime.working_dir().to_path_buf(),
                runtime.model().clone(),
                command_name,
                argument,
            ),
            cancellation,
        ))
    }

    /// 从 HandlerIndex 缓存收集斜杠命令。
    async fn collect_commands_for_typed(
        &self,
        working_dir: &str,
    ) -> Vec<(String, SlashCommand, Arc<dyn CommandHandler>)> {
        let index = &self.index;
        let mut cmds = index.static_commands.clone();
        for (extension_id, discovery) in &index.command_discoveries {
            let cancellation = CancellationToken::new();
            let call = self.make_registered_extension_call_context(
                extension_id,
                ExtensionCallContextInput {
                    working_dir: Some(PathBuf::from(working_dir)),
                    ..ExtensionCallContextInput::unscoped(cancellation.clone())
                },
            );
            let discovered = match call {
                Ok(call) => {
                    let cancellation = call.cancellation().clone();
                    let ctx = command_discovery_context(
                        call,
                        PathBuf::from(working_dir),
                        self.generation(),
                    );
                    self.run_recorded_hook(
                        extension_id,
                        "command_discovery",
                        cancellation,
                        discovery.discover(ctx),
                    )
                    .await
                },
                Err(error) => Err(error),
            };
            match discovered {
                Ok(discovered) => {
                    for command in discovered.into_commands() {
                        let (cmd, handler) = command.into_parts();
                        if !command_execution_is_authorized(index, extension_id, &cmd) {
                            tracing::warn!(
                                extension_id,
                                command = %cmd.name,
                                "slash command requested a host session command without capability"
                            );
                            continue;
                        }
                        cmds.push((extension_id.clone(), cmd, handler));
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        extension_id,
                        error = %error,
                        "command discovery failed"
                    );
                },
            }
        }
        cmds
    }

    /// Resolve visible slash commands and retain lower-priority declarations
    /// for diagnostics.
    pub async fn resolve_commands_for_typed(&self, working_dir: &str) -> Vec<ResolvedSlashCommand> {
        let mut commands = self.collect_commands_for_typed(working_dir).await;
        commands.sort_by(compare_command_registration);

        let mut resolved = Vec::<ResolvedSlashCommand>::new();
        for (extension_id, command, handler) in commands {
            if let Some(active) = resolved
                .iter_mut()
                .find(|resolved| resolved.command.name == command.name)
            {
                tracing::warn!(
                    command = %command.name,
                    extension_id = %extension_id,
                    priority = command.priority,
                    active_extension_id = %active.extension_id,
                    active_priority = active.command.priority,
                    "slash command shadowed by higher priority command"
                );
                active.shadowed.push(ShadowedSlashCommand {
                    extension_id,
                    priority: command.priority,
                });
                continue;
            }
            resolved.push(ResolvedSlashCommand {
                extension_id,
                command,
                shadowed: Vec::new(),
                handler,
                index: Arc::downgrade(&self.index),
            });
        }
        resolved
    }

    async fn resolve_command_surface(&self, working_dir: &str) -> ResolvedCommandSurface {
        let commands = self.resolve_commands_for_typed(working_dir).await;
        let ui = ExtensionUiContributions {
            keybindings: self.index.keybindings.clone(),
            status_items: self.index.status_items.clone(),
        };
        ResolvedCommandSurface { commands, ui }
    }

    /// Execute an already-resolved slash command without re-reading the command registry.
    pub async fn invoke_resolved_command_typed(
        &self,
        resolved: &ResolvedSlashCommand,
        arguments: &str,
        runtime: &RuntimeHookCallContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let active_index = resolved.index.upgrade().ok_or_else(|| {
            ExtensionError::NotFound(format!(
                "command {} generation is no longer available",
                resolved.command.name
            ))
        })?;
        let (ctx, cancellation) = self.extension_command_context(
            &active_index,
            &resolved.extension_id,
            &resolved.command.name,
            arguments,
            runtime,
        )?;
        let result = self
            .run_recorded_hook(
                &resolved.extension_id,
                "command",
                cancellation,
                resolved.handler.execute(ctx),
            )
            .await?;
        admit_command_result(&active_index, resolved, result)
    }

    /// Complete arguments for an already-resolved slash command without re-reading the registry.
    pub async fn complete_resolved_command_typed(
        &self,
        resolved: &ResolvedSlashCommand,
        argument: &str,
        cursor: usize,
        runtime: &RuntimeHookCallContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        let active_index = resolved.index.upgrade().ok_or_else(|| {
            ExtensionError::NotFound(format!(
                "command {} generation is no longer available",
                resolved.command.name
            ))
        })?;
        let (ctx, cancellation) = self.extension_command_context(
            &active_index,
            &resolved.extension_id,
            &resolved.command.name,
            argument,
            runtime,
        )?;
        let ctx = command_completion_context(ctx, cursor);
        self.run_recorded_hook(
            &resolved.extension_id,
            "command_complete",
            cancellation,
            resolved.handler.complete(ctx),
        )
        .await
    }
}

fn command_execution_is_authorized(
    index: &HandlerIndex,
    extension_id: &str,
    command: &SlashCommand,
) -> bool {
    !matches!(command.execution, CommandExecution::Host(_))
        || index
            .extensions
            .get(extension_id)
            .is_some_and(|generation| {
                generation
                    .capabilities
                    .contains(&ExtensionCapability::SessionCommand)
            })
}

fn admit_command_result(
    index: &HandlerIndex,
    resolved: &ResolvedSlashCommand,
    result: ExtensionCommandResult,
) -> Result<ExtensionCommandResult, ExtensionError> {
    let ExtensionCommandResult::HostCommand { intent } = &result else {
        if matches!(resolved.command.execution, CommandExecution::Host(_)) {
            return Err(ExtensionError::InvalidRegistration {
                extension_id: resolved.extension_id.clone(),
                reason: format!(
                    "host command {} did not return a host command intent",
                    resolved.command.name
                ),
            });
        }
        return Ok(result);
    };
    let generation = index
        .extensions
        .get(&resolved.extension_id)
        .ok_or_else(|| ExtensionError::NotFound(resolved.extension_id.clone()))?;
    if !generation
        .capabilities
        .contains(&ExtensionCapability::SessionCommand)
    {
        return Err(ExtensionError::MissingCapability {
            extension_id: resolved.extension_id.clone(),
            hook: "command",
            capability: ExtensionCapability::SessionCommand,
        });
    }
    if resolved.command.execution != CommandExecution::Host(intent.kind()) {
        return Err(ExtensionError::InvalidRegistration {
            extension_id: resolved.extension_id.clone(),
            reason: format!(
                "command {} returned undeclared host intent {:?}",
                resolved.command.name,
                intent.kind()
            ),
        });
    }
    Ok(result)
}

impl ExtensionRunner {
    pub async fn resolve_command_surface(&self, working_dir: &str) -> ResolvedCommandSurface {
        self.extension_view()
            .await
            .resolve_command_surface(working_dir)
            .await
    }

    pub async fn resolve_commands_for_typed(&self, working_dir: &str) -> Vec<ResolvedSlashCommand> {
        self.extension_view()
            .await
            .resolve_commands_for_typed(working_dir)
            .await
    }

    pub async fn invoke_resolved_command_typed(
        &self,
        resolved: &ResolvedSlashCommand,
        arguments: &str,
        runtime: &RuntimeHookCallContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        self.extension_view()
            .await
            .invoke_resolved_command_typed(resolved, arguments, runtime)
            .await
    }

    pub async fn complete_resolved_command_typed(
        &self,
        resolved: &ResolvedSlashCommand,
        argument: &str,
        cursor: usize,
        runtime: &RuntimeHookCallContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        self.extension_view()
            .await
            .complete_resolved_command_typed(resolved, argument, cursor, runtime)
            .await
    }
}

fn compare_command_registration(
    left: &(String, SlashCommand, Arc<dyn CommandHandler>),
    right: &(String, SlashCommand, Arc<dyn CommandHandler>),
) -> std::cmp::Ordering {
    right
        .1
        .priority
        .cmp(&left.1.priority)
        .then_with(|| left.0.cmp(&right.0))
        .then_with(|| left.1.name.cmp(&right.1.name))
}
