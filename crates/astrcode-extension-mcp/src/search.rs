use astrcode_core::tool::ToolDefinition;
use serde_json::{Value, json};

const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS_LIMIT: usize = 50;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ToolSearchArgs {
    pub(crate) query: String,
    #[serde(default = "default_max_results")]
    pub(crate) max_results: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchCandidate {
    pub(crate) definition: ToolDefinition,
    pub(crate) server: String,
    pub(crate) tool: String,
}

#[derive(Debug)]
pub(crate) struct ToolSearchOutput {
    pub(crate) query: String,
    pub(crate) total_mcp_tools: usize,
    pub(crate) matches: Vec<SearchCandidate>,
}

pub(crate) fn search_mcp_tools(
    candidates: &[SearchCandidate],
    args: ToolSearchArgs,
) -> ToolSearchOutput {
    let query = args.query.trim().to_string();
    let max_results = args.max_results.clamp(1, MAX_RESULTS_LIMIT);

    let matches = if let Some(selected) = query.strip_prefix("select:") {
        select_tools(candidates, selected, max_results)
    } else {
        keyword_search(candidates, &query, max_results)
    };

    ToolSearchOutput {
        query,
        total_mcp_tools: candidates.len(),
        matches,
    }
}

pub(crate) fn render_search_output(output: &ToolSearchOutput) -> String {
    if output.matches.is_empty() {
        return format!(
            "No matching MCP tools found for query '{}'. Total MCP tools searched: {}.",
            output.query, output.total_mcp_tools
        );
    }

    let mut rendered = format!(
        "Matched {} MCP tool(s) for query '{}' out of {} total.\n<functions>",
        output.matches.len(),
        output.query,
        output.total_mcp_tools
    );
    for candidate in &output.matches {
        rendered.push('\n');
        rendered.push_str("<function>");
        rendered.push_str(&function_json(candidate));
        rendered.push_str("</function>");
    }
    rendered.push_str("\n</functions>");
    rendered
}

pub(crate) fn output_metadata(output: &ToolSearchOutput) -> Value {
    json!({
        "query": output.query,
        "totalMcpTools": output.total_mcp_tools,
        "matches": output.matches.iter().map(candidate_json).collect::<Vec<_>>(),
    })
}

fn select_tools(
    candidates: &[SearchCandidate],
    selected: &str,
    max_results: usize,
) -> Vec<SearchCandidate> {
    let requested = selected
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    let mut matches = Vec::new();
    for name in requested {
        let Some(candidate) = find_by_name(candidates, name) else {
            continue;
        };
        if !matches
            .iter()
            .any(|existing: &SearchCandidate| existing.definition.name == candidate.definition.name)
        {
            matches.push(candidate.clone());
        }
        if matches.len() >= max_results {
            break;
        }
    }
    matches
}

fn keyword_search(
    candidates: &[SearchCandidate],
    query: &str,
    max_results: usize,
) -> Vec<SearchCandidate> {
    let query_lower = query.to_ascii_lowercase();
    if let Some(candidate) = find_by_name(candidates, &query_lower) {
        return vec![candidate.clone()];
    }
    if query_lower.starts_with("mcp__") && query_lower.len() > "mcp__".len() {
        let prefix_matches = candidates
            .iter()
            .filter(|candidate| {
                candidate
                    .definition
                    .name
                    .to_ascii_lowercase()
                    .starts_with(&query_lower)
            })
            .take(max_results)
            .cloned()
            .collect::<Vec<_>>();
        if !prefix_matches.is_empty() {
            return prefix_matches;
        }
    }

    let terms = query_lower
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    let (required_terms, optional_terms): (Vec<_>, Vec<_>) = terms
        .into_iter()
        .partition(|term| term.starts_with('+') && term.len() > 1);
    let required_terms = required_terms
        .into_iter()
        .map(|term| &term[1..])
        .collect::<Vec<_>>();
    let scoring_terms = if required_terms.is_empty() {
        optional_terms
    } else {
        required_terms
            .iter()
            .copied()
            .chain(optional_terms)
            .collect()
    };

    let mut scored = candidates
        .iter()
        .filter_map(|candidate| {
            let parsed = ParsedToolName::parse(&candidate.definition.name);
            let description = candidate.description_text();
            if !required_terms
                .iter()
                .all(|term| candidate_matches_term(&parsed, &description, term))
            {
                return None;
            }
            let score = scoring_terms
                .iter()
                .map(|term| score_candidate_term(&parsed, &description, term))
                .sum::<usize>();
            (score > 0).then_some((candidate, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(
        |(left_candidate, left_score), (right_candidate, right_score)| {
            right_score.cmp(left_score).then_with(|| {
                left_candidate
                    .definition
                    .name
                    .cmp(&right_candidate.definition.name)
            })
        },
    );
    scored
        .into_iter()
        .take(max_results)
        .map(|(candidate, _)| candidate.clone())
        .collect()
}

fn find_by_name<'a>(candidates: &'a [SearchCandidate], name: &str) -> Option<&'a SearchCandidate> {
    let name = name.to_ascii_lowercase();
    candidates
        .iter()
        .find(|candidate| candidate.definition.name.to_ascii_lowercase() == name)
}

fn candidate_matches_term(parsed: &ParsedToolName, description: &str, term: &str) -> bool {
    parsed.parts.iter().any(|part| part.contains(term))
        || parsed.full.contains(term)
        || description.contains(term)
}

fn score_candidate_term(parsed: &ParsedToolName, description: &str, term: &str) -> usize {
    let mut score = 0;
    if parsed.parts.iter().any(|part| part == term) {
        score += 12;
    } else if parsed.parts.iter().any(|part| part.contains(term)) {
        score += 6;
    }
    if parsed.full.contains(term) && score == 0 {
        score += 3;
    }
    if description_contains_word(description, term) {
        score += 2;
    }
    score
}

fn description_contains_word(description: &str, term: &str) -> bool {
    description
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|word| word == term)
}

fn function_json(candidate: &SearchCandidate) -> String {
    json!({
        "name": candidate.definition.name,
        "description": candidate.definition.description,
        "parameters": candidate.definition.parameters,
    })
    .to_string()
}

fn candidate_json(candidate: &SearchCandidate) -> Value {
    json!({
        "name": candidate.definition.name,
        "server": candidate.server,
        "tool": candidate.tool,
        "description": candidate.definition.description,
        "parameters": candidate.definition.parameters,
    })
}

fn default_max_results() -> usize {
    DEFAULT_MAX_RESULTS
}

impl SearchCandidate {
    fn description_text(&self) -> String {
        self.definition.description.to_ascii_lowercase()
    }
}

struct ParsedToolName {
    parts: Vec<String>,
    full: String,
}

impl ParsedToolName {
    fn parse(name: &str) -> Self {
        let body = name.strip_prefix("mcp__").unwrap_or(name);
        let full = body.replace("__", " ").replace('_', " ");
        let parts = full
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        Self { parts, full }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::tool::{ExecutionMode, ToolOrigin};

    use super::*;

    #[test]
    fn select_query_returns_exact_requested_tools() {
        let candidates = candidates();

        let output = search_mcp_tools(
            &candidates,
            ToolSearchArgs {
                query: "select:mcp__github__create_issue".into(),
                max_results: 5,
            },
        );

        assert_eq!(output.matches.len(), 1);
        assert_eq!(
            output.matches[0].definition.name,
            "mcp__github__create_issue"
        );
    }

    #[test]
    fn keyword_search_matches_server_action_and_description() {
        let candidates = candidates();

        let output = search_mcp_tools(
            &candidates,
            ToolSearchArgs {
                query: "+github issue".into(),
                max_results: 5,
            },
        );

        let names = output
            .matches
            .iter()
            .map(|candidate| candidate.definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["mcp__github__create_issue"]);
    }

    #[test]
    fn render_output_uses_functions_block() {
        let output = ToolSearchOutput {
            query: "github".into(),
            total_mcp_tools: 1,
            matches: vec![candidates().remove(0)],
        };

        let rendered = render_search_output(&output);

        assert!(rendered.contains("<functions>"));
        assert!(rendered.contains("<function>{\"description\""));
        assert!(rendered.contains("\"name\":\"mcp__github__create_issue\""));
    }

    fn candidates() -> Vec<SearchCandidate> {
        vec![
            candidate("mcp__github__create_issue", "GitHub", "Create Issue"),
            candidate("mcp__slack__send_message", "Slack", "Send Message"),
        ]
    }

    fn candidate(name: &str, server: &str, tool: &str) -> SearchCandidate {
        SearchCandidate {
            definition: ToolDefinition {
                name: name.into(),
                description: format!("MCP tool from server '{server}': {tool}"),
                parameters: json!({"type": "object"}),
                origin: ToolOrigin::Bundled,
                execution_mode: ExecutionMode::Sequential,
            },
            server: server.into(),
            tool: tool.into(),
        }
    }
}
