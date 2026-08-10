use astrcode_core::types::SessionId;
use astrcode_extensions::runner::{
    CustomEventConsumerAction, CustomEventConsumerControlError, CustomEventConsumerStatus,
};
use astrcode_protocol::http::{
    CustomEventConsumerActionDto, CustomEventConsumerControlRequest,
    CustomEventConsumerListResponseDto, CustomEventConsumerStatusDto,
};
use astrcode_storage::StorageError;
use axum::{
    Json,
    extract::{Path, State},
    response::{IntoResponse, Response},
};

use super::HttpState;
use crate::{
    http::{conflict_response, internal_error_response, not_found_response},
    protocol_mapping::custom_event_subscription_to_dto,
};

pub(in crate::http) async fn list_event_consumers(
    State(state): State<HttpState>,
    Path(raw_session_id): Path<String>,
) -> Response {
    let session_id = SessionId::from(raw_session_id);
    let session = state
        .app
        .runtime()
        .session_manager()
        .custom_event_session(&session_id);
    match state
        .app
        .runtime()
        .extension_runner()
        .custom_event_consumer_statuses(&session_id, &session)
        .await
    {
        Ok(consumers) => Json(CustomEventConsumerListResponseDto {
            consumers: consumers.into_iter().map(status_to_dto).collect(),
        })
        .into_response(),
        Err(error) => consumer_error_response(error),
    }
}

pub(in crate::http) async fn control_event_consumer(
    State(state): State<HttpState>,
    Path(raw_session_id): Path<String>,
    Json(request): Json<CustomEventConsumerControlRequest>,
) -> Response {
    let session_id = SessionId::from(raw_session_id);
    let session = state
        .app
        .runtime()
        .session_manager()
        .custom_event_session(&session_id);
    let action = match request.action {
        CustomEventConsumerActionDto::Pause => CustomEventConsumerAction::Pause,
        CustomEventConsumerActionDto::Resume => CustomEventConsumerAction::Resume,
        CustomEventConsumerActionDto::ReplayFromBeginning => {
            CustomEventConsumerAction::ReplayFromBeginning
        },
        CustomEventConsumerActionDto::SkipToStreamHead => {
            CustomEventConsumerAction::SkipToStreamHead
        },
    };
    match state
        .app
        .runtime()
        .extension_runner()
        .control_custom_event_consumer(
            &session_id,
            &request.extension_id,
            &request.subscription_id,
            action,
            &session,
        )
        .await
    {
        Ok(status) => Json(status_to_dto(status)).into_response(),
        Err(error) => consumer_error_response(error),
    }
}

fn status_to_dto(status: CustomEventConsumerStatus) -> CustomEventConsumerStatusDto {
    CustomEventConsumerStatusDto {
        extension_id: status.extension_id,
        subscription: custom_event_subscription_to_dto(status.subscription),
        paused: status.paused,
        checkpoint: status.checkpoint.map(|checkpoint| checkpoint.to_string()),
        stream_head: status
            .stream_head
            .map(|stream_head| stream_head.to_string()),
        pending_events: status.pending_events,
        in_flight: status.in_flight,
        failed_attempts: status.failed_attempts,
        consecutive_failures: status.consecutive_failures,
        quarantined_events: status.quarantined_events,
    }
}

fn consumer_error_response(error: CustomEventConsumerControlError) -> Response {
    match error {
        CustomEventConsumerControlError::SubscriptionNotFound { .. } => not_found_response(
            "event_consumer_not_found",
            "Custom event consumer not found",
        ),
        CustomEventConsumerControlError::Storage(StorageError::NotFound(_)) => {
            not_found_response("session_not_found", "Session not found")
        },
        CustomEventConsumerControlError::ConsumerBusy(_) => conflict_response(
            "event_consumer_busy",
            "Custom event consumer did not become idle before the control timeout",
        ),
        error => internal_error_response("event_consumer_control_failed", error),
    }
}
