use std::sync::Arc;

use astrcode_core::{event::Event, tool::FileObservationStore};
use astrcode_tools::registry::ToolRegistry;
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::{background::BackgroundTaskManager, tool_exec::InMemoryFileObservationStore};

/// session 内部 broadcast channel 容量。
///
/// 256 是「单 session 内并发订阅者较少」的折中值；订阅者消费速度跟不上时会触发
/// `RecvError::Lagged`，订阅者需自行通过游标补齐。
const SESSION_EVENT_BROADCAST_CAPACITY: usize = 256;

/// 单个 session 在当前进程内持有的瞬态状态。
///
/// 这里的状态随 session 生命周期存在，但不属于可持久化事实。
///
/// `event_tx` 故意放在 `SessionRuntimeState` 而非 `Session`：同一 sid 多次
/// `Session::open` 会得到多个 `Session` 实例（廉价的 store handle clone），
/// 但所有实例必须共享同一份 `SessionRuntimeState`（含 broadcast）才能让所有订阅者
/// 看到全部事件。SessionRuntimeRegistry / SessionManager 保证 per-sid 唯一。
///
/// 注意：直接通过 `Session::create_with_id` 而绕过 `SessionRuntimeRegistry` 创建的 session
/// 会得到独立 runtime，订阅者不会跨实例可见——这是给 `spawn_child` 这类「我就是要新 runtime」
/// 的场景用的。SessionManager 走 registry 路径。
pub struct SessionRuntimeState {
    file_observation_store: Arc<dyn FileObservationStore>,
    tool_registry: Mutex<Arc<ToolRegistry>>,
    bg_tasks: Arc<Mutex<BackgroundTaskManager>>,
    /// 子 session 专用的额外 system prompt，由 SpawnRequest 注入。
    extra_system_prompt: Mutex<Option<String>>,
    /// 本 session 事件的 fanout 通道。同一 sid 下所有 Session 实例共享这份 sender，
    /// 通过 SessionRuntimeState 的 Arc 共享保证订阅一致性。
    event_tx: broadcast::Sender<Event>,
}

impl Default for SessionRuntimeState {
    fn default() -> Self {
        let (event_tx, _) = broadcast::channel(SESSION_EVENT_BROADCAST_CAPACITY);
        Self {
            file_observation_store: Arc::new(InMemoryFileObservationStore::default()),
            tool_registry: Mutex::new(Arc::new(ToolRegistry::new())),
            bg_tasks: Arc::new(Mutex::new(BackgroundTaskManager::new())),
            extra_system_prompt: Mutex::new(None),
            event_tx,
        }
    }
}

impl SessionRuntimeState {
    pub fn file_observation_store(&self) -> Arc<dyn FileObservationStore> {
        Arc::clone(&self.file_observation_store)
    }

    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.tool_registry.lock().clone()
    }

    pub fn set_tool_registry(&self, registry: Arc<ToolRegistry>) {
        *self.tool_registry.lock() = registry;
    }

    pub fn background_tasks(&self) -> Arc<Mutex<BackgroundTaskManager>> {
        Arc::clone(&self.bg_tasks)
    }

    pub fn extra_system_prompt(&self) -> Option<String> {
        self.extra_system_prompt.lock().clone()
    }

    pub fn set_extra_system_prompt(&self, prompt: Option<String>) {
        *self.extra_system_prompt.lock() = prompt;
    }

    /// 订阅本 session 的事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    /// 向本 session 的 broadcast 推一个事件。返回是否真的发出（无订阅者时返回 false）。
    pub(crate) fn fanout(&self, event: Event) -> bool {
        self.event_tx.send(event).is_ok()
    }
}
