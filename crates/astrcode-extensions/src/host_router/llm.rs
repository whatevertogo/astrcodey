//! LLM host capabilities.

use std::sync::Arc;

use astrcode_core::llm::{LlmError, LlmEvent, LlmMessage, LlmProvider};
use astrcode_extension_contract::{ModelEventStream, WireErrorCode, protocol::ModelStreamEvent};
use astrcode_extension_sdk::{
    host::{
        HostLlmChatOutput, HostLlmChatRequest, HostOperation, HostOperationGroup,
        llm_messages_from_wire,
    },
    s5r::ErrorPayload,
};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{HOST_INVOKE_TIMEOUT, invalid_group_operation, serialize_wire_response};

pub(super) struct LlmGroup {
    main: Option<Arc<dyn LlmProvider>>,
    small: Option<Arc<dyn LlmProvider>>,
}

impl LlmGroup {
    pub(super) fn new(
        main: Option<Arc<dyn LlmProvider>>,
        small: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self { main, small }
    }

    pub(super) async fn invoke(
        &self,
        operation: HostOperation,
        input: Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        let (provider, model_label) = self.provider_for(operation)?;
        invoke_llm_chat(provider, model_label, input, cancel_token).await
    }

    pub(super) async fn invoke_event_stream(
        &self,
        operation: HostOperation,
        input: Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<ModelEventStream, ErrorPayload> {
        let (provider, model_label) = self.provider_for(operation)?;
        let request = serde_json::from_value::<HostLlmChatRequest>(input).map_err(|error| {
            ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("invalid {model_label}.chat request: {error}"),
            )
        })?;
        if request.messages.is_empty() {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                "messages must contain at least one typed LLM message",
            ));
        }
        let messages = llm_messages_from_wire(request.messages);
        let receiver = super::run_until_deadline(
            async {
                provider
                    .generate(messages, vec![])
                    .await
                    .map_err(llm_error_payload)
            },
            Instant::now() + HOST_INVOKE_TIMEOUT,
            cancel_token,
            || {
                ErrorPayload::new(
                    WireErrorCode::Timeout,
                    format!("{model_label}.chat timed out"),
                )
            },
            || ErrorPayload::new(WireErrorCode::Cancelled, "invoke cancelled"),
        )
        .await?;
        let model = model_label.to_owned();
        let events = futures_util::stream::unfold(
            (receiver, String::new(), 0u32, false),
            move |(mut receiver, mut content, mut retry_attempt, terminal)| {
                let model = model.clone();
                async move {
                    if terminal {
                        return None;
                    }
                    let Some(event) = receiver.recv().await else {
                        return Some((
                            ModelStreamEvent::Failed {
                                error: ErrorPayload::new(
                                    WireErrorCode::StreamClosed,
                                    "model provider closed before a terminal event",
                                ),
                            },
                            (receiver, content, retry_attempt, true),
                        ));
                    };
                    let (event, terminal) = match event {
                        LlmEvent::Retrying {
                            attempt, delay_ms, ..
                        } => {
                            retry_attempt = attempt;
                            (ModelStreamEvent::Retrying { attempt, delay_ms }, false)
                        },
                        LlmEvent::RetryRecovered => (
                            ModelStreamEvent::Recovered {
                                attempt: retry_attempt,
                            },
                            false,
                        ),
                        LlmEvent::ContentDelta { delta } => {
                            content.push_str(&delta);
                            (ModelStreamEvent::ContentDelta { content: delta }, false)
                        },
                        LlmEvent::ThinkingDelta { delta } => {
                            (ModelStreamEvent::ThinkingDelta { content: delta }, false)
                        },
                        LlmEvent::ToolCallStart {
                            call_id,
                            name,
                            arguments,
                        } => (
                            ModelStreamEvent::ToolCallStart {
                                tool_call_id: call_id,
                                name,
                                arguments,
                            },
                            false,
                        ),
                        LlmEvent::ToolCallDelta { call_id, delta } => (
                            ModelStreamEvent::ToolCallDelta {
                                tool_call_id: call_id,
                                delta,
                            },
                            false,
                        ),
                        LlmEvent::ToolCallCompleted { call_id } => (
                            ModelStreamEvent::ToolCallCompleted {
                                tool_call_id: call_id,
                            },
                            false,
                        ),
                        LlmEvent::Usage { usage } => (
                            ModelStreamEvent::Usage {
                                input_tokens: usage.input_tokens.unwrap_or(0),
                                output_tokens: usage.output_tokens.unwrap_or(0),
                            },
                            false,
                        ),
                        LlmEvent::Done { finish_reason } => (
                            ModelStreamEvent::Completed {
                                output: serde_json::json!({
                                    "content": content.clone(),
                                    "model": model,
                                    "finish_reason": finish_reason,
                                }),
                            },
                            true,
                        ),
                        LlmEvent::Error { message } => (
                            ModelStreamEvent::Failed {
                                error: ErrorPayload::new(WireErrorCode::LlmStreamError, message),
                            },
                            true,
                        ),
                    };
                    Some((event, (receiver, content, retry_attempt, terminal)))
                }
            },
        );
        Ok(Box::pin(
            futures_util::stream::once(async { ModelStreamEvent::Started }).chain(events),
        ))
    }

    fn provider_for(
        &self,
        operation: HostOperation,
    ) -> Result<(&Arc<dyn LlmProvider>, &'static str), ErrorPayload> {
        let (provider, model_label) = match operation {
            HostOperation::LlmMainChat => (self.main.as_ref(), "main_llm"),
            HostOperation::LlmSmallChat => (self.small.as_ref(), "small_llm"),
            _ => return Err(invalid_group_operation(operation, HostOperationGroup::Llm)),
        };
        let provider = provider.ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::BackendUnavailable,
                format!("{model_label} not configured"),
            )
        })?;
        Ok((provider, model_label))
    }

    pub(super) fn has_main(&self) -> bool {
        self.main.is_some()
    }

    pub(super) fn has_small(&self) -> bool {
        self.small.is_some()
    }
}

async fn invoke_llm_chat(
    provider: &Arc<dyn LlmProvider>,
    model_label: &'static str,
    input: Value,
    cancel_token: Option<&CancellationToken>,
) -> Result<Value, ErrorPayload> {
    let request = serde_json::from_value::<HostLlmChatRequest>(input).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("invalid {model_label}.chat request: {error}"),
        )
    })?;

    if request.messages.is_empty() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "messages must contain at least one typed LLM message",
        ));
    }
    let messages = llm_messages_from_wire(request.messages);

    super::run_until_deadline(
        run_host_llm_chat(&**provider, model_label, messages),
        Instant::now() + HOST_INVOKE_TIMEOUT,
        cancel_token,
        || {
            ErrorPayload::new(
                WireErrorCode::Timeout,
                format!("{model_label}.chat timed out"),
            )
        },
        || ErrorPayload::new(WireErrorCode::Cancelled, "invoke cancelled"),
    )
    .await
}

async fn run_host_llm_chat(
    provider: &dyn LlmProvider,
    model_label: &str,
    messages: Vec<LlmMessage>,
) -> Result<Value, ErrorPayload> {
    let mut rx = provider
        .generate(messages, vec![])
        .await
        .map_err(llm_error_payload)?;

    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => {
                text.push_str(&delta);
            },
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => {
                let mut payload = ErrorPayload::new(WireErrorCode::LlmStreamError, message);
                payload.details = Some(serde_json::json!({ "kind": "stream_error" }));
                return Err(payload);
            },
            _ => {},
        }
    }
    serialize_wire_response(
        HostLlmChatOutput {
            content: text,
            model: model_label.to_owned(),
        },
        model_label,
    )
}

fn llm_error_payload(error: LlmError) -> ErrorPayload {
    let details = serde_json::to_value(&error).ok();
    let mut payload = super::wire_payload(error);
    payload.details = details;
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_errors_keep_stable_codes_retryability_and_details() {
        let rate_limited = llm_error_payload(LlmError::RateLimited {
            status: 429,
            retry_after_ms: Some(250),
            message: "slow down".into(),
        });
        assert_eq!(rate_limited.code_enum(), Some(WireErrorCode::RateLimited));
        assert!(rate_limited.retryable);
        assert_eq!(
            rate_limited.details.as_ref().unwrap()["retry_after_ms"],
            250
        );

        let cancelled = llm_error_payload(LlmError::Interrupted);
        assert_eq!(cancelled.code_enum(), Some(WireErrorCode::Cancelled));
        assert!(!cancelled.retryable);
        assert_eq!(cancelled.details.as_ref().unwrap()["kind"], "interrupted");

        let unsupported = llm_error_payload(LlmError::Unsupported {
            message: "counting".into(),
        });
        assert_eq!(unsupported.code_enum(), Some(WireErrorCode::Unsupported));
        assert!(!unsupported.retryable);
    }
}
