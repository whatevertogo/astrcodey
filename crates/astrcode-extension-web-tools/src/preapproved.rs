//! Documentation hosts that may return raw markdown without secondary-model processing.

use std::collections::HashMap;

const PREAPPROVED_HOSTS: &[&str] = &[
    "developer.mozilla.org",
    "docs.python.org",
    "doc.rust-lang.org",
    "go.dev",
    "pkg.go.dev",
    "react.dev",
    "nextjs.org",
    "nodejs.org",
    "www.typescriptlang.org",
    "learn.microsoft.com",
    "docs.djangoproject.com",
    "fastapi.tiangolo.com",
    "kubernetes.io",
    "git-scm.com",
    "graphql.org",
    "modelcontextprotocol.io",
    "github.com/anthropics",
];

struct PreapprovedIndex {
    hostname_only: std::collections::HashSet<&'static str>,
    path_prefixes: HashMap<&'static str, Vec<String>>,
}

fn build_index() -> PreapprovedIndex {
    let mut hostname_only = std::collections::HashSet::new();
    let mut path_prefixes: HashMap<&'static str, Vec<String>> = HashMap::new();
    for entry in PREAPPROVED_HOSTS {
        if let Some((host, path)) = entry.split_once('/') {
            path_prefixes
                .entry(host)
                .or_default()
                .push(format!("/{path}"));
        } else {
            hostname_only.insert(*entry);
        }
    }
    PreapprovedIndex {
        hostname_only,
        path_prefixes,
    }
}

static INDEX: std::sync::OnceLock<PreapprovedIndex> = std::sync::OnceLock::new();

fn index() -> &'static PreapprovedIndex {
    INDEX.get_or_init(build_index)
}

pub(crate) fn is_preapproved_url(url: &url::Url) -> bool {
    let Some(hostname) = url.host_str() else {
        return false;
    };
    let pathname = url.path();
    if index().hostname_only.contains(hostname) {
        return true;
    }
    index().path_prefixes.get(hostname).is_some_and(|prefixes| {
        prefixes.iter().any(|prefix| {
            pathname == prefix.as_str() || pathname.starts_with(&format!("{prefix}/"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_hostname_only_entries() {
        let url = url::Url::parse("https://react.dev/learn").expect("url");
        assert!(is_preapproved_url(&url));
    }

    #[test]
    fn matches_path_scoped_entries() {
        let url = url::Url::parse("https://github.com/anthropics/claude-code").expect("url");
        assert!(is_preapproved_url(&url));
        let other = url::Url::parse("https://github.com/other/repo").expect("url");
        assert!(!is_preapproved_url(&other));
    }
}
