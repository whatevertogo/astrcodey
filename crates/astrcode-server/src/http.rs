//! Axum HTTP/SSE 入口。
//!
//! 这层只做 wire 适配：显式 session 命令进入并发安全的 session 服务，读接口从
//! storage read model 映射到 `astrcode_protocol::http` DTO。

use std::sync::Arc;

use astrcode_protocol::http::ConversationErrorEnvelopeDto;
use astrcode_storage::StorageError;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::{SessionManagerError, bootstrap::ServerApp, session_command_contract::HandlerError};

mod auth;
mod conversation_timeline;
mod projection;
mod routes;
mod server;
mod stream;

pub use server::{HttpServerError, router, run_http_server};

/// HTTP router shared state.
#[derive(Clone)]
pub(crate) struct HttpState {
    pub(crate) app: Arc<ServerApp>,
    pub(in crate::http) conversation_timeline:
        Arc<dyn conversation_timeline::ConversationTimelineReader>,
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

fn session_read_error_response(error: SessionManagerError) -> Response {
    match error {
        SessionManagerError::Storage(StorageError::NotFound(_) | StorageError::InvalidId(_)) => {
            not_found_response("session_not_found", "Session not found")
        },
        SessionManagerError::Storage(StorageError::UnsupportedEventFormat { .. }) => {
            conflict_response(
                "unsupported_session_log_format",
                "This session uses an unsupported event log format. Open it with a compatible \
                 version.",
            )
        },
        SessionManagerError::Storage(
            StorageError::CorruptLog(_) | StorageError::Serialization(_),
        ) => {
            tracing::warn!(%error, "failed to decode session event log");
            internal_error_response("session_log_corrupt", "The session event log is corrupt")
        },
        error => {
            tracing::warn!(%error, "failed to read session");
            internal_error_response("session_read_failed", "Failed to read session")
        },
    }
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
