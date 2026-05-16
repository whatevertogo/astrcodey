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
    extension::*,
    tool::{ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolResult},
};
use tokio::sync::RwLock;

use crate::runtime::{SessionSpawner, SpawnRequest, SpawnResult};

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
    /// 会话创建器（在 bind() 调用前为 None）
    spawner: Arc<StdRwLock<Option<Arc<dyn SessionSpawner>>>>,
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
}

fn build_handler_index(records: &[ExtensionRecord]) -> HandlerIndex {
    let mut pre: Vec<(i32, HookMode, Arc<dyn PreToolUseHandler>)> = Vec::new();
    let mut post: Vec<(i32, HookMode, Arc<dyn PostToolUseHandler>)> = Vec::new();
    let mut prov: Vec<(ProviderEvent, i32, HookMode, Arc<dyn ProviderHandler>)> = Vec::new();
    let mut pb: Vec<(i32, Arc<dyn PromptBuildHandler>)> = Vec::new();
    let mut cmp: Vec<(CompactEvent, i32, Arc<dyn CompactHandler>)> = Vec::new();
    let mut ptuf: Vec<(i32, Arc<dyn PostToolUseFailureHandler>)> = Vec::new();
    let mut lc: Vec<(ExtensionEvent, i32, HookMode, Arc<dyn LifecycleHandler>)> = Vec::new();

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
            })),
            spawner: Arc::new(StdRwLock::new(None)),
            timeout,
        }
    }

    /// 注册一个扩展。
    pub async fn register(&self, ext: Arc<dyn Extension>) {
        let id = ext.id().to_string();

        // ext.register() 只读扩展自身元数据，不涉及共享状态，在锁外调用
        let mut reg = Registrar::new();
        ext.register(&mut reg);

        // 单次写锁：去重检查 + 插入，消除 TOCTOU
        let mut exts = self.extensions.write().await;
        if exts.iter().any(|e| e.id() == id) {
            tracing::warn!(extension_id = %id, "extension already registered, skipping duplicate");
            return;
        }

        // 在释放 extensions 锁之前先插入 ext，确保去重结果一致
        exts.push(ext);
        drop(exts);

        // records + index 的更新与 extensions 写锁解耦，减少阻塞读并发的时间
        if !reg.is_empty() {
            let mut records = self.records.write().await;
            records.push(ExtensionRecord { id, reg });
            let index = Arc::new(build_handler_index(&records));
            *self.index.write() = index;
        }
    }

    /// 绑定会话创建能力。
    pub fn bind(&self, spawner: Arc<dyn SessionSpawner>) {
        *self.spawner.write().unwrap_or_else(|e| e.into_inner()) = Some(spawner);
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
                            ctx.tool_result = ToolResult {
                                content: content.clone(),
                                error: if is_error {
                                    Some(content)
                                } else {
                                    ctx.tool_result.error.clone()
                                },
                                ..ctx.tool_result.clone()
                            };
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
    pub async fn emit_lifecycle(
        &self,
        event: ExtensionEvent,
        ctx: LifecycleContext,
    ) -> Result<HookResult, ExtensionError> {
        let index = self.load_index();
        let handlers = index.lifecycle.get(&event);

        let Some(handlers) = handlers else {
            return Ok(HookResult::Allow);
        };

        for (mode, handler) in handlers {
            match mode {
                HookMode::Blocking => {
                    let result = tokio::time::timeout(self.timeout, handler.handle(ctx.clone()))
                        .await
                        .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))??;
                    if let HookResult::Block { reason } = result {
                        return Ok(HookResult::Block { reason });
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
        Ok(HookResult::Allow)
    }

    // ─── 收集方法（仍从 records 读取，注册时不变） ──────────────────

    /// 从 ExtensionRecord 收集工具适配器。
    pub async fn collect_tool_adapters_typed(&self, working_dir: &str) -> Vec<Arc<dyn Tool>> {
        let records = self.records.read().await;
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for record in records.iter() {
            for (def, handler) in record.reg.tools().iter() {
                tools.push(Arc::new(HandlerTool {
                    definition: def.clone(),
                    handler: Arc::clone(handler),
                    working_dir: working_dir.to_string(),
                    spawner: Arc::clone(&self.spawner),
                }));
            }
            for discovery in record.reg.tool_discoveries().iter() {
                match tokio::time::timeout(self.timeout, discovery.discover(working_dir)).await {
                    Ok(discovered) => {
                        for (def, handler) in discovered {
                            tools.push(Arc::new(HandlerTool {
                                definition: def,
                                handler,
                                working_dir: working_dir.to_string(),
                                spawner: Arc::clone(&self.spawner),
                            }));
                        }
                    },
                    Err(_) => {
                        tracing::warn!("tool discovery timed out for extension {}", record.id);
                    },
                }
            }
        }
        tools
    }

    /// 从 ExtensionRecord 收集工具提示词元数据。
    pub async fn collect_tool_prompt_metadata_typed(
        &self,
    ) -> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata> {
        let records = self.records.read().await;
        let mut map = std::collections::HashMap::new();
        for record in records.iter() {
            map.extend(record.reg.all_tool_metadata().clone());
        }
        map
    }

    /// 从 ExtensionRecord 收集斜杠命令。
    pub async fn collect_commands_for_typed(
        &self,
        working_dir: &str,
    ) -> Vec<(String, SlashCommand, Arc<dyn CommandHandler>)> {
        let records = self.records.read().await;
        let mut cmds = Vec::new();
        for record in records.iter() {
            for (cmd, handler) in record.reg.commands().iter() {
                cmds.push((record.id.clone(), cmd.clone(), Arc::clone(handler)));
            }
            for discovery in record.reg.command_discoveries().iter() {
                match tokio::time::timeout(self.timeout, discovery.discover(working_dir)).await {
                    Ok(discovered) => {
                        for (cmd, handler) in discovered {
                            cmds.push((record.id.clone(), cmd, handler));
                        }
                    },
                    Err(_) => {
                        tracing::warn!("command discovery timed out for extension {}", record.id);
                    },
                }
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
    working_dir: String,
    spawner: Arc<StdRwLock<Option<Arc<dyn SessionSpawner>>>>,
}

impl HandlerTool {
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let spawner = {
            let guard = self.spawner.read().unwrap_or_else(|e| e.into_inner());
            match &*guard {
                Some(s) => Arc::clone(s),
                None => return Err("Session spawner not bound".into()),
            }
        };
        spawner.spawn(parent_session_id, request).await
    }
}

#[async_trait::async_trait]
impl Tool for HandlerTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.definition.execution_mode
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
            if let Ok(ExtensionToolOutcome::RunSession {
                name,
                system_prompt,
                user_prompt,
                model_preference,
                wait_for_result,
            }) = serde_json::from_value(outcome_value)
            {
                let request = SpawnRequest {
                    name,
                    system_prompt,
                    user_prompt,
                    working_dir: _ctx.working_dir.clone(),
                    model_preference,
                    tool_call_id: _ctx.tool_call_id.clone(),
                    event_tx: _ctx.event_tx.clone(),
                    wait_for_result,
                };

                match self.spawn(_ctx.session_id.as_str(), request).await {
                    Ok(output) => {
                        result.content = output.content;
                        result
                            .metadata
                            .insert("child_session_id".into(), output.child_session_id.into());
                        if let Some(task_id) = output.background_task_id {
                            result
                                .metadata
                                .insert("backgrounded".into(), serde_json::json!(true));
                            result
                                .metadata
                                .insert("task_id".into(), serde_json::json!(task_id));
                        }
                    },
                    Err(e) => {
                        result.content = format!("Failed to spawn child session: {e}");
                        result.is_error = true;
                        result.error = Some(e);
                    },
                }
            }
        }

        Ok(result)
    }
}

/// 将 [`ExtensionError`] 转换为结构化的错误 [`ToolResult`]。
fn extension_error_result(tool_name: &str, extension_id: &str, err: ExtensionError) -> ToolResult {
    use astrcode_core::tool::tool_metadata;

    let (content, suggestion) = match &err {
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
            "A hook policy prevented this operation. Check the reason above and adjust your \
             approach.",
        ),
        ExtensionError::Internal(message) => (
            format!("Tool `{tool_name}` failed: {message}"),
            "The extension encountered an internal error. Try again with different arguments, or \
             use a builtin tool as an alternative.",
        ),
    };

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
