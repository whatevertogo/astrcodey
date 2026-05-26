//! 扩展运行器 — 将生命周期事件分发到已注册的扩展。
//!
//! 负责管理扩展注册、事件分发，并强制执行 HookMode 语义：
//! - Blocking: 同步执行，可返回 Block 或 ModifiedInput/ModifiedResult
//! - NonBlocking: 以即发即弃方式派生任务，使用快照上下文
//! - Advisory: 结果仅记录日志，不强制执行

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use astrcode_core::event::EventPayload;
use astrcode_extension_sdk::{
    extension::*,
    tool::{
        ExecutionMode, SessionOperations, Tool, ToolDefinition, ToolError, ToolExecutionContext,
        ToolResult,
    },
};
use tokio::sync::{Mutex as AsyncMutex, RwLock, mpsc};

/// 将生命周期事件分发到所有已注册的扩展。
///
/// 强制执行 HookMode 语义：
/// - Blocking: 同步执行，可返回 Block 或 ModifiedInput/ModifiedResult
/// - NonBlocking: 以即发即弃方式派生任务，使用快照上下文
/// - Advisory: 结果仅记录日志，不强制执行
pub struct ExtensionRunner {
    /// 串行化注册/注销，避免同一扩展并发 start/stop。
    lifecycle_lock: AsyncMutex<()>,
    /// 已注册的扩展列表（读写锁保护）
    extensions: RwLock<Vec<Arc<dyn Extension>>>,
    /// 从 register() 收集的类型化能力记录
    records: RwLock<Vec<ExtensionRecord>>,
    /// 预计算的 handler 索引，注册时重建，分发时直接查表
    index: parking_lot::RwLock<Arc<HandlerIndex>>,
    /// 会话原子操作能力（在 bind_session_ops() 调用前为 None）
    session_ops: Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>>,
    /// 每个扩展的宿主管理任务集合。
    extension_tasks: RwLock<HashMap<String, ExtensionTasks>>,
    /// 钩子执行超时时间
    timeout: Duration,
    /// 扩展专有配置映射。key 为扩展 id，value 为用户配置的 JSON。
    /// 通过 `update_extension_configs()` 替换，支持热更新。
    /// 使用 parking_lot::RwLock 以便在同步上下文中替换（不需要 async）。
    extension_configs: parking_lot::RwLock<BTreeMap<String, serde_json::Value>>,
    /// 扩展 `start()` 阶段发送自定义事件的宿主通道。
    startup_event_tx: parking_lot::RwLock<Option<mpsc::UnboundedSender<EventPayload>>>,
    /// 统一注入给 bundled extension 的宿主运行态服务。
    host_services: parking_lot::RwLock<Option<Arc<ExtensionHostServices>>>,
}

/// 从 `register()` 调用中收集的扩展能力记录。
struct ExtensionRecord {
    id: String,
    reg: Registrar,
    capabilities: Vec<ExtensionCapability>,
    /// 注册时的配置快照，用于 diff 检测热更新。
    config: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct RegisteredSlashCommand {
    pub extension_id: String,
    pub command: astrcode_extension_sdk::extension::SlashCommand,
}

/// 一次主动健康检查的扩展级结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionHealthReport {
    pub extension_id: String,
    pub error: Option<String>,
}

impl ExtensionHealthReport {
    pub fn is_healthy(&self) -> bool {
        self.error.is_none()
    }
}

// ─── BoundExtensionEventSink ──────────────────────────────────────────────

/// 绑定了 extension_id 和声明校验的事件发射器。
///
/// 由 `ExtensionRunner::make_extension_event_sink` 构造，传给扩展钩子上下文。
/// `extension_id` 在构造时注入，调用方无法伪造身份。
///
/// TODO: 补单元测试覆盖校验逻辑——未声明的 event_type、schema_version 超限、
/// payload 超过 max_payload_bytes、正常发射路径。
struct BoundExtensionEventSink {
    extension_id: String,
    declarations: HashMap<String, ExtensionEventDecl>,
    event_tx: mpsc::UnboundedSender<EventPayload>,
}

fn bind_extension_event_sink(
    extension_id: &str,
    declarations: &[ExtensionEventDecl],
    event_tx: mpsc::UnboundedSender<EventPayload>,
) -> Option<Arc<dyn ExtensionEventSink>> {
    if declarations.is_empty() {
        return None;
    }
    let declarations = declarations
        .iter()
        .map(|decl| (decl.event_type.clone(), decl.clone()))
        .collect();
    Some(Arc::new(BoundExtensionEventSink {
        extension_id: extension_id.to_owned(),
        declarations,
        event_tx,
    }))
}

fn attach_extension_event_sink(
    index: &HandlerIndex,
    extension_id: &str,
    event_tx: &Option<mpsc::UnboundedSender<EventPayload>>,
) -> Option<Arc<dyn ExtensionEventSink>> {
    if !index.allows(extension_id, ExtensionCapability::EmitEvents) {
        return None;
    }
    let tx = event_tx.as_ref()?;
    let decls = index.extension_event_decls.get(extension_id)?;
    bind_extension_event_sink(extension_id, decls, tx.clone())
}

#[async_trait::async_trait]
impl ExtensionEventSink for BoundExtensionEventSink {
    async fn emit(
        &self,
        event_type: &str,
        schema_version: u32,
        payload: serde_json::Value,
    ) -> Result<(), ExtensionError> {
        crate::host_router::emit_for_sink(
            &self.extension_id,
            &self.declarations,
            &self.event_tx,
            event_type,
            schema_version,
            payload,
        )
    }
}

// ─── Handler Index ──────────────────────────────────────────────────────

type ExtensionHandler<H> = (String, HookMode, Arc<H>);
type PrioritizedHandler<H> = (i32, String, HookMode, Arc<H>);
type PrioritizedEventHandler<K, H> = (K, i32, String, HookMode, Arc<H>);

/// 预排序的 handler 索引。
///
/// 在每次 `register()` 后从所有 records 重建，确保分发时无需遍历+排序。
/// 各列表按 priority 降序排列，provider/compact/lifecycle 按 event 分组。
#[allow(clippy::type_complexity)]
struct HandlerIndex {
    pre_tool_use: Vec<ExtensionHandler<dyn PreToolUseHandler>>,
    post_tool_use: Vec<ExtensionHandler<dyn PostToolUseHandler>>,
    provider: HashMap<ProviderEvent, Vec<ExtensionHandler<dyn ProviderHandler>>>,
    prompt_build: Vec<Arc<dyn PromptBuildHandler>>,
    compact: HashMap<CompactEvent, Vec<Arc<dyn CompactHandler>>>,
    post_tool_use_failure: Vec<Arc<dyn PostToolUseFailureHandler>>,
    lifecycle: HashMap<ExtensionEvent, Vec<ExtensionHandler<dyn LifecycleHandler>>>,
    // 预计算的 collect 缓存
    tool_metadata:
        std::collections::HashMap<String, astrcode_extension_sdk::tool::ToolPromptMetadata>,
    static_tools: Vec<(
        ToolDefinition,
        Arc<dyn ToolHandler>,
        String,
        Vec<ExtensionCapability>,
    )>,
    tool_discoveries: Vec<(
        String,
        Arc<dyn ToolDiscoveryHandler>,
        Vec<ExtensionCapability>,
    )>,
    static_commands: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)>,
    command_discoveries: Vec<(String, Arc<dyn CommandDiscoveryHandler>)>,
    keybindings: Vec<astrcode_extension_sdk::extension::Keybinding>,
    status_items: Vec<astrcode_extension_sdk::extension::StatusItem>,
    extension_event_decls: HashMap<String, Vec<ExtensionEventDecl>>,
    extension_data_dir_extensions: std::collections::HashSet<String>,
    capabilities: HashMap<String, Vec<ExtensionCapability>>,
}

impl HandlerIndex {
    fn allows(&self, extension_id: &str, capability: ExtensionCapability) -> bool {
        self.capabilities
            .get(extension_id)
            .is_some_and(|capabilities| capabilities.contains(&capability))
    }
}

fn build_handler_index(records: &[ExtensionRecord]) -> HandlerIndex {
    let mut pre: Vec<PrioritizedHandler<dyn PreToolUseHandler>> = Vec::new();
    let mut post: Vec<PrioritizedHandler<dyn PostToolUseHandler>> = Vec::new();
    let mut prov: Vec<PrioritizedEventHandler<ProviderEvent, dyn ProviderHandler>> = Vec::new();
    let mut pb: Vec<(i32, Arc<dyn PromptBuildHandler>)> = Vec::new();
    let mut cmp: Vec<(CompactEvent, i32, Arc<dyn CompactHandler>)> = Vec::new();
    let mut ptuf: Vec<(i32, Arc<dyn PostToolUseFailureHandler>)> = Vec::new();
    let mut lc: Vec<PrioritizedEventHandler<ExtensionEvent, dyn LifecycleHandler>> = Vec::new();
    let mut tool_metadata = std::collections::HashMap::new();
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
    let mut keybindings: Vec<astrcode_extension_sdk::extension::Keybinding> = Vec::new();
    let mut status_items: Vec<astrcode_extension_sdk::extension::StatusItem> = Vec::new();
    let mut extension_event_decls: HashMap<String, Vec<ExtensionEventDecl>> = HashMap::new();
    let mut extension_data_dir_extensions: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut capabilities = HashMap::new();

    for record in records {
        capabilities.insert(record.id.clone(), record.capabilities.clone());
        for (mode, pri, h) in record.reg.pre_tool_use() {
            pre.push((*pri, record.id.clone(), *mode, Arc::clone(h)));
        }
        for (mode, pri, h) in record.reg.post_tool_use() {
            post.push((*pri, record.id.clone(), *mode, Arc::clone(h)));
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
        for (ev, mode, pri, h) in record.reg.lifecycle() {
            lc.push((ev.clone(), *pri, record.id.clone(), *mode, Arc::clone(h)));
        }
        // collect 缓存
        tool_metadata.extend(record.reg.all_tool_metadata().clone());
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
    }

    pre.sort_by_key(|b| std::cmp::Reverse(b.0));
    post.sort_by_key(|b| std::cmp::Reverse(b.0));
    prov.sort_by_key(|b| std::cmp::Reverse(b.1));
    pb.sort_by_key(|b| std::cmp::Reverse(b.0));
    cmp.sort_by_key(|b| std::cmp::Reverse(b.1));
    ptuf.sort_by_key(|b| std::cmp::Reverse(b.0));
    lc.sort_by_key(|b| std::cmp::Reverse(b.1));

    HandlerIndex {
        pre_tool_use: pre.into_iter().map(|(_, id, m, h)| (id, m, h)).collect(),
        post_tool_use: post.into_iter().map(|(_, id, m, h)| (id, m, h)).collect(),
        provider: group_by_event_with_mode(prov),
        prompt_build: pb.into_iter().map(|(_, h)| h).collect(),
        compact: group_by_event_plain(cmp),
        post_tool_use_failure: ptuf.into_iter().map(|(_, h)| h).collect(),
        lifecycle: group_by_event_with_mode(lc),
        tool_metadata,
        static_tools,
        tool_discoveries,
        static_commands,
        command_discoveries,
        keybindings,
        status_items,
        extension_event_decls,
        extension_data_dir_extensions,
        capabilities,
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
fn log_handler_dispatch_order(records: &[ExtensionRecord]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }

    let mut pre: Vec<(&str, i32, HookMode)> = Vec::new();
    let mut post: Vec<(&str, i32, HookMode)> = Vec::new();
    let mut provider: Vec<(&str, ProviderEvent, i32, HookMode)> = Vec::new();
    let mut prompt: Vec<(&str, i32)> = Vec::new();
    let mut compact: Vec<(&str, CompactEvent, i32)> = Vec::new();
    let mut lifecycle: Vec<(&str, ExtensionEvent, i32, HookMode)> = Vec::new();

    for record in records {
        let id = record.id.as_str();
        for (mode, pri, _) in record.reg.pre_tool_use() {
            pre.push((id, *pri, *mode));
        }
        for (mode, pri, _) in record.reg.post_tool_use() {
            post.push((id, *pri, *mode));
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

// ─── ExtensionRunner impl ───────────────────────────────────────────────

impl ExtensionRunner {
    /// 创建新的扩展运行器。
    pub fn new(timeout: Duration) -> Self {
        Self {
            lifecycle_lock: AsyncMutex::new(()),
            extensions: RwLock::new(Vec::new()),
            records: RwLock::new(Vec::new()),
            index: parking_lot::RwLock::new(Arc::new(HandlerIndex {
                pre_tool_use: Vec::new(),
                post_tool_use: Vec::new(),
                provider: HashMap::new(),
                prompt_build: Vec::new(),
                compact: HashMap::new(),
                post_tool_use_failure: Vec::new(),
                lifecycle: HashMap::new(),
                tool_metadata: std::collections::HashMap::new(),
                static_tools: Vec::new(),
                tool_discoveries: Vec::new(),
                static_commands: Vec::new(),
                command_discoveries: Vec::new(),
                keybindings: Vec::new(),
                status_items: Vec::new(),
                extension_event_decls: HashMap::new(),
                extension_data_dir_extensions: std::collections::HashSet::new(),
                capabilities: HashMap::new(),
            })),
            session_ops: Arc::new(StdRwLock::new(None)),
            extension_tasks: RwLock::new(HashMap::new()),
            timeout,
            extension_configs: parking_lot::RwLock::new(BTreeMap::new()),
            startup_event_tx: parking_lot::RwLock::new(None),
            host_services: parking_lot::RwLock::new(None),
        }
    }

    /// 注册一个扩展。
    pub async fn register(&self, ext: Arc<dyn Extension>) -> Result<bool, ExtensionError> {
        self.register_with_startup_working_dir(ext, None).await
    }

    /// 注册扩展，并向 `start()` 传递宿主启动时已知的项目目录。
    pub async fn register_with_startup_working_dir(
        &self,
        ext: Arc<dyn Extension>,
        startup_working_dir: Option<&str>,
    ) -> Result<bool, ExtensionError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let id = ext.id().to_string();
        let capabilities = ext.capabilities().to_vec();

        if self.extensions.read().await.iter().any(|e| e.id() == id) {
            tracing::warn!(extension_id = %id, "extension already registered, skipping duplicate");
            return Ok(false);
        }

        // register() 只收集声明；start() 才进入运行态。
        let mut reg = Registrar::new();
        ext.register(&mut reg);
        if reg.needs_extension_data_dir() {
            let dir = astrcode_support::hostpaths::extensions_data_dir(&id);
            std::fs::create_dir_all(&dir).map_err(|e| {
                ExtensionError::Internal(format!("failed to create extension data dir: {e}"))
            })?;
        }

        let tasks = ExtensionTasks::new(id.clone());

        // 查找该扩展的专有配置，回退到空对象
        let ext_config = self
            .extension_configs
            .read()
            .get(&id)
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

        let event_sink =
            self.startup_event_tx.read().as_ref().and_then(|tx| {
                bind_extension_event_sink(&id, reg.extension_event_decls(), tx.clone())
            });
        let needs_host_services = capabilities.contains(&ExtensionCapability::SessionHistory)
            || capabilities.contains(&ExtensionCapability::SmallModel);
        let host_services = needs_host_services
            .then(|| {
                self.host_services.read().as_ref().map(|services| {
                    Arc::new(ExtensionHostServices {
                        session_read: capabilities
                            .contains(&ExtensionCapability::SessionHistory)
                            .then(|| services.session_read.clone())
                            .flatten(),
                        small_llm: capabilities
                            .contains(&ExtensionCapability::SmallModel)
                            .then(|| services.small_llm.clone())
                            .flatten(),
                    })
                })
            })
            .flatten();
        let ctx = ExtensionCtx::with_host_services(
            tasks.clone(),
            ExtensionConfig(ext_config.clone()),
            startup_working_dir.map(str::to_string),
            event_sink,
            host_services,
        );
        ext.start(ctx).await?;

        self.extensions.write().await.push(ext);
        self.extension_tasks.write().await.insert(id.clone(), tasks);

        if !reg.is_empty() {
            let mut records = self.records.write().await;
            records.push(ExtensionRecord {
                id: id.clone(),
                reg,
                capabilities,
                config: ext_config,
            });
            log_handler_dispatch_order(&records);
            let new_index = Arc::new(build_handler_index(&records));
            self.ensure_extensions_data_dir_dirs(&new_index);
            *self.index.write() = new_index;
        }

        Ok(true)
    }

    /// 注销一个扩展，并重建分发表。
    ///
    /// 返回是否真的移除了该扩展。
    pub async fn unregister(
        &self,
        extension_id: &str,
        reason: StopReason,
    ) -> Result<bool, ExtensionError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let mut exts = self.extensions.write().await;
        let Some(pos) = exts.iter().position(|ext| ext.id() == extension_id) else {
            return Ok(false);
        };
        let ext = exts.remove(pos);
        drop(exts);

        let mut records = self.records.write().await;
        records.retain(|record| record.id != extension_id);
        log_handler_dispatch_order(&records);
        let new_index = Arc::new(build_handler_index(&records));
        self.ensure_extensions_data_dir_dirs(&new_index);
        *self.index.write() = new_index;
        drop(records);

        let tasks = self.extension_tasks.write().await.remove(extension_id);
        if let Some(tasks) = &tasks {
            tasks.cancel();
        }
        if let Some(tasks) = tasks {
            tasks.wait(self.timeout).await;
        }
        let stop_result = ext.stop(reason).await;
        stop_result?;
        Ok(true)
    }

    /// 停止所有已注册扩展。用于宿主进程关闭。
    pub async fn shutdown(&self) -> Vec<String> {
        let ids = self.registered_extension_ids().await;
        let mut errors = Vec::new();
        for id in ids {
            if let Err(e) = self.unregister(&id, StopReason::Shutdown).await {
                errors.push(format!("failed to stop extension {id}: {e}"));
            }
        }
        errors
    }

    /// 返回当前已注册扩展的 id 列表。
    pub async fn registered_extension_ids(&self) -> Vec<String> {
        self.extensions
            .read()
            .await
            .iter()
            .map(|ext| ext.id().to_string())
            .collect()
    }

    fn ensure_extensions_data_dir_dirs(&self, index: &HandlerIndex) {
        for extension_id in &index.extension_data_dir_extensions {
            let dir = astrcode_support::hostpaths::extensions_data_dir(extension_id);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(extension_id = %extension_id, error = %e, "failed to create extension data dir");
            }
        }
    }

    /// 绑定会话原子操作能力。
    pub fn bind_session_ops(&self, ops: Arc<dyn SessionOperations>) {
        *self.session_ops.write().unwrap_or_else(|e| e.into_inner()) = Some(ops);
    }

    /// 绑定扩展在标准 `start()` 生命周期中可取得的宿主服务。
    pub fn bind_host_services(&self, services: Arc<ExtensionHostServices>) {
        *self.host_services.write() = Some(services);
    }

    /// 获取共享的 session_ops 引用（供 HandlerTool 使用）。
    pub fn session_ops_ref(&self) -> Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>> {
        Arc::clone(&self.session_ops)
    }

    /// 原子替换所有扩展的专有配置映射。
    ///
    /// 新注册的扩展将使用新配置；已注册的扩展需调用
    /// [`notify_config_changed`] 来更新运行态实例。
    pub fn update_extension_configs(&self, configs: BTreeMap<String, serde_json::Value>) {
        *self.extension_configs.write() = configs;
    }

    /// 通知所有已注册扩展其配置已变更。
    ///
    /// 将当前 `extension_configs` 与各 `ExtensionRecord` 中保存的快照做 diff，
    /// 仅在有变化时调用 `ext.on_config_changed()`。
    /// 返回每个扩展的 notify 结果（仅记录错误，不中断）。
    pub async fn notify_config_changed(&self) -> Vec<String> {
        let current_configs = self.extension_configs.read().clone();
        let mut records = self.records.write().await;
        let mut errors = Vec::new();

        for record in records.iter_mut() {
            let new_config = current_configs
                .get(&record.id)
                .cloned()
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

            if record.config == new_config {
                continue;
            }

            let ext = {
                let extensions = self.extensions.read().await;
                extensions
                    .iter()
                    .find(|e| e.id() == record.id)
                    .map(Arc::clone)
            };

            if let Some(ext) = ext {
                if let Err(e) = ext
                    .on_config_changed(ExtensionConfig(new_config.clone()))
                    .await
                {
                    errors.push(format!(
                        "config changed handler failed for {}: {e}",
                        record.id
                    ));
                } else {
                    record.config = new_config;
                }
            }
        }

        errors
    }

    pub async fn count(&self) -> usize {
        self.extensions.read().await.len()
    }

    /// 为后续启动的扩展绑定启动阶段自定义事件通道。
    ///
    /// 该通道不属于某个 session；宿主负责决定如何消费这些进程级事件。
    pub fn bind_startup_event_channel(&self, event_tx: mpsc::UnboundedSender<EventPayload>) {
        *self.startup_event_tx.write() = Some(event_tx);
    }

    /// 主动采样已运行扩展的健康状态，不创建后台轮询任务。
    pub async fn check_health(&self) -> Vec<ExtensionHealthReport> {
        let extensions = self.extensions.read().await.clone();
        let mut reports = Vec::with_capacity(extensions.len());
        for extension in extensions {
            let extension_id = extension.id().to_string();
            let error = match tokio::time::timeout(self.timeout, extension.health()).await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => {
                    Some(ExtensionError::Timeout(self.timeout.as_millis() as u64).to_string())
                },
            };
            reports.push(ExtensionHealthReport {
                extension_id,
                error,
            });
        }
        reports
    }

    fn load_index(&self) -> Arc<HandlerIndex> {
        Arc::clone(&self.index.read())
    }

    async fn spawn_extension_task<F>(&self, extension_id: &str, task_name: &'static str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let tasks = self.extension_tasks.read().await.get(extension_id).cloned();
        if let Some(tasks) = tasks {
            tasks.spawn(task_name, fut);
        } else {
            tracing::debug!(
                extension_id,
                task = task_name,
                "skip spawning task for stopped extension"
            );
        }
    }

    // ─── 类型化分发方法 ──────────────────────────────────────────────

    /// PreToolUse 钩子分发。
    pub async fn emit_pre_tool_use(
        &self,
        ctx: PreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
        let index = self.load_index();
        let mut ctx = ctx;
        let mut modified = false;

        for (extension_id, mode, handler) in &index.pre_tool_use {
            let mut handler_ctx = ctx.clone();
            if !index.allows(extension_id, ExtensionCapability::SessionState) {
                handler_ctx.session_store_dir = None;
            }
            handler_ctx.extension_event_sink =
                attach_extension_event_sink(&index, extension_id, &ctx.event_tx);
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(handler_ctx))
                        .await
                        .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;
                    match result {
                        PreToolUseResult::Block { reason } => {
                            return Ok(PreToolUseResult::Block { reason });
                        },
                        PreToolUseResult::ModifyInput { tool_input } => {
                            ctx = PreToolUseContext { tool_input, ..ctx };
                            modified = true;
                        },
                        PreToolUseResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = handler.handle(handler_ctx).await {
                        tracing::warn!(error = %e, "advisory pre_tool_use handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let handler = Arc::clone(handler);
                    self.spawn_extension_task(extension_id, "pre_tool_use", async move {
                        if let Err(e) = handler.handle(handler_ctx).await {
                            tracing::warn!(error = %e, "non-blocking pre_tool_use handler failed");
                        }
                    })
                    .await;
                },
            }
        }
        if modified {
            Ok(PreToolUseResult::ModifyInput {
                tool_input: ctx.tool_input,
            })
        } else {
            Ok(PreToolUseResult::Allow)
        }
    }

    /// PostToolUse 钩子分发。
    pub async fn emit_post_tool_use(
        &self,
        ctx: PostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        let index = self.load_index();
        let mut ctx = ctx;
        let mut modified = false;

        for (extension_id, mode, handler) in &index.post_tool_use {
            let mut handler_ctx = ctx.clone();
            if !index.allows(extension_id, ExtensionCapability::SessionState) {
                handler_ctx.session_store_dir = None;
            }
            handler_ctx.extension_event_sink =
                attach_extension_event_sink(&index, extension_id, &ctx.event_tx);
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(handler_ctx))
                        .await
                        .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;
                    match result {
                        PostToolUseResult::Block { reason } => {
                            return Ok(PostToolUseResult::Block { reason });
                        },
                        PostToolUseResult::ModifyResult { content } => {
                            let is_error = ctx.tool_result.is_error;
                            if is_error {
                                ctx.tool_result.error = Some(content.clone());
                                ctx.tool_result.content = content;
                            } else {
                                ctx.tool_result.content = content;
                            }
                            modified = true;
                        },
                        PostToolUseResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = handler.handle(handler_ctx).await {
                        tracing::warn!(error = %e, "advisory post_tool_use handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let handler = Arc::clone(handler);
                    self.spawn_extension_task(extension_id, "post_tool_use", async move {
                        if let Err(e) = handler.handle(handler_ctx).await {
                            tracing::warn!(error = %e, "non-blocking post_tool_use handler failed");
                        }
                    })
                    .await;
                },
            }
        }
        if modified {
            Ok(PostToolUseResult::ModifyResult {
                content: ctx.tool_result.content,
            })
        } else {
            Ok(PostToolUseResult::Allow)
        }
    }

    /// Provider 钩子分发。
    pub async fn emit_provider(
        &self,
        event: ProviderEvent,
        ctx: ProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        let index = self.load_index();
        let handlers = index.provider.get(&event);

        let Some(handlers) = handlers else {
            return Ok(ProviderResult::Allow);
        };

        let mut ctx = ctx;
        let mut modified = false;
        for (extension_id, mode, handler) in handlers {
            let mut handler_ctx = ctx.clone();
            if !index.allows(extension_id, ExtensionCapability::SessionState) {
                handler_ctx.session_store_dir = None;
            }
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(handler_ctx))
                        .await
                        .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;
                    match result {
                        ProviderResult::Block { reason } => {
                            return Ok(ProviderResult::Block { reason });
                        },
                        ProviderResult::ReplaceMessages { messages } => {
                            ctx = ProviderContext { messages, ..ctx };
                            modified = true;
                        },
                        ProviderResult::AppendMessages { messages } => {
                            let mut new_messages = ctx.messages;
                            new_messages.extend(messages);
                            ctx = ProviderContext {
                                messages: new_messages,
                                ..ctx
                            };
                            modified = true;
                        },
                        ProviderResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = handler.handle(handler_ctx).await {
                        tracing::warn!(error = %e, "advisory provider handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let handler = Arc::clone(handler);
                    self.spawn_extension_task(extension_id, "provider", async move {
                        if let Err(e) = handler.handle(handler_ctx).await {
                            tracing::warn!(error = %e, "non-blocking provider handler failed");
                        }
                    })
                    .await;
                },
            }
        }
        if modified {
            Ok(ProviderResult::ReplaceMessages {
                messages: ctx.messages,
            })
        } else {
            Ok(ProviderResult::Allow)
        }
    }

    /// PromptBuild 贡献收集。
    pub async fn collect_prompt_contributions_typed(
        &self,
        ctx: PromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        let index = self.load_index();

        let mut collected = PromptContributions::default();
        for handler in &index.prompt_build {
            let contributions = tokio::time::timeout(self.timeout, handler.handle(ctx.clone()))
                .await
                .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;
            collected.merge(contributions);
        }
        Ok(collected)
    }

    /// Compact 钩子分发。
    pub async fn emit_compact(
        &self,
        event: CompactEvent,
        ctx: CompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        let index = self.load_index();
        let handlers = index.compact.get(&event);

        let Some(handlers) = handlers else {
            return Ok(CompactResult::Allow);
        };

        let mut collected = CompactContributions::default();
        for handler in handlers {
            let result = tokio::time::timeout(self.timeout, handler.handle(ctx.clone()))
                .await
                .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;
            match result {
                CompactResult::Block { reason } => {
                    return Ok(CompactResult::Block { reason });
                },
                CompactResult::Contributions(c) => {
                    collected.merge(c);
                },
                CompactResult::Allow => {},
            }
        }
        if collected.instructions.is_empty() {
            Ok(CompactResult::Allow)
        } else {
            Ok(CompactResult::Contributions(collected))
        }
    }

    /// PostToolUseFailure 通知型钩子分发。
    pub async fn emit_post_tool_use_failure(&self, ctx: PostToolUseFailureContext) {
        let index = self.load_index();

        for handler in &index.post_tool_use_failure {
            match tokio::time::timeout(self.timeout, handler.handle(ctx.clone())).await {
                Ok(Ok(())) => {},
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "post tool use failure handler failed");
                },
                Err(_) => {
                    tracing::warn!("post tool use failure handler timed out");
                },
            }
        }
    }

    /// 通用生命周期事件分发。
    ///
    /// `HookResult::Block` 转换成 `Err(ExtensionError::Blocked)` 返回，让调用方
    /// 的 `?` 正常传播——历史上 callers 拿到 `Ok(Block)` 后没人 match，导致 Block
    /// 形同虚设。这条转换让 lifecycle 的 Block 与 `PreToolUse::Block` 语义对齐：
    /// 都是「显式拦截」，调用方拿到 ExtensionError 后决定中止/降级。
    pub async fn emit_lifecycle(
        &self,
        event: ExtensionEvent,
        ctx: LifecycleContext,
    ) -> Result<(), ExtensionError> {
        let index = self.load_index();
        let Some(handlers) = index.lifecycle.get(&event) else {
            return Ok(());
        };

        for (extension_id, mode, handler) in handlers {
            let mut handler_ctx = ctx.clone();
            handler_ctx.extension_event_sink =
                attach_extension_event_sink(&index, extension_id, &ctx.event_tx);
            match mode {
                HookMode::Blocking => {
                    let result =
                        tokio::time::timeout(self.timeout, handler.handle(handler_ctx.clone()))
                            .await
                            .map_err(|_| {
                                ExtensionError::Timeout(self.timeout.as_millis() as u64)
                            })??;
                    if let HookResult::Block { reason } = result {
                        return Err(ExtensionError::Blocked { reason });
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = handler.handle(handler_ctx).await {
                        tracing::warn!(error = %e, "advisory lifecycle handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let handler = Arc::clone(handler);
                    self.spawn_extension_task(extension_id, "lifecycle", async move {
                        if let Err(e) = handler.handle(handler_ctx).await {
                            tracing::warn!(error = %e, "non-blocking lifecycle handler failed");
                        }
                    })
                    .await;
                },
            }
        }
        Ok(())
    }

    // ─── 收集方法（仍从 records 读取，注册时不变） ──────────────────

    /// 从 HandlerIndex 缓存收集工具适配器。
    pub async fn collect_tool_adapters_typed(&self, working_dir: &str) -> Vec<Arc<dyn Tool>> {
        let index = self.load_index();
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for (def, handler, ext_id, capabilities) in &index.static_tools {
            let prompt_metadata = index.tool_metadata.get(&def.name).cloned();
            tools.push(Arc::new(HandlerTool {
                definition: def.clone(),
                handler: Arc::clone(handler),
                prompt_metadata,
                working_dir: working_dir.to_string(),
                extension_id: ext_id.clone(),
                capabilities: capabilities.clone(),
                event_declarations: index
                    .extension_event_decls
                    .get(ext_id)
                    .cloned()
                    .unwrap_or_default(),
            }));
        }
        for (ext_id, discovery, capabilities) in &index.tool_discoveries {
            match tokio::time::timeout(self.timeout, discovery.discover(working_dir)).await {
                Ok(discovered) => {
                    for discovered_tool in discovered {
                        tools.push(Arc::new(HandlerTool {
                            definition: discovered_tool.definition,
                            handler: discovered_tool.handler,
                            prompt_metadata: discovered_tool.prompt_metadata,
                            working_dir: working_dir.to_string(),
                            extension_id: ext_id.clone(),
                            capabilities: capabilities.clone(),
                            event_declarations: index
                                .extension_event_decls
                                .get(ext_id)
                                .cloned()
                                .unwrap_or_default(),
                        }));
                    }
                },
                Err(_) => {
                    tracing::warn!("tool discovery timed out");
                },
            }
        }
        tools
    }

    /// 从 HandlerIndex 缓存收集工具提示词元数据。
    pub async fn collect_tool_prompt_metadata_typed(
        &self,
    ) -> std::collections::HashMap<String, astrcode_extension_sdk::tool::ToolPromptMetadata> {
        self.load_index().tool_metadata.clone()
    }

    /// 收集所有插件注册的快捷键绑定。
    pub fn collect_keybindings(&self) -> Vec<astrcode_extension_sdk::extension::Keybinding> {
        self.load_index().keybindings.clone()
    }

    /// 收集所有插件注册的状态栏项。
    pub fn collect_status_items(&self) -> Vec<astrcode_extension_sdk::extension::StatusItem> {
        self.load_index().status_items.clone()
    }

    /// 为指定插件构造绑定身份的事件发射器。
    ///
    /// 返回 `None` 表示该插件未声明任何 extension event type。
    pub fn make_extension_event_sink(
        &self,
        extension_id: &str,
        event_tx: mpsc::UnboundedSender<EventPayload>,
    ) -> Option<Arc<dyn ExtensionEventSink>> {
        let index = self.load_index();
        let decls = index.extension_event_decls.get(extension_id)?;
        bind_extension_event_sink(extension_id, decls, event_tx)
    }

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

    /// 命令派发。
    pub async fn dispatch_command_typed(
        &self,
        command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let index = self.load_index();
        let cmds = self.collect_commands_for_typed(working_dir).await;
        let mut matched: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)> = cmds
            .into_iter()
            .filter(|(_, cmd, _)| cmd.name == command_name)
            .collect();
        matched.sort_by_key(|a| std::cmp::Reverse(command_dispatch_priority(&a.0)));

        if let Some((extension_id, _, handler)) = matched.into_iter().next() {
            let mut scoped_ctx = ctx.clone();
            if !index.allows(&extension_id, ExtensionCapability::SessionState) {
                scoped_ctx.session_store_dir = None;
            }
            handler
                .execute(command_name, arguments, working_dir, &scoped_ctx)
                .await
        } else {
            Err(ExtensionError::NotFound(command_name.into()))
        }
    }

    /// 判断是否有任何扩展注册了类型化能力。
    pub async fn has_records(&self) -> bool {
        !self.records.read().await.is_empty()
    }
}

/// Lower value = higher dispatch priority.
fn command_dispatch_priority(extension_id: &str) -> u8 {
    if extension_id == "astrcode-skill" {
        0
    } else {
        1
    }
}

/// 类型化工具适配器，将 `ToolHandler` 包装为 `Tool` trait 实现。
struct HandlerTool {
    definition: ToolDefinition,
    handler: Arc<dyn ToolHandler>,
    prompt_metadata: Option<astrcode_extension_sdk::tool::ToolPromptMetadata>,
    working_dir: String,
    extension_id: String,
    capabilities: Vec<ExtensionCapability>,
    event_declarations: Vec<ExtensionEventDecl>,
}

#[async_trait::async_trait]
impl Tool for HandlerTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.definition.execution_mode
    }

    fn prompt_metadata(&self) -> Option<astrcode_extension_sdk::tool::ToolPromptMetadata> {
        self.prompt_metadata.clone()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let mut ctx = ctx.clone();
        if !self
            .capabilities
            .contains(&ExtensionCapability::SessionState)
        {
            ctx.capabilities.session_store_dir = None;
        }
        if !self
            .capabilities
            .contains(&ExtensionCapability::SessionControl)
        {
            ctx.capabilities.session_ops = None;
        }
        if !self.capabilities.contains(&ExtensionCapability::SmallModel) {
            ctx.capabilities.small_model_id = None;
        }
        ctx.capabilities.extension_event_sink = if self
            .capabilities
            .contains(&ExtensionCapability::EmitEvents)
        {
            ctx.event_tx.clone().and_then(|event_tx| {
                bind_extension_event_sink(&self.extension_id, &self.event_declarations, event_tx)
            })
        } else {
            None
        };
        let mut result = match self
            .handler
            .execute(&self.definition.name, arguments, &self.working_dir, &ctx)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                return Ok(extension_error_result(
                    &self.definition.name,
                    "handler",
                    err,
                ));
            },
        };

        if let Some(outcome_value) = result
            .metadata
            .remove(astrcode_extension_sdk::extension::EXTENSION_TOOL_OUTCOME_KEY)
        {
            match serde_json::from_value::<ExtensionToolOutcome>(outcome_value) {
                Ok(ExtensionToolOutcome::Text { content, is_error }) => {
                    result.content = content;
                    result.is_error = is_error;
                },
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse ExtensionToolOutcome, treating as plain result");
                },
            }
        }

        Ok(result)
    }
}

/// 将 [`ExtensionError`] 转换为结构化的错误 [`ToolResult`]。
fn extension_error_result(tool_name: &str, extension_id: &str, err: ExtensionError) -> ToolResult {
    use astrcode_extension_sdk::tool::tool_metadata;

    let (message, suggestion) = match &err {
        ExtensionError::NotFound(_) => (
            format!("Tool `{tool_name}` is not available."),
            "This tool may have been unregistered. Try `tool_search_tool` to discover available \
             tools, or proceed without it.",
        ),
        ExtensionError::Timeout(ms) => (
            format!("Tool `{tool_name}` timed out after {ms}ms."),
            "The extension is still processing. Try again with a simpler request, or proceed \
             without this tool.",
        ),
        ExtensionError::Blocked { reason } => (
            format!("Tool `{tool_name}` was blocked: {reason}"),
            "A hook policy prevented this. Read the reason and adjust your approach.",
        ),
        ExtensionError::Internal(message) => (
            format!("Tool `{tool_name}` failed: {message}"),
            "Try different arguments or use a builtin tool as an alternative. Do not retry the \
             identical call.",
        ),
    };

    // suggestion 拼进 content 让 LLM 看到——metadata 不会进 LLM prompt。
    let content = format!("{message}\nSuggestion: {suggestion}");

    let mut metadata = tool_metadata([
        ("extensionId", serde_json::json!(extension_id)),
        ("toolName", serde_json::json!(tool_name)),
        ("suggestion", serde_json::json!(suggestion)),
    ]);
    if let ExtensionError::Timeout(ms) = &err {
        metadata.insert("timeoutMs".into(), serde_json::json!(ms));
    }

    ToolResult::text(content, true, metadata)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use astrcode_core::event::EventPayload;
    use astrcode_extension_sdk::{
        extension::{
            Extension, ExtensionCapability, ExtensionCtx, ExtensionError, Registrar, StopReason,
            ToolHandler,
        },
        tool::{
            ExecutionMode, ToolCapabilities, ToolDefinition, ToolExecutionContext, ToolOrigin,
            ToolResult,
        },
    };
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::ExtensionRunner;

    struct ManagedTaskExtension {
        started: Arc<AtomicUsize>,
        stopped: Arc<AtomicUsize>,
        task_stopped: Arc<AtomicBool>,
        expected_reason: StopReason,
    }

    struct StartupDirectoryExtension {
        received: Arc<Mutex<Option<String>>>,
    }

    struct StartupEventExtension;

    struct UnhealthyExtension;

    struct StateProbeExtension {
        allowed: bool,
    }

    struct StateProbeTool;

    struct SmallModelProbeExtension {
        small_model_allowed: bool,
        session_control_allowed: bool,
    }

    struct SmallModelProbeTool;

    #[async_trait::async_trait]
    impl Extension for StateProbeExtension {
        fn id(&self) -> &str {
            "state-probe"
        }

        fn capabilities(&self) -> &[ExtensionCapability] {
            if self.allowed {
                &[ExtensionCapability::SessionState]
            } else {
                &[]
            }
        }

        fn register(&self, reg: &mut Registrar) {
            reg.tool(
                ToolDefinition {
                    name: "stateProbe".into(),
                    description: String::new(),
                    parameters: json!({"type": "object"}),
                    origin: ToolOrigin::Extension,
                    execution_mode: ExecutionMode::Sequential,
                },
                Arc::new(StateProbeTool),
            );
        }
    }

    #[async_trait::async_trait]
    impl ToolHandler for StateProbeTool {
        async fn execute(
            &self,
            _tool_name: &str,
            _arguments: serde_json::Value,
            _working_dir: &str,
            ctx: &ToolExecutionContext,
        ) -> Result<ToolResult, ExtensionError> {
            Ok(ToolResult::text(
                ctx.capabilities.session_store_dir.is_some().to_string(),
                false,
                Default::default(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl Extension for SmallModelProbeExtension {
        fn id(&self) -> &str {
            "small-model-probe"
        }

        fn capabilities(&self) -> &[ExtensionCapability] {
            match (self.small_model_allowed, self.session_control_allowed) {
                (true, true) => &[
                    ExtensionCapability::SmallModel,
                    ExtensionCapability::SessionControl,
                ],
                (true, false) => &[ExtensionCapability::SmallModel],
                (false, true) => &[ExtensionCapability::SessionControl],
                (false, false) => &[],
            }
        }

        fn register(&self, reg: &mut Registrar) {
            reg.tool(
                ToolDefinition {
                    name: "smallModelProbe".into(),
                    description: String::new(),
                    parameters: json!({"type": "object"}),
                    origin: ToolOrigin::Extension,
                    execution_mode: ExecutionMode::Sequential,
                },
                Arc::new(SmallModelProbeTool),
            );
        }
    }

    #[async_trait::async_trait]
    impl ToolHandler for SmallModelProbeTool {
        async fn execute(
            &self,
            _tool_name: &str,
            _arguments: serde_json::Value,
            _working_dir: &str,
            ctx: &ToolExecutionContext,
        ) -> Result<ToolResult, ExtensionError> {
            Ok(ToolResult::text(
                ctx.capabilities.small_model_id.is_some().to_string(),
                false,
                Default::default(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl Extension for StartupDirectoryExtension {
        fn id(&self) -> &str {
            "startup-directory"
        }

        async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
            *self.received.lock().unwrap() = ctx.startup_working_dir().map(str::to_string);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Extension for StartupEventExtension {
        fn id(&self) -> &str {
            "startup-event"
        }

        fn register(&self, reg: &mut Registrar) {
            reg.extension_event("startup_ready").register();
        }

        async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
            let sink = ctx
                .event_sink()
                .ok_or_else(|| ExtensionError::Internal("missing startup event sink".into()))?;
            sink.emit("startup_ready", 1, json!({"ready": true})).await
        }
    }

    #[async_trait::async_trait]
    impl Extension for UnhealthyExtension {
        fn id(&self) -> &str {
            "unhealthy"
        }

        async fn health(&self) -> Result<(), ExtensionError> {
            Err(ExtensionError::Internal("dependency unavailable".into()))
        }
    }

    #[async_trait::async_trait]
    impl Extension for ManagedTaskExtension {
        fn id(&self) -> &str {
            "managed-task"
        }

        fn register(&self, _reg: &mut Registrar) {}

        async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let shutdown = ctx.shutdown();
            let task_stopped = Arc::clone(&self.task_stopped);
            ctx.tasks().spawn("wait-for-stop", async move {
                shutdown.cancelled().await;
                task_stopped.store(true, Ordering::SeqCst);
            });
            Ok(())
        }

        async fn stop(&self, reason: StopReason) -> Result<(), ExtensionError> {
            assert_eq!(reason, self.expected_reason);
            assert!(self.task_stopped.load(Ordering::SeqCst));
            self.stopped.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn unregister_stops_extension_and_managed_tasks() {
        let started = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicUsize::new(0));
        let task_stopped = Arc::new(AtomicBool::new(false));
        let runner = ExtensionRunner::new(Duration::from_secs(1));

        let registered = runner
            .register(Arc::new(ManagedTaskExtension {
                started: Arc::clone(&started),
                stopped: Arc::clone(&stopped),
                task_stopped: Arc::clone(&task_stopped),
                expected_reason: StopReason::Disabled,
            }))
            .await
            .unwrap();
        assert!(registered);

        let unregistered = runner
            .unregister("managed-task", StopReason::Disabled)
            .await
            .unwrap();
        assert!(unregistered);
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert!(task_stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shutdown_stops_all_extensions_with_shutdown_reason() {
        let started = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicUsize::new(0));
        let task_stopped = Arc::new(AtomicBool::new(false));
        let runner = ExtensionRunner::new(Duration::from_secs(1));

        runner
            .register(Arc::new(ManagedTaskExtension {
                started: Arc::clone(&started),
                stopped: Arc::clone(&stopped),
                task_stopped: Arc::clone(&task_stopped),
                expected_reason: StopReason::Shutdown,
            }))
            .await
            .unwrap();

        let errors = runner.shutdown().await;
        assert!(errors.is_empty());
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert!(task_stopped.load(Ordering::SeqCst));
        assert_eq!(runner.count().await, 0);
    }

    #[tokio::test]
    async fn register_passes_startup_working_dir_to_extension() {
        let received = Arc::new(Mutex::new(None));
        let runner = ExtensionRunner::new(Duration::from_secs(1));

        runner
            .register_with_startup_working_dir(
                Arc::new(StartupDirectoryExtension {
                    received: Arc::clone(&received),
                }),
                Some("D:/workspace"),
            )
            .await
            .unwrap();

        assert_eq!(received.lock().unwrap().as_deref(), Some("D:/workspace"));
    }

    #[tokio::test]
    async fn start_can_emit_declared_event_through_bound_startup_channel() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        runner.bind_startup_event_channel(event_tx);

        runner
            .register(Arc::new(StartupEventExtension))
            .await
            .unwrap();

        let event = event_rx.recv().await.unwrap();
        assert!(matches!(
            event,
            EventPayload::ExtensionEvent {
                extension_id,
                event_type,
                schema_version: 1,
                payload,
            } if extension_id == "startup-event"
                && event_type == "startup_ready"
                && payload == json!({"ready": true})
        ));
    }

    #[tokio::test]
    async fn check_health_reports_extension_failure() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        runner.register(Arc::new(UnhealthyExtension)).await.unwrap();

        let reports = runner.check_health().await;

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].extension_id, "unhealthy");
        assert!(!reports[0].is_healthy());
        assert!(
            reports[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("dependency unavailable"))
        );
    }

    #[tokio::test]
    async fn extension_tool_receives_session_state_only_when_declared() {
        for (allowed, expected) in [(false, "false"), (true, "true")] {
            let runner = ExtensionRunner::new(Duration::from_secs(1));
            runner
                .register(Arc::new(StateProbeExtension { allowed }))
                .await
                .unwrap();
            let tool = runner
                .collect_tool_adapters_typed("D:/workspace")
                .await
                .into_iter()
                .next()
                .unwrap();
            let ctx = ToolExecutionContext {
                session_id: "session".into(),
                working_dir: "D:/workspace".into(),
                tool_call_id: None,
                event_tx: None,
                capabilities: ToolCapabilities {
                    session_store_dir: Some("D:/session".into()),
                    ..Default::default()
                },
            };

            let result = tool.execute(json!({}), &ctx).await.unwrap();
            assert_eq!(result.content, expected);
        }
    }

    #[tokio::test]
    async fn extension_tool_receives_small_model_only_when_declared() {
        for (small_model_allowed, session_control_allowed, expected) in [
            (false, false, "false"),
            (true, false, "true"),
            (false, true, "false"),
        ] {
            let runner = ExtensionRunner::new(Duration::from_secs(1));
            runner
                .register(Arc::new(SmallModelProbeExtension {
                    small_model_allowed,
                    session_control_allowed,
                }))
                .await
                .unwrap();
            let tool = runner
                .collect_tool_adapters_typed("D:/workspace")
                .await
                .into_iter()
                .next()
                .unwrap();
            let ctx = ToolExecutionContext {
                session_id: "session".into(),
                working_dir: "D:/workspace".into(),
                tool_call_id: None,
                event_tx: None,
                capabilities: ToolCapabilities {
                    small_model_id: Some("small-model".into()),
                    ..Default::default()
                },
            };

            let result = tool.execute(json!({}), &ctx).await.unwrap();
            assert_eq!(result.content, expected);
        }
    }
}
