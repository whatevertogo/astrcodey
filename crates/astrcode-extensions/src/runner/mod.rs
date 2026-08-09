//! 扩展运行器 — 将生命周期事件分发到已注册的扩展。

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, RwLock as StdRwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use astrcode_core::event::{
    DurableEventPayload, Event, EventDeliveryReceipt, EventPayload, EventSendError, EventSender,
};
use astrcode_extension_sdk::{
    extension::{internal::CustomEventSink, *},
    runtime_ports::{
        PromptContributor, RuntimeSnapshotProvider, RuntimeSnapshotState,
        SessionOperationsProvider, ToolCatalogProvider,
        TurnExtensionView as RuntimeTurnExtensionView, TurnExtensionViewProvider, TurnHooks,
    },
    tool::SessionOperations,
};
use astrcode_storage::{EventConsumerCheckpointOutcome, SessionStore};
use tokio::sync::{
    Mutex as AsyncMutex, Notify, OwnedMutexGuard, OwnedSemaphorePermit, RwLock, Semaphore, mpsc,
};

const CUSTOM_EVENT_DISPATCH_CAPACITY: usize = 1024;
const MAX_CUSTOM_EVENT_CASCADE_DEPTH: u8 = 8;
const CUSTOM_EVENT_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(100);
const CUSTOM_EVENT_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

type CustomEventLanes = parking_lot::Mutex<HashMap<CustomEventLaneId, Weak<CustomEventLane>>>;

type CustomEventSenderFactory =
    Arc<dyn Fn(Option<astrcode_core::types::TurnId>) -> EventSender + Send + Sync>;

fn custom_event_consumer_id(extension_id: &str, subscription: &CustomEventSubscription) -> String {
    format!("{extension_id}:{}", subscription.id)
}

#[derive(Clone)]
#[doc(hidden)]
pub struct CustomEventSession {
    event_store: Arc<dyn SessionStore>,
    event_sender: CustomEventSenderFactory,
}

impl CustomEventSession {
    pub fn new(
        event_store: Arc<dyn SessionStore>,
        event_sender: impl Fn(Option<astrcode_core::types::TurnId>) -> EventSender
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            event_store,
            event_sender: Arc::new(event_sender),
        }
    }

    fn event_sender(&self, turn_id: Option<astrcode_core::types::TurnId>) -> EventSender {
        (self.event_sender)(turn_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CustomEventLaneId {
    generation: u64,
    session_id: astrcode_core::types::SessionId,
    consumer_id: String,
}

struct CustomEventLane {
    sender: mpsc::UnboundedSender<CustomEventLaneCommand>,
    durable_reconciliation_queued: AtomicBool,
    consumer: Arc<CustomEventConsumer>,
}

enum CustomEventLaneCommand {
    Live {
        _lane: Arc<CustomEventLane>,
        _permit: OwnedSemaphorePermit,
        invocation: Box<CustomEventInvocation>,
    },
    ReconcileDurable {
        _lane: Arc<CustomEventLane>,
        session: CustomEventSession,
    },
}

struct CustomEventConsumer {
    view: Arc<ExtensionView>,
    extension_id: String,
    consumer_id: String,
    subscription: CustomEventSubscription,
    cancellation: tokio_util::sync::CancellationToken,
    handler: Arc<dyn CustomEventHandler>,
    metrics: Arc<CustomEventConsumerMetrics>,
    session_id: astrcode_core::types::SessionId,
}

#[derive(Clone)]
struct CustomEventInvocation {
    _lane: Arc<CustomEventLane>,
    view: Arc<ExtensionView>,
    extension_id: String,
    consumer_id: String,
    cancellation: tokio_util::sync::CancellationToken,
    handler: Arc<dyn CustomEventHandler>,
    metrics: Arc<CustomEventConsumerMetrics>,
    context: CustomEventContext,
    event_store: Arc<dyn SessionStore>,
    session_id: astrcode_core::types::SessionId,
    seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CustomEventInvocationOutcome {
    Consumed,
    Paused,
    Retry,
}

mod commands;
mod custom_event_control;
mod diagnostics;
mod host_invoker;
mod http;
mod index;
mod manifest;
mod registration;
mod retirement;
mod snapshot;
mod tool_adapter;

pub use commands::{
    CommandSource, ResolvedCommandSurface, ResolvedSlashCommand, ShadowedSlashCommand,
};
pub use custom_event_control::{
    CustomEventConsumerAction, CustomEventConsumerControlError, CustomEventConsumerStatus,
};
use custom_event_control::{CustomEventConsumerMetrics, CustomEventConsumerMetricsMap};
use diagnostics::{
    ExtensionDiagnosticStage as DiagnosticStage, ExtensionStageOutcome as StageOutcome,
};
pub use diagnostics::{
    ExtensionDiagnostics, ExtensionHealthReport, ExtensionStageDiagnostics, ExtensionStageStatus,
};
pub(crate) use host_invoker::transport_invoke_context;
use host_invoker::{ExtensionCallContextFactory, ExtensionCallContextInput};
pub use http::ExtensionHttpDispatchResult;
use index::{HandlerIndex, build_handler_index, log_handler_dispatch_order};
use manifest::ResolvedExtensionManifest;
use registration::validate_registration_conflicts;
use retirement::{
    ActiveTurnViewLease, ActiveTurnViews, ExtensionPublicationLease, RetirementSupervisor,
    RetirementTicket,
};
pub use snapshot::{ExtensionDeclarationSnapshot, ExtensionRegistrySnapshot};

/// 管理扩展生命周期、运行态发布与 hook 分发。
///
/// 强制执行 HookMode 语义：
/// - Blocking: 同步执行，可返回 Block 或 ModifiedInput/ModifiedResult
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
    /// 扩展专有配置映射。key 为扩展 id，value 为用户配置的 JSON。
    /// 通过 `update_extension_configs()` 替换，支持热更新。
    extension_configs: parking_lot::RwLock<BTreeMap<String, serde_json::Value>>,
    /// 宿主等待扩展控制面操作和同步 hook 的统一超时。
    operation_timeout: Duration,
    retirements: RetirementSupervisor,
    custom_event_permits: Arc<Semaphore>,
    custom_event_lanes: Arc<CustomEventLanes>,
    custom_event_metrics: CustomEventConsumerMetricsMap,
    shutting_down: AtomicBool,
}

/// Immutable registration and handler view for one extension-runtime generation.
pub(crate) struct ExtensionView {
    generation: u64,
    index: Arc<HandlerIndex>,
    diagnostics: Arc<parking_lot::RwLock<BTreeMap<String, ExtensionDiagnostics>>>,
    operation_timeout: Duration,
    pub(super) call_context_factory: ExtensionCallContextFactory,
    custom_event_permits: Arc<Semaphore>,
    custom_event_lanes: Arc<CustomEventLanes>,
    _active_turn_lease: Option<ActiveTurnViewLease>,
}

/// Keybinding and status declarations captured from one stable runtime generation.
pub struct ExtensionUiContributions {
    pub keybindings: Vec<Keybinding>,
    pub status_items: Vec<StatusItem>,
}

struct LifecycleCoordination {
    /// 串行化全局注册表生命周期变更。
    registry: AsyncMutex<()>,
    /// 串行化来源发现与增量协调，避免并发 reload 基于过期快照互相覆盖。
    source_reconcile: AsyncMutex<()>,
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
    active_turn_views: Arc<ActiveTurnViews>,
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
    tasks: ExtensionTasks,
    /// 注册时的配置快照，用于 diff 检测热更新。
    config: serde_json::Value,
    /// 串行化同一扩展的配置回调与 stop，不阻塞其他扩展。
    operation_gate: Arc<AsyncMutex<()>>,
    publication_lease: Arc<ExtensionPublicationLease>,
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

/// 保证批量注册的取消安全：已发布任务会在批次边界显式激活，
/// 中断批次则通过析构此句柄激活。
pub(crate) struct DeferredTaskActivation {
    tasks: Option<ExtensionTasks>,
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

impl DeferredTaskActivation {
    fn new(tasks: ExtensionTasks) -> Self {
        Self { tasks: Some(tasks) }
    }

    pub(crate) fn activate(mut self) {
        if let Some(tasks) = self.tasks.take() {
            tasks.activate();
        }
    }
}

impl Drop for DeferredTaskActivation {
    fn drop(&mut self) {
        if let Some(tasks) = self.tasks.take() {
            tasks.activate();
        }
    }
}

// ─── BoundCustomEventSink ─────────────────────────────────────────────────

/// 绑定了 extension_id 的事件发射器。
///
/// 由 runtime call-context factory 构造并传给扩展钩子；`extension_id` 在构造时
/// 注入，调用方无法伪造身份。
struct BoundCustomEventSink {
    extension_id: String,
    event_tx: astrcode_core::event::EventSender,
    causation: Option<(astrcode_core::types::EventId, u8)>,
}

fn bind_custom_event_sink(
    extension_id: &str,
    declarations: &[CustomEventDeclaration],
    event_tx: astrcode_core::event::EventSender,
    causation: Option<(astrcode_core::types::EventId, u8)>,
) -> Option<Arc<dyn CustomEventSink>> {
    if declarations.is_empty() {
        return None;
    }
    Some(Arc::new(BoundCustomEventSink {
        extension_id: extension_id.to_owned(),
        event_tx,
        causation,
    }))
}

#[async_trait::async_trait]
impl CustomEventSink for BoundCustomEventSink {
    async fn emit(
        &self,
        event_type: &str,
        schema_version: u32,
        durable: bool,
        payload: serde_json::Value,
    ) -> Result<EventDeliveryReceipt, EventSendError> {
        self.event_tx
            .send_confirmed(crate::host_router::custom_event_payload(
                &self.extension_id,
                event_type,
                schema_version,
                durable,
                self.causation.clone(),
                payload,
            ))
            .await
    }

    fn try_emit(
        &self,
        event_type: &str,
        schema_version: u32,
        durable: bool,
        payload: serde_json::Value,
    ) -> Result<(), EventSendError> {
        self.event_tx.send(crate::host_router::custom_event_payload(
            &self.extension_id,
            event_type,
            schema_version,
            durable,
            self.causation.clone(),
            payload,
        ))
    }
}

async fn run_custom_event_lane(
    consumer: Arc<CustomEventConsumer>,
    mut receiver: mpsc::UnboundedReceiver<CustomEventLaneCommand>,
) {
    while let Some(command) = receiver.recv().await {
        match command {
            CustomEventLaneCommand::Live {
                _permit,
                invocation,
                ..
            } => {
                let _ = run_custom_event_invocation(&invocation).await;
                drop(_permit);
            },
            CustomEventLaneCommand::ReconcileDurable { _lane, session } => {
                _lane
                    .durable_reconciliation_queued
                    .store(false, Ordering::Release);
                reconcile_durable_custom_events(&consumer, &_lane, &session).await;
            },
        }
    }
}

async fn reconcile_durable_custom_events(
    consumer: &CustomEventConsumer,
    lane: &Arc<CustomEventLane>,
    session: &CustomEventSession,
) {
    let mut retry_delay = CUSTOM_EVENT_RETRY_INITIAL_DELAY;
    loop {
        if reconcile_durable_custom_events_once(consumer, lane, session).await {
            return;
        }
        tokio::select! {
            () = consumer.cancellation.cancelled() => return,
            () = tokio::time::sleep(retry_delay) => {},
        }
        retry_delay = retry_delay
            .saturating_mul(2)
            .min(CUSTOM_EVENT_RETRY_MAX_DELAY);
    }
}

async fn reconcile_durable_custom_events_once(
    consumer: &CustomEventConsumer,
    lane: &Arc<CustomEventLane>,
    session: &CustomEventSession,
) -> bool {
    let state = match session
        .event_store
        .event_consumer_state(&consumer.session_id, &consumer.consumer_id)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            consumer.metrics.record_failure();
            tracing::warn!(
                extension_id = consumer.extension_id,
                session_id = %consumer.session_id,
                %error,
                "failed to read custom event consumer checkpoint"
            );
            return false;
        },
    };
    if state.paused {
        return true;
    }
    let stored_events = match state.checkpoint {
        Some(seq) => {
            let cursor = seq.to_string();
            session
                .event_store
                .replay_from(&consumer.session_id, &cursor)
                .await
        },
        None => {
            session
                .event_store
                .replay_events(&consumer.session_id)
                .await
        },
    };
    let stored_events = match stored_events {
        Ok(events) => events,
        Err(error) => {
            consumer.metrics.record_failure();
            tracing::warn!(
                extension_id = consumer.extension_id,
                session_id = %consumer.session_id,
                %error,
                "failed to replay durable custom events"
            );
            return false;
        },
    };

    let replay_head = stored_events.last().map(|event| event.seq);
    for stored in stored_events {
        let custom_event = match &stored.payload {
            DurableEventPayload::CustomEvent(custom_event) => custom_event,
            _ => continue,
        };
        if !consumer
            .subscription
            .matches(&custom_event.extension_id, &custom_event.event_type)
        {
            continue;
        }
        if custom_event.cascade_depth > MAX_CUSTOM_EVENT_CASCADE_DEPTH {
            tracing::warn!(
                event_id = %stored.id,
                cascade_depth = custom_event.cascade_depth,
                "custom event cascade depth exceeded"
            );
            continue;
        }

        let event = Arc::new(Event::from(stored));
        let Some(invocation) = consumer.invocation(Arc::clone(lane), &event, session) else {
            return false;
        };
        let mut retry_delay = CUSTOM_EVENT_RETRY_INITIAL_DELAY;
        loop {
            let permit = tokio::select! {
                result = Arc::clone(&consumer.view.custom_event_permits).acquire_owned() => {
                    match result {
                        Ok(permit) => permit,
                        Err(_) => return false,
                    }
                },
                () = consumer.cancellation.cancelled() => return false,
            };
            let outcome = run_custom_event_invocation(&invocation).await;
            drop(permit);
            match outcome {
                CustomEventInvocationOutcome::Consumed => break,
                CustomEventInvocationOutcome::Paused => return true,
                CustomEventInvocationOutcome::Retry => {},
            }
            tokio::select! {
                () = consumer.cancellation.cancelled() => return false,
                () = tokio::time::sleep(retry_delay) => {},
            }
            retry_delay = retry_delay
                .saturating_mul(2)
                .min(CUSTOM_EVENT_RETRY_MAX_DELAY);
        }
    }
    if let Some(replay_head) = replay_head {
        match session
            .event_store
            .checkpoint_event_consumer(
                &consumer.session_id,
                &consumer.consumer_id,
                state.revision,
                replay_head,
            )
            .await
        {
            Ok(EventConsumerCheckpointOutcome::Accepted) => {},
            Ok(EventConsumerCheckpointOutcome::StaleRevision) => return false,
            Err(error) => {
                consumer.metrics.record_failure();
                tracing::warn!(
                    extension_id = consumer.extension_id,
                    %error,
                    "failed to checkpoint inspected custom events"
                );
                return false;
            },
        }
    }
    consumer.metrics.record_success();
    true
}

impl CustomEventConsumer {
    fn invocation(
        &self,
        lane: Arc<CustomEventLane>,
        event: &Arc<Event>,
        session: &CustomEventSession,
    ) -> Option<CustomEventInvocation> {
        let custom_event = event.payload.custom_event()?;
        let call = match self.view.make_registered_extension_call_context(
            &self.extension_id,
            ExtensionCallContextInput {
                session_id: Some(event.session_id.clone()),
                turn_id: event.turn_id.as_ref().map(ToString::to_string),
                tool_call_id: None,
                working_dir: None,
                session_store_dir: None,
                event_tx: Some(session.event_sender(event.turn_id.clone())),
                event_causation: Some((event.id.clone(), custom_event.cascade_depth)),
                cancellation: self.cancellation.clone(),
            },
        ) {
            Ok(call) => call,
            Err(error) => {
                tracing::warn!(
                    event_id = %event.id,
                    extension_id = self.extension_id,
                    %error,
                    "failed to build custom event context"
                );
                return None;
            },
        };
        let context = CustomEventContext::from_runtime(
            call,
            event.session_id.clone(),
            event.id.clone(),
            event.seq,
            custom_event.extension_id.clone(),
            custom_event.event_type.clone(),
            custom_event.schema_version,
            custom_event.causation_id.clone(),
            custom_event.cascade_depth,
            custom_event.payload.clone(),
        );
        Some(CustomEventInvocation {
            _lane: lane,
            view: Arc::clone(&self.view),
            extension_id: self.extension_id.clone(),
            consumer_id: self.consumer_id.clone(),
            cancellation: self.cancellation.clone(),
            handler: Arc::clone(&self.handler),
            metrics: Arc::clone(&self.metrics),
            context,
            event_store: Arc::clone(&session.event_store),
            session_id: event.session_id.clone(),
            seq: event.seq,
        })
    }
}

async fn run_custom_event_invocation(
    invocation: &CustomEventInvocation,
) -> CustomEventInvocationOutcome {
    let _active_delivery = invocation.metrics.track_delivery();
    let state = match invocation
        .event_store
        .event_consumer_state(&invocation.session_id, &invocation.consumer_id)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            invocation.metrics.record_failure();
            tracing::warn!(
                extension_id = invocation.extension_id,
                %error,
                "failed to read custom event consumer state"
            );
            return CustomEventInvocationOutcome::Retry;
        },
    };
    if state.paused {
        return CustomEventInvocationOutcome::Paused;
    }
    if let Some(seq) = invocation.seq {
        if state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq) {
            return CustomEventInvocationOutcome::Consumed;
        }
    }
    let result = invocation
        .view
        .run_recorded_hook(
            &invocation.extension_id,
            "custom_event",
            invocation.cancellation.clone(),
            invocation.handler.handle(invocation.context.clone()),
        )
        .await;
    if let Err(error) = result {
        invocation.metrics.record_failure();
        tracing::warn!(
            extension_id = invocation.extension_id,
            %error,
            "custom event handler failed"
        );
        return CustomEventInvocationOutcome::Retry;
    }
    if let Some(seq) = invocation.seq {
        match invocation
            .event_store
            .checkpoint_event_consumer(
                &invocation.session_id,
                &invocation.consumer_id,
                state.revision,
                seq,
            )
            .await
        {
            Ok(EventConsumerCheckpointOutcome::Accepted) => {},
            Ok(EventConsumerCheckpointOutcome::StaleRevision) => {
                return CustomEventInvocationOutcome::Retry;
            },
            Err(error) => {
                invocation.metrics.record_failure();
                tracing::warn!(
                    extension_id = invocation.extension_id,
                    %error,
                    "failed to checkpoint custom event consumer"
                );
                return CustomEventInvocationOutcome::Retry;
            },
        }
    }
    invocation.metrics.record_success();
    CustomEventInvocationOutcome::Consumed
}

// ─── ExtensionRunner impl ───────────────────────────────────────────────

impl ExtensionRunner {
    /// 创建新的扩展运行器。
    pub fn new(operation_timeout: Duration) -> Self {
        Self {
            coordination: LifecycleCoordination {
                registry: AsyncMutex::new(()),
                source_reconcile: AsyncMutex::new(()),
                shutdown: AsyncMutex::new(()),
            },
            registry: RuntimeRegistry {
                extensions: RwLock::new(Vec::new()),
                index: ArcSwap::from_pointee(HandlerIndex::default()),
                publication: parking_lot::Mutex::new(RuntimePublication::default()),
                publication_stable: Notify::new(),
                active_turn_views: ActiveTurnViews::new(),
            },
            bindings: parking_lot::RwLock::new(HostBindings {
                session_ops: Arc::new(StdRwLock::new(None)),
                startup_event_tx: None,
                host_router: Arc::new(crate::host_router::HostRouter::from_backends(
                    crate::host_router::HostBackends::default(),
                )),
            }),
            diagnostics: Arc::new(parking_lot::RwLock::new(BTreeMap::new())),
            extension_configs: parking_lot::RwLock::new(BTreeMap::new()),
            operation_timeout,
            retirements: RetirementSupervisor::new(),
            custom_event_permits: Arc::new(Semaphore::new(CUSTOM_EVENT_DISPATCH_CAPACITY)),
            custom_event_lanes: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            custom_event_metrics: parking_lot::Mutex::new(HashMap::new()),
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
        let Some(tasks) = self.finish_registration(publication).await? else {
            return Ok(false);
        };
        tasks.activate();
        Ok(true)
    }

    pub(crate) async fn register_deferred(
        &self,
        ext: Arc<dyn Extension>,
        startup_working_dir: Option<&str>,
        source_key: String,
        source_fingerprint: String,
    ) -> Result<Option<DeferredTaskActivation>, ExtensionError> {
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
                ExtensionOrigin::Source {
                    key: source_key,
                    fingerprint: source_fingerprint,
                },
            )
            .await
        }?;
        Ok(self
            .finish_registration(publication)
            .await?
            .map(DeferredTaskActivation::new))
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
        if let Err(error) = validate_registration_conflicts(
            &id,
            &registrations,
            &self.registry.extensions.read().await,
        ) {
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

        let tasks = ExtensionTasks::new_suspended(id.clone());
        let ext_config = extension_config(&self.extension_configs.read(), &id);

        let startup_event_tx = self.bindings.read().startup_event_tx.clone();
        let call = self
            .extension_call_context_factory()
            .make_extension_call_context(
                &id,
                &capabilities,
                registrations.custom_event_declarations(),
                tasks.clone(),
                ExtensionCallContextInput {
                    working_dir: startup_working_dir.map(std::path::PathBuf::from),
                    event_tx: startup_event_tx,
                    ..ExtensionCallContextInput::unscoped(tasks.cancellation())
                },
            )
            .retain_cancellation_after_context_drop();
        let ctx = ExtensionStartContext::from_runtime(
            call,
            ExtensionConfig::from_runtime(&id, ext_config.clone()),
        );
        let pending_registration = self.retirements.pending_registration(
            id.clone(),
            Arc::clone(&ext),
            tasks.clone(),
            operation_guard,
            self.operation_timeout,
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

        {
            let mut extensions = self.registry.extensions.write().await;
            extensions.push(HostedExtension {
                extension: ext,
                manifest: ResolvedExtensionManifest {
                    author: manifest,
                    registrations,
                },
                origin,
                tasks: tasks.clone(),
                config: ext_config,
                operation_gate,
                publication_lease: ExtensionPublicationLease::new(),
            });
            self.rebuild_index(&extensions);
        }
        pending_registration.disarm();
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
    /// 返回是否从当前分发表移除了该扩展。旧实例会在所有已发布视图释放后异步停止；
    /// 停止失败会记录日志，并由等待该退休结果的调用方或 [`Self::shutdown`] 汇总。
    pub async fn unregister(
        &self,
        extension_id: &str,
        reason: StopReason,
    ) -> Result<bool, ExtensionError> {
        Ok(self
            .unregister_with_retirement(extension_id, reason)
            .await?
            .is_some())
    }

    pub(crate) async fn unregister_with_retirement(
        &self,
        extension_id: &str,
        reason: StopReason,
    ) -> Result<Option<RetirementTicket>, ExtensionError> {
        let (operation_guard, _lifecycle) = loop {
            let operation_gate = self
                .registry
                .extensions
                .read()
                .await
                .iter()
                .find_map(|hosted| {
                    (hosted.manifest.id() == extension_id)
                        .then(|| Arc::clone(&hosted.operation_gate))
                });
            let operation_guard = match &operation_gate {
                Some(gate) => Some(Arc::clone(gate).lock_owned().await),
                None => None,
            };
            let lifecycle = self.coordination.registry.lock().await;
            let operation_gate_is_current = self
                .registry
                .extensions
                .read()
                .await
                .iter()
                .find(|hosted| hosted.manifest.id() == extension_id)
                .map(|hosted| {
                    operation_gate
                        .as_ref()
                        .is_some_and(|gate| Arc::ptr_eq(gate, &hosted.operation_gate))
                })
                .unwrap_or(operation_gate.is_none());
            if operation_gate_is_current {
                break (operation_guard, lifecycle);
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
        self.diagnostics.write().remove(extension_id);
        let retirement =
            self.retirements
                .retire(hosted, reason, self.operation_timeout, operation_guard);
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

    pub(crate) async fn lock_source_reconcile(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.coordination.source_reconcile.lock().await
    }

    /// 在完整来源批次协调结束前保持 `Updating`，并阻止新的外部 extension view。
    pub(crate) fn begin_source_batch_publication(&self) -> impl Drop + '_ {
        RuntimePublicationGuard::begin(
            &self.registry.publication,
            &self.registry.publication_stable,
        )
    }

    pub(crate) async fn reorder_source_extensions(&self, desired_ids: &[String]) {
        let order = desired_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let _lifecycle = self.coordination.registry.lock().await;
        let mut extensions = self.registry.extensions.write().await;
        let already_ordered = extensions
            .iter()
            .map(|hosted| {
                order
                    .get(hosted.manifest.id())
                    .copied()
                    .unwrap_or(usize::MAX)
            })
            .is_sorted();
        if already_ordered {
            return;
        }
        extensions.sort_by_key(|hosted| {
            order
                .get(hosted.manifest.id())
                .copied()
                .unwrap_or(usize::MAX)
        });
        self.rebuild_index(&extensions);
    }

    fn rebuild_index(&self, extensions: &[HostedExtension]) {
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
        self.registry.index.store(index);
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

    /// 原子替换所有扩展的专有配置映射。
    ///
    /// 新注册的扩展将使用新配置；已注册的扩展需调用
    /// [`notify_config_changed`] 来更新运行态实例。
    pub fn update_extension_configs(&self, configs: BTreeMap<String, serde_json::Value>) {
        *self.extension_configs.write() = configs;
    }

    /// 通知所有已注册扩展其配置已变更。
    ///
    /// 将当前 `extension_configs` 与各已发布扩展保存的快照做 diff，
    /// 仅在有变化时调用 `ext.on_config_changed()`。
    /// 返回每个扩展的 notify 结果（仅记录错误，不中断）。
    pub async fn notify_config_changed(&self) -> Vec<String> {
        let current_configs = self.extension_configs.read().clone();
        let pending: Vec<_> = self
            .registry
            .extensions
            .read()
            .await
            .iter()
            .filter_map(|hosted| {
                let config = extension_config(&current_configs, hosted.manifest.id());
                (hosted.config != config).then(|| {
                    (
                        hosted.manifest.id().to_owned(),
                        Arc::clone(&hosted.extension),
                        config,
                        Arc::clone(&hosted.operation_gate),
                    )
                })
            })
            .collect();
        if pending.is_empty() {
            return Vec::new();
        }
        let _publication = RuntimePublicationGuard::begin(
            &self.registry.publication,
            &self.registry.publication_stable,
        );

        if let Err(active_views) = self
            .registry
            .active_turn_views
            .wait_until_idle(self.operation_timeout)
            .await
        {
            let extensions = self.registry.extensions.read().await;
            self.rebuild_index(&extensions);
            return vec![format!(
                "timed out after {} ms waiting for {active_views} active turn extension view(s); \
                 extension configuration was not applied",
                self.operation_timeout.as_millis()
            )];
        }

        let mut errors = Vec::new();
        for (extension_id, extension, new_config, operation_gate) in pending {
            let _operation = operation_gate.lock().await;
            let extension_is_current =
                self.registry.extensions.read().await.iter().any(|current| {
                    current.manifest.id() == extension_id
                        && Arc::ptr_eq(&current.operation_gate, &operation_gate)
                        && Arc::ptr_eq(&current.extension, &extension)
                        && current.config != new_config
                });
            if !extension_is_current
                || extension_config(&self.extension_configs.read(), &extension_id) != new_config
            {
                continue;
            }

            if let Err(error) = self
                .run_with_timeout(extension.on_config_changed(ExtensionConfig::from_runtime(
                    &extension_id,
                    new_config.clone(),
                )))
                .await
            {
                errors.push(format!(
                    "config changed handler failed for {extension_id}: {error}"
                ));
            } else {
                let mut extensions = self.registry.extensions.write().await;
                if extension_config(&self.extension_configs.read(), &extension_id) == new_config {
                    if let Some(hosted) = extensions.iter_mut().find(|hosted| {
                        hosted.manifest.id() == extension_id
                            && Arc::ptr_eq(&hosted.operation_gate, &operation_gate)
                            && Arc::ptr_eq(&hosted.extension, &extension)
                    }) {
                        hosted.config = new_config;
                    }
                }
            }
        }

        let extensions = self.registry.extensions.read().await;
        self.rebuild_index(&extensions);

        errors
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

    /// Dispatches an already-published custom event without affecting producer success.
    pub fn observe_custom_event(&self, event: Arc<Event>, session: CustomEventSession) -> bool {
        let Some(custom_event) = event.payload.custom_event() else {
            return true;
        };
        let durable = event.payload.as_durable().is_some();
        if custom_event.cascade_depth > MAX_CUSTOM_EVENT_CASCADE_DEPTH {
            tracing::warn!(
                event_id = %event.id,
                cascade_depth = custom_event.cascade_depth,
                "custom event cascade depth exceeded"
            );
            return true;
        }

        let view = self.turn_extension_view_with_lease();
        let mut fully_admitted = true;
        for (extension_id, subscription, handler) in &view.index.custom_event {
            if !subscription.matches(&custom_event.extension_id, &custom_event.event_type) {
                continue;
            }
            let Some(lane) = self.custom_event_lane(
                &view,
                extension_id,
                subscription,
                handler,
                &event.session_id,
            ) else {
                fully_admitted = false;
                continue;
            };
            if durable {
                if !Self::signal_durable_custom_events(&lane, session.clone()) {
                    fully_admitted = false;
                }
                continue;
            }

            let permit = match Arc::clone(&view.custom_event_permits).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::warn!(
                        event_id = %event.id,
                        extension_id,
                        "live custom event dispatch capacity exhausted"
                    );
                    fully_admitted = false;
                    continue;
                },
            };
            let Some(invocation) = lane
                .consumer
                .invocation(Arc::clone(&lane), &event, &session)
            else {
                fully_admitted = false;
                continue;
            };
            if lane
                .sender
                .send(CustomEventLaneCommand::Live {
                    _lane: Arc::clone(&lane),
                    _permit: permit,
                    invocation: Box::new(invocation),
                })
                .is_err()
            {
                tracing::warn!(
                    event_id = %event.id,
                    extension_id,
                    "custom event lane stopped before admission"
                );
                fully_admitted = false;
            }
        }
        fully_admitted
    }

    /// Wakes every durable consumer for one session after startup or extension reload.
    #[doc(hidden)]
    pub fn reconcile_custom_events(
        &self,
        session_id: &astrcode_core::types::SessionId,
        session: CustomEventSession,
    ) -> bool {
        let view = self.turn_extension_view_with_lease();
        let mut fully_admitted = true;
        for (extension_id, subscription, handler) in &view.index.custom_event {
            let Some(lane) =
                self.custom_event_lane(&view, extension_id, subscription, handler, session_id)
            else {
                fully_admitted = false;
                continue;
            };
            if !Self::signal_durable_custom_events(&lane, session.clone()) {
                fully_admitted = false;
            }
        }
        fully_admitted
    }

    fn custom_event_lane(
        &self,
        view: &Arc<ExtensionView>,
        extension_id: &str,
        subscription: &CustomEventSubscription,
        handler: &Arc<dyn CustomEventHandler>,
        session_id: &astrcode_core::types::SessionId,
    ) -> Option<Arc<CustomEventLane>> {
        let consumer_id = custom_event_consumer_id(extension_id, subscription);
        let lane_id = CustomEventLaneId {
            generation: view.generation,
            session_id: session_id.clone(),
            consumer_id: consumer_id.clone(),
        };
        let mut lanes = view.custom_event_lanes.lock();
        lanes.retain(|_, lane| lane.strong_count() > 0);
        if let Some(lane) = lanes
            .get(&lane_id)
            .and_then(Weak::upgrade)
            .filter(|lane| !lane.sender.is_closed())
        {
            return Some(lane);
        }

        let consumer = Arc::new(self.custom_event_consumer(
            view,
            extension_id,
            subscription,
            handler,
            session_id,
        )?);
        let (sender, receiver) = mpsc::unbounded_channel();
        let lane = Arc::new(CustomEventLane {
            sender,
            durable_reconciliation_queued: AtomicBool::new(false),
            consumer: Arc::clone(&consumer),
        });
        lanes.insert(lane_id, Arc::downgrade(&lane));
        view.spawn_extension_task(
            extension_id,
            "custom-event-lane",
            run_custom_event_lane(consumer, receiver),
        );
        Some(lane)
    }

    fn custom_event_consumer(
        &self,
        view: &Arc<ExtensionView>,
        extension_id: &str,
        subscription: &CustomEventSubscription,
        handler: &Arc<dyn CustomEventHandler>,
        session_id: &astrcode_core::types::SessionId,
    ) -> Option<CustomEventConsumer> {
        let Some(tasks) = view.index.extension_tasks.get(extension_id) else {
            tracing::warn!(extension_id, "custom event consumer has no task owner");
            return None;
        };
        let consumer_id = custom_event_consumer_id(extension_id, subscription);
        Some(CustomEventConsumer {
            view: Arc::clone(view),
            extension_id: extension_id.to_owned(),
            consumer_id: consumer_id.clone(),
            subscription: subscription.clone(),
            cancellation: tasks.cancellation().child_token(),
            handler: Arc::clone(handler),
            metrics: self.custom_event_consumer_metrics(session_id, &consumer_id),
            session_id: session_id.clone(),
        })
    }

    fn signal_durable_custom_events(
        lane: &Arc<CustomEventLane>,
        session: CustomEventSession,
    ) -> bool {
        if lane
            .durable_reconciliation_queued
            .swap(true, Ordering::AcqRel)
        {
            return true;
        }
        if lane
            .sender
            .send(CustomEventLaneCommand::ReconcileDurable {
                _lane: Arc::clone(lane),
                session,
            })
            .is_ok()
        {
            return true;
        }
        lane.durable_reconciliation_queued
            .store(false, Ordering::Release);
        false
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
        let tasks = self.index.extension_tasks.get(extension_id);
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

    async fn run_recorded_hook<T>(
        &self,
        extension_id: &str,
        hook_name: &'static str,
        cancellation: tokio_util::sync::CancellationToken,
        future: impl std::future::Future<Output = Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        let started = std::time::Instant::now();
        match tokio::time::timeout(self.operation_timeout, future).await {
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

    fn make_hook_call_context(
        &self,
        extension_id: &str,
        runtime: &RuntimeHookCallContext,
    ) -> Result<(ExtensionCallContext, tokio_util::sync::CancellationToken), ExtensionError> {
        let cancellation = runtime.cancellation().child_token();
        let call = self.make_registered_extension_call_context(
            extension_id,
            ExtensionCallContextInput::from_hook(runtime, cancellation.clone()),
        )?;
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

    /// PreToolUse 钩子分发。
    pub async fn emit_pre_tool_use(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
        let index = &self.index;
        let mut ctx = ctx;
        let mut modified = false;

        for (extension_id, mode, target, handler) in &index.pre_tool_use {
            if !target.matches(ctx.tool_name()) {
                continue;
            }
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = PreToolUseContext::from_runtime(call, &ctx);
            match mode {
                HookMode::Blocking => {
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
                            return Ok(PreToolUseResult::Block { reason });
                        },
                        PreToolUseResult::Ask { prompt, rule_key } => {
                            return Ok(PreToolUseResult::Ask { prompt, rule_key });
                        },
                        PreToolUseResult::ModifyInput { tool_input } => {
                            ctx.replace_tool_input(tool_input);
                            modified = true;
                        },
                        PreToolUseResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = self
                        .run_recorded_hook(
                            extension_id,
                            "pre_tool_use",
                            cancellation,
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
                    });
                },
            }
        }
        if modified {
            Ok(PreToolUseResult::ModifyInput {
                tool_input: ctx.tool_input().clone(),
            })
        } else {
            Ok(PreToolUseResult::Allow)
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
            let handler_ctx = PostToolUseContext::from_runtime(call, &ctx);
            match mode {
                HookMode::Blocking => {
                    let result = self
                        .run_recorded_hook(
                            extension_id,
                            "post_tool_use",
                            cancellation,
                            handler.handle(handler_ctx),
                        )
                        .await?;
                    match result {
                        PostToolUseResult::Block { reason } => {
                            return Ok(PostToolUseResult::Block { reason });
                        },
                        PostToolUseResult::ModifyResult { content } => {
                            ctx.replace_result_content(content);
                            modified = true;
                        },
                        PostToolUseResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = self
                        .run_recorded_hook(
                            extension_id,
                            "post_tool_use",
                            cancellation,
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
                    });
                },
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
            let handler_ctx = ProviderContext::from_runtime(call, &ctx);
            match mode {
                HookMode::Blocking => {
                    let result = self
                        .run_recorded_hook(
                            extension_id,
                            provider_hook_name(event),
                            cancellation,
                            handler.handle(handler_ctx),
                        )
                        .await?;
                    match result {
                        ProviderResult::Block { reason } => {
                            return Ok(ProviderResult::Block { reason });
                        },
                        ProviderResult::ReplaceMessages { messages } => {
                            ctx.replace_messages(messages);
                            modified = true;
                        },
                        ProviderResult::AppendMessages { messages } => {
                            ctx.append_messages(messages);
                            modified = true;
                        },
                        ProviderResult::Allow => {},
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = self
                        .run_recorded_hook(
                            extension_id,
                            provider_hook_name(event),
                            cancellation,
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
                    });
                },
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

    /// PromptBuild 贡献收集。
    pub async fn collect_prompt_contributions_typed(
        &self,
        ctx: RuntimePromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        let index = &self.index;

        let mut collected = PromptContributions::default();
        for (extension_id, handler) in &index.prompt_build {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = PromptBuildContext::from_runtime(call, &ctx);
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

    /// Compact 钩子分发。
    pub async fn emit_compact(
        &self,
        event: CompactEvent,
        ctx: RuntimeCompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        let index = &self.index;
        let handlers = index.compact.get(&event);

        let Some(handlers) = handlers else {
            return Ok(CompactResult::Allow);
        };

        let mut collected = CompactContributions::default();
        for (extension_id, handler) in handlers {
            let (call, cancellation) = self.make_hook_call_context(extension_id, ctx.call())?;
            let handler_ctx = CompactContext::from_runtime(call, &ctx);
            let result = self
                .run_recorded_hook(
                    extension_id,
                    "compact",
                    cancellation,
                    handler.handle(handler_ctx),
                )
                .await?;
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
            let handler_ctx = ContinueAfterStopContext::from_runtime(call, &ctx);
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
            let handler_ctx = UserMessageEnvelopeContext::from_runtime(call, &ctx);
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
                    ctx.replace_text(text);
                    modified = true;
                },
                UserMessageEnvelopeResult::AppendText { text } => {
                    ctx.append_text(&text);
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
            let handler_ctx = LifecycleContext::from_runtime(call, &ctx);
            match mode {
                HookMode::Blocking => {
                    let result = self
                        .run_recorded_hook(
                            extension_id,
                            "lifecycle",
                            cancellation,
                            handler.handle(handler_ctx),
                        )
                        .await?;
                    if let HookResult::Block { reason } = result {
                        return Err(ExtensionError::Blocked { reason });
                    }
                },
                HookMode::Advisory => {
                    if let Err(e) = self
                        .run_recorded_hook(
                            extension_id,
                            "lifecycle",
                            cancellation,
                            handler.handle(handler_ctx),
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "advisory lifecycle handler failed");
                    }
                },
                HookMode::NonBlocking => {
                    let handler = Arc::clone(handler);
                    self.spawn_extension_task(extension_id, "lifecycle", async move {
                        if let Err(e) = handler.handle(handler_ctx).await {
                            tracing::warn!(error = %e, "non-blocking lifecycle handler failed");
                        }
                    });
                },
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
                return self.extension_view_for_index(index, None);
            }
            publication_stable.await;
        }
    }

    fn turn_extension_view_with_lease(&self) -> Arc<ExtensionView> {
        let (index, active_turn_lease) = {
            let publication = self.registry.publication.lock();
            let index = self.load_index();
            let active_turn_lease = publication
                .is_stable_generation(index.generation)
                .then(|| self.registry.active_turn_views.acquire());
            (index, active_turn_lease)
        };
        self.extension_view_for_index(index, active_turn_lease)
    }

    fn extension_view_for_index(
        &self,
        index: Arc<HandlerIndex>,
        active_turn_lease: Option<ActiveTurnViewLease>,
    ) -> Arc<ExtensionView> {
        Arc::new(ExtensionView {
            generation: index.generation,
            index,
            diagnostics: Arc::clone(&self.diagnostics),
            operation_timeout: self.operation_timeout,
            call_context_factory: self.extension_call_context_factory(),
            custom_event_permits: Arc::clone(&self.custom_event_permits),
            custom_event_lanes: Arc::clone(&self.custom_event_lanes),
            _active_turn_lease: active_turn_lease,
        })
    }

    async fn run_with_timeout<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        run_with_timeout(self.operation_timeout, future).await
    }

    pub async fn emit_pre_tool_use(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
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

    pub async fn emit_compact(
        &self,
        event: CompactEvent,
        ctx: RuntimeCompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        self.extension_view().await.emit_compact(event, ctx).await
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
        let view = self.turn_extension_view_with_lease();
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
    async fn emit_pre_tool_use(
        &self,
        ctx: RuntimePreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
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

    async fn emit_compact(
        &self,
        event: CompactEvent,
        ctx: RuntimeCompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        ExtensionView::emit_compact(self, event, ctx).await
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
