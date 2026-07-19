//! 扩展运行器 — 将生命周期事件分发到已注册的扩展。

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use astrcode_core::{event::EventPayload, tool::ToolPromptMetadata};
use astrcode_extension_sdk::{
    extension::*,
    tool::{SessionOperations, Tool},
    trusted::ExtensionHostServices,
};
use astrcode_kernel::ExtensionRuntime;
use tokio::sync::{Mutex as AsyncMutex, RwLock, mpsc};

mod commands;
mod diagnostics;
mod http;
mod index;
mod snapshot;
mod tool_adapter;

pub use commands::{
    CommandSource, RegisteredSlashCommand, ResolvedSlashCommand, ShadowedSlashCommand,
};
use diagnostics::{
    ExtensionDiagnosticStage as DiagnosticStage, ExtensionStageOutcome as StageOutcome,
};
pub use diagnostics::{
    ExtensionDiagnostics, ExtensionHealthReport, ExtensionStageDiagnostics, ExtensionStageStatus,
};
pub use http::ExtensionHttpDispatchResult;
use index::{
    HandlerIndex, build_handler_index, log_handler_dispatch_order,
    validate_http_route_registrations,
};
pub use snapshot::{ExtensionDeclarationSnapshot, ExtensionRegistrySnapshot};

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
    diagnostics: parking_lot::RwLock<BTreeMap<String, ExtensionDiagnostics>>,
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
    pub(super) id: String,
    pub(super) reg: Registrar,
    pub(super) capabilities: Vec<ExtensionCapability>,
    /// 注册时的配置快照，用于 diff 检测热更新。
    config: serde_json::Value,
    /// 串行化同一扩展的配置回调与 stop，不阻塞其他扩展。
    operation_gate: Arc<AsyncMutex<()>>,
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

// ─── ExtensionRunner impl ───────────────────────────────────────────────

impl ExtensionRunner {
    /// 创建新的扩展运行器。
    pub fn new(timeout: Duration) -> Self {
        Self {
            lifecycle_lock: AsyncMutex::new(()),
            extensions: RwLock::new(Vec::new()),
            records: RwLock::new(Vec::new()),
            index: parking_lot::RwLock::new(Arc::new(HandlerIndex::default())),
            diagnostics: parking_lot::RwLock::new(BTreeMap::new()),
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
            self.record_stage_result(
                &id,
                DiagnosticStage::Register,
                Some(Duration::ZERO),
                StageOutcome::Skipped,
            );
            return Ok(false);
        }

        // register() 只收集声明；start() 才进入运行态。
        self.record_stage_running(&id, DiagnosticStage::Register);
        let register_started = std::time::Instant::now();
        let mut reg = Registrar::new();
        ext.register(&mut reg);
        if let Err(message) = validate_http_route_registrations(
            &id,
            &capabilities,
            reg.http_routes(),
            &self.records.read().await,
        ) {
            let error = ExtensionError::Internal(message);
            self.record_stage_result(
                &id,
                DiagnosticStage::Register,
                Some(register_started.elapsed()),
                StageOutcome::Failed(error.to_string()),
            );
            return Err(error);
        }
        if reg.needs_extension_data_dir() {
            let dir = astrcode_support::hostpaths::extensions_data_dir(&id);
            if let Err(error) = std::fs::create_dir_all(&dir) {
                let error = ExtensionError::Internal(format!(
                    "failed to create extension data dir: {error}"
                ));
                self.record_stage_result(
                    &id,
                    DiagnosticStage::Register,
                    Some(register_started.elapsed()),
                    StageOutcome::Failed(error.to_string()),
                );
                return Err(error);
            }
        }
        self.record_stage_result(
            &id,
            DiagnosticStage::Register,
            Some(register_started.elapsed()),
            StageOutcome::Succeeded,
        );

        let tasks = ExtensionTasks::new(id.clone());
        let ext_config = extension_config(&self.extension_configs.read(), &id);

        let event_sink =
            self.startup_event_tx.read().as_ref().and_then(|tx| {
                bind_extension_event_sink(&id, reg.extension_event_decls(), tx.clone())
            });
        let host_services = self
            .host_services
            .read()
            .as_ref()
            .and_then(|services| services.scoped_to(&capabilities))
            .map(Arc::new);
        let ctx = ExtensionCtx::with_host_services(
            tasks.clone(),
            ExtensionConfig(ext_config.clone()),
            startup_working_dir.map(str::to_string),
            event_sink,
            host_services,
        );
        self.record_stage_running(&id, DiagnosticStage::Start);
        let start_started = std::time::Instant::now();
        let start_result = self.run_with_timeout(ext.start(ctx)).await;
        if let Err(error) = start_result {
            self.record_stage_result(
                &id,
                DiagnosticStage::Start,
                Some(start_started.elapsed()),
                StageOutcome::Failed(error.to_string()),
            );
            tasks.cancel();
            tasks.wait(self.timeout).await;
            let rollback_result = self
                .run_with_timeout(ext.stop(StopReason::StartupFailed))
                .await;
            if let Err(rollback_error) = rollback_result {
                tracing::warn!(
                    extension_id = %id,
                    error = %rollback_error,
                    "extension startup rollback failed"
                );
            }
            return Err(error);
        }
        self.record_stage_result(
            &id,
            DiagnosticStage::Start,
            Some(start_started.elapsed()),
            StageOutcome::Succeeded,
        );

        self.extensions.write().await.push(ext);
        self.extension_tasks.write().await.insert(id.clone(), tasks);

        if !reg.is_empty() {
            let mut records = self.records.write().await;
            records.push(ExtensionRecord {
                id,
                reg,
                capabilities,
                config: ext_config,
                operation_gate: Arc::new(AsyncMutex::new(())),
            });
            self.rebuild_index(&records);
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
        let (_operation_guard, _lifecycle) = loop {
            let operation_gate = self.records.read().await.iter().find_map(|record| {
                (record.id == extension_id).then(|| Arc::clone(&record.operation_gate))
            });
            let operation_guard = match &operation_gate {
                Some(gate) => Some(Arc::clone(gate).lock_owned().await),
                None => None,
            };
            let lifecycle = self.lifecycle_lock.lock().await;
            let operation_gate_is_current = self
                .records
                .read()
                .await
                .iter()
                .find(|record| record.id == extension_id)
                .map(|record| {
                    operation_gate
                        .as_ref()
                        .is_some_and(|gate| Arc::ptr_eq(gate, &record.operation_gate))
                })
                .unwrap_or(operation_gate.is_none());
            if operation_gate_is_current {
                break (operation_guard, lifecycle);
            }
        };
        let mut exts = self.extensions.write().await;
        let Some(pos) = exts.iter().position(|ext| ext.id() == extension_id) else {
            return Ok(false);
        };
        let ext = exts.remove(pos);
        drop(exts);

        let mut records = self.records.write().await;
        records.retain(|record| record.id != extension_id);
        self.rebuild_index(&records);
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
        self.diagnostics.write().remove(extension_id);
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

    fn rebuild_index(&self, records: &[ExtensionRecord]) {
        log_handler_dispatch_order(records);
        let index = Arc::new(build_handler_index(records));
        self.ensure_extensions_data_dir_dirs(&index);
        *self.index.write() = index;
    }

    /// 绑定会话原子操作能力。
    pub fn bind_session_ops(&self, ops: Arc<dyn SessionOperations>) {
        *self.session_ops.write().unwrap_or_else(|e| e.into_inner()) = Some(ops);
    }

    /// 绑定扩展在标准 `start()` 生命周期中可取得的宿主服务。
    pub fn bind_host_services(&self, services: Arc<ExtensionHostServices>) {
        *self.host_services.write() = Some(services);
    }

    /// 返回进程内稳定复用的宿主出站网络服务。
    pub fn outbound_network_service(
        &self,
    ) -> Option<Arc<dyn astrcode_core::extension::OutboundNetworkService>> {
        self.host_services
            .read()
            .as_ref()
            .and_then(|services| services.outbound_network.clone())
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
        let pending: Vec<_> = self
            .records
            .read()
            .await
            .iter()
            .filter_map(|record| {
                let config = extension_config(&current_configs, &record.id);
                (record.config != config).then(|| {
                    (
                        record.id.clone(),
                        config,
                        Arc::clone(&record.operation_gate),
                    )
                })
            })
            .collect();
        if pending.is_empty() {
            return Vec::new();
        }
        let changes: Vec<_> = {
            let extensions = self.extensions.read().await;
            pending
                .into_iter()
                .filter_map(|(extension_id, config, operation_gate)| {
                    extensions
                        .iter()
                        .find(|extension| extension.id() == extension_id)
                        .map(|extension| {
                            (extension_id, Arc::clone(extension), config, operation_gate)
                        })
                })
                .collect()
        };

        let mut errors = Vec::new();
        for (extension_id, extension, new_config, operation_gate) in changes {
            let _operation = operation_gate.lock().await;
            let record_is_current = self.records.read().await.iter().any(|record| {
                record.id == extension_id
                    && Arc::ptr_eq(&record.operation_gate, &operation_gate)
                    && record.config != new_config
            });
            let extension_is_current =
                self.extensions.read().await.iter().any(|current| {
                    current.id() == extension_id && Arc::ptr_eq(current, &extension)
                });
            if !record_is_current
                || !extension_is_current
                || extension_config(&self.extension_configs.read(), &extension_id) != new_config
            {
                continue;
            }

            if let Err(error) = self
                .run_with_timeout(extension.on_config_changed(ExtensionConfig(new_config.clone())))
                .await
            {
                errors.push(format!(
                    "config changed handler failed for {extension_id}: {error}"
                ));
            } else {
                let mut records = self.records.write().await;
                if extension_config(&self.extension_configs.read(), &extension_id) == new_config {
                    if let Some(record) = records.iter_mut().find(|record| {
                        record.id == extension_id
                            && Arc::ptr_eq(&record.operation_gate, &operation_gate)
                    }) {
                        record.config = new_config;
                    }
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

    async fn run_recorded_blocking_hook<T>(
        &self,
        extension_id: &str,
        hook_name: &'static str,
        future: impl std::future::Future<Output = Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        let started = std::time::Instant::now();
        match tokio::time::timeout(self.timeout, future).await {
            Ok(result) => {
                self.record_hook_result(
                    extension_id,
                    hook_name,
                    started.elapsed(),
                    result.as_ref().err().map(ToString::to_string),
                    false,
                );
                result
            },
            Err(_) => {
                let error = ExtensionError::Timeout(self.timeout.as_millis() as u64);
                self.record_hook_result(
                    extension_id,
                    hook_name,
                    started.elapsed(),
                    Some(error.to_string()),
                    true,
                );
                Err(error)
            },
        }
    }

    async fn run_with_timeout<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        tokio::time::timeout(self.timeout, future)
            .await
            .map_err(|_| ExtensionError::Timeout(self.timeout.as_millis() as u64))?
    }

    async fn run_recorded_advisory<T>(
        &self,
        extension_id: &str,
        hook_name: &'static str,
        future: impl std::future::Future<Output = Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        let started = std::time::Instant::now();
        let result = future.await;
        self.record_hook_result(
            extension_id,
            hook_name,
            started.elapsed(),
            result.as_ref().err().map(ToString::to_string),
            false,
        );
        result
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

        for (extension_id, mode, target, handler) in &index.pre_tool_use {
            if !target.matches(&ctx.tool_name) {
                continue;
            }
            let mut handler_ctx = ctx.clone();
            handler_ctx.extension_event_sink =
                attach_extension_event_sink(&index, extension_id, &ctx.event_tx);
            match mode {
                HookMode::Blocking => {
                    let result = self
                        .run_recorded_blocking_hook(
                            extension_id,
                            "pre_tool_use",
                            handler.handle(handler_ctx),
                        )
                        .await?;
                    match result {
                        PreToolUseResult::Block { reason } => {
                            return Ok(PreToolUseResult::Block { reason });
                        },
                        PreToolUseResult::Ask { prompt, rule_key } => {
                            return Ok(PreToolUseResult::Ask { prompt, rule_key });
                        },
                        PreToolUseResult::ModifyInput { tool_input } => {
                            ctx = PreToolUseContext { tool_input, ..ctx };
                            modified = true;
                        },
                        PreToolUseResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = self
                        .run_recorded_advisory(
                            extension_id,
                            "pre_tool_use",
                            handler.handle(handler_ctx),
                        )
                        .await
                    {
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

        for (extension_id, mode, target, handler) in &index.post_tool_use {
            if !target.matches(&ctx.tool_name) {
                continue;
            }
            let mut handler_ctx = ctx.clone();
            handler_ctx.extension_event_sink =
                attach_extension_event_sink(&index, extension_id, &ctx.event_tx);
            match mode {
                HookMode::Blocking => {
                    let result = self
                        .run_recorded_blocking_hook(
                            extension_id,
                            "post_tool_use",
                            handler.handle(handler_ctx),
                        )
                        .await?;
                    match result {
                        PostToolUseResult::Block { reason } => {
                            return Ok(PostToolUseResult::Block { reason });
                        },
                        PostToolUseResult::ModifyResult { content } => {
                            let error = ctx.tool_result.is_error.then(|| content.clone());
                            ctx.tool_result.content = content;
                            ctx.tool_result.error = error;
                            modified = true;
                        },
                        PostToolUseResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = self
                        .run_recorded_advisory(
                            extension_id,
                            "post_tool_use",
                            handler.handle(handler_ctx),
                        )
                        .await
                    {
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
            let handler_ctx = ctx.clone();
            match mode {
                HookMode::Blocking => {
                    let result = self
                        .run_recorded_blocking_hook(
                            extension_id,
                            provider_hook_name(event),
                            handler.handle(handler_ctx),
                        )
                        .await?;
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
                    if let Err(e) = self
                        .run_recorded_advisory(
                            extension_id,
                            provider_hook_name(event),
                            handler.handle(handler_ctx),
                        )
                        .await
                    {
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
            let contributions = self.run_with_timeout(handler.handle(ctx.clone())).await?;
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
            let result = self.run_with_timeout(handler.handle(ctx.clone())).await?;
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

    /// LLM 自然结束（无 tool call）后询问扩展是否再跑一个 step。
    ///
    /// 按优先级降序；首个返回 [`ContinueAfterStopResult::ContinueOneStep`] 的 blocking
    /// handler 生效。每个 handler 的每轮预算由插件注册时声明。
    pub async fn emit_continue_after_stop(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        let index = self.load_index();
        for (extension_id, options, handler) in &index.continue_after_stop {
            if !options.allows(ctx.continuations_this_turn) {
                tracing::debug!(
                    extension_id = %extension_id,
                    continuations_this_turn = ctx.continuations_this_turn,
                    "ContinueAfterStop: extension continuation limit exhausted"
                );
                continue;
            }
            let result = self.run_with_timeout(handler.handle(ctx.clone())).await?;
            if result == ContinueAfterStopResult::ContinueOneStep {
                tracing::debug!(
                    extension_id = %extension_id,
                    "ContinueAfterStop: extension requested one more step"
                );
                return Ok(ContinueAfterStopResult::ContinueOneStep);
            }
        }
        Ok(ContinueAfterStopResult::EndTurn)
    }

    /// 用户消息写入 durable transcript 前的 envelope 变换。
    pub async fn emit_user_message_envelope(
        &self,
        ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        let index = self.load_index();
        let mut ctx = ctx;
        let mut modified = false;
        for (extension_id, handler) in &index.user_message_envelope {
            let result = self
                .run_recorded_blocking_hook(
                    extension_id,
                    "user_message_envelope",
                    handler.handle(ctx.clone()),
                )
                .await?;

            match result {
                UserMessageEnvelopeResult::Allow => {},
                UserMessageEnvelopeResult::ReplaceText { text } => {
                    ctx.text = text;
                    modified = true;
                },
                UserMessageEnvelopeResult::AppendText { text } => {
                    append_user_message_text(&mut ctx.text, &text);
                    modified = true;
                },
                UserMessageEnvelopeResult::Block { reason } => {
                    return Ok(UserMessageEnvelopeResult::Block { reason });
                },
            }
        }

        if modified {
            Ok(UserMessageEnvelopeResult::ReplaceText { text: ctx.text })
        } else {
            Ok(UserMessageEnvelopeResult::Allow)
        }
    }

    /// 一批工具结果落盘后的继续/结束决策。
    pub async fn emit_after_tool_results(
        &self,
        ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError> {
        let index = self.load_index();
        for (extension_id, handler) in &index.after_tool_results {
            let result = self
                .run_recorded_blocking_hook(
                    extension_id,
                    "after_tool_results",
                    handler.handle(ctx.clone()),
                )
                .await?;

            if let AfterToolResultsResult::EndTurn { reason } = result {
                return Ok(AfterToolResultsResult::EndTurn { reason });
            }
        }

        Ok(AfterToolResultsResult::Continue)
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
                    let result = self.run_with_timeout(handler.handle(handler_ctx)).await?;
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

    /// 从 HandlerIndex 缓存收集工具提示词元数据。
    pub async fn collect_tool_prompt_metadata_typed(
        &self,
    ) -> std::collections::HashMap<String, astrcode_extension_sdk::tool::ToolPromptMetadata> {
        self.load_index().tool_metadata.clone()
    }

    pub async fn collect_tool_ui(
        &self,
    ) -> std::collections::HashMap<String, astrcode_extension_sdk::tool::ToolUiWire> {
        self.load_index().tool_ui.clone()
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

    /// 判断是否有任何扩展注册了类型化能力。
    pub async fn has_records(&self) -> bool {
        !self.records.read().await.is_empty()
    }
}

#[async_trait::async_trait]
impl ExtensionRuntime for ExtensionRunner {
    async fn emit_pre_tool_use(
        &self,
        ctx: PreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
        ExtensionRunner::emit_pre_tool_use(self, ctx).await
    }

    async fn emit_post_tool_use(
        &self,
        ctx: PostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        ExtensionRunner::emit_post_tool_use(self, ctx).await
    }

    async fn emit_provider(
        &self,
        event: ProviderEvent,
        ctx: ProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        ExtensionRunner::emit_provider(self, event, ctx).await
    }

    async fn collect_prompt_contributions(
        &self,
        ctx: PromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        ExtensionRunner::collect_prompt_contributions_typed(self, ctx).await
    }

    async fn emit_compact(
        &self,
        event: CompactEvent,
        ctx: CompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        ExtensionRunner::emit_compact(self, event, ctx).await
    }

    async fn emit_post_tool_use_failure(&self, ctx: PostToolUseFailureContext) {
        ExtensionRunner::emit_post_tool_use_failure(self, ctx).await;
    }

    async fn emit_continue_after_stop(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        ExtensionRunner::emit_continue_after_stop(self, ctx).await
    }

    async fn emit_user_message_envelope(
        &self,
        ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        ExtensionRunner::emit_user_message_envelope(self, ctx).await
    }

    async fn emit_after_tool_results(
        &self,
        ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError> {
        ExtensionRunner::emit_after_tool_results(self, ctx).await
    }

    async fn emit_lifecycle(
        &self,
        event: ExtensionEvent,
        ctx: LifecycleContext,
    ) -> Result<(), ExtensionError> {
        ExtensionRunner::emit_lifecycle(self, event, ctx).await
    }

    async fn collect_tool_adapters(&self, working_dir: &str) -> Vec<Arc<dyn Tool>> {
        ExtensionRunner::collect_tool_adapters_typed(self, working_dir).await
    }

    async fn collect_tool_prompt_metadata(
        &self,
    ) -> std::collections::HashMap<String, ToolPromptMetadata> {
        ExtensionRunner::collect_tool_prompt_metadata_typed(self).await
    }

    fn session_ops(&self) -> Option<Arc<dyn SessionOperations>> {
        let ops_ref = self.session_ops_ref();
        let guard = ops_ref.read().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    }
}

fn provider_hook_name(event: ProviderEvent) -> &'static str {
    match event {
        ProviderEvent::BeforeRequest => "before_provider_request",
        ProviderEvent::AfterResponse => "after_provider_response",
    }
}

fn append_user_message_text(base: &mut String, addition: &str) {
    if addition.is_empty() {
        return;
    }
    if !base.is_empty() {
        base.push_str("\n\n");
    }
    base.push_str(addition);
}

fn extension_config(
    configs: &BTreeMap<String, serde_json::Value>,
    extension_id: &str,
) -> serde_json::Value {
    configs
        .get(extension_id)
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

#[cfg(test)]
mod tests;
