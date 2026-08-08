use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Weak},
};

use astrcode_extension_sdk::extension::*;
use tokio_util::sync::CancellationToken;

use super::{
    ExtensionCallContextInput, ExtensionRunner, ExtensionUiContributions, ExtensionView,
    HandlerIndex,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Extension,
    Skill,
}

impl CommandSource {
    fn for_extension(extension_id: &str) -> Self {
        if extension_id == "astrcode-skill" {
            Self::Skill
        } else {
            Self::Extension
        }
    }

    const fn precedence(self) -> u8 {
        match self {
            Self::Extension => 2,
            Self::Skill => 1,
        }
    }
}

#[derive(Clone)]
pub struct ResolvedSlashCommand {
    pub extension_id: String,
    pub command: astrcode_extension_sdk::extension::SlashCommand,
    pub source: CommandSource,
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
            .field("source", &self.source)
            .field("shadowed", &self.shadowed)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct ShadowedSlashCommand {
    pub extension_id: String,
    pub source: CommandSource,
    pub priority: i32,
}

impl ExtensionView {
    fn extension_command_context(
        &self,
        index: &HandlerIndex,
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
        Ok((
            CommandContext::from_runtime(call, runtime.model().clone(), command_name, argument),
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
                    let ctx = CommandDiscoveryContext::from_runtime(call, self.generation());
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

    /// Resolve visible slash commands and report commands hidden by the
    /// explicit source/priority policy.
    pub async fn resolve_commands_for_typed(&self, working_dir: &str) -> Vec<ResolvedSlashCommand> {
        let mut commands = self.collect_commands_for_typed(working_dir).await;
        commands.sort_by(compare_command_registration);

        let mut resolved = Vec::<ResolvedSlashCommand>::new();
        for (extension_id, command, handler) in commands {
            let source = CommandSource::for_extension(&extension_id);
            if let Some(active) = resolved
                .iter_mut()
                .find(|resolved| resolved.command.name == command.name)
            {
                tracing::warn!(
                    command = %command.name,
                    extension_id = %extension_id,
                    source = ?source,
                    priority = command.priority,
                    active_extension_id = %active.extension_id,
                    active_source = ?active.source,
                    active_priority = active.command.priority,
                    "slash command shadowed by higher priority command"
                );
                active.shadowed.push(ShadowedSlashCommand {
                    extension_id,
                    source,
                    priority: command.priority,
                });
                continue;
            }
            resolved.push(ResolvedSlashCommand {
                extension_id,
                command,
                source,
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
        self.run_recorded_hook(
            &resolved.extension_id,
            "command",
            cancellation,
            resolved.handler.execute(ctx),
        )
        .await
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
        let ctx = CommandCompletionContext::for_runtime(ctx, cursor);
        self.run_recorded_hook(
            &resolved.extension_id,
            "command_complete",
            cancellation,
            resolved.handler.complete(ctx),
        )
        .await
    }
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
    CommandSource::for_extension(&right.0)
        .precedence()
        .cmp(&CommandSource::for_extension(&left.0).precedence())
        .then_with(|| right.1.priority.cmp(&left.1.priority))
        .then_with(|| left.0.cmp(&right.0))
        .then_with(|| left.1.name.cmp(&right.1.name))
}
