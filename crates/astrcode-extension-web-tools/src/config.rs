use astrcode_extension_sdk::extension::ExtensionConfig;
use serde::Deserialize;

pub(crate) const EXTENSION_ID: &str = "astrcode-web-tools";

pub(crate) const WEB_SEARCH_TOOL_NAME: &str = "web-search";
pub(crate) const FETCH_URL_TOOL_NAME: &str = "fetch-url";

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WebToolsConfig {
    #[serde(default)]
    pub(crate) search: SearchConfig,
    #[serde(default)]
    pub(crate) fetch: FetchConfig,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SearchProvider {
    #[default]
    DuckDuckGo,
    Brave,
    Serper,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SearchConfig {
    #[serde(default)]
    pub(crate) provider: SearchProvider,
    pub(crate) brave_api_key: Option<String>,
    pub(crate) brave_api_key_env: Option<String>,
    pub(crate) serper_api_key: Option<String>,
    pub(crate) serper_api_key_env: Option<String>,
    #[serde(default = "default_max_results")]
    pub(crate) default_max_results: usize,
    #[serde(default = "default_search_timeout_secs")]
    pub(crate) request_timeout_secs: u64,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: SearchProvider::default(),
            brave_api_key: None,
            brave_api_key_env: None,
            serper_api_key: None,
            serper_api_key_env: None,
            default_max_results: default_max_results(),
            request_timeout_secs: default_search_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FetchConfig {
    #[serde(default = "default_fetch_timeout_secs")]
    pub(crate) request_timeout_secs: u64,
    #[serde(default = "default_max_content_bytes")]
    pub(crate) max_content_bytes: usize,
    #[serde(default = "default_max_output_chars")]
    pub(crate) max_output_chars: usize,
    #[serde(default = "default_user_agent")]
    pub(crate) user_agent: String,
    #[serde(default = "default_cache_ttl_secs")]
    pub(crate) cache_ttl_secs: u64,
    #[serde(default = "default_cache_max_entries")]
    pub(crate) cache_max_entries: usize,
    #[serde(default = "default_cache_max_bytes")]
    pub(crate) cache_max_bytes: usize,
    #[serde(default = "default_max_redirects")]
    pub(crate) max_redirects: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: default_fetch_timeout_secs(),
            max_content_bytes: default_max_content_bytes(),
            max_output_chars: default_max_output_chars(),
            user_agent: default_user_agent(),
            cache_ttl_secs: default_cache_ttl_secs(),
            cache_max_entries: default_cache_max_entries(),
            cache_max_bytes: default_cache_max_bytes(),
            max_redirects: default_max_redirects(),
        }
    }
}

const fn default_max_results() -> usize {
    5
}

const fn default_search_timeout_secs() -> u64 {
    30
}

const fn default_fetch_timeout_secs() -> u64 {
    60
}

const fn default_max_content_bytes() -> usize {
    10 * 1024 * 1024
}

const fn default_max_output_chars() -> usize {
    100_000
}

fn default_user_agent() -> String {
    "AstrCode/1.0 (+https://github.com/astrcode/astrcode)".into()
}

const fn default_cache_ttl_secs() -> u64 {
    15 * 60
}

const fn default_cache_max_entries() -> usize {
    64
}

const fn default_cache_max_bytes() -> usize {
    50 * 1024 * 1024
}

const fn default_max_redirects() -> usize {
    10
}

pub(crate) fn load_config(config: &ExtensionConfig) -> WebToolsConfig {
    config.deserialize().unwrap_or_default()
}

pub(crate) fn resolve_api_key(inline: Option<&str>, env_name: Option<&str>) -> Option<String> {
    inline
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            env_name.and_then(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_api_key_prefers_inline_value() {
        assert_eq!(
            resolve_api_key(Some("inline-key"), Some("MISSING_ENV")),
            Some("inline-key".into())
        );
    }

    #[test]
    fn empty_config_uses_defaults() {
        let config = load_config(&ExtensionConfig::default());
        assert_eq!(config.search.provider, SearchProvider::DuckDuckGo);
        assert_eq!(config.search.default_max_results, 5);
        assert_eq!(config.fetch.max_output_chars, 100_000);
    }
}
