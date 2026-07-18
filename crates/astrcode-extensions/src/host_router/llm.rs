//! LLM host capabilities.

use std::sync::Arc;

use astrcode_core::llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole};
use astrcode_extension_sdk::s5r::ErrorPayload;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{HOST_INVOKE_TIMEOUT, block_on_async, capability::LlmCapability};

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

    pub(super) fn invoke(
        &self,
        capability: LlmCapability,
        input: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        self.invoke_with_mode(capability, input, false, cancel_token)
    }

    pub(super) fn invoke_stream(
        &self,
        capability: LlmCapability,
        input: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        self.invoke_with_mode(capability, input, true, cancel_token)
    }

    fn invoke_with_mode(
        &self,
        capability: LlmCapability,
        input: &Value,
        collect_chunks: bool,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        match capability {
            LlmCapability::MainChat => {
                let provider = self.main.as_ref().ok_or_else(|| {
                    ErrorPayload::new("backend_unavailable", "main_llm not configured")
                })?;
                invoke_llm_chat(provider, "main_llm", input, collect_chunks, cancel_token)
            },
            LlmCapability::SmallChat => {
                let provider = self.small.as_ref().ok_or_else(|| {
                    ErrorPayload::new("backend_unavailable", "small_llm not configured")
                })?;
                invoke_llm_chat(provider, "small_llm", input, collect_chunks, cancel_token)
            },
        }
    }
}

fn invoke_llm_chat(
    provider: &Arc<dyn LlmProvider>,
    model_label: &'static str,
    input: &Value,
    collect_chunks: bool,
    cancel_token: Option<&CancellationToken>,
) -> Result<Value, ErrorPayload> {
    let messages = input["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| {
                    let role = match message["role"].as_str()? {
                        "user" => LlmRole::User,
                        "assistant" => LlmRole::Assistant,
                        "system" => LlmRole::System,
                        _ => LlmRole::User,
                    };
                    let content = message["content"].as_str().unwrap_or("").to_string();
                    Some(LlmMessage {
                        role,
                        content: vec![LlmContent::Text { text: content }],
                        name: None,
                        reasoning_content: None,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if messages.is_empty() {
        return Err(ErrorPayload::new(
            "invalid_input",
            "messages array is empty or missing",
        ));
    }

    let cancel = cancel_token.cloned();
    let provider = Arc::clone(provider);
    let label = model_label.to_string();
    block_on_async(async move {
        tokio::time::timeout(
            HOST_INVOKE_TIMEOUT,
            run_host_llm_chat(
                &*provider,
                &label,
                messages,
                collect_chunks,
                cancel.as_ref(),
            ),
        )
        .await
        .map_err(|_| ErrorPayload::new("timeout", format!("{label}.chat timed out")))?
    })?
}

async fn run_host_llm_chat(
    provider: &dyn LlmProvider,
    model_label: &str,
    messages: Vec<LlmMessage>,
    collect_chunks: bool,
    cancel_token: Option<&CancellationToken>,
) -> Result<Value, ErrorPayload> {
    let mut rx = provider
        .generate(messages, vec![])
        .await
        .map_err(|error| ErrorPayload::new("llm_error", error.to_string()))?;

    let mut text = String::new();
    let mut chunks = Vec::new();
    loop {
        let event = if let Some(token) = cancel_token {
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    return Err(ErrorPayload::new("cancelled", "invoke cancelled"));
                }
                event = rx.recv() => event,
            }
        } else {
            rx.recv().await
        };
        let Some(event) = event else {
            break;
        };
        match event {
            LlmEvent::ContentDelta { delta } => {
                if collect_chunks {
                    chunks.push(json!({ "delta": delta }));
                }
                text.push_str(&delta);
            },
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => {
                return Err(ErrorPayload::new("llm_error", message));
            },
            _ => {},
        }
    }
    if collect_chunks {
        Ok(json!({
            "content": text,
            "model": model_label,
            "chunks": chunks
        }))
    } else {
        Ok(json!({ "content": text, "model": model_label }))
    }
}
