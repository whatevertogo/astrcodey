//! 扩展运行器 — 将生命周期事件分发到已注册的扩展。
//!
//! 负责管理扩展注册、事件分发，并强制执行 HookMode 语义：
//! - Blocking: 同步执行，可返回 Block 或 ModifiedInput/ModifiedResult
//! - NonBlocking: 以即发即弃方式派生任务，使用快照上下文
//! - Advisory: 结果仅记录日志，不强制执行

use std::{
    collections::HashMap,
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use astrcode_core::{
    event::EventPayload,
    extension::*,
    tool::{ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolResult},
};
use tokio::sync::{RwLock, mpsc};

use crate::runtime::SessionOperations;

/// 将生命周期事件分发到所有已注册的扩展。
///
/// 强制执行 HookMode 语义：
/// - Blocking: 同步执行，可返回 Block 或 ModifiedInput/ModifiedResult
/// - NonBlocking: 以即发即弃方式派生任务，使用快照上下文
/// - Advisory: 结果仅记录日志，不强制执行
pub struct ExtensionRunner {
    /// 已注册的扩展列表（读写锁保护）
    extensions: RwLock<Vec<Arc<dyn Extension>>>,
    /// 从 register() 收集的类型化能力记录
    records: RwLock<Vec<ExtensionRecord>>,
    /// 预计算的 handler 索引，注册时重建，分发时直接查表
    index: parking_lot::RwLock<Arc<HandlerIndex>>,
    /// 会话原子操作能力（在 bind_session_ops() 调用前为 None）
    session_ops: Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>>,
    /// 钩子执行超时时间
    timeout: Duration,
}

/// 从 `register()` 调用中收集的扩展能力记录。
struct ExtensionRecord {
    id: String,
    reg: Registrar,
}

#[derive(Debug, Clone)]
pub struct RegisteredSlashCommand {
    pub extension_id: String,
    pub command: astrcode_core::extension::SlashCommand,
}

// ─── BoundPluginEventSink ──────────────────────────────────────────────

/// 绑定了 plugin_id 和声明校验的事件发射器。
///
/// 由 `ExtensionRunner::make_plugin_event_sink` 构造，传给扩展钩子上下文。
/// `plugin_id` 在构造时注入，调用方无法伪造身份。
///
/// TODO: 补单元测试覆盖校验逻辑——未声明的 event_type、schema_version 超限、
/// payload 超过 max_payload_bytes、正常发射路径。
struct BoundPluginEventSink {
    plugin_id: String,
    declarations: HashMap<String, PluginEventDecl>,
    event_tx: mpsc::UnboundedSender<EventPayload>,
}

#[async_trait::async_trait]
impl PluginEventSink for BoundPluginEventSink {
    async fn emit(
        &self,
        event_type: &str,
        schema_version: u32,
        payload: serde_json::Value,
    ) -> Result<(), ExtensionError> {
        let decl = self.declarations.get(event_type).ok_or_else(|| {
            ExtensionError::Internal(format!("undeclared plugin event type: {event_type}"))
        })?;

        if schema_version > decl.schema_version {
            return Err(ExtensionError::Internal(format!(
                "schema_version {schema_version} exceeds declared {} for {event_type}",
                decl.schema_version
            )));
        }

        let serialized =
            serde_json::to_string(&payload).map_err(|e| ExtensionError::Internal(e.to_string()))?;
        if serialized.len() > decl.max_payload_bytes {
            return Err(ExtensionError::Internal(format!(
                "payload exceeds {} bytes for {event_type}",
                decl.max_payload_bytes
            )));
        }

        self.event_tx
            .send(EventPayload::PluginEvent {
                plugin_id: self.plugin_id.clone(),
                event_type: event_type.to_owned(),
                schema_version,
                payload,
            })
            .map_err(|_| ExtensionError::Internal("event channel closed".into()))
    }
}

// ─── Handler Index ──────────────────────────────────────────────────────

/// 预排序的 handler 索引。
///
/// 在每次 `register()` 后从所有 records 重建，确保分发时无需遍历+排序。
/// 各列表按 priority 降序排列，provider/compact/lifecycle 按 event 分组。
#[allow(clippy::type_complexity)]
struct HandlerIndex {
    pre_tool_use: Vec<(HookMode, Arc<dyn PreToolUseHandler>)>,
    post_tool_use: Vec<(HookMode, Arc<dyn PostToolUseHandler>)>,
    provider: HashMap<ProviderEvent, Vec<(HookMode, Arc<dyn ProviderHandler>)>>,
    prompt_build: Vec<Arc<dyn PromptBuildHandler>>,
    compact: HashMap<CompactEvent, Vec<Arc<dyn CompactHandler>>>,
    post_tool_use_failure: Vec<Arc<dyn PostToolUseFailureHandler>>,
    lifecycle: HashMap<ExtensionEvent, Vec<(HookMode, Arc<dyn LifecycleHandler>)>>,
    // 预计算的 collect 缓存
    tool_metadata: std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata>,
    static_tools: Vec<(ToolDefinition, Arc<dyn ToolHandler>, String)>,
    tool_discoveries: Vec<(String, Arc<dyn ToolDiscoveryHandler>)>,
    static_commands: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)>,
    command_discoveries: Vec<Arc<dyn CommandDiscoveryHandler>>,
    keybindings: Vec<astrcode_core::extension::Keybinding>,
    status_items: Vec<astrcode_core::extension::StatusItem>,
    plugin_event_decls: HashMap<String, Vec<PluginEventDecl>>,
    plugin_data_dir_plugins: std::collections::HashSet<String>,
}

fn build_handler_index(records: &[ExtensionRecord]) -> HandlerIndex {
    let mut pre: Vec<(i32, HookMode, Arc<dyn PreToolUseHandler>)> = Vec::new();
    let mut post: Vec<(i32, HookMode, Arc<dyn PostToolUseHandler>)> = Vec::new();
    let mut prov: Vec<(ProviderEvent, i32, HookMode, Arc<dyn ProviderHandler>)> = Vec::new();
    let mut pb: Vec<(i32, Arc<dyn PromptBuildHandler>)> = Vec::new();
    let mut cmp: Vec<(CompactEvent, i32, Arc<dyn CompactHandler>)> = Vec::new();
    let mut ptuf: Vec<(i32, Arc<dyn PostToolUseFailureHandler>)> = Vec::new();
    let mut lc: Vec<(ExtensionEvent, i32, HookMode, Arc<dyn LifecycleHandler>)> = Vec::new();
    let mut tool_metadata = std::collections::HashMap::new();
    let mut static_tools: Vec<(ToolDefinition, Arc<dyn ToolHandler>, String)> = Vec::new();
    let mut tool_discoveries: Vec<(String, Arc<dyn ToolDiscoveryHandler>)> = Vec::new();
    let mut static_commands: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)> = Vec::new();
    let mut command_discoveries: Vec<Arc<dyn CommandDiscoveryHandler>> = Vec::new();
    let mut keybindings: Vec<astrcode_core::extension::Keybinding> = Vec::new();
    let mut status_items: Vec<astrcode_core::extension::StatusItem> = Vec::new();
    let mut plugin_event_decls: HashMap<String, Vec<PluginEventDecl>> = HashMap::new();
    let mut plugin_data_dir_plugins: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for record in records {
        for (mode, pri, h) in record.reg.pre_tool_use() {
            pre.push((*pri, *mode, Arc::clone(h)));
        }
        for (mode, pri, h) in record.reg.post_tool_use() {
            post.push((*pri, *mode, Arc::clone(h)));
        }
        for (ev, mode, pri, h) in record.reg.provider() {
            prov.push((*ev, *pri, *mode, Arc::clone(h)));
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
            lc.push((ev.clone(), *pri, *mode, Arc::clone(h)));
        }
        // collect 缓存
        tool_metadata.extend(record.reg.all_tool_metadata().clone());
        for (def, handler) in record.reg.tools().iter() {
            static_tools.push((def.clone(), Arc::clone(handler), record.id.clone()));
        }
        for discovery in record.reg.tool_discoveries().iter() {
            tool_discoveries.push((record.id.clone(), Arc::clone(discovery)));
        }
        for (cmd, handler) in record.reg.commands().iter() {
            static_commands.push((record.id.clone(), cmd.clone(), Arc::clone(handler)));
        }
        for discovery in record.reg.command_discoveries().iter() {
            command_discoveries.push(Arc::clone(discovery));
        }
        for kb in record.reg.keybindings() {
            keybindings.push(kb.clone());
        }
        for item in record.reg.status_items() {
            status_items.push(item.clone());
        }
        if !record.reg.plugin_event_decls().is_empty() {
            plugin_event_decls.insert(record.id.clone(), record.reg.plugin_event_decls().to_vec());
        }
        if record.reg.needs_plugin_data_dir() {
            plugin_data_dir_plugins.insert(record.id.clone());
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
        pre_tool_use: pre.into_iter().map(|(_, m, h)| (m, h)).collect(),
        post_tool_use: post.into_iter().map(|(_, m, h)| (m, h)).collect(),
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
        plugin_event_decls,
        plugin_data_dir_plugins,
    }
}

fn group_by_event_with_mode<K, H>(
    mut items: Vec<(K, i32, HookMode, Arc<H>)>,
) -> HashMap<K, Vec<(HookMode, Arc<H>)>>
where
    K: std::hash::Hash + Eq,
    H: ?Sized,
{
    let mut map: HashMap<K, Vec<(HookMode, Arc<H>)>> = HashMap::new();
    for (ev, _, mode, h) in items.drain(..) {
        map.entry(ev).or_default().push((mode, h));
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
                plugin_event_decls: HashMap::new(),
                plugin_data_dir_plugins: std::collections::HashSet::new(),
            })),
            session_ops: Arc::new(StdRwLock::new(None)),
            timeout,
        }
    }

    /// 注册一个扩展。
    ///
    /// 锁持有顺序：`extensions` → `records` → `index`，全程不释放，确保
    /// 「ext 已加入 extensions」与「records/index 已重建」对外原子可见。
    /// 否则同 sid 并发 register 时可能出现 A 已 push 进 extensions 但还没
    /// 写到 records，B 看到 extensions 重复短路返回，但 A 的 records 永远
    /// 没机会 push 进去。
    pub async fn register(&self, ext: Arc<dyn Extension>) {
        let id = ext.id().to_string();

        // ext.register() 只读扩展自身元数据，不涉及共享状态，在锁外调用。
        let mut reg = Registrar::new();
        ext.register(&mut reg);

        let mut exts = self.extensions.write().await;
        if exts.iter().any(|e| e.id() == id) {
            tracing::warn!(extension_id = %id, "extension already registered, skipping duplicate");
            return;
        }
        exts.push(ext);

        // 立刻在持有 extensions 写锁的同时更新 records/index，让三者保持原子一致。
        // register 是 startup 一次性路径，多持几毫秒锁不影响性能。
        if !reg.is_empty() {
            let mut records = self.records.write().await;
            records.push(ExtensionRecord {
                id: id.clone(),
                reg,
            });
            log_handler_dispatch_order(&records);
            let new_index = Arc::new(build_handler_index(&records));
            self.ensure_plugin_data_dirs(&new_index);
            *self.index.write() = new_index;
        }
    }

    fn ensure_plugin_data_dirs(&self, index: &HandlerIndex) {
        for plugin_id in &index.plugin_data_dir_plugins {
            let dir = astrcode_support::hostpaths::plugin_data_dir(plugin_id);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(plugin_id = %plugin_id, error = %e, "failed to create plugin data dir");
            }
        }
    }

    /// 绑定会话原子操作能力。
    pub fn bind_session_ops(&self, ops: Arc<dyn SessionOperations>) {
        *self.session_ops.write().unwrap_or_else(|e| e.into_inner()) = Some(ops);
    }

    /// 获取共享的 session_ops 引用（供 HandlerTool 使用）。
    pub fn session_ops_ref(&self) -> Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>> {
        Arc::clone(&self.session_ops)
    }

    pub async fn count(&self) -> usize {
        self.extensions.read().await.len()
    }

    fn load_index(&self) -> Arc<HandlerIndex> {
        Arc::clone(&self.index.read())
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

        for (mode, handler) in &index.pre_tool_use {
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(ctx.clone()))
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
                    if let Err(e) = handler.handle(ctx.clone()).await {
                        tracing::warn!(error = %e, "advisory pre_tool_use handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let ctx = ctx.clone();
                    let handler = Arc::clone(handler);
                    spawn_nonblocking(async move {
                        if let Err(e) = handler.handle(ctx).await {
                            tracing::warn!(error = %e, "non-blocking pre_tool_use handler failed");
                        }
                    });
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

        for (mode, handler) in &index.post_tool_use {
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(ctx.clone()))
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
                    if let Err(e) = handler.handle(ctx.clone()).await {
                        tracing::warn!(error = %e, "advisory post_tool_use handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let ctx = ctx.clone();
                    let handler = Arc::clone(handler);
                    spawn_nonblocking(async move {
                        if let Err(e) = handler.handle(ctx).await {
                            tracing::warn!(error = %e, "non-blocking post_tool_use handler failed");
                        }
                    });
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
        for (mode, handler) in handlers {
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(ctx.clone()))
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
                    if let Err(e) = handler.handle(ctx.clone()).await {
                        tracing::warn!(error = %e, "advisory provider handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let ctx = ctx.clone();
                    let handler = Arc::clone(handler);
                    spawn_nonblocking(async move {
                        if let Err(e) = handler.handle(ctx).await {
                            tracing::warn!(error = %e, "non-blocking provider handler failed");
                        }
                    });
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

        for (mode, handler) in handlers {
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(ctx.clone()))
                        .await
                        .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;
                    if let HookResult::Block { reason } = result {
                        return Err(ExtensionError::Blocked { reason });
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = handler.handle(ctx.clone()).await {
                        tracing::warn!(error = %e, "advisory lifecycle handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let ctx = ctx.clone();
                    let handler = Arc::clone(handler);
                    spawn_nonblocking(async move {
                        if let Err(e) = handler.handle(ctx).await {
                            tracing::warn!(error = %e, "non-blocking lifecycle handler failed");
                        }
                    });
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
        for (def, handler, _ext_id) in &index.static_tools {
            let prompt_metadata = index.tool_metadata.get(&def.name).cloned();
            tools.push(Arc::new(HandlerTool {
                definition: def.clone(),
                handler: Arc::clone(handler),
                prompt_metadata,
                working_dir: working_dir.to_string(),
            }));
        }
        for (_ext_id, discovery) in &index.tool_discoveries {
            match tokio::time::timeout(self.timeout, discovery.discover(working_dir)).await {
                Ok(discovered) => {
                    for discovered_tool in discovered {
                        tools.push(Arc::new(HandlerTool {
                            definition: discovered_tool.definition,
                            handler: discovered_tool.handler,
                            prompt_metadata: discovered_tool.prompt_metadata,
                            working_dir: working_dir.to_string(),
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
    ) -> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata> {
        self.load_index().tool_metadata.clone()
    }

    /// 收集所有插件注册的快捷键绑定。
    pub fn collect_keybindings(&self) -> Vec<astrcode_core::extension::Keybinding> {
        self.load_index().keybindings.clone()
    }

    /// 收集所有插件注册的状态栏项。
    pub fn collect_status_items(&self) -> Vec<astrcode_core::extension::StatusItem> {
        self.load_index().status_items.clone()
    }

    /// 为指定插件构造绑定身份的事件发射器。
    ///
    /// 返回 `None` 表示该插件未声明任何 plugin event type。
    pub fn make_plugin_event_sink(
        &self,
        plugin_id: &str,
        event_tx: mpsc::UnboundedSender<EventPayload>,
    ) -> Option<Arc<dyn PluginEventSink>> {
        let index = self.load_index();
        let decls = index.plugin_event_decls.get(plugin_id)?;
        let decl_map: HashMap<String, PluginEventDecl> = decls
            .iter()
            .map(|d| (d.event_type.clone(), d.clone()))
            .collect();
        Some(Arc::new(BoundPluginEventSink {
            plugin_id: plugin_id.to_owned(),
            declarations: decl_map,
            event_tx,
        }))
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
        for discovery in &index.command_discoveries {
            match tokio::time::timeout(self.timeout, discovery.discover(working_dir)).await {
                Ok(discovered) => {
                    for (cmd, handler) in discovered {
                        cmds.push(("discovery".into(), cmd, handler));
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
        let cmds = self.collect_commands_for_typed(working_dir).await;
        let mut matched: Vec<(String, SlashCommand, Arc<dyn CommandHandler>)> = cmds
            .into_iter()
            .filter(|(_, cmd, _)| cmd.name == command_name)
            .collect();
        matched.sort_by_key(|a| std::cmp::Reverse(command_dispatch_priority(&a.0)));

        if let Some((_, _, handler)) = matched.into_iter().next() {
            handler
                .execute(command_name, arguments, working_dir, ctx)
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

/// 以即发即弃方式派生异步任务，观察 panic 并记录错误日志。
fn spawn_nonblocking<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(join_err) = tokio::spawn(fut).await {
            if join_err.is_panic() {
                tracing::error!("non-blocking handler panicked");
            }
        }
    });
}

/// 类型化工具适配器，将 `ToolHandler` 包装为 `Tool` trait 实现。
struct HandlerTool {
    definition: ToolDefinition,
    handler: Arc<dyn ToolHandler>,
    prompt_metadata: Option<astrcode_core::tool::ToolPromptMetadata>,
    working_dir: String,
}

#[async_trait::async_trait]
impl Tool for HandlerTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.definition.execution_mode
    }

    fn prompt_metadata(&self) -> Option<astrcode_core::tool::ToolPromptMetadata> {
        self.prompt_metadata.clone()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let mut result = match self
            .handler
            .execute(&self.definition.name, arguments, &self.working_dir, _ctx)
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
            .remove(astrcode_core::extension::EXTENSION_TOOL_OUTCOME_KEY)
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
    use astrcode_core::tool::tool_metadata;

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
