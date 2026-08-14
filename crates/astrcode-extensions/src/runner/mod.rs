//! 扩展运行器 — 将生命周期事件分发到已注册的扩展。

use std::{
    collections::BTreeMap,
    sync::{
        Arc, RwLock as StdRwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use astrcode_core::{event::EventPayload, tool::SessionOperations};
use astrcode_extension_sdk::{
    extension::{
        internal::{
            RuntimeContinueAfterStopContext, RuntimeHookCallContext, RuntimeLifecycleContext,
            RuntimePostCompactContext, RuntimePostToolUseContext, RuntimePreCompactContext,
            RuntimePreToolUseContext, RuntimePromptBuildContext, RuntimeProviderContext,
            RuntimeProviderSettlementContext, RuntimeUserMessageEnvelopeContext,
            activate_extension_tasks, append_provider_messages, append_user_message_text,
            author_hook_context, author_provider_settlement_context, extension_config,
            extension_start_context, replace_post_tool_result, replace_pre_tool_input,
            replace_provider_messages, replace_user_message_text, retain_call_cancellation,
            suspended_extension_tasks,
        },
        *,
    },
    runtime_ports::{
        PromptContributor, ProviderRequestAcknowledgements, ProviderRequestPreparation,
        RuntimeSnapshotProvider, RuntimeSnapshotState, SessionOperationsProvider,
        ToolCatalogProvider, TurnExtensionView as RuntimeTurnExtensionView,
        TurnExtensionViewProvider, TurnHooks,
    },
};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, RwLock, Semaphore, mpsc};

mod commands;
mod custom_event_control;
mod custom_event_delivery;
mod diagnostics;
mod host_invoker;
mod http;
mod index;
mod manifest;
mod registration;
mod retirement;
mod snapshot;
mod supervisor;
mod tool_adapter;
mod tool_catalog_cache;

pub use commands::{ResolvedCommandSurface, ResolvedSlashCommand, ShadowedSlashCommand};
pub use custom_event_control::{
    CustomEventConsumerAction, CustomEventConsumerControlError, CustomEventConsumerStatus,
};
pub use custom_event_delivery::CustomEventSession;
use custom_event_delivery::{
    CUSTOM_EVENT_CONCURRENCY, CustomEventConsumerMetricsMap, CustomEventLanes, CustomEventQuiescing,
};
use diagnostics::{
    ExtensionDiagnosticStage as DiagnosticStage, ExtensionStageOutcome as StageOutcome,
};
pub use diagnostics::{
    ExtensionDiagnostics, ExtensionHealthReport, ExtensionStageDiagnostics, ExtensionStageStatus,
};
pub(crate) use host_invoker::transport_invoke_context;
use host_invoker::{ExtensionCallContextFactory, ExtensionCallContextInput};
pub use http::ExtensionHttpDispatchResult;
use index::{
    ExtensionGenerationEntry, HandlerIndex, build_handler_index, log_handler_dispatch_order,
};
use manifest::ResolvedExtensionManifest;
use registration::validate_registration_conflicts;
use retirement::{
    ExtensionPublicationLease, PendingRegistrationResources, RetirementSupervisor, RetirementTicket,
};
pub use snapshot::{
    ExtensionDeclarationSnapshot, ExtensionRegistrySnapshot, ExtensionRuntimeState,
};
use supervisor::ExtensionSupervisor;

use crate::host_router::{ExtensionGenerationGate, ExtensionInstanceId};

/// 管理扩展生命周期、运行态发布与 hook 分发。
///
/// 强制执行 HookMode 语义：
/// - 工具参数变换与 PreToolUse 准入始终同步执行，并由独立注册类型约束返回值
/// - Blocking: 同步执行，可返回阻断或替换结果
/// - NonBlocking: 以即发即弃方式派生任务，使用快照上下文
/// - Advisory: 结果仅记录日志，不强制执行
///
/// 锁顺序为来源协调、单扩展操作门、注册表生命周期锁；已发布索引只在持有扩展列表
/// 写锁时重建。
pub struct ExtensionRunner {
    coordination: LifecycleCoordination,
    registry: RuntimeRegistry,
    bindings: parking_lot::RwLock<HostBindings>,
    diagnostics: Arc<parking_lot::RwLock<BTreeMap<String, ExtensionDiagnostics>>>,
    /// 宿主等待扩展控制面操作和同步 hook 的统一超时。
    operation_timeout: Duration,
    retirements: RetirementSupervisor,
    custom_event_permits: Arc<Semaphore>,
    custom_event_lanes: Arc<CustomEventLanes>,
    custom_event_quiescing: Arc<CustomEventQuiescing>,
    custom_event_metrics: CustomEventConsumerMetricsMap,
    shutting_down: AtomicBool,
}

/// Immutable registration and handler view for one extension-runtime generation.
pub(crate) struct ExtensionView {
    generation: u64,
    index: Arc<HandlerIndex>,
    diagnostics: Arc<parking_lot::RwLock<BTreeMap<String, ExtensionDiagnostics>>>,
    operation_timeout: Duration,
    call_context_factory: ExtensionCallContextFactory,
    custom_event_permits: Arc<Semaphore>,
    custom_event_lanes: Arc<CustomEventLanes>,
    custom_event_quiescing: Arc<CustomEventQuiescing>,
}

/// Keybinding and status declarations captured from one stable runtime generation.
pub struct ExtensionUiContributions {
    pub keybindings: Vec<Keybinding>,
    pub status_items: Vec<StatusItem>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid configuration for extension {extension_id}: {source}")]
pub struct ExtensionConfigValidationError {
    extension_id: String,
    #[source]
    source: Box<ExtensionError>,
}

impl ExtensionConfigValidationError {
    pub fn new(extension_id: impl Into<String>, source: ExtensionError) -> Self {
        Self {
            extension_id: extension_id.into(),
            source: Box::new(source),
        }
    }
}

struct LifecycleCoordination {
    /// 串行化全局注册表生命周期变更。
    registry: AsyncMutex<()>,
    /// 串行化来源发现与增量协调，避免并发 reload 基于过期快照互相覆盖。
    source_reconcile: Arc<AsyncMutex<()>>,
    /// 让并发 shutdown 调用共享同一条终止屏障。
    shutdown: AsyncMutex<()>,
}

struct RuntimeRegistry {
    /// 已发布的扩展实例、清单与生命周期资源。
    extensions: RwLock<Vec<HostedExtension>>,
    /// 预计算的 handler 索引；writer 活跃时可能是释放旧代租约所需的批次中间态。
    index: ArcSwap<HandlerIndex>,
    publication: parking_lot::Mutex<RuntimePublication>,
    publication_stable: Notify,
}

struct HostBindings {
    /// 会话原子操作能力（在 bind_session_ops() 调用前为 None）。
    session_ops: Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>>,
    /// 扩展 `start()` 阶段取得的进程级事件通道。
    startup_event_tx: Option<astrcode_core::event::EventSender>,
    /// 进程内 SDK host 与磁盘扩展共同使用的能力路由。
    host_router: Arc<crate::host_router::HostRouter>,
}

#[derive(Default)]
struct RuntimePublication {
    generation: u64,
    active_writers: usize,
    /// 已写入 `index`、等待最后一个 writer 退出后提交的 generation。
    pending_generation: Option<u64>,
}

impl RuntimePublication {
    fn is_stable_generation(&self, generation: u64) -> bool {
        self.active_writers == 0 && self.generation == generation
    }
}

struct RuntimePublicationGuard<'a> {
    publication: &'a parking_lot::Mutex<RuntimePublication>,
    publication_stable: &'a Notify,
}

impl<'a> RuntimePublicationGuard<'a> {
    fn begin(
        publication: &'a parking_lot::Mutex<RuntimePublication>,
        publication_stable: &'a Notify,
    ) -> Self {
        let mut state = publication.lock();
        state.active_writers = state.active_writers.saturating_add(1);
        Self {
            publication,
            publication_stable,
        }
    }
}

impl Drop for RuntimePublicationGuard<'_> {
    fn drop(&mut self) {
        let became_stable = {
            let mut state = self.publication.lock();
            if state.active_writers == 0 {
                tracing::error!("runtime publication guard dropped without an active writer");
                return;
            }
            state.active_writers -= 1;
            if state.active_writers != 0 {
                false
            } else {
                if let Some(generation) = state.pending_generation.take() {
                    state.generation = generation;
                }
                true
            }
        };
        if became_stable {
            self.publication_stable.notify_waiters();
        }
    }
}

struct HostedExtension {
    extension: Arc<dyn Extension>,
    manifest: ResolvedExtensionManifest,
    origin: ExtensionOrigin,
    instance_id: ExtensionInstanceId,
    tasks: ExtensionTasks,
    /// 串行化同一扩展实例的生命周期操作，不阻塞其他扩展。
    operation_gate: Arc<AsyncMutex<()>>,
    supervisor: ExtensionSupervisor,
    publication_lease: Arc<ExtensionPublicationLease>,
    generation_gate: ExtensionGenerationGate,
}

enum ExtensionOrigin {
    Direct,
    Source { key: String, fingerprint: String },
}

pub(crate) struct RegisteredSourceExtension {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) fingerprint: String,
}

pub(crate) enum SourceGenerationEntry {
    Retain {
        id: String,
        key: String,
        fingerprint: String,
    },
    Start {
        extension: Arc<dyn Extension>,
        key: String,
        fingerprint: String,
        config: serde_json::Value,
    },
}

struct PreparedSourceExtension {
    hosted: HostedExtension,
    operation_guard: OwnedMutexGuard<()>,
}

enum PreparedSourceEntry {
    Retain {
        id: String,
        key: String,
        fingerprint: String,
    },
    Start(Box<PreparedSourceExtension>),
}

struct ResolvedSourceExtension {
    extension: Arc<dyn Extension>,
    manifest: ResolvedExtensionManifest,
    key: String,
    fingerprint: String,
    config: serde_json::Value,
}

enum ResolvedSourceEntry {
    Retain {
        id: String,
        key: String,
        fingerprint: String,
    },
    Start(Box<ResolvedSourceExtension>),
}

pub struct PreparedExtensionGeneration {
    runner: Arc<ExtensionRunner>,
    _source_transaction: Option<OwnedMutexGuard<()>>,
    entries: Vec<PreparedSourceEntry>,
    retiring_gates: Vec<Arc<AsyncMutex<()>>>,
    changed: bool,
}

enum RegistrationPublication {
    Published(ExtensionTasks),
    Skipped,
    StartupFailed {
        extension_id: String,
        error: ExtensionError,
        retirement: RetirementTicket,
    },
}

impl PreparedExtensionGeneration {
    pub async fn commit_with(mut self, publish: impl FnOnce(u64)) {
        if !self.changed {
            self.entries.clear();
            let generation = self.runner.registry.publication.lock().generation;
            publish(generation);
            self._source_transaction.take();
            return;
        }

        let runner = Arc::clone(&self.runner);
        let retained = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                PreparedSourceEntry::Retain {
                    id,
                    key,
                    fingerprint,
                } => Some((id.clone(), key.clone(), fingerprint.clone())),
                PreparedSourceEntry::Start(_) => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let mut retiring_operation_guards = Vec::with_capacity(self.retiring_gates.len());
        for gate in &self.retiring_gates {
            retiring_operation_guards.push(Arc::clone(gate).lock_owned().await);
        }
        let lifecycle = runner.coordination.registry.lock().await;
        let mut active = runner.registry.extensions.write().await;
        let entries = std::mem::take(&mut self.entries);
        let mut previous = std::mem::take(&mut *active);
        let mut next = previous
            .extract_if(.., |hosted| match &hosted.origin {
                ExtensionOrigin::Direct => true,
                ExtensionOrigin::Source { key, fingerprint } => retained.contains(&(
                    hosted.manifest.id().to_owned(),
                    key.clone(),
                    fingerprint.clone(),
                )),
            })
            .collect::<Vec<_>>();
        let mut fresh_operation_guards = Vec::new();
        let desired_ids = entries
            .iter()
            .map(|entry| match entry {
                PreparedSourceEntry::Retain { id, .. } => id.clone(),
                PreparedSourceEntry::Start(prepared) => prepared.hosted.manifest.id().to_owned(),
            })
            .collect::<std::collections::HashSet<_>>();

        for entry in entries {
            match entry {
                PreparedSourceEntry::Retain { .. } => {},
                PreparedSourceEntry::Start(prepared) => {
                    let PreparedSourceExtension {
                        hosted,
                        operation_guard,
                    } = *prepared;
                    fresh_operation_guards.push(operation_guard);
                    next.push(hosted);
                },
            }
        }

        log_handler_dispatch_order(&next);
        let publication = RuntimePublicationGuard::begin(
            &runner.registry.publication,
            &runner.registry.publication_stable,
        );
        let generation = {
            let mut state = runner.registry.publication.lock();
            let generation = state.generation.wrapping_add(1);
            state.pending_generation = Some(generation);
            generation
        };
        let index = Arc::new(build_handler_index(&next, generation));
        for hosted in &next {
            hosted.supervisor.mark_ready(generation);
        }
        runner.registry.index.store(index);
        *active = next;
        publish(generation);
        for hosted in active.iter() {
            hosted.generation_gate.activate();
            activate_extension_tasks(&hosted.tasks);
        }
        drop(fresh_operation_guards);
        drop(active);
        drop(lifecycle);
        drop(publication);

        for (hosted, operation_guard) in previous.into_iter().zip(retiring_operation_guards) {
            let reason = if desired_ids.contains(hosted.manifest.id()) {
                StopReason::Reload
            } else {
                StopReason::Disabled
            };
            runner.retirements.retire_replaced(
                hosted,
                reason,
                runner.operation_timeout,
                operation_guard,
                runner.host_router(),
            );
        }
        self._source_transaction.take();
    }

    pub async fn abort(mut self) {
        let prepared = self
            .entries
            .drain(..)
            .filter_map(|entry| match entry {
                PreparedSourceEntry::Retain { .. } => None,
                PreparedSourceEntry::Start(prepared) => Some(*prepared),
            })
            .collect();
        self.runner.abort_prepared_source_extensions(prepared).await;
        self._source_transaction.take();
    }
}

impl Drop for PreparedExtensionGeneration {
    fn drop(&mut self) {
        let entries = std::mem::take(&mut self.entries);
        for entry in entries {
            let PreparedSourceEntry::Start(prepared) = entry else {
                continue;
            };
            let PreparedSourceExtension {
                hosted,
                operation_guard,
            } = *prepared;
            self.runner.retirements.retire(
                hosted,
                StopReason::StartupFailed,
                self.runner.operation_timeout,
                operation_guard,
                self.runner.host_router(),
            );
        }
    }
}

impl ExtensionRunner {
    /// 创建新的扩展运行器。
    pub fn new(operation_timeout: Duration) -> Self {
        Self {
            coordination: LifecycleCoordination {
                registry: AsyncMutex::new(()),
                source_reconcile: Arc::new(AsyncMutex::new(())),
                shutdown: AsyncMutex::new(()),
            },
            registry: RuntimeRegistry {
                extensions: RwLock::new(Vec::new()),
                index: ArcSwap::from_pointee(HandlerIndex::default()),
                publication: parking_lot::Mutex::new(RuntimePublication::default()),
                publication_stable: Notify::new(),
            },
            bindings: parking_lot::RwLock::new(HostBindings {
                session_ops: Arc::new(StdRwLock::new(None)),
                startup_event_tx: None,
                host_router: Arc::new(crate::host_router::HostRouter::from_backends(
                    crate::host_router::HostBackends::default(),
                )),
            }),
            diagnostics: Arc::new(parking_lot::RwLock::new(BTreeMap::new())),
            operation_timeout,
            retirements: RetirementSupervisor::new(),
            custom_event_permits: Arc::new(Semaphore::new(CUSTOM_EVENT_CONCURRENCY)),
            custom_event_lanes: Arc::new(CustomEventLanes::default()),
            custom_event_quiescing: Arc::new(CustomEventQuiescing::default()),
            custom_event_metrics: CustomEventConsumerMetricsMap::default(),
            shutting_down: AtomicBool::new(false),
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
        let _source_transaction = self.coordination.source_reconcile.lock().await;
        let manifest = ext.manifest();
        let (operation_gate, operation_guard) =
            self.lock_registration_operation(manifest.id()).await;
        let publication = {
            let _lifecycle = self.coordination.registry.lock().await;
            self.publish_registration_locked(
                ext,
                manifest,
                operation_gate,
                operation_guard,
                startup_working_dir,
                ExtensionOrigin::Direct,
            )
            .await
        }?;
        let Some(_tasks) = self.finish_registration(publication).await? else {
            return Ok(false);
        };
        Ok(true)
    }

    async fn publish_registration_locked(
        &self,
        ext: Arc<dyn Extension>,
        manifest: ExtensionManifest,
        operation_gate: Arc<AsyncMutex<()>>,
        operation_guard: OwnedMutexGuard<()>,
        startup_working_dir: Option<&str>,
        origin: ExtensionOrigin,
    ) -> Result<RegistrationPublication, ExtensionError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ExtensionError::Internal(
                "extension runner is shutting down".into(),
            ));
        }
        let id = manifest.id().to_owned();
        let capabilities = manifest.capabilities().to_vec();

        if self
            .registry
            .extensions
            .read()
            .await
            .iter()
            .any(|hosted| hosted.manifest.id() == id)
        {
            tracing::warn!(extension_id = %id, "extension already registered, skipping duplicate");
            self.record_stage_result(
                &id,
                DiagnosticStage::Register,
                Some(Duration::ZERO),
                StageOutcome::Skipped,
            );
            return Ok(RegistrationPublication::Skipped);
        }

        // register() 只收集声明；start() 才进入运行态。
        self.record_stage_running(&id, DiagnosticStage::Register);
        let register_started = std::time::Instant::now();
        let mut reg = Registrar::new();
        ext.register(&mut reg);
        let (manifest, registrations) = match reg.finish(manifest) {
            Ok(resolved) => resolved,
            Err(error) => {
                let error = ExtensionError::from(error);
                self.record_stage_result(
                    &id,
                    DiagnosticStage::Register,
                    Some(register_started.elapsed()),
                    StageOutcome::Failed(error.to_string()),
                );
                return Err(error);
            },
        };
        let conflict = {
            let extensions = self.registry.extensions.read().await;
            let existing_manifests = extensions
                .iter()
                .map(|hosted| &hosted.manifest)
                .collect::<Vec<_>>();
            validate_registration_conflicts(&id, &registrations, &existing_manifests)
        };
        if let Err(error) = conflict {
            self.record_stage_result(
                &id,
                DiagnosticStage::Register,
                Some(register_started.elapsed()),
                StageOutcome::Failed(error.to_string()),
            );
            return Err(error);
        }
        self.record_stage_result(
            &id,
            DiagnosticStage::Register,
            Some(register_started.elapsed()),
            StageOutcome::Succeeded,
        );

        let tasks = suspended_extension_tasks(id.clone());
        let ext_config = serde_json::Value::Null;
        let runtime_config = extension_config(&id, ext_config.clone());
        let generation_gate = ExtensionGenerationGate::candidate();
        let instance_id = ExtensionInstanceId::new();

        self.record_stage_running(&id, DiagnosticStage::Start);
        let start_started = std::time::Instant::now();
        if let Err(error) = ext.validate_config(&runtime_config) {
            self.record_stage_result(
                &id,
                DiagnosticStage::Start,
                Some(start_started.elapsed()),
                StageOutcome::Failed(error.to_string()),
            );
            return Err(error);
        }

        let startup_event_tx = self.bindings.read().startup_event_tx.clone();
        let call = retain_call_cancellation(
            self.extension_call_context_factory()
                .make_extension_call_context(
                    &id,
                    instance_id,
                    &capabilities,
                    registrations.custom_event_declarations(),
                    tasks.clone(),
                    ExtensionCallContextInput {
                        working_dir: startup_working_dir.map(std::path::PathBuf::from),
                        event_tx: startup_event_tx,
                        generation_gate: generation_gate.clone(),
                        ..ExtensionCallContextInput::unscoped(tasks.cancellation())
                    },
                ),
        );
        let ctx = extension_start_context(
            call,
            tasks.clone(),
            runtime_config,
            startup_working_dir.map(std::path::PathBuf::from),
        );
        let pending_registration = self.retirements.pending_registration(
            PendingRegistrationResources {
                extension_id: id.clone(),
                instance_id,
                extension: Arc::clone(&ext),
                tasks: tasks.clone(),
                generation_gate: generation_gate.clone(),
                host_router: self.host_router(),
            },
            operation_guard,
            self.operation_timeout,
        );
        let start_result = self.run_with_timeout(ext.start(ctx)).await;
        if let Err(error) = start_result {
            self.record_stage_result(
                &id,
                DiagnosticStage::Start,
                Some(start_started.elapsed()),
                StageOutcome::Failed(error.to_string()),
            );
            let retirement = match pending_registration.retire() {
                Ok(retirement) => retirement,
                Err(rollback_error) => {
                    tracing::warn!(
                        extension_id = %id,
                        error = %rollback_error,
                        "extension startup rollback handoff failed"
                    );
                    return Err(error);
                },
            };
            return Ok(RegistrationPublication::StartupFailed {
                extension_id: id,
                error,
                retirement,
            });
        }
        self.record_stage_result(
            &id,
            DiagnosticStage::Start,
            Some(start_started.elapsed()),
            StageOutcome::Succeeded,
        );
        let operation_guard = pending_registration
            .disarm()
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;

        let supervisor = ExtensionSupervisor::spawn(id.clone());
        {
            let mut extensions = self.registry.extensions.write().await;
            extensions.push(HostedExtension {
                extension: ext,
                manifest: ResolvedExtensionManifest {
                    author: manifest,
                    registrations,
                },
                origin,
                instance_id,
                tasks: tasks.clone(),
                operation_gate,
                supervisor,
                publication_lease: ExtensionPublicationLease::new(),
                generation_gate: generation_gate.clone(),
            });
            self.rebuild_index_before_stable(&extensions, || {
                generation_gate.activate();
                activate_extension_tasks(&tasks);
            });
        }
        drop(operation_guard);
        Ok(RegistrationPublication::Published(tasks))
    }

    async fn finish_registration(
        &self,
        publication: RegistrationPublication,
    ) -> Result<Option<ExtensionTasks>, ExtensionError> {
        match publication {
            RegistrationPublication::Published(tasks) => Ok(Some(tasks)),
            RegistrationPublication::Skipped => Ok(None),
            RegistrationPublication::StartupFailed {
                extension_id,
                error,
                retirement,
            } => {
                if let Err(rollback_error) = retirement.wait().await {
                    tracing::warn!(
                        %extension_id,
                        error = %rollback_error,
                        "extension startup rollback failed"
                    );
                }
                Err(error)
            },
        }
    }

    async fn lock_registration_operation(
        &self,
        extension_id: &str,
    ) -> (Arc<AsyncMutex<()>>, OwnedMutexGuard<()>) {
        let operation_gate = self.retirements.operation_gate(extension_id);
        let operation_guard = Arc::clone(&operation_gate).lock_owned().await;
        // A completed stop failure belongs to the retirement waiter or shutdown report; crossing
        // this barrier must not reclassify it as a failure of the replacement registration.
        (operation_gate, operation_guard)
    }

    /// 注销一个扩展，并重建分发表。
    ///
    /// 返回是否从当前分发表移除了该扩展。generation gate 会先关闭并排空调用，
    /// 停止失败会记录日志，并由等待该退休结果的调用方或 [`Self::shutdown`] 汇总。
    pub async fn unregister(
        &self,
        extension_id: &str,
        reason: StopReason,
    ) -> Result<bool, ExtensionError> {
        let _source_transaction = self.coordination.source_reconcile.lock().await;
        let removed = self
            .unregister_with_retirement(extension_id, reason)
            .await?
            .is_some();
        Ok(removed)
    }

    pub(crate) async fn unregister_with_retirement(
        &self,
        extension_id: &str,
        reason: StopReason,
    ) -> Result<Option<RetirementTicket>, ExtensionError> {
        let (operation_guard, supervisor, _lifecycle) = loop {
            let lifecycle_handles =
                self.registry
                    .extensions
                    .read()
                    .await
                    .iter()
                    .find_map(|hosted| {
                        (hosted.manifest.id() == extension_id).then(|| {
                            (
                                Arc::clone(&hosted.operation_gate),
                                hosted.supervisor.control(),
                            )
                        })
                    });
            let operation_guard = match &lifecycle_handles {
                Some((gate, _)) => Some(Arc::clone(gate).lock_owned().await),
                None => None,
            };
            let supervisor = lifecycle_handles
                .as_ref()
                .map(|(_, supervisor)| supervisor.clone());
            if let Some(supervisor) = &supervisor {
                supervisor.begin_draining().await?;
            }
            let lifecycle = self.coordination.registry.lock().await;
            let handles_are_current = self
                .registry
                .extensions
                .read()
                .await
                .iter()
                .find(|hosted| hosted.manifest.id() == extension_id)
                .map(|hosted| {
                    lifecycle_handles.as_ref().is_some_and(|(gate, control)| {
                        Arc::ptr_eq(gate, &hosted.operation_gate)
                            && control.same_generation(&hosted.supervisor.control())
                    })
                })
                .unwrap_or(lifecycle_handles.is_none());
            if handles_are_current {
                break (operation_guard, supervisor, lifecycle);
            }
        };
        let hosted = {
            let mut extensions = self.registry.extensions.write().await;
            let Some(position) = extensions
                .iter()
                .position(|hosted| hosted.manifest.id() == extension_id)
            else {
                return Ok(None);
            };
            let hosted = extensions.remove(position);
            self.rebuild_index(&extensions);
            hosted
        };

        let operation_guard = operation_guard.ok_or_else(|| {
            ExtensionError::Internal(format!(
                "extension {extension_id} lost its lifecycle gate during retirement"
            ))
        })?;
        let supervisor = supervisor.ok_or_else(|| {
            ExtensionError::Internal(format!(
                "extension {extension_id} lost its supervisor during retirement"
            ))
        })?;
        if !supervisor.same_generation(&hosted.supervisor.control()) {
            return Err(ExtensionError::Internal(format!(
                "extension {extension_id} changed generation during retirement"
            )));
        }
        self.diagnostics.write().remove(extension_id);
        let retirement = self.retirements.retire(
            hosted,
            reason,
            self.operation_timeout,
            operation_guard,
            self.host_router(),
        );
        drop(_lifecycle);
        Ok(Some(retirement))
    }

    /// 停止所有已注册扩展。用于宿主进程关闭。
    pub async fn shutdown(&self) -> Vec<String> {
        let _shutdown = self.coordination.shutdown.lock().await;
        self.shutting_down.store(true, Ordering::Release);
        // 等待已进入注册/注销临界区的调用发布完最终 registry 状态。
        {
            let _lifecycle = self.coordination.registry.lock().await;
        }
        let ids = self.registered_extension_ids().await;
        let mut errors = Vec::new();
        for id in ids {
            if let Err(e) = self.unregister(&id, StopReason::Shutdown).await {
                errors.push(format!("failed to stop extension {id}: {e}"));
            }
        }
        // unregister 在释放 registry 锁前登记 retirement；此屏障保证 drain
        // 之后不会再出现属于本次 shutdown 的退休任务。
        {
            let _lifecycle = self.coordination.registry.lock().await;
        }
        errors.extend(self.retirements.drain().await);
        errors
    }

    /// 返回当前已注册扩展的 id 列表。
    pub async fn registered_extension_ids(&self) -> Vec<String> {
        self.registry
            .extensions
            .read()
            .await
            .iter()
            .map(|hosted| hosted.manifest.id().to_owned())
            .collect()
    }

    pub(crate) async fn registered_source_extensions(&self) -> Vec<RegisteredSourceExtension> {
        self.registry
            .extensions
            .read()
            .await
            .iter()
            .filter_map(|hosted| match &hosted.origin {
                ExtensionOrigin::Direct => None,
                ExtensionOrigin::Source { key, fingerprint } => Some(RegisteredSourceExtension {
                    id: hosted.manifest.id().to_owned(),
                    key: key.clone(),
                    fingerprint: fingerprint.clone(),
                }),
            })
            .collect()
    }

    pub(crate) async fn begin_source_transaction(self: &Arc<Self>) -> OwnedMutexGuard<()> {
        Arc::clone(&self.coordination.source_reconcile)
            .lock_owned()
            .await
    }

    pub(crate) async fn prepare_source_generation(
        self: &Arc<Self>,
        source_transaction: OwnedMutexGuard<()>,
        entries: Vec<SourceGenerationEntry>,
        startup_working_dir: Option<&str>,
    ) -> Result<PreparedExtensionGeneration, ExtensionError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ExtensionError::Internal(
                "extension runner is shutting down".into(),
            ));
        }

        let current_sources = self.registered_source_extensions().await;
        let desired_sources = entries
            .iter()
            .map(|entry| match entry {
                SourceGenerationEntry::Retain {
                    id,
                    key,
                    fingerprint,
                } => (id.clone(), key.clone(), fingerprint.clone()),
                SourceGenerationEntry::Start {
                    extension,
                    key,
                    fingerprint,
                    ..
                } => (
                    extension.manifest().id().to_owned(),
                    key.clone(),
                    fingerprint.clone(),
                ),
            })
            .collect::<Vec<_>>();
        let current_source_set = current_sources
            .iter()
            .map(|current| {
                (
                    current.id.clone(),
                    current.key.clone(),
                    current.fingerprint.clone(),
                )
            })
            .collect::<std::collections::HashSet<_>>();
        for entry in &entries {
            if let SourceGenerationEntry::Retain {
                id,
                key,
                fingerprint,
            } = entry
                && !current_source_set.contains(&(id.clone(), key.clone(), fingerprint.clone()))
            {
                return Err(ExtensionError::Internal(format!(
                    "retained extension {id} is not part of the active source generation"
                )));
            }
        }
        let desired_source_set = desired_sources
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let changed = entries
            .iter()
            .any(|entry| matches!(entry, SourceGenerationEntry::Start { .. }))
            || current_source_set != desired_source_set;

        let mut resolved = Vec::with_capacity(entries.len());
        for entry in entries {
            match entry {
                SourceGenerationEntry::Retain {
                    id,
                    key,
                    fingerprint,
                } => resolved.push(ResolvedSourceEntry::Retain {
                    id,
                    key,
                    fingerprint,
                }),
                SourceGenerationEntry::Start {
                    extension,
                    key,
                    fingerprint,
                    config,
                } => {
                    let manifest = extension.manifest();
                    let extension_id = manifest.id().to_owned();
                    self.record_stage_running(&extension_id, DiagnosticStage::Register);
                    let started = std::time::Instant::now();
                    let mut registrar = Registrar::new();
                    extension.register(&mut registrar);
                    let (manifest, registrations) = match registrar.finish(manifest) {
                        Ok(registration) => registration,
                        Err(error) => {
                            let error = ExtensionError::from(error);
                            self.record_stage_result(
                                &extension_id,
                                DiagnosticStage::Register,
                                Some(started.elapsed()),
                                StageOutcome::Failed(error.to_string()),
                            );
                            return Err(error);
                        },
                    };
                    self.record_stage_result(
                        &extension_id,
                        DiagnosticStage::Register,
                        Some(started.elapsed()),
                        StageOutcome::Succeeded,
                    );
                    resolved.push(ResolvedSourceEntry::Start(Box::new(
                        ResolvedSourceExtension {
                            extension,
                            manifest: ResolvedExtensionManifest {
                                author: manifest,
                                registrations,
                            },
                            key,
                            fingerprint,
                            config,
                        },
                    )));
                },
            }
        }

        {
            let extensions = self.registry.extensions.read().await;
            let mut accepted = extensions
                .iter()
                .filter(|hosted| match &hosted.origin {
                    ExtensionOrigin::Direct => true,
                    ExtensionOrigin::Source { key, fingerprint } => resolved.iter().any(|entry| {
                        matches!(
                            entry,
                            ResolvedSourceEntry::Retain {
                                id,
                                key: retained_key,
                                fingerprint: retained_fingerprint,
                            } if hosted.manifest.id() == id
                                && key == retained_key
                                && fingerprint == retained_fingerprint
                        )
                    }),
                })
                .map(|hosted| &hosted.manifest)
                .collect::<Vec<_>>();
            for candidate in resolved.iter().filter_map(|entry| match entry {
                ResolvedSourceEntry::Retain { .. } => None,
                ResolvedSourceEntry::Start(candidate) => Some(candidate.as_ref()),
            }) {
                validate_registration_conflicts(
                    candidate.manifest.id(),
                    &candidate.manifest.registrations,
                    &accepted,
                )?;
                accepted.push(&candidate.manifest);
            }
        }

        for candidate in resolved.iter().filter_map(|entry| match entry {
            ResolvedSourceEntry::Retain { .. } => None,
            ResolvedSourceEntry::Start(candidate) => Some(candidate.as_ref()),
        }) {
            let config = extension_config(candidate.manifest.id(), candidate.config.clone());
            candidate.extension.validate_config(&config)?;
        }

        let retained = resolved
            .iter()
            .filter_map(|entry| match entry {
                ResolvedSourceEntry::Retain {
                    id,
                    key,
                    fingerprint,
                } => Some((id.clone(), key.clone(), fingerprint.clone())),
                ResolvedSourceEntry::Start(_) => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let retiring_gates = self
            .registry
            .extensions
            .read()
            .await
            .iter()
            .filter_map(|hosted| match &hosted.origin {
                ExtensionOrigin::Direct => None,
                ExtensionOrigin::Source { key, fingerprint }
                    if retained.contains(&(
                        hosted.manifest.id().to_owned(),
                        key.clone(),
                        fingerprint.clone(),
                    )) =>
                {
                    None
                },
                ExtensionOrigin::Source { .. } => Some(Arc::clone(&hosted.operation_gate)),
            })
            .collect::<Vec<_>>();
        let mut prepared_generation = PreparedExtensionGeneration {
            runner: Arc::clone(self),
            _source_transaction: Some(source_transaction),
            entries: Vec::with_capacity(resolved.len()),
            retiring_gates,
            changed,
        };
        for entry in resolved {
            let prepared = match entry {
                ResolvedSourceEntry::Retain {
                    id,
                    key,
                    fingerprint,
                } => PreparedSourceEntry::Retain {
                    id,
                    key,
                    fingerprint,
                },
                ResolvedSourceEntry::Start(candidate) => match self
                    .start_source_candidate(*candidate, startup_working_dir)
                    .await
                {
                    Ok(candidate) => PreparedSourceEntry::Start(Box::new(candidate)),
                    Err(error) => {
                        prepared_generation.abort().await;
                        return Err(error);
                    },
                },
            };
            prepared_generation.entries.push(prepared);
        }

        Ok(prepared_generation)
    }

    async fn start_source_candidate(
        &self,
        candidate: ResolvedSourceExtension,
        startup_working_dir: Option<&str>,
    ) -> Result<PreparedSourceExtension, ExtensionError> {
        let ResolvedSourceExtension {
            extension,
            manifest,
            key,
            fingerprint,
            config,
        } = candidate;
        let extension_id = manifest.id().to_owned();
        let capabilities = manifest.capabilities().to_vec();
        let tasks = suspended_extension_tasks(extension_id.clone());
        let generation_gate = ExtensionGenerationGate::candidate();
        let instance_id = ExtensionInstanceId::new();
        let operation_gate = Arc::new(AsyncMutex::new(()));
        let operation_guard = Arc::clone(&operation_gate).lock_owned().await;
        let runtime_config = extension_config(&extension_id, config);
        let startup_event_tx = self.bindings.read().startup_event_tx.clone();
        let call = retain_call_cancellation(
            self.extension_call_context_factory()
                .make_extension_call_context(
                    &extension_id,
                    instance_id,
                    &capabilities,
                    manifest.registrations.custom_event_declarations(),
                    tasks.clone(),
                    ExtensionCallContextInput {
                        working_dir: startup_working_dir.map(std::path::PathBuf::from),
                        event_tx: startup_event_tx,
                        generation_gate: generation_gate.clone(),
                        ..ExtensionCallContextInput::unscoped(tasks.cancellation())
                    },
                ),
        );
        let context = extension_start_context(
            call,
            tasks.clone(),
            runtime_config,
            startup_working_dir.map(std::path::PathBuf::from),
        );
        let pending = self.retirements.pending_registration(
            PendingRegistrationResources {
                extension_id: extension_id.clone(),
                instance_id,
                extension: Arc::clone(&extension),
                tasks: tasks.clone(),
                generation_gate: generation_gate.clone(),
                host_router: self.host_router(),
            },
            operation_guard,
            self.operation_timeout,
        );

        self.record_stage_running(&extension_id, DiagnosticStage::Start);
        let started = std::time::Instant::now();
        if let Err(error) = self.run_with_timeout(extension.start(context)).await {
            self.record_stage_result(
                &extension_id,
                DiagnosticStage::Start,
                Some(started.elapsed()),
                StageOutcome::Failed(error.to_string()),
            );
            if let Ok(retirement) = pending.retire()
                && let Err(rollback) = retirement.wait().await
            {
                tracing::warn!(extension_id, error = %rollback, "candidate startup rollback failed");
            }
            return Err(error);
        }
        self.record_stage_result(
            &extension_id,
            DiagnosticStage::Start,
            Some(started.elapsed()),
            StageOutcome::Succeeded,
        );
        let operation_guard = pending
            .disarm()
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;
        Ok(PreparedSourceExtension {
            hosted: HostedExtension {
                extension,
                manifest,
                origin: ExtensionOrigin::Source { key, fingerprint },
                instance_id,
                tasks,
                operation_gate,
                supervisor: ExtensionSupervisor::spawn(extension_id),
                publication_lease: ExtensionPublicationLease::new(),
                generation_gate,
            },
            operation_guard,
        })
    }

    async fn abort_prepared_source_extensions(&self, prepared: Vec<PreparedSourceExtension>) {
        let retirements = prepared
            .into_iter()
            .map(|prepared| {
                self.retirements.retire(
                    prepared.hosted,
                    StopReason::StartupFailed,
                    self.operation_timeout,
                    prepared.operation_guard,
                    self.host_router(),
                )
            })
            .collect::<Vec<_>>();
        for retirement in retirements {
            if let Err(error) = retirement.wait().await {
                tracing::warn!(%error, "candidate extension rollback failed");
            }
        }
    }

    fn rebuild_index(&self, extensions: &[HostedExtension]) {
        self.rebuild_index_before_stable(extensions, || {});
    }

    fn rebuild_index_before_stable(
        &self,
        extensions: &[HostedExtension],
        before_stable: impl FnOnce(),
    ) {
        log_handler_dispatch_order(extensions);
        let _publication = RuntimePublicationGuard::begin(
            &self.registry.publication,
            &self.registry.publication_stable,
        );
        let generation = {
            let mut publication = self.registry.publication.lock();
            let generation = publication.generation.wrapping_add(1);
            publication.pending_generation = Some(generation);
            generation
        };
        let index = Arc::new(build_handler_index(extensions, generation));
        for hosted in extensions {
            hosted.supervisor.mark_ready(generation);
        }
        self.registry.index.store(index);
        before_stable();
    }

    /// 绑定会话原子操作能力。
    pub fn bind_session_ops(&self, ops: Arc<dyn SessionOperations>) {
        let session_ops = self.session_ops_ref();
        *session_ops
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(ops);
    }

    /// 获取共享的 session_ops 引用（供 HandlerTool 使用）。
    pub fn session_ops_ref(&self) -> Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>> {
        Arc::clone(&self.bindings.read().session_ops)
    }

    pub async fn count(&self) -> usize {
        self.registry.extensions.read().await.len()
    }

    /// 为后续启动的扩展绑定启动阶段自定义事件通道。
    ///
    /// 该通道不属于某个 session；宿主负责决定如何消费这些进程级事件。
    pub fn bind_startup_event_channel(&self, event_tx: mpsc::UnboundedSender<EventPayload>) {
        self.bindings.write().startup_event_tx = Some(event_tx.into());
    }
}

impl ExtensionView {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    fn spawn_extension_task<F>(&self, extension_id: &str, task_name: &'static str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let generation = self.index.extensions.get(extension_id);
        if let Some(generation) = generation {
            generation.tasks.spawn(task_name, fut);
        } else {
            tracing::debug!(
                extension_id,
                task = task_name,
                "skip spawning task for stopped extension"
            );
        }
    }

    #[tracing::instrument(
        name = "extension.invoke",
        skip(self, cancellation, future),
        fields(extension_id, operation = hook_name, generation = self.generation)
    )]
    async fn run_recorded_hook<T>(
        &self,
        extension_id: &str,
        hook_name: &'static str,
        cancellation: tokio_util::sync::CancellationToken,
        future: impl std::future::Future<Output = Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        let _call_lifetime = cancellation.clone().drop_guard();
        let admission = self.index.extensions.get(extension_id).ok_or_else(|| {
            ExtensionError::NotFound(format!(
                "extension {extension_id} generation is no longer available"
            ))
        })?;
        let draining = admission.admission.draining_token();
        let _admission = admission.admission.acquire().await?;
        let started = std::time::Instant::now();
        let future = tokio::time::timeout(self.operation_timeout, future);
        tokio::pin!(future);
        let outcome = tokio::select! {
            biased;
            outcome = &mut future => outcome,
            () = draining.cancelled() => {
                cancellation.cancel();
                future.await
            },
        };
        match outcome {
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
                cancellation.cancel();
                let error = ExtensionError::Timeout(self.operation_timeout.as_millis() as u64);
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

    /// 按 [`HookMode`] 分发单个 handler 调用。
    ///
    /// Blocking 记录诊断并返回 `Ok(Some(result))`;Advisory 记录诊断但仅告警吞错;
    /// NonBlocking 派生扩展任务即发即弃。后两者返回 `Ok(None)`。
    /// `names` 是 `(诊断记录的 hook 名, 派生任务与告警文案的 task 名)`——provider
    /// 钩子两者不同(诊断按事件区分,任务沿用 "provider")。
    async fn dispatch_hook_by_mode<H, F, Fut, R>(
        &self,
        extension_id: &str,
        names: (&'static str, &'static str),
        mode: HookMode,
        handler: &Arc<H>,
        invoke: F,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Option<R>, ExtensionError>
    where
        H: ?Sized + Send + Sync + 'static,
        F: FnOnce(Arc<H>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<R, ExtensionError>> + Send + 'static,
        R: Send + 'static,
    {
        let (hook_name, task_name) = names;
        match mode {
            HookMode::Blocking => self
                .run_recorded_hook(
                    extension_id,
                    hook_name,
                    cancellation,
                    invoke(Arc::clone(handler)),
                )
                .await
                .map(Some),
            HookMode::Advisory => {
                if let Err(e) = self
                    .run_recorded_hook(
                        extension_id,
                        hook_name,
                        cancellation,
                        invoke(Arc::clone(handler)),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "advisory {} handler failed", task_name);
                }
                Ok(None)
            },
            HookMode::NonBlocking => {
                let handler = Arc::clone(handler);
                self.spawn_extension_task(extension_id, task_name, async move {
                    let _call_lifetime = cancellation.drop_guard();
                    if let Err(e) = invoke(handler).await {
                        tracing::warn!(error = %e, "non-blocking {} handler failed", task_name);
                    }
                });
                Ok(None)
            },
        }
    }

    fn make_hook_call_context(
        &self,
        extension_id: &str,
        runtime: &RuntimeHookCallContext,
    ) -> Result<(ExtensionCallContext, tokio_util::sync::CancellationToken), ExtensionError> {
        let caller_cancellation = runtime.cancellation().child_token();
        let call = self.make_registered_extension_call_context(
            extension_id,
            ExtensionCallContextInput::from_hook(runtime, caller_cancellation),
        )?;
        let cancellation = call.cancellation().clone();
        Ok((call, cancellation))
    }

    fn record_hook_result(
        &self,
        extension_id: &str,
        hook: &'static str,
        elapsed: Duration,
        error: Option<String>,
        timed_out: bool,
    ) {
        let mut diagnostics = self.diagnostics.write();
        let entry = diagnostics.entry(extension_id.to_string()).or_default();
        entry.hook_calls = entry.hook_calls.saturating_add(1);
        if timed_out {
            entry.hook_timeouts = entry.hook_timeouts.saturating_add(1);
        }
        entry.last_hook = Some(hook.to_string());
        entry.last_duration_ms = Some(elapsed.as_millis() as u64);
        entry.last_error = error;
    }

    // ─── 类型化分发方法 ──────────────────────────────────────────────

    /// 按确定顺序折叠所有工具参数变换。
    pub async fn transform_tool_input(
        &self,
        mut ctx: RuntimePreToolUseContext,
    ) -> Result<serde_json::Value, ExtensionError> {
        for (extension_id, target, handler) in &self.index.tool_input_transform {
            if !target.matches(ctx.tool_name()) {
                continue;
            }
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let handler = Arc::clone(handler);
            match self
                .run_recorded_hook(
                    extension_id,
                    "tool_input_transform",
                    cancellation,
                    handler.transform(handler_ctx),
                )
                .await?
            {
                ToolInputTransformResult::Unchanged => {},
                ToolInputTransformResult::Replace { tool_input } => {
                    replace_pre_tool_input(&mut ctx, tool_input);
                },
            }
        }
        Ok(ctx.tool_input().clone())
    }

    /// 在同一份最终参数上组合所有 PreToolUse 准入决策。
    pub async fn emit_pre_tool_use(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<PreToolUseAdmission, ExtensionError> {
        let mut requirements = Vec::new();
        for (extension_id, target, handler) in &self.index.pre_tool_use {
            if !target.matches(ctx.tool_name()) {
                continue;
            }
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let handler = Arc::clone(handler);
            let result = self
                .run_recorded_hook(
                    extension_id,
                    "pre_tool_use",
                    cancellation,
                    handler.handle(handler_ctx),
                )
                .await?;
            match result {
                PreToolUseResult::Block { reason } => {
                    return Ok(PreToolUseAdmission::Block { reason });
                },
                PreToolUseResult::Ask { prompt, rule_key } => {
                    requirements.push(PreToolUseRequirement {
                        prompt,
                        rule_key: rule_key
                            .map(|rule_key| format!("extension:{extension_id}:{rule_key}")),
                    });
                },
                PreToolUseResult::Allow => {},
            }
        }
        if requirements.is_empty() {
            Ok(PreToolUseAdmission::Allow)
        } else {
            Ok(PreToolUseAdmission::Ask { requirements })
        }
    }

    /// PostToolUse 钩子分发。
    pub async fn emit_post_tool_use(
        &self,
        ctx: RuntimePostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        let index = &self.index;
        let mut ctx = ctx;
        let mut modified = false;

        for (extension_id, mode, target, handler) in &index.post_tool_use {
            if !target.matches(ctx.tool_name()) {
                continue;
            }
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let Some(result) = self
                .dispatch_hook_by_mode(
                    extension_id,
                    ("post_tool_use", "post_tool_use"),
                    *mode,
                    handler,
                    move |handler: Arc<dyn PostToolUseHandler>| async move {
                        handler.handle(handler_ctx).await
                    },
                    cancellation,
                )
                .await?
            else {
                continue;
            };
            match result {
                PostToolUseResult::Block { reason } => {
                    return Ok(PostToolUseResult::Block { reason });
                },
                PostToolUseResult::ModifyResult { content } => {
                    replace_post_tool_result(&mut ctx, content);
                    modified = true;
                },
                PostToolUseResult::Allow => {},
            }
        }
        if modified {
            Ok(PostToolUseResult::ModifyResult {
                content: ctx.tool_result().content.clone(),
            })
        } else {
            Ok(PostToolUseResult::Allow)
        }
    }

    /// Provider 钩子分发。
    pub async fn emit_provider(
        &self,
        event: ProviderEvent,
        ctx: RuntimeProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        let index = &self.index;
        let handlers = index.provider.get(&event);

        let Some(handlers) = handlers else {
            return Ok(ProviderResult::Allow);
        };

        let mut ctx = ctx;
        let mut modified = false;
        for (extension_id, mode, handler) in handlers {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let Some(result) = self
                .dispatch_hook_by_mode(
                    extension_id,
                    (provider_hook_name(event), "provider"),
                    *mode,
                    handler,
                    move |handler: Arc<dyn ProviderHandler>| async move {
                        handler.handle(handler_ctx).await
                    },
                    cancellation,
                )
                .await?
            else {
                continue;
            };
            match result {
                ProviderResult::Block { reason } => {
                    return Ok(ProviderResult::Block { reason });
                },
                ProviderResult::ReplaceMessages { messages } => {
                    replace_provider_messages(&mut ctx, messages);
                    modified = true;
                },
                ProviderResult::AppendMessages { messages } => {
                    append_provider_messages(&mut ctx, messages);
                    modified = true;
                },
                ProviderResult::Allow => {},
            }
        }
        if modified {
            Ok(ProviderResult::ReplaceMessages {
                messages: ctx.messages().to_vec(),
            })
        } else {
            Ok(ProviderResult::Allow)
        }
    }

    /// Prepare request-local provider contributions without consuming their backing state.
    pub async fn prepare_provider_request(
        &self,
        ctx: RuntimeProviderContext,
    ) -> Result<ProviderRequestPreparation, ExtensionError> {
        let mut ctx = ctx;
        let mut modified = false;
        match self
            .emit_provider(ProviderEvent::BeforeRequest, ctx.clone())
            .await?
        {
            ProviderResult::Block { reason } => {
                return Ok(ProviderRequestPreparation::without_acknowledgements(
                    ProviderResult::Block { reason },
                ));
            },
            ProviderResult::ReplaceMessages { messages } => {
                replace_provider_messages(&mut ctx, messages);
                modified = true;
            },
            ProviderResult::AppendMessages { messages } => {
                append_provider_messages(&mut ctx, messages);
                modified = true;
            },
            ProviderResult::Allow => {},
        }

        let mut acknowledgements = ProviderRequestAcknowledgements::default();
        for (extension_id, handler) in &self.index.provider_contributions {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let Some(contribution) = self
                .run_recorded_hook(
                    extension_id,
                    "provider_contribution_prepare",
                    cancellation,
                    handler.prepare(handler_ctx),
                )
                .await?
            else {
                continue;
            };
            let (contribution_id, effect) = contribution.into_parts();
            acknowledgements.push_runtime(
                extension_id.clone(),
                Arc::clone(handler),
                contribution_id,
            );
            match effect {
                PreparedProviderEffect::Unchanged => {},
                PreparedProviderEffect::ReplaceMessages(messages) => {
                    replace_provider_messages(&mut ctx, messages);
                    modified = true;
                },
                PreparedProviderEffect::AppendMessages(messages) => {
                    append_provider_messages(&mut ctx, messages);
                    modified = true;
                },
            }
        }
        let result = if modified {
            ProviderResult::ReplaceMessages {
                messages: ctx.messages().to_vec(),
            }
        } else {
            ProviderResult::Allow
        };
        Ok(ProviderRequestPreparation::from_runtime(
            result,
            acknowledgements,
        ))
    }

    /// Acknowledge every contribution prepared by this exact extension view.
    pub async fn acknowledge_provider_request(
        &self,
        ctx: RuntimeProviderSettlementContext,
        acknowledgements: ProviderRequestAcknowledgements,
    ) -> Result<(), ExtensionError> {
        let mut first_error = None;
        for (extension_id, handler, contribution_id) in acknowledgements.into_runtime_entries() {
            let result = match self.make_hook_call_context(&extension_id, ctx.call()) {
                Ok((call, cancellation)) => {
                    let handler_ctx =
                        author_provider_settlement_context(call, &ctx, contribution_id);
                    self.run_recorded_hook(
                        &extension_id,
                        "provider_contribution_acknowledge",
                        cancellation,
                        handler.acknowledge(handler_ctx),
                    )
                    .await
                },
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                tracing::warn!(
                    extension_id,
                    error = %error,
                    "provider contribution acknowledgement failed"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// PromptBuild 贡献收集。
    pub async fn collect_prompt_contributions_typed(
        &self,
        ctx: RuntimePromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        let index = &self.index;

        let mut collected = PromptContributions::default();
        for (extension_id, handler) in &index.prompt_build {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let contributions = self
                .run_recorded_hook(
                    extension_id,
                    "prompt_build",
                    cancellation,
                    handler.handle(handler_ctx),
                )
                .await?;
            collected.merge(contributions);
        }
        Ok(collected)
    }

    /// Collect ordered contributions before compacting.
    pub async fn collect_pre_compact(
        &self,
        ctx: RuntimePreCompactContext,
    ) -> Result<PreCompactResult, ExtensionError> {
        let index = &self.index;
        let mut collected = CompactContributions::default();
        for (extension_id, handler) in &index.pre_compact {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let result = self
                .run_recorded_hook(
                    extension_id,
                    "pre_compact",
                    cancellation,
                    handler.handle(handler_ctx),
                )
                .await?;
            match result {
                PreCompactResult::Block { reason } => {
                    return Ok(PreCompactResult::Block { reason });
                },
                PreCompactResult::Contributions(c) => {
                    collected.merge(c);
                },
                PreCompactResult::Allow => {},
            }
        }
        if collected.instructions.is_empty() && collected.retained_context.is_empty() {
            Ok(PreCompactResult::Allow)
        } else {
            Ok(PreCompactResult::Contributions(collected))
        }
    }

    /// Notify extensions only after the compact rewrite is durable.
    pub async fn notify_post_compact(
        &self,
        ctx: RuntimePostCompactContext,
    ) -> Result<(), ExtensionError> {
        let index = &self.index;
        for (extension_id, handler) in &index.post_compact {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            self.run_recorded_hook(
                extension_id,
                "post_compact",
                cancellation,
                handler.handle(handler_ctx),
            )
            .await?;
        }
        Ok(())
    }

    /// LLM 自然结束（无 tool call）后询问扩展是否再跑一个 step。
    ///
    /// 按优先级降序；首个返回 [`ContinueAfterStopResult::ContinueOneStep`] 的 blocking
    /// handler 生效。每个 handler 的每轮预算由插件注册时声明。
    pub async fn emit_continue_after_stop(
        &self,
        ctx: RuntimeContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        let index = &self.index;
        for (extension_id, options, handler) in &index.continue_after_stop {
            if !options.allows(ctx.continuations_this_turn()) {
                tracing::debug!(
                    extension_id = %extension_id,
                    continuations_this_turn = ctx.continuations_this_turn(),
                    "ContinueAfterStop: extension continuation limit exhausted"
                );
                continue;
            }
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let result = self
                .run_recorded_hook(
                    extension_id,
                    "continue_after_stop",
                    cancellation,
                    handler.handle(handler_ctx),
                )
                .await?;
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
        ctx: RuntimeUserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        let index = &self.index;
        let mut ctx = ctx;
        let mut modified = false;
        for (extension_id, handler) in &index.user_message_envelope {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let result = self
                .run_recorded_hook(
                    extension_id,
                    "user_message_envelope",
                    cancellation,
                    handler.handle(handler_ctx),
                )
                .await?;

            match result {
                UserMessageEnvelopeResult::Allow => {},
                UserMessageEnvelopeResult::ReplaceText { text } => {
                    replace_user_message_text(&mut ctx, text);
                    modified = true;
                },
                UserMessageEnvelopeResult::AppendText { text } => {
                    append_user_message_text(&mut ctx, &text);
                    modified = true;
                },
                UserMessageEnvelopeResult::Block { reason } => {
                    return Ok(UserMessageEnvelopeResult::Block { reason });
                },
            }
        }

        if modified {
            Ok(UserMessageEnvelopeResult::ReplaceText {
                text: ctx.text().to_owned(),
            })
        } else {
            Ok(UserMessageEnvelopeResult::Allow)
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
        event: LifecycleEvent,
        ctx: RuntimeLifecycleContext,
    ) -> Result<(), ExtensionError> {
        let index = &self.index;
        let Some(handlers) = index.lifecycle.get(&event) else {
            return Ok(());
        };

        for (extension_id, mode, handler) in handlers {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = author_hook_context(call, &ctx);
            let Some(result) = self
                .dispatch_hook_by_mode(
                    extension_id,
                    ("lifecycle", "lifecycle"),
                    *mode,
                    handler,
                    move |handler: Arc<dyn LifecycleHandler>| async move {
                        handler.handle(handler_ctx).await
                    },
                    cancellation,
                )
                .await?
            else {
                continue;
            };
            if let HookResult::Block { reason } = result {
                return Err(ExtensionError::Blocked { reason });
            }
        }
        Ok(())
    }
}

impl ExtensionRunner {
    /// Captures one stable handler index, waiting for an in-progress publication batch.
    pub(crate) async fn extension_view(&self) -> Arc<ExtensionView> {
        loop {
            let publication_stable = self.registry.publication_stable.notified();
            tokio::pin!(publication_stable);
            publication_stable.as_mut().enable();
            let stable_index = {
                let publication = self.registry.publication.lock();
                let index = self.load_index();
                publication
                    .is_stable_generation(index.generation)
                    .then_some(index)
            };
            if let Some(index) = stable_index {
                return self.extension_view_for_index(index);
            }
            publication_stable.await;
        }
    }

    fn turn_extension_view(&self) -> Arc<ExtensionView> {
        self.extension_view_for_index(self.load_index())
    }

    fn extension_view_for_index(&self, index: Arc<HandlerIndex>) -> Arc<ExtensionView> {
        Arc::new(ExtensionView {
            generation: index.generation,
            index,
            diagnostics: Arc::clone(&self.diagnostics),
            operation_timeout: self.operation_timeout,
            call_context_factory: self.extension_call_context_factory(),
            custom_event_permits: Arc::clone(&self.custom_event_permits),
            custom_event_lanes: Arc::clone(&self.custom_event_lanes),
            custom_event_quiescing: Arc::clone(&self.custom_event_quiescing),
        })
    }

    async fn run_with_timeout<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        run_with_timeout(self.operation_timeout, future).await
    }

    pub async fn transform_tool_input(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<serde_json::Value, ExtensionError> {
        self.extension_view().await.transform_tool_input(ctx).await
    }

    pub async fn emit_pre_tool_use(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<PreToolUseAdmission, ExtensionError> {
        self.extension_view().await.emit_pre_tool_use(ctx).await
    }

    pub async fn emit_post_tool_use(
        &self,
        ctx: RuntimePostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        self.extension_view().await.emit_post_tool_use(ctx).await
    }

    pub async fn emit_provider(
        &self,
        event: ProviderEvent,
        ctx: RuntimeProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        self.extension_view().await.emit_provider(event, ctx).await
    }

    pub async fn collect_prompt_contributions_typed(
        &self,
        ctx: RuntimePromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        self.extension_view()
            .await
            .collect_prompt_contributions_typed(ctx)
            .await
    }

    pub async fn collect_pre_compact(
        &self,
        ctx: RuntimePreCompactContext,
    ) -> Result<PreCompactResult, ExtensionError> {
        self.extension_view().await.collect_pre_compact(ctx).await
    }

    pub async fn notify_post_compact(
        &self,
        ctx: RuntimePostCompactContext,
    ) -> Result<(), ExtensionError> {
        self.extension_view().await.notify_post_compact(ctx).await
    }

    pub async fn emit_continue_after_stop(
        &self,
        ctx: RuntimeContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        self.extension_view()
            .await
            .emit_continue_after_stop(ctx)
            .await
    }

    pub async fn emit_user_message_envelope(
        &self,
        ctx: RuntimeUserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        self.extension_view()
            .await
            .emit_user_message_envelope(ctx)
            .await
    }

    pub async fn emit_lifecycle(
        &self,
        event: LifecycleEvent,
        ctx: RuntimeLifecycleContext,
    ) -> Result<(), ExtensionError> {
        self.extension_view().await.emit_lifecycle(event, ctx).await
    }
}

impl TurnExtensionViewProvider for ExtensionRunner {
    fn turn_extension_view(&self) -> RuntimeTurnExtensionView {
        let view = ExtensionRunner::turn_extension_view(self);
        let tool_catalog: Arc<dyn ToolCatalogProvider> = view.clone();
        let prompt_contributor: Arc<dyn PromptContributor> = view.clone();
        let turn_hooks: Arc<dyn TurnHooks> = view.clone();
        RuntimeTurnExtensionView::new(
            view.generation(),
            tool_catalog,
            prompt_contributor,
            turn_hooks,
        )
    }
}

#[async_trait::async_trait]
impl TurnHooks for ExtensionView {
    async fn transform_tool_input(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<serde_json::Value, ExtensionError> {
        ExtensionView::transform_tool_input(self, ctx).await
    }

    async fn emit_pre_tool_use(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<PreToolUseAdmission, ExtensionError> {
        ExtensionView::emit_pre_tool_use(self, ctx).await
    }

    async fn emit_post_tool_use(
        &self,
        ctx: RuntimePostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        ExtensionView::emit_post_tool_use(self, ctx).await
    }

    async fn emit_provider(
        &self,
        event: ProviderEvent,
        ctx: RuntimeProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        ExtensionView::emit_provider(self, event, ctx).await
    }

    async fn prepare_provider_request(
        &self,
        ctx: RuntimeProviderContext,
    ) -> Result<ProviderRequestPreparation, ExtensionError> {
        ExtensionView::prepare_provider_request(self, ctx).await
    }

    async fn acknowledge_provider_request(
        &self,
        ctx: RuntimeProviderSettlementContext,
        acknowledgements: ProviderRequestAcknowledgements,
    ) -> Result<(), ExtensionError> {
        ExtensionView::acknowledge_provider_request(self, ctx, acknowledgements).await
    }

    async fn collect_pre_compact(
        &self,
        ctx: RuntimePreCompactContext,
    ) -> Result<PreCompactResult, ExtensionError> {
        ExtensionView::collect_pre_compact(self, ctx).await
    }

    async fn notify_post_compact(
        &self,
        ctx: RuntimePostCompactContext,
    ) -> Result<(), ExtensionError> {
        ExtensionView::notify_post_compact(self, ctx).await
    }

    async fn emit_continue_after_stop(
        &self,
        ctx: RuntimeContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        ExtensionView::emit_continue_after_stop(self, ctx).await
    }

    async fn emit_user_message_envelope(
        &self,
        ctx: RuntimeUserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        ExtensionView::emit_user_message_envelope(self, ctx).await
    }

    async fn emit_lifecycle(
        &self,
        event: LifecycleEvent,
        ctx: RuntimeLifecycleContext,
    ) -> Result<(), ExtensionError> {
        ExtensionView::emit_lifecycle(self, event, ctx).await
    }
}

#[async_trait::async_trait]
impl PromptContributor for ExtensionView {
    async fn collect_prompt_contributions(
        &self,
        ctx: RuntimePromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        ExtensionView::collect_prompt_contributions_typed(self, ctx).await
    }
}

impl RuntimeSnapshotProvider for ExtensionRunner {
    fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
        let publication = self.registry.publication.lock();
        if publication.active_writers == 0 {
            RuntimeSnapshotState::Stable(publication.generation)
        } else {
            RuntimeSnapshotState::Updating
        }
    }
}

impl SessionOperationsProvider for ExtensionRunner {
    fn session_ops(&self) -> Option<Arc<dyn SessionOperations>> {
        let ops_ref = self.session_ops_ref();
        let guard = ops_ref.read().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    }
}

async fn run_with_timeout<T>(
    operation_timeout: Duration,
    future: impl std::future::Future<Output = Result<T, ExtensionError>>,
) -> Result<T, ExtensionError> {
    tokio::time::timeout(operation_timeout, future)
        .await
        .map_err(|_| ExtensionError::Timeout(operation_timeout.as_millis() as u64))?
}

fn provider_hook_name(event: ProviderEvent) -> &'static str {
    match event {
        ProviderEvent::BeforeRequest => "before_provider_request",
        ProviderEvent::AfterResponse => "after_provider_response",
    }
}


#[cfg(test)]
mod tests;
