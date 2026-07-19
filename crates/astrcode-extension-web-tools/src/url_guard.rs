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

    parsed
        .host_str()
        .ok_or_else(|| UrlGuardError::Invalid("missing host".into()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_url_shape_without_duplicating_host_network_policy() {
        assert!(validate_fetch_url("http://localhost/docs").is_ok());
        assert!(matches!(
            validate_fetch_url("file:///tmp/page"),
            Err(UrlGuardError::UnsupportedScheme)
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
