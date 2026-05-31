use std::{sync::Arc, time::Instant};

use astrcode_extension_sdk::llm::{
    LlmContent, LlmMessage, LlmProvider, LlmRole, collect_stream_text,
};
use parking_lot::Mutex;
use reqwest::{
    Client,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, LOCATION},
};
use serde::Deserialize;
use url::Url;

use crate::{
    cache::{FetchCacheEntry, FetchUrlCache},
    config::FetchConfig,
    http::build_fetch_client,
    preapproved::is_preapproved_url,
    url_guard::{UrlGuardError, is_permitted_redirect, upgrade_http_to_https, validate_fetch_url},
};

const MAX_MARKDOWN_LENGTH: usize = 100_000;

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
    small_llm: Option<&'a dyn LlmProvider>,
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
    #[error("redirect response missing Location header")]
    MissingRedirectLocation,
    #[error("prompt processing failed: {0}")]
    PromptProcessing(String),
}

pub(crate) async fn run_fetch_url(
    config: &FetchConfig,
    cache: &Arc<Mutex<FetchUrlCache>>,
    small_llm: Option<Arc<dyn LlmProvider>>,
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
            small_llm: small_llm.as_deref(),
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

    let client = build_fetch_client(config.request_timeout_secs, &config.user_agent)
        .map_err(|error| FetchError::Http(error.to_string()))?;
    let fetched = fetch_with_redirect_policy(
        &client,
        &request_url,
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
                small_llm: small_llm.as_deref(),
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
    client: &Client,
    url: &Url,
    max_content_bytes: usize,
    max_redirects: usize,
) -> Result<FetchResponse, FetchError> {
    let mut current = url.clone();
    for depth in 0..=max_redirects {
        let response = client
            .get(current.clone())
            .header(
                ACCEPT,
                "text/markdown, text/html, text/plain, application/json, */*",
            )
            .send()
            .await
            .map_err(|error| FetchError::Http(error.to_string()))?;

        if response.status().is_redirection() {
            if depth == max_redirects {
                return Err(FetchError::TooManyRedirects {
                    limit: max_redirects,
                });
            }
            let status_code = response.status().as_u16();
            let redirect_url = resolve_redirect_location(response.headers(), current.as_str())?;
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

        let status_code = response.status().as_u16();
        let final_url = response.url().to_string();
        let headers = response.headers().clone();
        if let Some(content_length) = response.content_length() {
            if content_length as usize > max_content_bytes {
                return Err(FetchError::ResponseTooLarge {
                    limit: max_content_bytes,
                });
            }
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| FetchError::Http(error.to_string()))?;
        if body.len() > max_content_bytes {
            return Err(FetchError::ResponseTooLarge {
                limit: max_content_bytes,
            });
        }

        let content_type = content_type(&headers);
        let markdown = extract_markdown(&content_type, &body)?;
        return Ok(FetchResponse::Body {
            final_url,
            status_code,
            content_type,
            bytes: body.len(),
            markdown,
        });
    }

    Err(FetchError::TooManyRedirects {
        limit: max_redirects,
    })
}

fn resolve_redirect_location(headers: &HeaderMap, base_url: &str) -> Result<String, FetchError> {
    let location = headers
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(FetchError::MissingRedirectLocation)?;
    Url::parse(location)
        .or_else(|_| Url::parse(base_url).and_then(|base| base.join(location)))
        .map(|url| url.to_string())
        .map_err(|error| FetchError::Http(error.to_string()))
}

async fn finalize_result(input: FinalizeInput<'_>) -> Result<String, FetchError> {
    if input.status_code >= 400 {
        return Ok(format!(
            "Fetched `{}` returned HTTP {}.\nContent-Type: {}\nBytes: {}\n\n{}",
            input.final_url, input.status_code, input.content_type, input.bytes, input.markdown
        ));
    }

    if input.is_preapproved
        && input.content_type.contains("markdown")
        && input.markdown.chars().count() < MAX_MARKDOWN_LENGTH
    {
        return Ok(input.markdown.to_string());
    }

    if let Some(llm) = input.small_llm {
        return apply_prompt_to_markdown(
            llm,
            input.prompt,
            input.markdown,
            input.is_preapproved,
            input.max_output_chars,
        )
        .await;
    }

    Ok(format!(
        "Fetched `{}` from `{}`.\n\nPrompt: {}\n\nContent:\n---\n{}\n---\n\nNote: Small LLM is \
         not configured, so the raw page content was returned instead of a prompt-focused summary.",
        input.final_url, input.original_url, input.prompt, input.markdown
    ))
}

async fn apply_prompt_to_markdown(
    small_llm: &dyn LlmProvider,
    prompt: &str,
    markdown: &str,
    is_preapproved: bool,
    max_output_chars: usize,
) -> Result<String, FetchError> {
    let truncated = truncate_markdown(markdown, max_output_chars.min(MAX_MARKDOWN_LENGTH));
    let user_prompt = make_secondary_model_prompt(&truncated, prompt, is_preapproved);
    let messages = vec![LlmMessage {
        role: LlmRole::User,
        content: vec![LlmContent::Text { text: user_prompt }],
        name: None,
        reasoning_content: None,
    }];
    let rx = small_llm
        .generate(messages, vec![])
        .await
        .map_err(|error| FetchError::PromptProcessing(error.to_string()))?;
    let text = collect_stream_text(rx)
        .await
        .unwrap_or_else(|_| "No response from model".into());
    Ok(text)
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

fn truncate_markdown(markdown: &str, max_chars: usize) -> String {
    if markdown.chars().count() <= max_chars {
        return markdown.to_string();
    }
    let truncated: String = markdown.chars().take(max_chars).collect();
    format!("{truncated}\n\n[Content truncated due to length...]")
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
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
        return Ok(html2text::from_read(html.as_bytes(), 120));
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

pub(crate) fn render_fetch_content(outcome: &FetchUrlOutcome) -> String {
    format!(
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
    )
}

pub(crate) fn render_fetch_redirect(outcome: &FetchRedirectOutcome) -> String {
    outcome.message.clone()
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
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, "/docs/page".parse().expect("header"));
        let resolved =
            resolve_redirect_location(&headers, "https://example.com/start").expect("redirect");
        assert_eq!(resolved, "https://example.com/docs/page");
    }

    #[test]
    fn redirect_status_codes_are_recognized() {
        use reqwest::StatusCode;
        assert!(StatusCode::FOUND.is_redirection());
        assert!(StatusCode::TEMPORARY_REDIRECT.is_redirection());
    }
}
