//! Axum HTTP/SSE 入口。
//!
//! 这层只做 wire 适配：命令统一进入 [`crate::handler::CommandHandler`]，读接口从
//! storage read model 映射到 `astrcode_protocol::http` DTO。

use std::sync::Arc;

use astrcode_protocol::http::ConversationErrorEnvelopeDto;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::{bootstrap::ServerRuntime, handler::HandlerError};

mod auth;
mod projection;
mod routes;
mod server;
mod stream;

pub use auth::ASTRCODE_HTTP_TOKEN_ENV;
pub use server::{HttpServerError, remove_run_info, router, run_http_server, write_run_info};
#[cfg(feature = "testing")]
pub use server::router_with_event_publisher;

/// HTTP router shared state.
#[derive(Clone)]
pub(crate) struct HttpState {
    pub(crate) runtime: Arc<ServerRuntime>,
    pub(crate) handler: crate::handler::CommandHandle,
    pub(crate) event_bus: Arc<crate::server_event_bus::ServerEventBus>,
}

pub(crate) fn error_response(
    status: StatusCode,
    code: impl Into<String>,
    message: impl ToString,
) -> Response {
    (
        status,
        Json(ConversationErrorEnvelopeDto {
            code: code.into(),
            message: message.to_string(),
        }),
    )
        .into_response()
}

pub(crate) fn bad_request_response(code: &'static str, message: impl ToString) -> Response {
    error_response(StatusCode::BAD_REQUEST, code, message)
}

pub(crate) fn not_found_response(code: &'static str, message: impl ToString) -> Response {
    error_response(StatusCode::NOT_FOUND, code, message)
}

pub(crate) fn conflict_response(code: &'static str, message: impl ToString) -> Response {
    error_response(StatusCode::CONFLICT, code, message)
}

pub(crate) fn internal_error_response(code: &'static str, message: impl ToString) -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, code, message)
}

pub(crate) fn handler_error_response(error: HandlerError, default_code: &'static str) -> Response {
    match error {
        HandlerError::TurnAlreadyRunning | HandlerError::CompactBlocked => {
            conflict_response("turn_running", "A turn is already running")
        },
        HandlerError::UnknownCommand(cmd) => {
            bad_request_response("unknown_command", format!("Unknown command: /{cmd}"))
        },
        HandlerError::NoActiveTurn => not_found_response("no_active_turn", "No active turn"),
        HandlerError::SessionNotFound(_) | HandlerError::NoActiveSession => {
            not_found_response("session_not_found", "Session not found")
        },
        HandlerError::InvalidRequest(message) => bad_request_response(default_code, message),
        other => internal_error_response(default_code, other.to_string()),
    }
}
