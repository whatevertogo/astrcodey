use std::{fmt, sync::Arc};

use astrcode_extension_sdk::extension::*;

use super::ExtensionRunner;

#[derive(Debug, Clone)]
pub struct RegisteredSlashCommand {
    pub extension_id: String,
    pub command: SlashCommand,
}

#[derive(Clone)]
pub struct ResolvedSlashCommand {
    pub extension_id: String,
    pub command: astrcode_extension_sdk::extension::SlashCommand,
    pub source: String,
    pub shadowed: Vec<ShadowedSlashCommand>,
    handler: Arc<dyn CommandHandler>,
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
    pub source: String,
    pub priority: i32,
}

impl ExtensionRunner {
    /// 从 HandlerIndex 缓存收集斜杠命令。
    pub async fn collect_commands_for_typed(
        &self,
        working_dir: &str,
    ) -> Vec<(String, SlashCommand, Arc<dyn CommandHandler>)> {
        let index = self.load_index();
        let mut cmds = Vec::new();
        for (ext_id, cmd, handler) in &index.static_commands {
            cmds.push((ext_id.clone(), cmd.clone(), Arc::clone(handler)));
        }
        for (extension_id, discovery) in &index.command_discoveries {
            match tokio::time::timeout(self.timeout, discovery.discover(working_dir)).await {
                Ok(discovered) => {
                    for (cmd, handler) in discovered {
                        cmds.push((extension_id.clone(), cmd, handler));
                    }
                },
                Err(_) => {
                    tracing::warn!("command discovery timed out");
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
            let source = command_source(&extension_id).to_string();
            if let Some(active) = resolved
                .iter_mut()
                .find(|resolved| resolved.command.name == command.name)
            {
                tracing::warn!(
                    command = %command.name,
                    extension_id = %extension_id,
                    source = %source,
                    priority = command.priority,
                    active_extension_id = %active.extension_id,
                    active_source = %active.source,
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
            });
        }
        resolved
    }

    /// Execute an already-resolved slash command without re-reading the command registry.
    pub async fn invoke_resolved_command_typed(
        &self,
        resolved: &ResolvedSlashCommand,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        resolved
            .handler
            .execute(&resolved.command.name, arguments, working_dir, ctx)
            .await
    }

    /// 命令派发。兼容入口统一复用 resolved command 选择策略。
    pub async fn dispatch_command_typed(
        &self,
        command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let resolved = self.resolve_commands_for_typed(working_dir).await;
        let command = resolved
            .iter()
            .find(|resolved| resolved.command.name == command_name)
            .ok_or_else(|| ExtensionError::NotFound(command_name.into()))?;
        self.invoke_resolved_command_typed(command, arguments, working_dir, ctx)
            .await
    }

    /// 命令参数补全派发。兼容入口统一复用 resolved command 选择策略。
    pub async fn complete_command_typed(
        &self,
        command_name: &str,
        argument: &str,
        cursor: usize,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        let resolved = self.resolve_commands_for_typed(working_dir).await;
        let command = resolved
            .iter()
            .find(|resolved| resolved.command.name == command_name)
            .ok_or_else(|| ExtensionError::NotFound(command_name.into()))?;
        self.complete_resolved_command_typed(command, argument, cursor, working_dir, ctx)
            .await
    }

    /// Complete arguments for an already-resolved slash command without re-reading the registry.
    pub async fn complete_resolved_command_typed(
        &self,
        resolved: &ResolvedSlashCommand,
        argument: &str,
        cursor: usize,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        resolved
            .handler
            .complete(&resolved.command.name, argument, cursor, working_dir, ctx)
            .await
    }
}

fn command_source(extension_id: &str) -> &'static str {
    if extension_id == "astrcode-skill" {
        "skill"
    } else {
        "extension"
    }
}

fn command_source_precedence(extension_id: &str) -> u8 {
    match command_source(extension_id) {
        "extension" => 2,
        "skill" => 1,
        _ => 0,
    }
}

fn compare_command_registration(
    left: &(String, SlashCommand, Arc<dyn CommandHandler>),
    right: &(String, SlashCommand, Arc<dyn CommandHandler>),
) -> std::cmp::Ordering {
    command_source_precedence(&right.0)
        .cmp(&command_source_precedence(&left.0))
        .then_with(|| right.1.priority.cmp(&left.1.priority))
        .then_with(|| left.0.cmp(&right.0))
        .then_with(|| left.1.name.cmp(&right.1.name))
}
