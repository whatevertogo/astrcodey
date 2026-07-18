//! 受限的扩展出站 HTTP 客户端。

use std::{
    collections::BTreeMap,
    error::Error,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use astrcode_core::extension::{
    NetworkRedirectPolicy, OutboundNetworkError, OutboundNetworkErrorKind, OutboundNetworkRequest,
    OutboundNetworkResponse, OutboundNetworkService,
};
use astrcode_extension_sdk::{
    s5r::ErrorPayload,
    worker::{HostNetworkRequest, HostNetworkResponse},
};
use futures_util::StreamExt;
use reqwest::{
    Method, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::{Attempt, Policy},
};
use tokio::{
    sync::{Semaphore, SemaphorePermit},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::{HOST_INVOKE_TIMEOUT, block_on_async, capability::NetworkCapability};

#[cfg(test)]
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 64;
const MAX_REDIRECTS: usize = 10;

type DnsError = Box<dyn Error + Send + Sync>;

pub(super) struct NetworkGroup {
    service: Option<Arc<dyn OutboundNetworkService>>,
}

impl NetworkGroup {
    pub(super) fn new(service: Option<Arc<dyn OutboundNetworkService>>) -> Self {
        Self { service }
    }

    pub(super) fn invoke(
        &self,
        capability: NetworkCapability,
        input: serde_json::Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<serde_json::Value, ErrorPayload> {
        match capability {
            NetworkCapability::Client => self.request(input, cancel_token),
        }
    }

    fn request(
        &self,
        input: serde_json::Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<serde_json::Value, ErrorPayload> {
        let request = serde_json::from_value::<HostNetworkRequest>(input)
            .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?;
        let service = self.service.as_ref().map(Arc::clone).ok_or_else(|| {
            ErrorPayload::new("backend_unavailable", "outbound network is not configured")
        })?;
        let cancel_token = cancel_token.cloned();
        block_on_async(async move {
            let response = service
                .request(
                    OutboundNetworkRequest {
                        url: request.url,
                        method: request.method.unwrap_or_else(|| "GET".into()),
                        headers: request.headers,
                        body: request.body.unwrap_or_default().into_bytes(),
                        max_bytes: request.max_bytes.unwrap_or(1024 * 1024).min(1024 * 1024)
                            as usize,
                        timeout: Duration::from_millis(request.timeout_ms.unwrap_or(30_000))
                            .min(HOST_INVOKE_TIMEOUT),
                        redirect_policy: NetworkRedirectPolicy::Follow,
                    },
                    cancel_token,
                )
                .await
                .map_err(network_error_payload)?;
            let body = String::from_utf8(response.body).map_err(|error| {
                ErrorPayload::new(
                    "invalid_response_encoding",
                    format!("network response is not valid UTF-8: {error}"),
                )
            })?;
            serde_json::to_value(HostNetworkResponse {
                final_url: response.final_url,
                status: response.status,
                headers: response.headers,
                body,
            })
            .map_err(|error| ErrorPayload::new("serialization_failed", error.to_string()))
        })?
    }
}

fn network_error_payload(error: OutboundNetworkError) -> ErrorPayload {
    let code = match error.kind {
        OutboundNetworkErrorKind::InvalidRequest => "invalid_input",
        OutboundNetworkErrorKind::PermissionDenied => "permission_denied",
        OutboundNetworkErrorKind::Unavailable => "backend_unavailable",
        OutboundNetworkErrorKind::RequestFailed => "network_error",
        OutboundNetworkErrorKind::Timeout => "timeout",
        OutboundNetworkErrorKind::ResponseTooLarge => "response_too_large",
        OutboundNetworkErrorKind::Cancelled => "cancelled",
    };
    ErrorPayload::new(code, error.message)
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct NetworkPolicyError(String);

#[derive(Debug, Default)]
struct PublicDnsResolver;

impl Resolve for PublicDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            validate_host_name(&host).map_err(dns_error)?;
            let resolved = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|error| Box::new(error) as DnsError)?
                .collect::<Vec<_>>();
            validate_resolved_addresses(&host, &resolved).map_err(dns_error)?;
            Ok(Box::new(resolved.into_iter()) as Addrs)
        })
    }
}

pub struct RestrictedNetworkService {
    follow_redirects_client: Result<reqwest::Client, String>,
    manual_redirects_client: Result<reqwest::Client, String>,
    permits: Semaphore,
}

impl Default for RestrictedNetworkService {
    fn default() -> Self {
        Self {
            follow_redirects_client: build_client(Policy::custom(validate_redirect)),
            manual_redirects_client: build_client(Policy::none()),
            permits: Semaphore::new(MAX_CONCURRENT_REQUESTS),
        }
    }
}

fn build_client(redirect_policy: Policy) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(PublicDnsResolver))
        .redirect(redirect_policy)
        .build()
        .map_err(|error| error.to_string())
}

#[async_trait::async_trait]
impl OutboundNetworkService for RestrictedNetworkService {
    async fn request(
        &self,
        input: OutboundNetworkRequest,
        cancel_token: Option<CancellationToken>,
    ) -> Result<OutboundNetworkResponse, OutboundNetworkError> {
        let timeout = input.timeout.min(MAX_TIMEOUT);
        let deadline = Instant::now() + timeout;
        let client = match input.redirect_policy {
            NetworkRedirectPolicy::Follow => &self.follow_redirects_client,
            NetworkRedirectPolicy::Manual => &self.manual_redirects_client,
        }
        .as_ref()
        .map_err(|message| {
            OutboundNetworkError::new(
                OutboundNetworkErrorKind::Unavailable,
                format!("failed to initialize network client: {message}"),
            )
        })?;
        let method = input.method.parse::<Method>().map_err(|error| {
            OutboundNetworkError::new(
                OutboundNetworkErrorKind::InvalidRequest,
                format!("invalid HTTP method: {error}"),
            )
        })?;
        let parsed_url = Url::parse(&input.url).map_err(|error| {
            OutboundNetworkError::new(OutboundNetworkErrorKind::InvalidRequest, error.to_string())
        })?;
        validate_network_url(&parsed_url).map_err(|message| {
            OutboundNetworkError::new(OutboundNetworkErrorKind::PermissionDenied, message)
        })?;

        let mut request = client
            .request(method, parsed_url)
            .headers(parse_headers(&input.headers)?);
        if !input.body.is_empty() {
            request = request.body(input.body);
        }
        let max_bytes = input.max_bytes.min(MAX_RESPONSE_BYTES);
        let redirect_policy = input.redirect_policy;
        let _permit = self.acquire_permit(deadline, cancel_token.as_ref()).await?;

        let operation = async move {
            let response = request.send().await.map_err(|error| {
                let kind = if error_chain_contains_policy_error(&error) {
                    OutboundNetworkErrorKind::PermissionDenied
                } else {
                    OutboundNetworkErrorKind::RequestFailed
                };
                OutboundNetworkError::new(kind, error.to_string())
            })?;
            let final_url = response.url().to_string();
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
            let body = if redirect_policy == NetworkRedirectPolicy::Manual
                && (300..400).contains(&status)
            {
                Vec::new()
            } else {
                read_limited_body(response, max_bytes).await?
            };
            Ok(OutboundNetworkResponse {
                final_url,
                status,
                headers,
                body,
            })
        };

        run_until_deadline(operation, deadline, cancel_token.as_ref()).await
    }
}

impl RestrictedNetworkService {
    async fn acquire_permit<'a>(
        &'a self,
        deadline: Instant,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<SemaphorePermit<'a>, OutboundNetworkError> {
        let acquire = async {
            timeout_at(deadline, self.permits.acquire())
                .await
                .map_err(|_| {
                    OutboundNetworkError::new(
                        OutboundNetworkErrorKind::Timeout,
                        "network request timed out waiting for capacity",
                    )
                })?
                .map_err(|_| {
                    OutboundNetworkError::new(
                        OutboundNetworkErrorKind::Unavailable,
                        "network client stopped",
                    )
                })
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

fn validate_redirect(attempt: Attempt<'_>) -> reqwest::redirect::Action {
    if attempt.previous().len() >= MAX_REDIRECTS {
        return attempt.error("too many redirects");
    }
    match validate_network_url(attempt.url()) {
        Ok(()) => attempt.follow(),
        Err(message) => attempt.error(NetworkPolicyError(message)),
    }
}

fn validate_network_url(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("network.client only supports HTTP and HTTPS URLs".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("network.client URLs must not contain credentials".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "network.client URL must contain a host".to_string())?;
    validate_host_name(host)
}

fn validate_host_name(host: &str) -> Result<(), String> {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    let ip_literal = normalized
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(&normalized);
    if normalized.is_empty()
        || normalized == "localhost"
        || normalized.ends_with(".localhost")
        || normalized.ends_with(".local")
        || normalized.ends_with(".internal")
        || normalized.ends_with(".home.arpa")
    {
        return Err(format!("network.client host is not public: {host}"));
    }
    match ip_literal.parse::<IpAddr>() {
        Ok(ip) if !is_public_ip(ip) => {
            return Err(format!("network.client address is not public: {ip}"));
        },
        Ok(_) => {},
        Err(_) if !normalized.contains('.') => {
            return Err(format!(
                "network.client host must be a public domain: {host}"
            ));
        },
        Err(_) => {},
    }
    Ok(())
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(ipv4);
    }
    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || (segments[0] == 0x0064
            && segments[1] == 0xff9b
            && (segments[2] == 1 || segments[2..6].iter().all(|segment| *segment == 0)))
        || (segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0)
        || (segments[0] == 0x2001 && segments[1] <= 0x01ff)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] & 0xff00) == 0xff00
        || segments[0] == 0x2002
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        || segments[0] == 0x5f00)
}

fn validate_resolved_addresses(
    host: &str,
    addresses: &[std::net::SocketAddr],
) -> Result<(), String> {
    if addresses.is_empty() {
        return Err(format!("host did not resolve: {host}"));
    }
    if let Some(address) = addresses.iter().find(|address| !is_public_ip(address.ip())) {
        return Err(format!(
            "host resolves to a non-public address: {}",
            address.ip()
        ));
    }
    Ok(())
}

fn dns_error(message: impl Into<String>) -> DnsError {
    Box::new(NetworkPolicyError(message.into()))
}

fn error_chain_contains_policy_error(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(source) = current {
        if source.downcast_ref::<NetworkPolicyError>().is_some() {
            return true;
        }
        current = source.source();
    }
    false
}

async fn run_until_deadline<F, T>(
    operation: F,
    deadline: Instant,
    cancel_token: Option<&CancellationToken>,
) -> Result<T, OutboundNetworkError>
where
    F: Future<Output = Result<T, OutboundNetworkError>>,
{
    let timed = async {
        timeout_at(deadline, operation).await.map_err(|_| {
            OutboundNetworkError::new(
                OutboundNetworkErrorKind::Timeout,
                "network request timed out",
            )
        })?
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
) -> Result<Vec<u8>, OutboundNetworkError> {
    let content_length = response.content_length();
    if content_length.is_some_and(|length| length > max_bytes as u64) {
        return Err(response_too_large(max_bytes));
    }

    let capacity = content_length
        .map(|length| length.min(max_bytes as u64) as usize)
        .unwrap_or_default();
    let mut body = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            OutboundNetworkError::new(OutboundNetworkErrorKind::RequestFailed, error.to_string())
        })?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(response_too_large(max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_headers(entries: &BTreeMap<String, String>) -> Result<HeaderMap, OutboundNetworkError> {
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        let name = name.parse::<HeaderName>().map_err(|error| {
            OutboundNetworkError::new(
                OutboundNetworkErrorKind::InvalidRequest,
                format!("invalid header name: {error}"),
            )
        })?;
        let value = value.parse::<HeaderValue>().map_err(|error| {
            OutboundNetworkError::new(
                OutboundNetworkErrorKind::InvalidRequest,
                format!("invalid header value: {error}"),
            )
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn response_too_large(max_bytes: usize) -> OutboundNetworkError {
    OutboundNetworkError::new(
        OutboundNetworkErrorKind::ResponseTooLarge,
        format!("response exceeds max_bytes {max_bytes}"),
    )
}

fn cancelled() -> OutboundNetworkError {
    OutboundNetworkError::new(
        OutboundNetworkErrorKind::Cancelled,
        "network request cancelled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> OutboundNetworkRequest {
        OutboundNetworkRequest {
            url: url.into(),
            method: "GET".into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            max_bytes: MAX_RESPONSE_BYTES,
            timeout: DEFAULT_TIMEOUT,
            redirect_policy: NetworkRedirectPolicy::Follow,
        }
    }

    #[tokio::test]
    async fn rejects_non_http_urls() {
        let client = RestrictedNetworkService::default();
        let error = client
            .request(request("file:///etc/passwd"), None)
            .await
            .expect_err("file URLs must be rejected");

        assert_eq!(error.kind, OutboundNetworkErrorKind::PermissionDenied);
    }

    #[test]
    fn rejects_non_public_network_destinations() {
        for url in [
            "http://localhost/admin",
            "http://127.0.0.1/admin",
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.1/internal",
            "http://[::1]/admin",
            "http://[64:ff9b:1::a00:1]/internal",
            "http://[100::1]/discard-only",
            "http://[2001:2::1]/benchmark",
            "http://[2001:db8::1]/documentation",
            "http://[3fff::1]/documentation",
            "http://user:secret@example.com/",
        ] {
            let parsed = Url::parse(url).expect("test URL");
            assert!(
                validate_network_url(&parsed).is_err(),
                "{url} must be rejected"
            );
        }
        assert!(validate_network_url(&Url::parse("https://example.com").expect("URL")).is_ok());
    }

    #[test]
    fn rejects_empty_or_mixed_private_dns_results() {
        assert!(validate_resolved_addresses("empty.example", &[]).is_err());
        assert!(
            validate_resolved_addresses(
                "mixed.example",
                &[
                    "93.184.216.34:0".parse().expect("public address"),
                    "127.0.0.1:0".parse().expect("private address"),
                ],
            )
            .is_err()
        );
        assert!(
            validate_resolved_addresses(
                "public.example",
                &["93.184.216.34:0".parse().expect("public address")],
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn capacity_wait_obeys_cancellation() {
        let client = RestrictedNetworkService::default();
        let _permits = client
            .permits
            .acquire_many(MAX_CONCURRENT_REQUESTS as u32)
            .await
            .expect("acquire all permits");
        let token = CancellationToken::new();
        token.cancel();

        let error = client
            .request(request("https://example.com"), Some(token))
            .await
            .expect_err("cancelled capacity wait must stop");

        assert_eq!(error.kind, OutboundNetworkErrorKind::Cancelled);
    }
}
