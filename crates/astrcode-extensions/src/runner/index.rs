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
pub(super) type PrioritizedToolHandler<H> = (i32, String, HookMode, ToolHookTarget, Arc<H>);
pub(super) type PrioritizedContinueAfterStopHandler<H> =
    (i32, String, ContinueAfterStopOptions, Arc<H>);
pub(super) type PrioritizedSimpleHandler<H> = (i32, String, Arc<H>);
pub(super) type PrioritizedEventHandler<K, H> = (K, i32, String, HookMode, Arc<H>);

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
    let mut pre: Vec<PrioritizedToolHandler<dyn PreToolUseHandler>> = Vec::new();
    let mut post: Vec<PrioritizedToolHandler<dyn PostToolUseHandler>> = Vec::new();
    let mut prov: Vec<PrioritizedEventHandler<ProviderEvent, dyn ProviderHandler>> = Vec::new();
    let mut pb: Vec<(i32, Arc<dyn PromptBuildHandler>)> = Vec::new();
    let mut cmp: Vec<(CompactEvent, i32, Arc<dyn CompactHandler>)> = Vec::new();
    let mut ptuf: Vec<(i32, Arc<dyn PostToolUseFailureHandler>)> = Vec::new();
    let mut cas: Vec<PrioritizedContinueAfterStopHandler<dyn ContinueAfterStopHandler>> =
        Vec::new();
    let mut ume: Vec<PrioritizedSimpleHandler<dyn UserMessageEnvelopeHandler>> = Vec::new();
    let mut atr: Vec<PrioritizedSimpleHandler<dyn AfterToolResultsHandler>> = Vec::new();
    let mut lc: Vec<PrioritizedEventHandler<ExtensionEvent, dyn LifecycleHandler>> = Vec::new();
    let mut tool_metadata = HashMap::new();
    let mut tool_ui = HashMap::new();
    #[allow(clippy::type_complexity)]
    let mut static_tools: Vec<(
        ToolDefinition,
        Arc<dyn ToolHandler>,
        String,
        Vec<ExtensionCapability>,
    )> = Vec::new();
    let mut tool_discoveries: Vec<(
        String,
        Arc<dyn ToolDiscoveryHandler>,
        Vec<ExtensionCapability>,
    )> = Vec::new();
    let mut static_commands: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)> = Vec::new();
    let mut command_discoveries: Vec<(String, Arc<dyn CommandDiscoveryHandler>)> = Vec::new();
    let mut keybindings: Vec<Keybinding> = Vec::new();
    let mut status_items: Vec<StatusItem> = Vec::new();
    let mut extension_event_decls: HashMap<String, Vec<ExtensionEventDecl>> = HashMap::new();
    let mut extension_data_dir_extensions = HashSet::new();
    let mut capabilities = HashMap::new();
    let mut http_routes = Vec::new();

    for record in records {
        capabilities.insert(record.id.clone(), record.capabilities.clone());
        for registration in record.reg.pre_tool_use() {
            pre.push((
                registration.priority,
                record.id.clone(),
                registration.mode,
                registration.target.clone(),
                Arc::clone(&registration.handler),
            ));
        }
        for registration in record.reg.post_tool_use() {
            post.push((
                registration.priority,
                record.id.clone(),
                registration.mode,
                registration.target.clone(),
                Arc::clone(&registration.handler),
            ));
        }
        for (ev, mode, pri, h) in record.reg.provider() {
            prov.push((*ev, *pri, record.id.clone(), *mode, Arc::clone(h)));
        }
        for (pri, h) in record.reg.prompt_build() {
            pb.push((*pri, Arc::clone(h)));
        }
        for (ev, pri, h) in record.reg.compact() {
            cmp.push((*ev, *pri, Arc::clone(h)));
        }
        for (pri, h) in record.reg.post_tool_use_failure() {
            ptuf.push((*pri, Arc::clone(h)));
        }
        for registration in record.reg.continue_after_stop() {
            cas.push((
                registration.priority,
                record.id.clone(),
                registration.options,
                Arc::clone(&registration.handler),
            ));
        }
        for registration in record.reg.user_message_envelope() {
            ume.push((
                registration.priority,
                record.id.clone(),
                Arc::clone(&registration.handler),
            ));
        }
        for registration in record.reg.after_tool_results() {
            atr.push((
                registration.priority,
                record.id.clone(),
                Arc::clone(&registration.handler),
            ));
        }
        for (ev, mode, pri, h) in record.reg.lifecycle() {
            lc.push((ev.clone(), *pri, record.id.clone(), *mode, Arc::clone(h)));
        }
        // collect 缓存
        tool_metadata.extend(record.reg.all_tool_metadata().clone());
        tool_ui.extend(record.reg.all_tool_ui().clone());
        for (def, handler) in record.reg.tools().iter() {
            static_tools.push((
                def.clone(),
                Arc::clone(handler),
                record.id.clone(),
                record.capabilities.clone(),
            ));
        }
        for discovery in record.reg.tool_discoveries().iter() {
            tool_discoveries.push((
                record.id.clone(),
                Arc::clone(discovery),
                record.capabilities.clone(),
            ));
        }
        for (cmd, handler) in record.reg.commands().iter() {
            static_commands.push((record.id.clone(), cmd.clone(), Arc::clone(handler)));
        }
        for discovery in record.reg.command_discoveries().iter() {
            command_discoveries.push((record.id.clone(), Arc::clone(discovery)));
        }
        for kb in record.reg.keybindings() {
            keybindings.push(kb.clone());
        }
        for item in record.reg.status_items() {
            status_items.push(item.clone());
        }
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

    pre.sort_by_key(|b| std::cmp::Reverse(b.0));
    post.sort_by_key(|b| std::cmp::Reverse(b.0));
    prov.sort_by_key(|b| std::cmp::Reverse(b.1));
    pb.sort_by_key(|b| std::cmp::Reverse(b.0));
    cmp.sort_by_key(|b| std::cmp::Reverse(b.1));
    ptuf.sort_by_key(|b| std::cmp::Reverse(b.0));
    cas.sort_by_key(|b| std::cmp::Reverse(b.0));
    ume.sort_by_key(|b| std::cmp::Reverse(b.0));
    atr.sort_by_key(|b| std::cmp::Reverse(b.0));
    lc.sort_by_key(|b| std::cmp::Reverse(b.1));

    HandlerIndex {
        pre_tool_use: pre
            .into_iter()
            .map(|(_, id, m, target, h)| (id, m, target, h))
            .collect(),
        post_tool_use: post
            .into_iter()
            .map(|(_, id, m, target, h)| (id, m, target, h))
            .collect(),
        provider: group_by_event_with_mode(prov),
        prompt_build: pb.into_iter().map(|(_, h)| h).collect(),
        compact: group_by_event_plain(cmp),
        post_tool_use_failure: ptuf.into_iter().map(|(_, h)| h).collect(),
        continue_after_stop: cas
            .into_iter()
            .map(|(_, id, options, h)| (id, options, h))
            .collect(),
        user_message_envelope: ume.into_iter().map(|(_, id, h)| (id, h)).collect(),
        after_tool_results: atr.into_iter().map(|(_, id, h)| (id, h)).collect(),
        lifecycle: group_by_event_with_mode(lc),
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

fn group_by_event_with_mode<K, H>(
    mut items: Vec<PrioritizedEventHandler<K, H>>,
) -> HashMap<K, Vec<ExtensionHandler<H>>>
where
    K: std::hash::Hash + Eq,
    H: ?Sized,
{
    let mut map: HashMap<K, Vec<ExtensionHandler<H>>> = HashMap::new();
    for (ev, _, extension_id, mode, h) in items.drain(..) {
        map.entry(ev).or_default().push((extension_id, mode, h));
    }
    map
}

fn group_by_event_plain<K, H>(mut items: Vec<(K, i32, Arc<H>)>) -> HashMap<K, Vec<Arc<H>>>
where
    K: std::hash::Hash + Eq,
    H: ?Sized,
{
    let mut map: HashMap<K, Vec<Arc<H>>> = HashMap::new();
    for (ev, _, h) in items.drain(..) {
        map.entry(ev).or_default().push(h);
    }
    map
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
        for (ev, mode, pri, _) in record.reg.provider() {
            provider.push((id, *ev, *pri, *mode));
        }
        for (pri, _) in record.reg.prompt_build() {
            prompt.push((id, *pri));
        }
        for (ev, pri, _) in record.reg.compact() {
            compact.push((id, *ev, *pri));
        }
        for (ev, mode, pri, _) in record.reg.lifecycle() {
            lifecycle.push((id, ev.clone(), *pri, *mode));
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
