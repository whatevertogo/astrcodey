//! 受限的扩展出站 HTTP 客户端。

use std::{collections::BTreeMap, future::Future, time::Duration};

use astrcode_extension_sdk::s5r::ErrorPayload;
use futures_util::StreamExt;
use reqwest::{
    Method,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use tokio::{
    sync::{Semaphore, SemaphorePermit},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 64;

pub(super) struct NetworkClient {
    client: Result<reqwest::Client, String>,
    permits: Semaphore,
}

impl Default for NetworkClient {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|error| error.to_string());
        Self {
            client,
            permits: Semaphore::new(MAX_CONCURRENT_REQUESTS),
        }
    }
}

impl NetworkClient {
    pub(super) async fn request(
        &self,
        input: Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        let timeout = bounded_timeout(&input, DEFAULT_TIMEOUT, MAX_TIMEOUT);
        let deadline = Instant::now() + timeout;
        let _permit = self.acquire_permit(deadline, cancel_token).await?;
        let client = self.client.as_ref().map_err(|message| {
            ErrorPayload::new(
                "backend_unavailable",
                format!("failed to initialize network client: {message}"),
            )
        })?;
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .parse::<Method>()
            .map_err(|error| {
                ErrorPayload::new("invalid_input", format!("invalid HTTP method: {error}"))
            })?;
        let url = required_string(&input, "url")?;
        let parsed_url = reqwest::Url::parse(url)
            .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(ErrorPayload::new(
                "permission_denied",
                "network.client only supports HTTP and HTTPS URLs",
            ));
        }

        let mut request = client.request(method, parsed_url);
        if let Some(headers) = input.get("headers") {
            request = request.headers(parse_headers(headers)?);
        }
        if let Some(body) = input.get("body").and_then(Value::as_str) {
            request = request.body(body.to_owned());
        }
        let max_bytes = input
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64)
            .min(DEFAULT_MAX_BYTES as u64) as usize;

        let operation = async move {
            let response = request
                .send()
                .await
                .map_err(|error| ErrorPayload::new("network_error", error.to_string()))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect::<BTreeMap<_, _>>();
            let body = read_limited_body(response, max_bytes).await?;
            Ok(json!({
                "status": status,
                "headers": headers,
                "body": String::from_utf8_lossy(&body),
            }))
        };

        run_until_deadline(operation, deadline, cancel_token).await
    }

    async fn acquire_permit<'a>(
        &'a self,
        deadline: Instant,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<SemaphorePermit<'a>, ErrorPayload> {
        let acquire = async {
            timeout_at(deadline, self.permits.acquire())
                .await
                .map_err(|_| {
                    ErrorPayload::new("timeout", "network request timed out waiting for capacity")
                })?
                .map_err(|_| ErrorPayload::new("backend_unavailable", "network client stopped"))
        };
        match cancel_token {
            Some(token) => {
                tokio::select! {
                    biased;
                    () = token.cancelled() => Err(cancelled()),
                    result = acquire => result,
                }
            },
            None => acquire.await,
        }
    }
}

async fn run_until_deadline<F>(
    operation: F,
    deadline: Instant,
    cancel_token: Option<&CancellationToken>,
) -> Result<Value, ErrorPayload>
where
    F: Future<Output = Result<Value, ErrorPayload>>,
{
    let timed = async {
        timeout_at(deadline, operation)
            .await
            .map_err(|_| ErrorPayload::new("timeout", "network request timed out"))?
    };
    match cancel_token {
        Some(token) => {
            tokio::select! {
                biased;
                () = token.cancelled() => Err(cancelled()),
                result = timed => result,
            }
        },
        None => timed.await,
    }
}

async fn read_limited_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ErrorPayload> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(response_too_large(max_bytes));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ErrorPayload::new("network_error", error.to_string()))?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(response_too_large(max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str, ErrorPayload> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ErrorPayload::new("invalid_input", format!("{key} must be a string")))
}

fn parse_headers(value: &Value) -> Result<HeaderMap, ErrorPayload> {
    let entries = value.as_object().ok_or_else(|| {
        ErrorPayload::new("invalid_input", "headers must be an object of strings")
    })?;
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        let name = name.parse::<HeaderName>().map_err(|error| {
            ErrorPayload::new("invalid_input", format!("invalid header name: {error}"))
        })?;
        let value = value
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "header values must be strings"))?;
        let value = value.parse::<HeaderValue>().map_err(|error| {
            ErrorPayload::new("invalid_input", format!("invalid header value: {error}"))
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn bounded_timeout(input: &Value, default: Duration, maximum: Duration) -> Duration {
    input
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(default)
        .min(maximum)
}

fn response_too_large(max_bytes: usize) -> ErrorPayload {
    ErrorPayload::new(
        "response_too_large",
        format!("response exceeds max_bytes {max_bytes}"),
    )
}

fn cancelled() -> ErrorPayload {
    ErrorPayload::new("cancelled", "network request cancelled")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_non_http_urls() {
        let client = NetworkClient::default();
        let error = client
            .request(json!({ "url": "file:///etc/passwd" }), None)
            .await
            .expect_err("file URLs must be rejected");

        assert_eq!(error.code, "permission_denied");
    }

    #[tokio::test]
    async fn capacity_wait_obeys_cancellation() {
        let client = NetworkClient::default();
        let _permits = client
            .permits
            .acquire_many(MAX_CONCURRENT_REQUESTS as u32)
            .await
            .expect("acquire all permits");
        let token = CancellationToken::new();
        token.cancel();

        let error = client
            .request(json!({ "url": "https://example.com" }), Some(&token))
            .await
            .expect_err("cancelled capacity wait must stop");

        assert_eq!(error.code, "cancelled");
    }
}
