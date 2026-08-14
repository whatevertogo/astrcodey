use std::{collections::HashMap, sync::Arc};

use astrcode_extension_sdk::{
    extension::*,
    tool::{ToolDefinition, ToolPromptMetadata},
};

use super::{
    ExtensionRunner, HostedExtension, retirement::ExtensionIndexLease,
    supervisor::ExtensionAdmission, tool_catalog_cache::ToolCatalogCache,
};

pub(super) type ExtensionHandler<H> = (String, HookMode, Arc<H>);
pub(super) type ToolExtensionHandler<H> = (String, HookMode, ToolHookTarget, Arc<H>);
pub(super) type ToolUseExtensionHandler<H> = (String, ToolHookTarget, Arc<H>);
type ContinueAfterStopExtensionHandler<H> = (String, ContinueAfterStopOptions, Arc<H>);
pub(super) type SimpleExtensionHandler<H> = (String, Arc<H>);
type CustomEventExtensionHandler = (String, CustomEventSubscription, Arc<dyn CustomEventHandler>);
type Prioritized<T> = (i32, T);
type PrioritizedEvent<K, T> = (K, i32, T);

pub(super) struct ExtensionGenerationEntry {
    pub(super) extension_id: Arc<str>,
    pub(super) instance_id: crate::host_router::ExtensionInstanceId,
    pub(super) generation_gate: crate::host_router::ExtensionGenerationGate,
    pub(super) capabilities: Arc<[ExtensionCapability]>,
    pub(super) custom_event_declarations: Vec<CustomEventDeclaration>,
    pub(super) tasks: ExtensionTasks,
    pub(super) admission: ExtensionAdmission,
}

pub(super) struct StaticToolEntry {
    pub(super) definition: ToolDefinition,
    pub(super) prompt_metadata: Option<ToolPromptMetadata>,
    pub(super) handler: Arc<dyn ToolHandler>,
    pub(super) generation: Arc<ExtensionGenerationEntry>,
}

pub(super) struct ToolDiscoveryEntry {
    pub(super) handler: Arc<dyn ToolDiscoveryHandler>,
    pub(super) generation: Arc<ExtensionGenerationEntry>,
}

#[derive(Clone)]
pub(super) struct HttpRouteEntry {
    pub(super) extension_id: String,
    pub(super) route: ExtensionHttpRoute,
    pub(super) handler: Arc<dyn ExtensionHttpHandler>,
}

/// 预排序的 handler 索引。
///
/// 在每次 registration 发布后从所有运行时清单重建，确保分发时无需遍历+排序。
/// 各列表按 priority 降序排列，provider/compact/lifecycle 按 event 分组。
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub(super) struct HandlerIndex {
    pub(super) generation: u64,
    pub(super) tool_input_transform: Vec<ToolUseExtensionHandler<dyn ToolInputTransformHandler>>,
    pub(super) pre_tool_use: Vec<ToolUseExtensionHandler<dyn PreToolUseHandler>>,
    pub(super) post_tool_use: Vec<ToolExtensionHandler<dyn PostToolUseHandler>>,
    pub(super) provider: HashMap<ProviderEvent, Vec<ExtensionHandler<dyn ProviderHandler>>>,
    pub(super) provider_contributions: Vec<SimpleExtensionHandler<dyn ProviderContributionHandler>>,
    pub(super) prompt_build: Vec<SimpleExtensionHandler<dyn PromptBuildHandler>>,
    pub(super) compact: HashMap<CompactEvent, Vec<SimpleExtensionHandler<dyn CompactHandler>>>,
    pub(super) continue_after_stop:
        Vec<ContinueAfterStopExtensionHandler<dyn ContinueAfterStopHandler>>,
    pub(super) user_message_envelope: Vec<SimpleExtensionHandler<dyn UserMessageEnvelopeHandler>>,
    pub(super) lifecycle: HashMap<LifecycleEvent, Vec<ExtensionHandler<dyn LifecycleHandler>>>,
    pub(super) custom_event: Vec<CustomEventExtensionHandler>,
    pub(super) static_tools: Vec<StaticToolEntry>,
    pub(super) tool_discoveries: Vec<ToolDiscoveryEntry>,
    pub(super) static_commands: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)>,
    pub(super) command_discoveries: Vec<(String, Arc<dyn CommandDiscoveryHandler>)>,
    pub(super) keybindings: Vec<Keybinding>,
    pub(super) status_items: Vec<StatusItem>,
    pub(super) http_routes: Vec<HttpRouteEntry>,
    pub(super) extensions: HashMap<String, Arc<ExtensionGenerationEntry>>,
    pub(super) tool_catalog_cache: ToolCatalogCache,
    _publication_leases: Vec<ExtensionIndexLease>,
}

pub(super) fn build_handler_index(extensions: &[HostedExtension], generation: u64) -> HandlerIndex {
    let mut tool_input_transform = Vec::new();
    let mut pre_tool_use = Vec::new();
    let mut post_tool_use = Vec::new();
    let mut provider = Vec::new();
    let mut provider_contributions = Vec::new();
    let mut prompt_build = Vec::new();
    let mut compact = Vec::new();
    let mut continue_after_stop = Vec::new();
    let mut user_message_envelope = Vec::new();
    let mut lifecycle = Vec::new();
    let mut custom_event = Vec::new();
    let mut static_tools = Vec::new();
    let mut tool_discoveries = Vec::new();
    let mut static_commands = Vec::new();
    let mut command_discoveries = Vec::new();
    let mut keybindings = Vec::new();
    let mut status_items = Vec::new();
    let mut http_routes = Vec::new();
    let mut indexed_extensions = HashMap::new();
    let mut publication_leases = Vec::with_capacity(extensions.len());

    let mut ordered_extensions = extensions.iter().collect::<Vec<_>>();
    ordered_extensions.sort_by(|left, right| left.manifest.id().cmp(right.manifest.id()));

    for hosted in ordered_extensions {
        let manifest = &hosted.manifest;
        let registrations = &manifest.registrations;
        let extension_id = manifest.id().to_owned();
        let generation_entry = Arc::new(ExtensionGenerationEntry {
            extension_id: Arc::from(extension_id.as_str()),
            instance_id: hosted.instance_id,
            generation_gate: hosted.generation_gate.clone(),
            capabilities: Arc::from(manifest.capabilities()),
            custom_event_declarations: registrations.custom_event_declarations().to_vec(),
            tasks: hosted.tasks.clone(),
            admission: hosted.supervisor.admission(),
        });
        publication_leases.push(hosted.publication_lease.acquire());
        for registration in registrations.tool_input_transforms() {
            tool_input_transform.push((
                registration.priority,
                (
                    extension_id.clone(),
                    registration.target.clone(),
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for registration in registrations.pre_tool_use() {
            pre_tool_use.push((
                registration.priority,
                (
                    extension_id.clone(),
                    registration.target.clone(),
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for registration in registrations.post_tool_use() {
            post_tool_use.push((
                registration.priority,
                (
                    extension_id.clone(),
                    registration.mode,
                    registration.target.clone(),
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for (event, mode, priority, handler) in registrations.provider() {
            provider.push((
                *event,
                *priority,
                (extension_id.clone(), *mode, Arc::clone(handler)),
            ));
        }
        for (priority, handler) in registrations.provider_contributions() {
            provider_contributions.push((*priority, (extension_id.clone(), Arc::clone(handler))));
        }
        for (priority, handler) in registrations.prompt_build() {
            prompt_build.push((*priority, (extension_id.clone(), Arc::clone(handler))));
        }
        for (event, priority, handler) in registrations.compact() {
            compact.push((
                *event,
                *priority,
                (extension_id.clone(), Arc::clone(handler)),
            ));
        }
        for registration in registrations.continue_after_stop() {
            continue_after_stop.push((
                registration.priority,
                (
                    extension_id.clone(),
                    registration.options,
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for registration in registrations.user_message_envelope() {
            user_message_envelope.push((
                registration.priority,
                (extension_id.clone(), Arc::clone(&registration.handler)),
            ));
        }
        for (event, mode, priority, handler) in registrations.lifecycle() {
            lifecycle.push((
                event.clone(),
                *priority,
                (extension_id.clone(), *mode, Arc::clone(handler)),
            ));
        }
        for registration in registrations.custom_event_subscriptions() {
            custom_event.push((
                registration.priority,
                (
                    extension_id.clone(),
                    registration.subscription.clone(),
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for registration in registrations.tools() {
            let definition = registration.definition();
            let prompt_metadata = (registration.prompt() != &ToolPromptMetadata::default())
                .then(|| registration.prompt().clone());
            static_tools.push(StaticToolEntry {
                definition: definition.clone(),
                prompt_metadata,
                handler: Arc::clone(registration.handler()),
                generation: Arc::clone(&generation_entry),
            });
        }
        for discovery in registrations.tool_discoveries() {
            tool_discoveries.push(ToolDiscoveryEntry {
                handler: Arc::clone(discovery),
                generation: Arc::clone(&generation_entry),
            });
        }
        for (command, handler) in registrations.commands() {
            static_commands.push((extension_id.clone(), command.clone(), Arc::clone(handler)));
        }
        for discovery in registrations.command_discoveries() {
            command_discoveries.push((extension_id.clone(), Arc::clone(discovery)));
        }
        keybindings.extend_from_slice(registrations.keybindings());
        status_items.extend_from_slice(registrations.status_items());
        http_routes.extend(
            registrations
                .http_routes()
                .iter()
                .map(|registration| HttpRouteEntry {
                    extension_id: extension_id.clone(),
                    route: registration.route.clone(),
                    handler: Arc::clone(&registration.handler),
                }),
        );
        indexed_extensions.insert(extension_id, generation_entry);
    }

    HandlerIndex {
        generation,
        tool_input_transform: handlers_by_priority(tool_input_transform),
        pre_tool_use: handlers_by_priority(pre_tool_use),
        post_tool_use: handlers_by_priority(post_tool_use),
        provider: handlers_by_event(provider),
        provider_contributions: handlers_by_priority(provider_contributions),
        prompt_build: handlers_by_priority(prompt_build),
        compact: handlers_by_event(compact),
        continue_after_stop: handlers_by_priority(continue_after_stop),
        user_message_envelope: handlers_by_priority(user_message_envelope),
        lifecycle: handlers_by_event(lifecycle),
        custom_event: handlers_by_priority(custom_event),
        static_tools,
        tool_discoveries,
        static_commands,
        command_discoveries,
        keybindings,
        status_items,
        http_routes,
        extensions: indexed_extensions,
        tool_catalog_cache: ToolCatalogCache::default(),
        _publication_leases: publication_leases,
    }
}

fn handlers_by_priority<T>(mut handlers: Vec<Prioritized<T>>) -> Vec<T> {
    handlers.sort_by_key(|handler| std::cmp::Reverse(handler.0));
    handlers.into_iter().map(|(_, handler)| handler).collect()
}

fn handlers_by_event<K, T>(mut handlers: Vec<PrioritizedEvent<K, T>>) -> HashMap<K, Vec<T>>
where
    K: std::hash::Hash + Eq,
{
    handlers.sort_by_key(|handler| std::cmp::Reverse(handler.1));
    let mut grouped: HashMap<K, Vec<T>> = HashMap::new();
    for (event, _, handler) in handlers {
        grouped.entry(event).or_default().push(handler);
    }
    grouped
}

/// 在 debug 级日志里输出每个事件的 handler 调度顺序（按优先级降序，extension_id 标注）。
///
/// 排查「我的 hook 没生效 / 顺序不对」时打开 `RUST_LOG=astrcode_extensions=debug`
/// 即可看到每次 register 后的最终调度表。同优先级的 hook 按 extension id
/// 升序、再按该 extension 内的注册顺序调度。
pub(super) fn log_handler_dispatch_order(extensions: &[HostedExtension]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }

    let mut transform: Vec<(&str, i32, ToolHookTarget)> = Vec::new();
    let mut pre: Vec<(&str, i32, ToolHookTarget)> = Vec::new();
    let mut post: Vec<(&str, i32, HookMode, ToolHookTarget)> = Vec::new();
    let mut provider: Vec<(&str, ProviderEvent, i32, HookMode)> = Vec::new();
    let mut prompt: Vec<(&str, i32)> = Vec::new();
    let mut compact: Vec<(&str, CompactEvent, i32)> = Vec::new();
    let mut lifecycle: Vec<(&str, LifecycleEvent, i32, HookMode)> = Vec::new();

    let mut ordered_extensions = extensions.iter().collect::<Vec<_>>();
    ordered_extensions.sort_by(|left, right| left.manifest.id().cmp(right.manifest.id()));

    for hosted in ordered_extensions {
        let manifest = &hosted.manifest;
        let registrations = &manifest.registrations;
        let id = manifest.id();
        for registration in registrations.tool_input_transforms() {
            transform.push((id, registration.priority, registration.target.clone()));
        }
        for registration in registrations.pre_tool_use() {
            pre.push((id, registration.priority, registration.target.clone()));
        }
        for registration in registrations.post_tool_use() {
            post.push((
                id,
                registration.priority,
                registration.mode,
                registration.target.clone(),
            ));
        }
        for (event, mode, priority, _) in registrations.provider() {
            provider.push((id, *event, *priority, *mode));
        }
        for (priority, _) in registrations.prompt_build() {
            prompt.push((id, *priority));
        }
        for (event, priority, _) in registrations.compact() {
            compact.push((id, *event, *priority));
        }
        for (event, mode, priority, _) in registrations.lifecycle() {
            lifecycle.push((id, event.clone(), *priority, *mode));
        }
    }

    transform.sort_by_key(|x| std::cmp::Reverse(x.1));
    pre.sort_by_key(|x| std::cmp::Reverse(x.1));
    post.sort_by_key(|x| std::cmp::Reverse(x.1));
    provider.sort_by_key(|x| std::cmp::Reverse(x.2));
    prompt.sort_by_key(|x| std::cmp::Reverse(x.1));
    compact.sort_by_key(|x| std::cmp::Reverse(x.2));
    lifecycle.sort_by_key(|x| std::cmp::Reverse(x.2));

    if !transform.is_empty() {
        tracing::debug!(target: "astrcode_extensions", order = ?transform, "tool_input_transform dispatch order");
    }
    if !pre.is_empty() {
        tracing::debug!(target: "astrcode_extensions", order = ?pre, "pre_tool_use dispatch order");
    }
    if !post.is_empty() {
        tracing::debug!(target: "astrcode_extensions", order = ?post, "post_tool_use dispatch order");
    }
    if !provider.is_empty() {
        tracing::debug!(target: "astrcode_extensions", order = ?provider, "provider dispatch order");
    }
    if !prompt.is_empty() {
        tracing::debug!(target: "astrcode_extensions", order = ?prompt, "prompt_build dispatch order");
    }
    if !compact.is_empty() {
        tracing::debug!(target: "astrcode_extensions", order = ?compact, "compact dispatch order");
    }
    if !lifecycle.is_empty() {
        tracing::debug!(target: "astrcode_extensions", order = ?lifecycle, "lifecycle dispatch order");
    }
}

impl ExtensionRunner {
    pub(super) fn load_index(&self) -> Arc<HandlerIndex> {
        self.registry.index.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::{handlers_by_event, handlers_by_priority};

    #[test]
    fn priority_helpers_sort_descending_and_preserve_ties() {
        let handlers = handlers_by_priority(vec![(0, "low"), (10, "first"), (10, "second")]);
        assert_eq!(handlers, ["first", "second", "low"]);

        let grouped = handlers_by_event(vec![
            ("a", 0, "a-low"),
            ("b", 5, "b"),
            ("a", 5, "a-first"),
            ("a", 5, "a-second"),
        ]);
        assert_eq!(grouped["a"], ["a-first", "a-second", "a-low"]);
        assert_eq!(grouped["b"], ["b"]);
    }
}
