use super::*;

pub(super) fn parse_compact_output(content: &str) -> Result<ParsedCompactOutput> {
    let normalized = strip_outer_markdown_code_fence(content);
    let has_analysis = extract_xml_block(&normalized, "analysis").is_some();
    let recent_user_context_digest = extract_xml_block(&normalized, "recent_user_context_digest");
    let has_recent_user_context_digest_block = recent_user_context_digest.is_some();
    if !has_analysis {
        log::warn!("compact: missing <analysis> block in LLM response");
    }

    if has_opening_xml_tag(&normalized, "summary") && !has_closing_xml_tag(&normalized, "summary") {
        return Err(AstrError::LlmStreamError(
            "compact response missing closing </summary> tag".to_string(),
        ));
    }
    if has_opening_xml_tag(&normalized, "recent_user_context_digest")
        && !has_closing_xml_tag(&normalized, "recent_user_context_digest")
    {
        return Err(AstrError::LlmStreamError(
            "compact response missing closing </recent_user_context_digest> tag".to_string(),
        ));
    }

    let mut used_fallback = false;
    let summary = if let Some(summary) = extract_xml_block(&normalized, "summary") {
        summary.to_string()
    } else if let Some(structured) = extract_structured_summary_fallback(&normalized) {
        used_fallback = true;
        structured
    } else {
        let fallback = strip_xml_block(&normalized, "analysis");
        let fallback = clean_compact_fallback_text(&fallback);
        if fallback.is_empty() {
            return Err(AstrError::LlmStreamError(
                "compact response missing <summary> block".to_string(),
            ));
        }
        log::warn!("compact: missing <summary> block, falling back to raw content");
        used_fallback = true;
        fallback
    };
    if summary.is_empty() {
        return Err(AstrError::LlmStreamError(
            "compact summary response was empty".to_string(),
        ));
    }

    Ok(ParsedCompactOutput {
        summary,
        recent_user_context_digest: recent_user_context_digest.map(str::to_string),
        has_analysis,
        has_recent_user_context_digest_block,
        used_fallback,
    })
}

fn extract_structured_summary_fallback(content: &str) -> Option<String> {
    let cleaned = clean_compact_fallback_text(content);
    let lower = cleaned.to_ascii_lowercase();
    let candidates = ["## summary", "# summary", "summary:"];
    for marker in candidates {
        if let Some(start) = lower.find(marker) {
            let body = cleaned[start + marker.len()..].trim();
            if !body.is_empty() {
                return Some(body.to_string());
            }
        }
    }
    None
}

fn extract_xml_block<'a>(content: &'a str, tag: &str) -> Option<&'a str> {
    xml_block_regex(tag)
        .captures(content)
        .and_then(|captures| captures.name("body"))
        .map(|body| body.as_str().trim())
}

fn strip_xml_block(content: &str, tag: &str) -> String {
    xml_block_regex(tag).replace(content, "").into_owned()
}

fn has_opening_xml_tag(content: &str, tag: &str) -> bool {
    xml_opening_tag_regex(tag).is_match(content)
}

fn has_closing_xml_tag(content: &str, tag: &str) -> bool {
    xml_closing_tag_regex(tag).is_match(content)
}

fn strip_markdown_code_fence(content: &str) -> String {
    let trimmed = content.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }

    let mut lines = trimmed.lines();
    let Some(first_line) = lines.next() else {
        return trimmed.to_string();
    };
    if !first_line.trim_start().starts_with("```") {
        return trimmed.to_string();
    }

    let body = lines.collect::<Vec<_>>().join("\n");
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim().to_string()
}

fn strip_outer_markdown_code_fence(content: &str) -> String {
    let mut current = content.trim().to_string();
    loop {
        let stripped = strip_markdown_code_fence(&current);
        if stripped == current {
            return current;
        }
        current = stripped;
    }
}

fn clean_compact_fallback_text(content: &str) -> String {
    let without_code_fence = strip_outer_markdown_code_fence(content);
    let lines = without_code_fence
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>();
    let first_meaningful = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(lines.len());
    let cleaned = lines
        .into_iter()
        .skip(first_meaningful)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    strip_leading_summary_preamble(&cleaned)
}

fn strip_leading_summary_preamble(content: &str) -> String {
    let mut lines = content.lines();
    let Some(first_line) = lines.next() else {
        return String::new();
    };
    let trimmed_first_line = first_line.trim();
    if is_summary_preamble_line(trimmed_first_line) {
        return lines.collect::<Vec<_>>().join("\n").trim().to_string();
    }
    content.trim().to_string()
}

fn is_summary_preamble_line(line: &str) -> bool {
    let normalized = line
        .trim_matches(|ch: char| matches!(ch, '*' | '#' | '-' | ':' | ' '))
        .trim();
    normalized.eq_ignore_ascii_case("summary")
        || normalized.eq_ignore_ascii_case("here is the summary")
        || normalized.eq_ignore_ascii_case("compact summary")
        || normalized.eq_ignore_ascii_case("here's the summary")
}

fn xml_block_regex(tag: &str) -> &'static Regex {
    static SUMMARY_REGEX: OnceLock<Regex> = OnceLock::new();
    static ANALYSIS_REGEX: OnceLock<Regex> = OnceLock::new();
    static RECENT_USER_CONTEXT_DIGEST_REGEX: OnceLock<Regex> = OnceLock::new();

    match tag {
        "summary" => SUMMARY_REGEX.get_or_init(|| {
            Regex::new(r"(?is)<summary(?:\s+[^>]*)?\s*>(?P<body>.*?)</summary\s*>")
                .expect("summary regex should compile")
        }),
        "analysis" => ANALYSIS_REGEX.get_or_init(|| {
            Regex::new(r"(?is)<analysis(?:\s+[^>]*)?\s*>(?P<body>.*?)</analysis\s*>")
                .expect("analysis regex should compile")
        }),
        "recent_user_context_digest" => RECENT_USER_CONTEXT_DIGEST_REGEX.get_or_init(|| {
            Regex::new(
                r"(?is)<recent_user_context_digest(?:\s+[^>]*)?\s*>(?P<body>.*?)</recent_user_context_digest\s*>",
            )
            .expect("recent user context digest regex should compile")
        }),
        other => panic!("unsupported compact xml tag: {other}"),
    }
}

fn xml_opening_tag_regex(tag: &str) -> &'static Regex {
    static SUMMARY_REGEX: OnceLock<Regex> = OnceLock::new();
    static ANALYSIS_REGEX: OnceLock<Regex> = OnceLock::new();
    static RECENT_USER_CONTEXT_DIGEST_REGEX: OnceLock<Regex> = OnceLock::new();

    match tag {
        "summary" => SUMMARY_REGEX.get_or_init(|| {
            Regex::new(r"(?i)<summary(?:\s+[^>]*)?\s*>")
                .expect("summary opening regex should compile")
        }),
        "analysis" => ANALYSIS_REGEX.get_or_init(|| {
            Regex::new(r"(?i)<analysis(?:\s+[^>]*)?\s*>")
                .expect("analysis opening regex should compile")
        }),
        "recent_user_context_digest" => RECENT_USER_CONTEXT_DIGEST_REGEX.get_or_init(|| {
            Regex::new(r"(?i)<recent_user_context_digest(?:\s+[^>]*)?\s*>")
                .expect("recent user context digest opening regex should compile")
        }),
        other => panic!("unsupported compact xml tag: {other}"),
    }
}

fn xml_closing_tag_regex(tag: &str) -> &'static Regex {
    static SUMMARY_REGEX: OnceLock<Regex> = OnceLock::new();
    static ANALYSIS_REGEX: OnceLock<Regex> = OnceLock::new();
    static RECENT_USER_CONTEXT_DIGEST_REGEX: OnceLock<Regex> = OnceLock::new();

    match tag {
        "summary" => SUMMARY_REGEX.get_or_init(|| {
            Regex::new(r"(?i)</summary\s*>").expect("summary closing regex should compile")
        }),
        "analysis" => ANALYSIS_REGEX.get_or_init(|| {
            Regex::new(r"(?i)</analysis\s*>").expect("analysis closing regex should compile")
        }),
        "recent_user_context_digest" => RECENT_USER_CONTEXT_DIGEST_REGEX.get_or_init(|| {
            Regex::new(r"(?i)</recent_user_context_digest\s*>")
                .expect("recent user context digest closing regex should compile")
        }),
        other => panic!("unsupported compact xml tag: {other}"),
    }
}

pub(super) fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}
