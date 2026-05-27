//! ACP（Agent Client Protocol）HTTP 入口。

use std::sync::Arc;

use axum::{
    extract::{State, WebSocketUpgrade},
    response::{IntoResponse, Response},
};

use crate::{
    acp::{AcpServices, serve_acp_websocket},
    http::HttpState,
};

pub(in crate::http) async fn acp_websocket(
    State(state): State<HttpState>,
    ws: WebSocketUpgrade,
) -> Response {
    let services = AcpServices {
        command_handle: state.handler.clone(),
        event_tx: Arc::clone(&state.event_tx),
    };
    ws.on_upgrade(move |socket| async move {
        if let Err(error) = serve_acp_websocket(socket, services).await {
            tracing::warn!(error = %error, "ACP websocket session ended");
        }
    })
    .into_response()
}
