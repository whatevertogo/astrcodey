//! Server 核心系统组装 — 事件总线 + scheduler + handler actor。

use std::sync::Arc;

use astrcode_core::{lifecycle::SessionResourceCleanup, types::SessionId};
use astrcode_protocol::events::ClientNotification;
use astrcode_support::event_fanout::EventFanout;

use super::ServerRuntime;
use crate::{
    handler::CommandHandle, server_event_bus::ServerEventBus,
    session_operations::ServerSessionOperations, turn_registry::TurnRegistry,
    turn_scheduler::TurnScheduler,
};

/// 包装 TurnScheduler 以适配 SessionResourceCleanup trait
struct TurnSchedulerCleanup {
    scheduler: Arc<TurnScheduler>,
}

impl SessionResourceCleanup for TurnSchedulerCleanup {
    fn cleanup(&self, session_id: &SessionId) {
        let scheduler = Arc::clone(&self.scheduler);
        let sid = session_id.clone();
        tokio::spawn(async move {
            scheduler.cleanup(&sid).await;
            tracing::debug!(session_id = %sid, "turn scheduler cleanup finished");
        });
    }
}

/// Server 核心系统句柄。
///
/// 封装事件总线、scheduler、handler actor 等共享组件的初始化，
/// 保证各传输层入口（stdio / in-process / ACP / HTTP）的组装顺序一致。
pub struct ServerSystem {
    /// 事件广播发送端，传输层用它订阅事件。
    pub event_tx: Arc<EventFanout<ClientNotification>>,
    /// 事件总线，传输层用它发送非 session 通知。
    pub event_bus: Arc<ServerEventBus>,
    /// 命令处理句柄，传输层用它发送命令。
    pub handler: CommandHandle,
    /// Turn 调度器，共享给 CommandHandler 和 SessionOperations。
    pub scheduler: Arc<TurnScheduler>,
}

/// 组装 server 核心组件：创建事件总线 → 创建 scheduler → 绑定 session ops → 启动 handler actor。
///
/// `event_tx` 由调用方创建并传入，传输层可保留自己的订阅端。
pub fn spawn_server_system(
    runtime: &Arc<ServerRuntime>,
    event_tx: Arc<EventFanout<ClientNotification>>,
) -> ServerSystem {
    let registry = Arc::new(TurnRegistry::new());
    let scheduler = Arc::new(TurnScheduler::new(
        runtime.session_manager().clone(),
        Arc::clone(&registry),
    ));

    let event_bus = Arc::new(ServerEventBus::new(
        Arc::clone(&event_tx),
        Arc::clone(&scheduler),
    ));

    runtime
        .session_manager()
        .bind_event_bus(Arc::clone(&event_bus));

    // 绑定子会话操作能力到扩展运行时
    runtime
        .extension_runner()
        .bind_session_ops(Arc::new(ServerSessionOperations {
            session_manager: Arc::clone(runtime.session_manager()),
            scheduler: Arc::clone(&scheduler),
        }));

    // 注册 TurnScheduler 到 session 资源清理链
    // 确保 session delete/recycle 时清理待处理消息队列
    runtime
        .session_manager()
        .add_resource_cleanup(Arc::new(TurnSchedulerCleanup {
            scheduler: Arc::clone(&scheduler),
        }));

    let handler = CommandHandle::spawn(
        Arc::clone(runtime),
        Arc::clone(&scheduler),
        Arc::clone(&event_bus),
    );

    ServerSystem {
        event_tx,
        event_bus,
        handler,
        scheduler,
    }
}
