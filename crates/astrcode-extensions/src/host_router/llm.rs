//! LLM host capabilities.

use std::sync::Arc;

use astrcode_core::llm::{LlmError, LlmEvent, LlmMessage, LlmProvider};
use astrcode_extension_sdk::{
    host::{
        HOST_ERROR_CODE_BACKEND_UNAVAILABLE, HOST_ERROR_CODE_CANCELLED,
        HOST_ERROR_CODE_INVALID_INPUT, HOST_ERROR_CODE_SERIALIZATION_FAILED,
        HOST_ERROR_CODE_TIMEOUT, HOST_ERROR_CODE_TRANSPORT, HostLlmChatOutput, HostLlmChatRequest,
        HostLlmCollectedStreamOutput, HostLlmTextDelta,
    },
    s5r::ErrorPayload,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{HOST_INVOKE_TIMEOUT, capability::LlmCapability};

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
        capability: LlmCapability,
        input: Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        self.invoke_with_mode(capability, input, false, cancel_token)
            .await
    }

    pub(super) fn is_available(&self, capability: LlmCapability) -> bool {
        match capability {
            LlmCapability::MainChat => self.main.is_some(),
            LlmCapability::SmallChat => self.small.is_some(),
        }
    }

    pub(super) async fn invoke_stream(
        &self,
        capability: LlmCapability,
        input: Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        self.invoke_with_mode(capability, input, true, cancel_token)
            .await
    }

    async fn invoke_with_mode(
        &self,
        capability: LlmCapability,
        input: Value,
        collect_chunks: bool,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        let (provider, model_label) = match capability {
            LlmCapability::MainChat => (self.main.as_ref(), "main_llm"),
            LlmCapability::SmallChat => (self.small.as_ref(), "small_llm"),
        };
        let provider = provider.ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
                format!("{model_label} not configured"),
            )
        })?;
        invoke_llm_chat(provider, model_label, input, collect_chunks, cancel_token).await
    }
}

async fn invoke_llm_chat(
    provider: &Arc<dyn LlmProvider>,
    model_label: &'static str,
    input: Value,
    collect_chunks: bool,
    cancel_token: Option<&CancellationToken>,
) -> Result<Value, ErrorPayload> {
    let request = serde_json::from_value::<HostLlmChatRequest>(input).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            format!("invalid {model_label}.chat request: {error}"),
        )
    })?;

    if request.messages.is_empty() {
        return Err(ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            "messages must contain at least one typed LLM message",
        ));
    }
    let messages = request.into_messages();

    let invoke = tokio::time::timeout(
        HOST_INVOKE_TIMEOUT,
        run_host_llm_chat(&**provider, model_label, messages, collect_chunks),
    );
    if let Some(token) = cancel_token {
        tokio::select! {
            biased;
            () = token.cancelled() => {
                Err(ErrorPayload::new(HOST_ERROR_CODE_CANCELLED, "invoke cancelled"))
            },
            output = invoke => output.map_err(|_| {
                ErrorPayload::new(
                    HOST_ERROR_CODE_TIMEOUT,
                    format!("{model_label}.chat timed out"),
                )
            })?,
        }
    } else {
        invoke.await.map_err(|_| {
            ErrorPayload::new(
                HOST_ERROR_CODE_TIMEOUT,
                format!("{model_label}.chat timed out"),
            )
        })?
    }
}

async fn run_host_llm_chat(
    provider: &dyn LlmProvider,
    model_label: &str,
    messages: Vec<LlmMessage>,
    collect_chunks: bool,
) -> Result<Value, ErrorPayload> {
    let mut rx = provider
        .generate(messages, vec![])
        .await
        .map_err(llm_error_payload)?;

    let mut text = String::new();
    let mut chunks = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => {
                if collect_chunks {
                    chunks.push(HostLlmTextDelta {
                        delta: delta.clone(),
                    });
                }
                text.push_str(&delta);
            },
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => {
                let mut payload = ErrorPayload::new("llm_stream_error", message);
                payload.details = Some(serde_json::json!({ "kind": "stream_error" }));
                return Err(payload);
            },
            _ => {},
        }
    }
    if collect_chunks {
        serialize_output(
            HostLlmCollectedStreamOutput {
                content: text,
                model: model_label.to_owned(),
                chunks,
            },
            model_label,
        )
    } else {
        serialize_output(
            HostLlmChatOutput {
                content: text,
                model: model_label.to_owned(),
            },
            model_label,
        )
    }
}

fn serialize_output(
    output: impl serde::Serialize,
    model_label: &str,
) -> Result<Value, ErrorPayload> {
    serde_json::to_value(output).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_SERIALIZATION_FAILED,
            format!("failed to serialize {model_label}.chat response: {error}"),
        )
    })
}

fn llm_error_payload(error: LlmError) -> ErrorPayload {
    let code = match &error {
        LlmError::InvalidApiKey { .. } => "invalid_api_key",
        LlmError::ModelNotFound { .. } => "model_not_found",
        LlmError::InvalidParameter { .. } => "invalid_parameter",
        LlmError::QuotaExceeded { .. } => "quota_exceeded",
        LlmError::ContextWindowExceeded { .. } => "context_window_exceeded",
        LlmError::RateLimited { .. } => "rate_limited",
        LlmError::ClientError { .. } => "client_error",
        LlmError::ServerError { .. } => "server_error",
        LlmError::Transport { .. } => HOST_ERROR_CODE_TRANSPORT,
        LlmError::StreamDisconnected { .. } => "stream_disconnected",
        LlmError::StreamParse { .. } => "stream_parse",
        LlmError::ContentFilter { .. } => "content_filtered",
        LlmError::TokenLimit { .. } => "token_limit",
        LlmError::EmptyResponse => "empty_response",
        LlmError::Interrupted => HOST_ERROR_CODE_CANCELLED,
        LlmError::Unsupported { .. } => "unsupported",
    };
    let retryable = error.is_retryable();
    let details = serde_json::to_value(&error).ok();
    let mut payload = ErrorPayload::new(code, error.to_string()).retryable(retryable);
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
        assert_eq!(rate_limited.code, "rate_limited");
        assert!(rate_limited.retryable);
        assert_eq!(
            rate_limited.details.as_ref().unwrap()["retry_after_ms"],
            250
        );

        let cancelled = llm_error_payload(LlmError::Interrupted);
        assert_eq!(cancelled.code, "cancelled");
        assert!(!cancelled.retryable);
        assert_eq!(cancelled.details.as_ref().unwrap()["kind"], "interrupted");

        let unsupported = llm_error_payload(LlmError::Unsupported {
            message: "counting".into(),
        });
        assert_eq!(unsupported.code, "unsupported");
        assert!(!unsupported.retryable);
    }
}
