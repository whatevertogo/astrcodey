//! 进程生命周期路由（目前只有 shutdown）。

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::super::HttpState;

pub(in crate::http) async fn shutdown(State(state): State<HttpState>) -> Response {
    tracing::info!("shutdown requested via HTTP");
    state.app.request_shutdown();
    StatusCode::NO_CONTENT.into_response()
}
