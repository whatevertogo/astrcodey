const REQUIRED_SUMMARY_SECTIONS: [&str; 9] = [
    "1. Primary Request and Intent:",
    "2. Key Technical Concepts:",
    "3. Files and Code Sections:",
    "4. Errors and fixes:",
    "5. Problem Solving:",
    "6. All user messages:",
    "7. Pending Tasks:",
    "8. Current Work:",
    "9. Optional Next Step:",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCompactOutput {
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactParseError {
    detail: String,
}

impl CompactParseError {
    pub(crate) fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for CompactParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for CompactParseError {}

pub fn parse_compact_output(content: &str) -> Result<ParsedCompactOutput, CompactParseError> {
    let normalized = strip_outer_markdown_code_fence(content);
    let summary = extract_xml_block(&normalized, "summary")?
        .ok_or_else(|| CompactParseError::new("compact response missing <summary> block"))?;

    if summary.trim().is_empty() {
        return Err(CompactParseError::new("compact summary response was empty"));
    }
    if let Some(missing_section) = REQUIRED_SUMMARY_SECTIONS
        .iter()
        .find(|section| !summary.contains(**section))
    {
        return Err(CompactParseError::new(format!(
            "compact summary missing required section title: {missing_section}"
        )));
    }

    Ok(ParsedCompactOutput { summary })
}

pub(crate) fn compact_contract_violation(_parsed: &ParsedCompactOutput) -> Option<String> {
    None
}

pub(crate) fn extract_summary_for_context(content: &str) -> String {
    let normalized = strip_outer_markdown_code_fence(content);
    if let Ok(Some(summary)) = extract_xml_block(&normalized, "summary") {
        return summary;
    }
    parse_compact_output(content)
        .map(|parsed| parsed.summary)
        .unwrap_or_else(|_| {
            let without_analysis =
                strip_xml_block(&normalized, "analysis").unwrap_or_else(|_| normalized.clone());
            clean_compact_fallback_text(&without_analysis)
        })
}

fn extract_xml_block(content: &str, tag: &str) -> Result<Option<String>, CompactParseError> {
    let Some((open_start, open_end)) = find_opening_tag(content, tag) else {
        return Ok(None);
    };
    let Some((close_start, close_end)) = find_closing_tag(&content[open_end..], tag) else {
        return Err(CompactParseError::new(format!(
            "compact response missing closing </{tag}> tag"
        )));
    };
    let close_start = open_end + close_start;
    let _close_end = open_end + close_end;
    if close_start < open_start {
        return Ok(None);
    }
    Ok(Some(content[open_end..close_start].trim().to_string()))
}

fn strip_xml_block(content: &str, tag: &str) -> Result<String, CompactParseError> {
    let Some((open_start, open_end)) = find_opening_tag(content, tag) else {
        return Ok(content.trim().to_string());
    };
    let Some((_close_start, close_end)) = find_closing_tag(&content[open_end..], tag) else {
        return Err(CompactParseError::new(format!(
            "compact response missing closing </{tag}> tag"
        )));
    };
    let close_end = open_end + close_end;
    let mut stripped = String::with_capacity(content.len().saturating_sub(close_end - open_start));
    stripped.push_str(&content[..open_start]);
    stripped.push_str(&content[close_end..]);
    Ok(stripped.trim().to_string())
}

fn find_opening_tag(content: &str, tag: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = format!("<{tag}");
    let mut start = 0;
    while let Some(relative) = lower[start..].find(&needle) {
        let tag_start = start + relative;
        let after = lower[tag_start + needle.len()..].chars().next();
        if after.is_some_and(|ch| ch != '>' && !ch.is_ascii_whitespace()) {
            start = tag_start + needle.len();
            continue;
        }
        let tag_end = lower[tag_start..].find('>')? + tag_start + 1;
        return Some((tag_start, tag_end));
    }
    None
}

fn find_closing_tag(content: &str, tag: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = format!("</{tag}");
    let start = lower.find(&needle)?;
    let after = lower[start + needle.len()..].chars().next();
    if after.is_some_and(|ch| ch != '>' && !ch.is_ascii_whitespace()) {
        return None;
    }
    let end = lower[start..].find('>')? + start + 1;
    Some((start, end))
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
    if is_summary_preamble_line(first_line.trim()) {
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
