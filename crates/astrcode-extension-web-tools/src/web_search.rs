use std::time::Instant;

use reqwest::Client;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::{
    config::{SearchConfig, SearchProvider, resolve_api_key},
    http::build_client,
};

const MAX_RESULTS_LIMIT: usize = 20;
const MIN_QUERY_LEN: usize = 2;
const BRAVE_SEARCH_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const SERPER_SEARCH_URL: &str = "https://google.serper.dev/search";
const DUCKDUCKGO_HTML_URL: &str = "https://html.duckduckgo.com/html/";
const HTML_SEARCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                                      AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 \
                                      Safari/537.36";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchHit {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) snippet: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum SearchError {
    #[error("search query must be at least {MIN_QUERY_LEN} characters")]
    QueryTooShort,
    #[error("cannot specify both allowedDomains and blockedDomains")]
    ConflictingDomainFilters,
    #[error("search provider `{0}` requires an API key in extension config")]
    MissingApiKey(&'static str),
    #[error("HTTP request failed: {0}")]
    Http(String),
    #[error("failed to parse search response: {0}")]
    Parse(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WebSearchArgs {
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) max_results: Option<usize>,
    #[serde(default)]
    pub(crate) allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) blocked_domains: Option<Vec<String>>,
}

pub(crate) struct WebSearchOutcome {
    pub(crate) query: String,
    pub(crate) hits: Vec<SearchHit>,
    pub(crate) duration_ms: u64,
}

pub(crate) async fn run_web_search(
    config: &SearchConfig,
    args: WebSearchArgs,
) -> Result<WebSearchOutcome, SearchError> {
    let started = Instant::now();
    validate_search_args(&args)?;

    let query = args.query.trim().to_string();
    let max_results = args
        .max_results
        .unwrap_or(config.default_max_results)
        .clamp(1, MAX_RESULTS_LIMIT);
    let client = build_client(config.request_timeout_secs, HTML_SEARCH_USER_AGENT)
        .map_err(|error| SearchError::Http(error.to_string()))?;

    let mut hits = match effective_provider(config) {
        SearchProvider::Brave => search_brave(&client, config, &query, max_results).await?,
        SearchProvider::Serper => search_serper(&client, config, &query, max_results).await?,
        SearchProvider::DuckDuckGo => search_duckduckgo(&client, &query, max_results).await?,
    };
    apply_domain_filters(&mut hits, &args);

    Ok(WebSearchOutcome {
        query,
        hits,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

fn validate_search_args(args: &WebSearchArgs) -> Result<(), SearchError> {
    if args.query.trim().len() < MIN_QUERY_LEN {
        return Err(SearchError::QueryTooShort);
    }
    let allowed = args
        .allowed_domains
        .as_ref()
        .is_some_and(|domains| !domains.is_empty());
    let blocked = args
        .blocked_domains
        .as_ref()
        .is_some_and(|domains| !domains.is_empty());
    if allowed && blocked {
        return Err(SearchError::ConflictingDomainFilters);
    }
    Ok(())
}

fn effective_provider(config: &SearchConfig) -> SearchProvider {
    match config.provider {
        SearchProvider::Brave
            if resolve_api_key(
                config.brave_api_key.as_deref(),
                config.brave_api_key_env.as_deref(),
            )
            .is_some() =>
        {
            SearchProvider::Brave
        },
        SearchProvider::Serper
            if resolve_api_key(
                config.serper_api_key.as_deref(),
                config.serper_api_key_env.as_deref(),
            )
            .is_some() =>
        {
            SearchProvider::Serper
        },
        SearchProvider::Brave | SearchProvider::Serper => SearchProvider::DuckDuckGo,
        SearchProvider::DuckDuckGo => SearchProvider::DuckDuckGo,
    }
}

async fn search_brave(
    client: &Client,
    config: &SearchConfig,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchHit>, SearchError> {
    let api_key = resolve_api_key(
        config.brave_api_key.as_deref(),
        config.brave_api_key_env.as_deref(),
    )
    .ok_or(SearchError::MissingApiKey("brave"))?;

    let response = client
        .get(BRAVE_SEARCH_URL)
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query), ("count", &max_results.to_string())])
        .send()
        .await
        .map_err(|error| SearchError::Http(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(SearchError::Http(format!(
            "Brave search returned HTTP {status}: {body}"
        )));
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|error| SearchError::Parse(error.to_string()))?;
    Ok(parse_provider_hits(
        payload.pointer("/web/results"),
        "title",
        "url",
        "description",
        max_results,
    ))
}

async fn search_serper(
    client: &Client,
    config: &SearchConfig,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchHit>, SearchError> {
    let api_key = resolve_api_key(
        config.serper_api_key.as_deref(),
        config.serper_api_key_env.as_deref(),
    )
    .ok_or(SearchError::MissingApiKey("serper"))?;

    let response = client
        .post(SERPER_SEARCH_URL)
        .header("X-API-KEY", api_key)
        .json(&json!({ "q": query, "num": max_results }))
        .send()
        .await
        .map_err(|error| SearchError::Http(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(SearchError::Http(format!(
            "Serper search returned HTTP {status}: {body}"
        )));
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|error| SearchError::Parse(error.to_string()))?;
    Ok(parse_provider_hits(
        payload.pointer("/organic"),
        "title",
        "link",
        "snippet",
        max_results,
    ))
}

fn parse_provider_hits(
    items: Option<&serde_json::Value>,
    title_key: &str,
    url_key: &str,
    snippet_key: &str,
    max_results: usize,
) -> Vec<SearchHit> {
    items
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(SearchHit {
                        title: item.get(title_key)?.as_str()?.trim().to_string(),
                        url: item.get(url_key)?.as_str()?.trim().to_string(),
                        snippet: item
                            .get(snippet_key)
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                    })
                })
                .filter(|hit| !hit.url.is_empty())
                .take(max_results)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

async fn search_duckduckgo(
    client: &Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchHit>, SearchError> {
    let response = client
        .post(DUCKDUCKGO_HTML_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("q={}", urlencoding(query)))
        .send()
        .await
        .map_err(|error| SearchError::Http(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(SearchError::Http(format!(
            "DuckDuckGo search returned HTTP {status}: {body}"
        )));
    }

    let html = response
        .text()
        .await
        .map_err(|error| SearchError::Http(error.to_string()))?;
    Ok(parse_duckduckgo_html(&html, max_results))
}

fn parse_duckduckgo_html(html: &str, max_results: usize) -> Vec<SearchHit> {
    let document = Html::parse_document(html);
    let result_selector = Selector::parse("div.result").ok();
    let title_selector = Selector::parse("a.result__a").ok();
    let snippet_selector = Selector::parse("a.result__snippet, div.result__snippet").ok();
    let (Some(result_selector), Some(title_selector)) = (result_selector, title_selector) else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for result in document.select(&result_selector).take(max_results) {
        let Some(title_link) = result.select(&title_selector).next() else {
            continue;
        };
        let title = title_link.text().collect::<String>().trim().to_string();
        let url = title_link
            .value()
            .attr("href")
            .unwrap_or_default()
            .trim()
            .to_string();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let snippet = snippet_selector
            .as_ref()
            .and_then(|selector| result.select(selector).next())
            .map(|node| node.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        hits.push(SearchHit {
            title,
            url,
            snippet,
        });
    }
    hits
}

fn apply_domain_filters(hits: &mut Vec<SearchHit>, args: &WebSearchArgs) {
    if let Some(allowed) = args
        .allowed_domains
        .as_ref()
        .filter(|domains| !domains.is_empty())
    {
        hits.retain(|hit| domain_matches_any(&hit.url, allowed));
        return;
    }
    if let Some(blocked) = args
        .blocked_domains
        .as_ref()
        .filter(|domains| !domains.is_empty())
    {
        hits.retain(|hit| !domain_matches_any(&hit.url, blocked));
    }
}

fn domain_matches_any(url: &str, domains: &[String]) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    domains.iter().any(|domain| {
        let domain = domain.trim().to_ascii_lowercase();
        host == domain || host.ends_with(&format!(".{domain}"))
    })
}

fn urlencoding(input: &str) -> String {
    input
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            },
            b' ' => "+".into(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

pub(crate) fn render_search_results(outcome: &WebSearchOutcome) -> String {
    if outcome.hits.is_empty() {
        return format!(
            "No web results found for query `{}` ({}ms).",
            outcome.query, outcome.duration_ms
        );
    }

    let mut rendered = format!(
        "Web search results for query: \"{}\" ({}ms)\n\n",
        outcome.query, outcome.duration_ms
    );
    for hit in &outcome.hits {
        rendered.push_str(&format!("- [{}]({})\n", hit.title, hit.url));
        if !hit.snippet.is_empty() {
            rendered.push_str(&format!("  {snippet}\n", snippet = hit.snippet));
        }
    }
    rendered.push_str(
        "\nREMINDER: Include relevant sources in your final response using markdown hyperlinks.",
    );
    rendered
}

pub(crate) fn current_month_year() -> String {
    chrono::Local::now().format("%B %Y").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SearchConfig, SearchProvider};

    #[test]
    fn effective_provider_falls_back_to_duckduckgo_without_api_key() {
        let config = SearchConfig {
            provider: SearchProvider::Brave,
            brave_api_key: None,
            brave_api_key_env: None,
            ..SearchConfig::default()
        };
        assert_eq!(effective_provider(&config), SearchProvider::DuckDuckGo);
    }

    #[test]
    fn parse_duckduckgo_html_extracts_results() {
        let html = r#"
        <div class="result">
          <a class="result__a" href="https://example.com/a">Example A</a>
          <a class="result__snippet">First snippet</a>
        </div>
        "#;
        let hits = parse_duckduckgo_html(html, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Example A");
    }

    #[test]
    fn allowed_domains_filter_results() {
        let mut hits = vec![
            SearchHit {
                title: "A".into(),
                url: "https://docs.python.org/3/".into(),
                snippet: String::new(),
            },
            SearchHit {
                title: "B".into(),
                url: "https://example.com".into(),
                snippet: String::new(),
            },
        ];
        let args = WebSearchArgs {
            query: "python".into(),
            max_results: None,
            allowed_domains: Some(vec!["docs.python.org".into()]),
            blocked_domains: None,
        };
        apply_domain_filters(&mut hits, &args);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].url.contains("python.org"));
    }

    #[test]
    fn rejects_conflicting_domain_filters() {
        let args = WebSearchArgs {
            query: "rust".into(),
            max_results: None,
            allowed_domains: Some(vec!["doc.rust-lang.org".into()]),
            blocked_domains: Some(vec!["example.com".into()]),
        };
        assert_eq!(
            validate_search_args(&args),
            Err(SearchError::ConflictingDomainFilters)
        );
    }
}
