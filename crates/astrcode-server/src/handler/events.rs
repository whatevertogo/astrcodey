use astrcode_core::{
    event::{Event, EventPayload},
    types::{SessionId, TurnId},
};
use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;

use crate::bootstrap::ServerRuntime;

/// 将事件持久化到存储（如果是持久化事件）并广播给所有订阅者。
///
/// 只有 `is_durable()` 返回 true 的事件才会写入磁盘，
/// 非持久化事件（如流式 delta）仅广播不存储。
pub(super) async fn record_and_broadcast(
    runtime: &ServerRuntime,
    event_tx: &broadcast::Sender<ClientNotification>,
    session_id: &SessionId,
    turn_id: Option<&TurnId>,
    payload: EventPayload,
) -> Result<Event, String> {
    let event = Event::new(session_id.clone(), turn_id.cloned(), payload);
    let event = if event.payload.is_durable() {
        runtime
            .session_manager
            .append_event(event)
            .await
            .map_err(|e| e.to_string())?
    } else {
        event
    };

    let _ = event_tx.send(ClientNotification::Event(event.clone()));
    Ok(event)
}
