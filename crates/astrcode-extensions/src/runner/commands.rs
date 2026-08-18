use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use astrcode_extension_sdk::extension::{
    internal::{
        RuntimeHookCallContext, canonicalize_command_name, command_completion_context,
        command_context, command_discovery_context,
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
    /// `None` for host-executed commands, which dispatch without an extension handler.
    handler: Option<Arc<dyn CommandHandler>>,
    index: Weak<HandlerIndex>,
}

#[derive(Clone)]
pub struct ResolvedCommandSurface {
    pub commands: Vec<ResolvedSlashCommand>,
    pub ui: ExtensionUiContributions,
}

/// How long a resolved command surface stays fresh. Discovery hooks may read
/// the filesystem, so entries age out quickly; the cache only removes the
/// repeated per-invocation recomputation of the full discovery set.
const COMMAND_SURFACE_CACHE_TTL: Duration = Duration::from_secs(1);
/// Upper bound on cached working directories within one generation.
const COMMAND_SURFACE_CACHE_MAX_WORKING_DIRS: usize = 32;

struct CommandSurfaceCacheEntry {
    surface: Arc<ResolvedCommandSurface>,
    refreshed_at: Instant,
}

#[derive(Default)]
pub(super) struct CommandSurfaceCache {
    entries: parking_lot::Mutex<HashMap<(u64, String), CommandSurfaceCacheEntry>>,
}

impl CommandSurfaceCache {
    fn get(&self, generation: u64, working_dir: &str) -> Option<Arc<ResolvedCommandSurface>> {
        let entries = self.entries.lock();
        let entry = entries.get(&(generation, working_dir.to_owned()))?;
        if entry.refreshed_at.elapsed() >= COMMAND_SURFACE_CACHE_TTL {
            return None;
        }
        Some(Arc::clone(&entry.surface))
    }

    fn insert(&self, generation: u64, working_dir: &str, surface: Arc<ResolvedCommandSurface>) {
        let mut entries = self.entries.lock();
        // Entries from older generations can never be hit again; drop them wholesale.
        entries.retain(|(entry_generation, _), _| *entry_generation == generation);
        if entries.len() >= COMMAND_SURFACE_CACHE_MAX_WORKING_DIRS
            && let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.refreshed_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest_key);
        }
        entries.insert(
            (generation, working_dir.to_owned()),
            CommandSurfaceCacheEntry {
                surface,
                refreshed_at: Instant::now(),
            },
        );
    }
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

    /// Collect slash commands from the HandlerIndex cache plus dynamic discovery.
    async fn collect_commands_for_typed(
        &self,
        working_dir: &str,
    ) -> Vec<(String, SlashCommand, Option<Arc<dyn CommandHandler>>)> {
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
                        let (mut cmd, handler) = command.into_parts();
                        if matches!(cmd.execution, CommandExecution::Host(_)) {
                            tracing::warn!(
                                extension_id,
                                command = %cmd.name,
                                "command discovery cannot declare host-executed commands"
                            );
                            continue;
                        }
                        if let Err(reason) = canonicalize_command_name(&mut cmd.name) {
                            tracing::warn!(
                                extension_id,
                                command = %cmd.name,
                                %reason,
                                "command discovery returned an invalid command name"
                            );
                            continue;
                        }
                        cmds.push((extension_id.clone(), cmd, Some(handler)));
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
    ///
    /// Host-executed commands never reach this path: the server dispatches them
    /// behind its session operation gate. The `None` handler guard below turns a
    /// violation of that invariant into a typed internal error instead of a panic.
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
        let handler = resolved.handler.as_ref().ok_or_else(|| {
            ExtensionError::Internal(format!(
                "host-executed command {} reached the extension invoke path",
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
                handler.execute(ctx),
            )
            .await?;
        Ok(result)
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
        let handler = resolved.handler.as_ref().ok_or_else(|| {
            ExtensionError::Internal(format!(
                "host-executed command {} reached the completion path",
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
            handler.complete(ctx),
        )
        .await
    }
}

impl ExtensionRunner {
    pub async fn resolve_command_surface(&self, working_dir: &str) -> ResolvedCommandSurface {
        let view = self.extension_view().await;
        let surface = self.cached_command_surface(&view, working_dir).await;
        (*surface).clone()
    }

    pub async fn resolve_commands_for_typed(&self, working_dir: &str) -> Vec<ResolvedSlashCommand> {
        let view = self.extension_view().await;
        self.cached_command_surface(&view, working_dir)
            .await
            .commands
            .clone()
    }

    async fn cached_command_surface(
        &self,
        view: &ExtensionView,
        working_dir: &str,
    ) -> Arc<ResolvedCommandSurface> {
        let generation = view.generation();
        if let Some(surface) = self.command_surface_cache.get(generation, working_dir) {
            return surface;
        }
        let surface = Arc::new(view.resolve_command_surface(working_dir).await);
        self.command_surface_cache
            .insert(generation, working_dir, Arc::clone(&surface));
        surface
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
    left: &(String, SlashCommand, Option<Arc<dyn CommandHandler>>),
    right: &(String, SlashCommand, Option<Arc<dyn CommandHandler>>),
) -> std::cmp::Ordering {
    right
        .1
        .priority
        .cmp(&left.1.priority)
        .then_with(|| left.0.cmp(&right.0))
        .then_with(|| left.1.name.cmp(&right.1.name))
}
