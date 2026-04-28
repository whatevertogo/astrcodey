//! 共享扩展运行时 — 借鉴自 pi-mono 的延迟绑定模式。
//!
//! 扩展在服务器完全启动之前就已加载。它们的注册（工具、命令）
//! 会被排队到此运行时中。当服务器就绪后，调用 `bind()` 注入
//! 实际的会话创建能力。

use std::sync::{Arc, Mutex, RwLock};

use astrcode_core::{event::EventPayload, tool::ToolDefinition};
use tokio::sync::mpsc;

/// 通用的会话创建原语。由服务器实现，由 runner 持有，扩展不可见。
#[async_trait::async_trait]
pub trait SessionSpawner: Send + Sync {
    /// 创建一个子会话并执行一轮对话。
    ///
    /// # 参数
    /// - `parent_session_id`: 父会话 ID
    /// - `request`: 子会话启动请求
    ///
    /// # 返回
    /// 成功时返回子会话的执行结果，失败时返回错误描述。
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String>;
}

/// 子会话启动请求。
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// 子会话名称
    pub name: String,
    /// 系统提示词
    pub system_prompt: String,
    /// 用户提示词
    pub user_prompt: String,
    /// 工作目录
    pub working_dir: String,
    /// 允许使用的工具名称列表
    pub allowed_tools: Vec<String>,
    /// 模型偏好（可选）
    pub model_preference: Option<String>,
    /// 父 agent 工具调用 ID，用于把子 agent 进度归属到父级工具调用。
    pub parent_tool_call_id: Option<String>,
    /// 父 agent 的事件发送器，仅用于发送非持久化进度事件。
    pub parent_event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
}

/// 子会话执行结果。
pub struct SpawnResult {
    /// 子会话输出内容
    pub content: String,
    /// 子会话 ID
    pub child_session_id: String,
}

/// 所有已加载扩展的共享状态。
///
/// 由 loader 创建，服务器就绪后调用 `bind()` 注入实际的会话创建能力。
pub struct ExtensionRuntime {
    /// 扩展在加载阶段注册的工具
    pending_tools: Mutex<Vec<ToolDefinition>>,
    /// 注入的会话创建器。在 `bind()` 调用前为 None。
    /// 使用 Arc 以支持 clone-then-drop-guard-before-await 模式。
    spawner: RwLock<Option<Arc<dyn SessionSpawner>>>,
}

impl Default for ExtensionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRuntime {
    /// 创建新的扩展运行时实例。
    pub fn new() -> Self {
        Self {
            pending_tools: Mutex::new(Vec::new()),
            spawner: RwLock::new(None),
        }
    }

    /// 绑定实际的会话创建器。在服务器启动后调用一次。
    pub fn bind(&self, spawner: Arc<dyn SessionSpawner>) {
        *self.spawner.write().unwrap() = Some(spawner);
    }

    /// 将工具注册加入队列。在 NativeExtension 的 factory() 调用期间使用。
    pub fn register_tool(&self, def: ToolDefinition) {
        self.pending_tools.lock().unwrap().push(def);
    }

    /// 取出所有待处理的工具注册（消费式取出）。
    pub fn take_pending_tools(&self) -> Vec<ToolDefinition> {
        std::mem::take(&mut *self.pending_tools.lock().unwrap())
    }

    /// 执行子会话的一轮对话。如果 `bind()` 尚未调用则返回错误。
    pub async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let spawner = {
            let guard = self.spawner.read().unwrap();
            match &*guard {
                Some(s) => Arc::clone(s),
                None => {
                    return Err("ExtensionRuntime not bound — bind() must be called before \
                                spawn()"
                        .into());
                },
            }
        };
        // 在 await 之前释放锁
        spawner.spawn(parent_session_id, request).await
    }
}
