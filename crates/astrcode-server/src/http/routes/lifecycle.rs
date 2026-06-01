//! 进程生命周期路由（目前只有 shutdown）。

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::super::HttpState;

pub(in crate::http) async fn shutdown(State(state): State<HttpState>) -> Response {
    tracing::info!("shutdown requested via HTTP");
    let runtime = Arc::clone(&state.runtime);
    let handler = state.handler.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        runtime.shutdown_token().cancel();
        handler.shutdown().await;
        runtime.scheduler().drain_detached_tasks().await;
        runtime.shutdown_extensions().await;
    });
    tokio::spawn(async move {
        if let Err(e) = handle.await {
            tracing::error!("shutdown task panicked: {e}");
        }
    });
    StatusCode::NO_CONTENT.into_response()
}
