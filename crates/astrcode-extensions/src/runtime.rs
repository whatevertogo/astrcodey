//! 会话创建和服务派生原语。

use astrcode_core::{event::EventPayload, extension::ChildToolPolicy};
use tokio::sync::mpsc;

/// 通用的会话创建原语。由服务器实现，由 runner 持有，扩展不可见。
#[async_trait::async_trait]
pub trait SessionSpawner: Send + Sync {
    /// 创建一个子会话并执行一轮对话。
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String>;
}

/// 子会话启动请求。
///
/// 所有字段均有合理默认值（字符串为空、Option 为 `None`、`wait_for_result` 为 `false`）。
/// 建议构造时使用 spread 语法 `SpawnRequest { name: ..., ..Default::default() }`，
/// 这样新增字段时旧的构造点不需要逐一更新。
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// 子会话名称
    pub name: String,
    /// 系统提示词
    pub system_prompt: String,
    /// 用户提示词
    pub user_prompt: String,
    /// 工作目录
    pub working_dir: String,
    /// 模型偏好（可选）
    pub model_preference: Option<String>,
    /// 触发此次派生的工具调用 ID，用于进度事件归属。
    pub tool_call_id: Option<String>,
    /// 父 agent 的事件发送器，子 agent 的进度事件由此通道转发。
    pub event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    /// 是否同步阻塞等待子 agent 完成。
    pub wait_for_result: bool,
    /// 子会话的工具集策略。`None` 表示继承父 session 的工具全集。
    pub tool_policy: Option<ChildToolPolicy>,
    /// 创建该子 session 的扩展 ID，用于按插件组织存储目录。
    pub source_plugin: Option<String>,
}

/// 子会话执行结果。
pub struct SpawnResult {
    /// 子会话输出内容
    pub content: String,
    /// 子会话 ID
    pub child_session_id: String,
    /// 后台任务 ID（仅异步模式有值）。
    pub background_task_id: Option<String>,
}
