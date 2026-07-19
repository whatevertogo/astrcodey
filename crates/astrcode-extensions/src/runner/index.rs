use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_extension_sdk::{
    extension::*,
    tool::{ToolDefinition, ToolPromptMetadata, ToolUiWire},
};

use super::{ExtensionRecord, ExtensionRunner};

pub(super) type ExtensionHandler<H> = (String, HookMode, Arc<H>);
pub(super) type ToolExtensionHandler<H> = (String, HookMode, ToolHookTarget, Arc<H>);
pub(super) type ContinueAfterStopExtensionHandler<H> = (String, ContinueAfterStopOptions, Arc<H>);
pub(super) type SimpleExtensionHandler<H> = (String, Arc<H>);
type Prioritized<T> = (i32, T);
type PrioritizedEvent<K, T> = (K, i32, T);

#[derive(Clone)]
pub(super) struct HttpRouteEntry {
    pub(super) extension_id: String,
    pub(super) route: ExtensionHttpRoute,
    pub(super) handler: Arc<dyn ExtensionHttpHandler>,
}

/// 预排序的 handler 索引。
///
/// 在每次 `register()` 后从所有 records 重建，确保分发时无需遍历+排序。
/// 各列表按 priority 降序排列，provider/compact/lifecycle 按 event 分组。
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub(super) struct HandlerIndex {
    pub(super) pre_tool_use: Vec<ToolExtensionHandler<dyn PreToolUseHandler>>,
    pub(super) post_tool_use: Vec<ToolExtensionHandler<dyn PostToolUseHandler>>,
    pub(super) provider: HashMap<ProviderEvent, Vec<ExtensionHandler<dyn ProviderHandler>>>,
    pub(super) prompt_build: Vec<Arc<dyn PromptBuildHandler>>,
    pub(super) compact: HashMap<CompactEvent, Vec<Arc<dyn CompactHandler>>>,
    pub(super) post_tool_use_failure: Vec<Arc<dyn PostToolUseFailureHandler>>,
    pub(super) continue_after_stop:
        Vec<ContinueAfterStopExtensionHandler<dyn ContinueAfterStopHandler>>,
    pub(super) user_message_envelope: Vec<SimpleExtensionHandler<dyn UserMessageEnvelopeHandler>>,
    pub(super) after_tool_results: Vec<SimpleExtensionHandler<dyn AfterToolResultsHandler>>,
    pub(super) lifecycle: HashMap<ExtensionEvent, Vec<ExtensionHandler<dyn LifecycleHandler>>>,
    // 预计算的 collect 缓存
    pub(super) tool_metadata: HashMap<String, ToolPromptMetadata>,
    pub(super) tool_ui: HashMap<String, ToolUiWire>,
    pub(super) static_tools: Vec<(
        ToolDefinition,
        Arc<dyn ToolHandler>,
        String,
        Vec<ExtensionCapability>,
    )>,
    pub(super) tool_discoveries: Vec<(
        String,
        Arc<dyn ToolDiscoveryHandler>,
        Vec<ExtensionCapability>,
    )>,
    pub(super) static_commands: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)>,
    pub(super) command_discoveries: Vec<(String, Arc<dyn CommandDiscoveryHandler>)>,
    pub(super) keybindings: Vec<Keybinding>,
    pub(super) status_items: Vec<StatusItem>,
    pub(super) extension_event_decls: HashMap<String, Vec<ExtensionEventDecl>>,
    pub(super) extension_data_dir_extensions: HashSet<String>,
    pub(super) capabilities: HashMap<String, Vec<ExtensionCapability>>,
    pub(super) http_routes: Vec<HttpRouteEntry>,
}

impl HandlerIndex {
    pub(super) fn allows(&self, extension_id: &str, capability: ExtensionCapability) -> bool {
        self.capabilities
            .get(extension_id)
            .is_some_and(|capabilities| capabilities.contains(&capability))
    }
}

pub(super) fn build_handler_index(records: &[ExtensionRecord]) -> HandlerIndex {
    let mut pre_tool_use = Vec::new();
    let mut post_tool_use = Vec::new();
    let mut provider = Vec::new();
    let mut prompt_build = Vec::new();
    let mut compact = Vec::new();
    let mut post_tool_use_failure = Vec::new();
    let mut continue_after_stop = Vec::new();
    let mut user_message_envelope = Vec::new();
    let mut after_tool_results = Vec::new();
    let mut lifecycle = Vec::new();
    let mut tool_metadata = HashMap::new();
    let mut tool_ui = HashMap::new();
    let mut static_tools = Vec::new();
    let mut tool_discoveries = Vec::new();
    let mut static_commands = Vec::new();
    let mut command_discoveries = Vec::new();
    let mut keybindings = Vec::new();
    let mut status_items = Vec::new();
    let mut extension_event_decls = HashMap::new();
    let mut extension_data_dir_extensions = HashSet::new();
    let mut capabilities = HashMap::new();
    let mut http_routes = Vec::new();

    for record in records {
        capabilities.insert(record.id.clone(), record.capabilities.clone());
        for registration in record.reg.pre_tool_use() {
            pre_tool_use.push((
                registration.priority,
                (
                    record.id.clone(),
                    registration.mode,
                    registration.target.clone(),
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for registration in record.reg.post_tool_use() {
            post_tool_use.push((
                registration.priority,
                (
                    record.id.clone(),
                    registration.mode,
                    registration.target.clone(),
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for (event, mode, priority, handler) in record.reg.provider() {
            provider.push((
                *event,
                *priority,
                (record.id.clone(), *mode, Arc::clone(handler)),
            ));
        }
        for (priority, handler) in record.reg.prompt_build() {
            prompt_build.push((*priority, Arc::clone(handler)));
        }
        for (event, priority, handler) in record.reg.compact() {
            compact.push((*event, *priority, Arc::clone(handler)));
        }
        for (priority, handler) in record.reg.post_tool_use_failure() {
            post_tool_use_failure.push((*priority, Arc::clone(handler)));
        }
        for registration in record.reg.continue_after_stop() {
            continue_after_stop.push((
                registration.priority,
                (
                    record.id.clone(),
                    registration.options,
                    Arc::clone(&registration.handler),
                ),
            ));
        }
        for registration in record.reg.user_message_envelope() {
            user_message_envelope.push((
                registration.priority,
                (record.id.clone(), Arc::clone(&registration.handler)),
            ));
        }
        for registration in record.reg.after_tool_results() {
            after_tool_results.push((
                registration.priority,
                (record.id.clone(), Arc::clone(&registration.handler)),
            ));
        }
        for (event, mode, priority, handler) in record.reg.lifecycle() {
            lifecycle.push((
                event.clone(),
                *priority,
                (record.id.clone(), *mode, Arc::clone(handler)),
            ));
        }
        for (name, metadata) in record.reg.all_tool_metadata() {
            tool_metadata.insert(name.clone(), metadata.clone());
        }
        for (name, ui) in record.reg.all_tool_ui() {
            tool_ui.insert(name.clone(), ui.clone());
        }
        for (definition, handler) in record.reg.tools() {
            static_tools.push((
                definition.clone(),
                Arc::clone(handler),
                record.id.clone(),
                record.capabilities.clone(),
            ));
        }
        for discovery in record.reg.tool_discoveries() {
            tool_discoveries.push((
                record.id.clone(),
                Arc::clone(discovery),
                record.capabilities.clone(),
            ));
        }
        for (command, handler) in record.reg.commands() {
            static_commands.push((record.id.clone(), command.clone(), Arc::clone(handler)));
        }
        for discovery in record.reg.command_discoveries() {
            command_discoveries.push((record.id.clone(), Arc::clone(discovery)));
        }
        keybindings.extend_from_slice(record.reg.keybindings());
        status_items.extend_from_slice(record.reg.status_items());
        if !record.reg.extension_event_decls().is_empty() {
            extension_event_decls.insert(
                record.id.clone(),
                record.reg.extension_event_decls().to_vec(),
            );
        }
        if record.reg.needs_extension_data_dir() {
            extension_data_dir_extensions.insert(record.id.clone());
        }
        http_routes.extend(
            record
                .reg
                .http_routes()
                .iter()
                .map(|registration| HttpRouteEntry {
                    extension_id: record.id.clone(),
                    route: registration.route.clone(),
                    handler: Arc::clone(&registration.handler),
                }),
        );
    }

    HandlerIndex {
        pre_tool_use: handlers_by_priority(pre_tool_use),
        post_tool_use: handlers_by_priority(post_tool_use),
        provider: handlers_by_event(provider),
        prompt_build: handlers_by_priority(prompt_build),
        compact: handlers_by_event(compact),
        post_tool_use_failure: handlers_by_priority(post_tool_use_failure),
        continue_after_stop: handlers_by_priority(continue_after_stop),
        user_message_envelope: handlers_by_priority(user_message_envelope),
        after_tool_results: handlers_by_priority(after_tool_results),
        lifecycle: handlers_by_event(lifecycle),
        tool_metadata,
        tool_ui,
        static_tools,
        tool_discoveries,
        static_commands,
        command_discoveries,
        keybindings,
        status_items,
        extension_event_decls,
        extension_data_dir_extensions,
        capabilities,
        http_routes,
    }
}

pub(super) fn validate_http_route_registrations(
    extension_id: &str,
    capabilities: &[ExtensionCapability],
    routes: &[ExtensionHttpRouteRegistration],
    existing_records: &[ExtensionRecord],
) -> Result<(), String> {
    for (index, registration) in routes.iter().enumerate() {
        let route = &registration.route;
        route.validate()?;
        if !capabilities.contains(&ExtensionCapability::PublicHttp) {
            return Err(format!(
                "extension {extension_id} route {} {} requires capability {}",
                http_method_name(route.method),
                route.path,
                astrcode_extension_sdk::s5r::capability_to_wire(ExtensionCapability::PublicHttp),
            ));
        }
        if route.path == "/api" || route.path.starts_with("/api/") {
            return Err(format!(
                "extension {extension_id} public route {} uses reserved /api namespace",
                route.path
            ));
        }
        if routes[..index].iter().any(|existing| {
            existing.route.method == route.method
                && extension_http_route_patterns_conflict(&existing.route.path, &route.path)
        }) {
            return Err(format!(
                "extension {extension_id} has conflicting {} routes for {}",
                http_method_name(route.method),
                route.path
            ));
        }
        if existing_records.iter().any(|record| {
            record.reg.http_routes().iter().any(|existing| {
                existing.route.method == route.method
                    && extension_http_route_patterns_conflict(&existing.route.path, &route.path)
            })
        }) {
            return Err(format!(
                "extension {extension_id} public route conflicts with an existing {} route: {}",
                http_method_name(route.method),
                route.path
            ));
        }
    }
    Ok(())
}

fn http_method_name(method: ExtensionHttpMethod) -> &'static str {
    match method {
        ExtensionHttpMethod::Get => "GET",
        ExtensionHttpMethod::Post => "POST",
        ExtensionHttpMethod::Put => "PUT",
        ExtensionHttpMethod::Patch => "PATCH",
        ExtensionHttpMethod::Delete => "DELETE",
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
/// 即可看到每次 register 后的最终调度表。同优先级的 hook 顺序由 records 的注册
/// 顺序决定（即 loader 加载顺序），日志按这个顺序原样输出。
pub(super) fn log_handler_dispatch_order(records: &[ExtensionRecord]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }

    let mut pre: Vec<(&str, i32, HookMode, ToolHookTarget)> = Vec::new();
    let mut post: Vec<(&str, i32, HookMode, ToolHookTarget)> = Vec::new();
    let mut provider: Vec<(&str, ProviderEvent, i32, HookMode)> = Vec::new();
    let mut prompt: Vec<(&str, i32)> = Vec::new();
    let mut compact: Vec<(&str, CompactEvent, i32)> = Vec::new();
    let mut lifecycle: Vec<(&str, ExtensionEvent, i32, HookMode)> = Vec::new();

    for record in records {
        let id = record.id.as_str();
        for registration in record.reg.pre_tool_use() {
            pre.push((
                id,
                registration.priority,
                registration.mode,
                registration.target.clone(),
            ));
        }
        for registration in record.reg.post_tool_use() {
            post.push((
                id,
                registration.priority,
                registration.mode,
                registration.target.clone(),
            ));
        }
        for (event, mode, priority, _) in record.reg.provider() {
            provider.push((id, *event, *priority, *mode));
        }
        for (priority, _) in record.reg.prompt_build() {
            prompt.push((id, *priority));
        }
        for (event, priority, _) in record.reg.compact() {
            compact.push((id, *event, *priority));
        }
        for (event, mode, priority, _) in record.reg.lifecycle() {
            lifecycle.push((id, event.clone(), *priority, *mode));
        }
    }

    pre.sort_by_key(|x| std::cmp::Reverse(x.1));
    post.sort_by_key(|x| std::cmp::Reverse(x.1));
    provider.sort_by_key(|x| std::cmp::Reverse(x.2));
    prompt.sort_by_key(|x| std::cmp::Reverse(x.1));
    compact.sort_by_key(|x| std::cmp::Reverse(x.2));
    lifecycle.sort_by_key(|x| std::cmp::Reverse(x.2));

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
        Arc::clone(&self.index.read())
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
