use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

pub(crate) const MAX_URL_LENGTH: usize = 2000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum UrlGuardError {
    #[error("URL must not be empty")]
    Empty,
    #[error("URL exceeds maximum length of {MAX_URL_LENGTH} characters")]
    TooLong,
    #[error("invalid URL: {0}")]
    Invalid(String),
    #[error("only http and https URLs are supported")]
    UnsupportedScheme,
    #[error("URLs with embedded credentials are not supported")]
    EmbeddedCredentials,
    #[error("URL host is not allowed: {0}")]
    BlockedHost(String),
}

pub(crate) fn validate_fetch_url(raw: &str) -> Result<Url, UrlGuardError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(UrlGuardError::Empty);
    }
    if trimmed.len() > MAX_URL_LENGTH {
        return Err(UrlGuardError::TooLong);
    }

    let parsed = Url::parse(trimmed).map_err(|error| UrlGuardError::Invalid(error.to_string()))?;
    match parsed.scheme() {
        "http" | "https" => {},
        _ => return Err(UrlGuardError::UnsupportedScheme),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(UrlGuardError::EmbeddedCredentials);
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| UrlGuardError::Invalid("missing host".into()))?;
    if is_blocked_host(host) {
        return Err(UrlGuardError::BlockedHost(host.to_string()));
    }
    if host.split('.').count() < 2 {
        return Err(UrlGuardError::Invalid(
            "hostname must contain a public domain".into(),
        ));
    }
    Ok(parsed)
}

pub(crate) fn upgrade_http_to_https(url: &Url) -> Url {
    if url.scheme() == "http" {
        let mut upgraded = url.clone();
        let _ = upgraded.set_scheme("https");
        return upgraded;
    }
    url.clone()
}

pub(crate) fn is_permitted_redirect(original: &Url, redirect: &Url) -> bool {
    if original.scheme() != redirect.scheme() {
        return false;
    }
    if original.port_or_known_default() != redirect.port_or_known_default() {
        return false;
    }
    if !redirect.username().is_empty() || redirect.password().is_some() {
        return false;
    }

    fn strip_www(host: &str) -> String {
        host.strip_prefix("www.")
            .unwrap_or(host)
            .to_ascii_lowercase()
    }
    let Some(original_host) = original.host_str() else {
        return false;
    };
    let Some(redirect_host) = redirect.host_str() else {
        return false;
    };
    strip_www(original_host) == strip_www(redirect_host)
}

fn is_blocked_host(host: &str) -> bool {
    let host_lower = host.to_ascii_lowercase();
    if matches!(
        host_lower.as_str(),
        "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"
    ) {
        return true;
    }
    if host_lower.ends_with(".local") || host_lower.ends_with(".internal") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(ip);
    }
    false
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_private_ipv6(v6),
    }
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.octets()[0] == 169 && ip.octets()[1] == 254
}

fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.segments()[0] == 0xfe80
        || ip.segments()[0] == 0xfc00
        || ip.segments()[0] == 0xfd00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_localhost_and_private_ips() {
        assert!(matches!(
            validate_fetch_url("http://localhost/docs"),
            Err(UrlGuardError::BlockedHost(_))
        ));
        assert!(matches!(
            validate_fetch_url("https://127.0.0.1/health"),
            Err(UrlGuardError::BlockedHost(_))
        ));
    }

    #[test]
    fn allows_www_redirect() {
        let original = Url::parse("https://example.com/a").expect("url");
        let redirect = Url::parse("https://www.example.com/b").expect("url");
        assert!(is_permitted_redirect(&original, &redirect));
    }

    #[test]
    fn blocks_cross_host_redirect() {
        let original = Url::parse("https://example.com/a").expect("url");
        let redirect = Url::parse("https://evil.example.net/b").expect("url");
        assert!(!is_permitted_redirect(&original, &redirect));
    }

    #[test]
    fn upgrades_http_to_https() {
        let url = Url::parse("http://example.com/docs").expect("url");
        let upgraded = upgrade_http_to_https(&url);
        assert_eq!(upgraded.scheme(), "https");
    }
}
