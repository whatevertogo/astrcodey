use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use astrcode_extension_sdk::{
    host::{
        HOST_NETWORK_MAX_BYTES, HostNetworkRedirectPolicy, HostNetworkRequest, HostNetworkResponse,
        ModelClient, NetworkClient, llm_chat_request,
    },
    llm::{LlmContent, LlmMessage, LlmRole},
};
use parking_lot::Mutex;
use serde::Deserialize;
use url::Url;

use crate::{
    cache::{FetchCacheEntry, FetchUrlCache},
    config::FetchConfig,
    host_timeout_ms,
    preapproved::is_preapproved_url,
    url_guard::{UrlGuardError, is_permitted_redirect, upgrade_http_to_https, validate_fetch_url},
};

const MAX_SUMMARIZER_INPUT_CHARS: usize = 100_000;
const TRUNCATION_MARKER: &str = "\n\n[Content truncated due to length...]";

struct FinalizeInput<'a> {
    prompt: &'a str,
    original_url: &'a str,
    final_url: &'a str,
    status_code: u16,
    content_type: &'a str,
    bytes: usize,
    markdown: &'a str,
    is_preapproved: bool,
    max_output_chars: usize,
    summarizer_max_output_tokens: usize,
    small_llm: Option<&'a ModelClient>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FetchUrlArgs {
    pub(crate) url: String,
    pub(crate) prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchUrlOutcome {
    pub(crate) url: String,
    pub(crate) final_url: String,
    pub(crate) status_code: u16,
    pub(crate) content_type: String,
    pub(crate) bytes: usize,
    pub(crate) duration_ms: u64,
    pub(crate) cached: bool,
    pub(crate) result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchRedirectOutcome {
    pub(crate) original_url: String,
    pub(crate) redirect_url: String,
    pub(crate) status_code: u16,
    pub(crate) duration_ms: u64,
    pub(crate) message: String,
}

#[derive(Debug)]
pub(crate) enum FetchUrlResult {
    Content(FetchUrlOutcome),
    Redirect(FetchRedirectOutcome),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchError {
    #[error("{0}")]
    Url(#[from] UrlGuardError),
    #[error("HTTP request failed: {0}")]
    Http(String),
    #[error("response body is too large (limit {limit} bytes)")]
    ResponseTooLarge { limit: usize },
    #[error("unsupported content type for text extraction: {0}")]
    UnsupportedContentType(String),
    #[error("too many redirects (limit {limit})")]
    TooManyRedirects { limit: usize },
    #[error("HTTP request timed out")]
    Timeout,
    #[error("redirect response missing Location header")]
    MissingRedirectLocation,
    #[error("prompt processing failed: {0}")]
    PromptProcessing(String),
}

pub(crate) async fn run_fetch_url(
    config: &FetchConfig,
    cache: &Arc<Mutex<FetchUrlCache>>,
    network: NetworkClient,
    small_llm: Option<ModelClient>,
    args: FetchUrlArgs,
) -> Result<FetchUrlResult, FetchError> {
    let started = Instant::now();
    let parsed = validate_fetch_url(&args.url)?;
    let original_url = parsed.to_string();
    let request_url = upgrade_http_to_https(&parsed);
    let cache_key = request_url.to_string();

    let cached = cache.lock().get(&cache_key);
    if let Some(entry) = cached {
        let result = finalize_result(FinalizeInput {
            prompt: &args.prompt,
            original_url: &original_url,
            final_url: request_url.as_ref(),
            status_code: entry.status_code,
            content_type: &entry.content_type,
            bytes: entry.bytes,
            markdown: &entry.content,
            is_preapproved: is_preapproved_url(&request_url),
            max_output_chars: config.max_output_chars,
            summarizer_max_output_tokens: config.summarizer_max_output_tokens,
            small_llm: small_llm.as_ref(),
        })
        .await?;
        return Ok(FetchUrlResult::Content(FetchUrlOutcome {
            url: original_url.clone(),
            final_url: request_url.to_string(),
            status_code: entry.status_code,
            content_type: entry.content_type,
            bytes: entry.bytes,
            duration_ms: started.elapsed().as_millis() as u64,
            cached: true,
            result,
        }));
    }

    let fetched = fetch_with_redirect_policy(
        &network,
        &request_url,
        config.request_timeout_secs,
        &config.user_agent,
        config.max_content_bytes,
        config.max_redirects,
    )
    .await?;

    match fetched {
        FetchResponse::Redirect {
            original_url,
            redirect_url,
            status_code,
        } => {
            let message = format!(
                "REDIRECT DETECTED: The URL redirects to a different host.\n\nOriginal URL: \
                 {original_url}\nRedirect URL: {redirect_url}\nStatus: {status_code}\n\nTo \
                 complete your request, fetch the redirected URL instead:\n- url: \
                 \"{redirect_url}\"\n- prompt: \"{}\"",
                escape_for_prompt(&args.prompt)
            );
            Ok(FetchUrlResult::Redirect(FetchRedirectOutcome {
                original_url,
                redirect_url,
                status_code,
                duration_ms: started.elapsed().as_millis() as u64,
                message,
            }))
        },
        FetchResponse::Body {
            final_url,
            status_code,
            content_type,
            bytes,
            markdown,
        } => {
            cache.lock().insert(
                cache_key,
                FetchCacheEntry {
                    content: markdown.clone(),
                    content_type: content_type.clone(),
                    status_code,
                    bytes,
                    cached_at: Instant::now(),
                },
            );
            let result = finalize_result(FinalizeInput {
                prompt: &args.prompt,
                original_url: &original_url,
                final_url: &final_url,
                status_code,
                content_type: &content_type,
                bytes,
                markdown: &markdown,
                is_preapproved: Url::parse(&final_url)
                    .ok()
                    .as_ref()
                    .is_some_and(is_preapproved_url),
                max_output_chars: config.max_output_chars,
                summarizer_max_output_tokens: config.summarizer_max_output_tokens,
                small_llm: small_llm.as_ref(),
            })
            .await?;
            Ok(FetchUrlResult::Content(FetchUrlOutcome {
                url: original_url,
                final_url,
                status_code,
                content_type,
                bytes,
                duration_ms: started.elapsed().as_millis() as u64,
                cached: false,
                result,
            }))
        },
    }
}

enum FetchResponse {
    Redirect {
        original_url: String,
        redirect_url: String,
        status_code: u16,
    },
    Body {
        final_url: String,
        status_code: u16,
        content_type: String,
        bytes: usize,
        markdown: String,
    },
}

async fn fetch_with_redirect_policy(
    network: &NetworkClient,
    url: &Url,
    timeout_secs: u64,
    user_agent: &str,
    max_content_bytes: usize,
    max_redirects: usize,
) -> Result<FetchResponse, FetchError> {
    let mut current = url.clone();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(1, 60));
    let response_limit = max_content_bytes.min(HOST_NETWORK_MAX_BYTES);
    for depth in 0..=max_redirects {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FetchError::Timeout);
        }
        let response = network
            .send(HostNetworkRequest {
                url: current.to_string(),
                method: "GET".into(),
                headers: BTreeMap::from([
                    (
                        "accept".into(),
                        "text/markdown, text/html, text/plain, application/json, */*".into(),
                    ),
                    ("user-agent".into(), user_agent.into()),
                ]),
                body: Vec::new(),
                max_bytes: response_limit,
                timeout_ms: host_timeout_ms(remaining),
                redirect_policy: HostNetworkRedirectPolicy::Manual,
            })
            .await
            .map_err(|error| match error.code.as_str() {
                "response_too_large" => FetchError::ResponseTooLarge {
                    limit: response_limit,
                },
                _ => FetchError::Http(error.to_string()),
            })?;

        if (300..400).contains(&response.status) {
            if depth == max_redirects {
                return Err(FetchError::TooManyRedirects {
                    limit: max_redirects,
                });
            }
            let status_code = response.status;
            let redirect_url = resolve_redirect_location(&response.headers, current.as_str())?;
            let redirect_parsed =
                Url::parse(&redirect_url).map_err(|error| FetchError::Http(error.to_string()))?;
            if is_permitted_redirect(&current, &redirect_parsed) {
                current = redirect_parsed;
                continue;
            }
            return Ok(FetchResponse::Redirect {
                original_url: url.to_string(),
                redirect_url,
                status_code,
            });
        }

        let status_code = response.status;
        let content_type = content_type(&response);
        let markdown = extract_markdown(&content_type, &response.body)?;
        let final_url = response.final_url;
        return Ok(FetchResponse::Body {
            final_url,
            status_code,
            content_type,
            bytes: response.body.len(),
            markdown,
        });
    }

    Err(FetchError::TooManyRedirects {
        limit: max_redirects,
    })
}

fn resolve_redirect_location(
    headers: &BTreeMap<String, String>,
    base_url: &str,
) -> Result<String, FetchError> {
    let location = headers
        .get("location")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(FetchError::MissingRedirectLocation)?;
    Url::parse(location)
        .or_else(|_| Url::parse(base_url).and_then(|base| base.join(location)))
        .map(|url| url.to_string())
        .map_err(|error| FetchError::Http(error.to_string()))
}

async fn finalize_result(input: FinalizeInput<'_>) -> Result<String, FetchError> {
    let result = if input.status_code >= 400 {
        format!(
            "Fetched `{}` returned HTTP {}.\nContent-Type: {}\nBytes: {}\n\n{}",
            input.final_url, input.status_code, input.content_type, input.bytes, input.markdown
        )
    } else if input.is_preapproved
        && input.content_type.contains("markdown")
        && input.markdown.chars().count() < MAX_SUMMARIZER_INPUT_CHARS
    {
        input.markdown.to_string()
    } else if let Some(llm) = input.small_llm {
        apply_prompt_to_markdown(
            llm,
            input.prompt,
            input.markdown,
            input.is_preapproved,
            input.summarizer_max_output_tokens,
        )
        .await?
    } else {
        format!(
            "Fetched `{}` from `{}`.\n\nPrompt: {}\n\nContent:\n---\n{}\n---\n\nNote: Small LLM \
             is not configured, so the raw page content was returned instead of a prompt-focused \
             summary.",
            input.final_url, input.original_url, input.prompt, input.markdown
        )
    };

    Ok(truncate_text(&result, input.max_output_chars))
}

async fn apply_prompt_to_markdown(
    small_llm: &ModelClient,
    prompt: &str,
    markdown: &str,
    is_preapproved: bool,
    max_output_tokens: usize,
) -> Result<String, FetchError> {
    let truncated = truncate_text(markdown, MAX_SUMMARIZER_INPUT_CHARS);
    let user_prompt = make_secondary_model_prompt(&truncated, prompt, is_preapproved);
    let messages = vec![LlmMessage {
        role: LlmRole::User,
        content: vec![LlmContent::Text { text: user_prompt }],
        name: None,
        reasoning_content: None,
    }];
    let output = small_llm
        .small_chat_collected_request(
            llm_chat_request(messages).with_max_output_tokens(max_output_tokens),
        )
        .await
        .map_err(|error| FetchError::PromptProcessing(error.to_string()))?;
    Ok(output.content)
}

fn make_secondary_model_prompt(
    markdown_content: &str,
    prompt: &str,
    is_preapproved_domain: bool,
) -> String {
    let guidelines = if is_preapproved_domain {
        "Provide a concise response based on the content above. Include relevant details, code \
         examples, and documentation excerpts as needed."
            .to_string()
    } else {
        "Provide a concise response based only on the content above. Use quotation marks for exact \
         language from the page; paraphrase everything else."
            .to_string()
    };

    format!("Web page content:\n---\n{markdown_content}\n---\n\n{prompt}\n\n{guidelines}")
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let marker_chars = TRUNCATION_MARKER.chars().count();
    if max_chars <= marker_chars {
        return text.chars().take(max_chars).collect();
    }
    let prefix: String = text.chars().take(max_chars - marker_chars).collect();
    format!("{prefix}{TRUNCATION_MARKER}")
}

fn content_type(response: &HostNetworkResponse) -> String {
    response
        .headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_ascii_lowercase()
}

fn extract_markdown(content_type: &str, body: &[u8]) -> Result<String, FetchError> {
    if content_type.starts_with("text/html") || content_type.contains("html") {
        let html = String::from_utf8_lossy(body);
        return html2text::from_read(html.as_bytes(), 120)
            .map_err(|error| FetchError::Http(error.to_string()));
    }
    if content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("markdown")
    {
        return Ok(String::from_utf8_lossy(body).into_owned());
    }
    Err(FetchError::UnsupportedContentType(content_type.to_string()))
}

fn escape_for_prompt(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn render_fetch_content(outcome: &FetchUrlOutcome, max_output_chars: usize) -> String {
    let content = format!(
        "Fetched `{}` (HTTP {})\nFinal URL: {}\nContent-Type: {}\nBytes: {}\nDuration: \
         {}ms{}\n\n{}",
        outcome.url,
        outcome.status_code,
        outcome.final_url,
        outcome.content_type,
        outcome.bytes,
        outcome.duration_ms,
        if outcome.cached { " (cache hit)" } else { "" },
        outcome.result
    );
    truncate_text(&content, max_output_chars)
}

pub(crate) fn render_fetch_redirect(
    outcome: &FetchRedirectOutcome,
    max_output_chars: usize,
) -> String {
    truncate_text(&outcome.message, max_output_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_is_converted_to_text() {
        let html = b"<html><body><h1>Title</h1><p>Body text</p></body></html>";
        let text = extract_markdown("text/html; charset=utf-8", html).expect("html text");
        assert!(text.contains("Title"));
        assert!(text.contains("Body text"));
    }

    #[test]
    fn rejects_unsupported_binary_content() {
        let err = extract_markdown("application/pdf", b"%PDF-1.4").expect_err("pdf");
        assert!(matches!(err, FetchError::UnsupportedContentType(_)));
    }

    #[test]
    fn resolve_redirect_location_supports_relative_paths() {
        let headers = BTreeMap::from([("location".into(), "/docs/page".into())]);
        let resolved =
            resolve_redirect_location(&headers, "https://example.com/start").expect("redirect");
        assert_eq!(resolved, "https://example.com/docs/page");
    }

    #[tokio::test]
    async fn final_output_limit_applies_to_every_non_model_path() {
        let markdown = "x".repeat(1_000);
        for (status_code, content_type, is_preapproved) in [
            (200, "text/markdown", true),
            (200, "text/plain", false),
            (500, "text/plain", false),
        ] {
            let result = finalize_result(FinalizeInput {
                prompt: "summarize",
                original_url: "https://example.com/start",
                final_url: "https://example.com/final",
                status_code,
                content_type,
                bytes: 1_000,
                markdown: &markdown,
                is_preapproved,
                max_output_chars: 80,
                summarizer_max_output_tokens: 256,
                small_llm: None,
            })
            .await
            .expect("bounded result");
            assert_eq!(result.chars().count(), 80);
            assert!(result.ends_with(TRUNCATION_MARKER));

            let rendered = render_fetch_content(
                &FetchUrlOutcome {
                    url: "https://example.com/start".into(),
                    final_url: "https://example.com/final".into(),
                    status_code,
                    content_type: content_type.into(),
                    bytes: 1_000,
                    duration_ms: 1,
                    cached: false,
                    result,
                },
                80,
            );
            assert_eq!(rendered.chars().count(), 80);
            assert!(rendered.ends_with(TRUNCATION_MARKER));
        }

        let redirect = render_fetch_redirect(
            &FetchRedirectOutcome {
                original_url: "https://example.com/start".into(),
                redirect_url: "https://other.example/final".into(),
                status_code: 302,
                duration_ms: 1,
                message: "x".repeat(1_000),
            },
            80,
        );
        assert_eq!(redirect.chars().count(), 80);
        assert!(redirect.ends_with(TRUNCATION_MARKER));
    }
}
