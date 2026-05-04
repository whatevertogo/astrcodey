#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpToolName {
    pub(crate) server: String,
    pub(crate) tool: String,
}

pub(crate) fn build_tool_name(server: &str, tool: &str) -> Option<String> {
    let server = normalize_component(server);
    let tool = normalize_component(tool);
    (!server.is_empty() && !tool.is_empty()).then(|| format!("mcp__{server}__{tool}"))
}

pub(crate) fn parse_tool_name(name: &str) -> Option<McpToolName> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then(|| McpToolName {
        server: server.to_string(),
        tool: tool.to_string(),
    })
}

pub(crate) fn normalized_name_matches(raw: &str, normalized: &str) -> bool {
    normalize_component(raw) == normalized
}

fn normalize_component(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut last_sep = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            output.push('_');
            last_sep = true;
        }
    }

    output.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_normalized_mcp_tool_names() {
        assert_eq!(
            build_tool_name("GitHub Server", "Create Issue!").as_deref(),
            Some("mcp__github_server__create_issue")
        );
    }

    #[test]
    fn rejects_empty_normalized_names() {
        assert_eq!(build_tool_name("!!!", "tool"), None);
        assert_eq!(build_tool_name("server", "???"), None);
    }

    #[test]
    fn parses_mcp_tool_names() {
        assert_eq!(
            parse_tool_name("mcp__github__create_issue"),
            Some(McpToolName {
                server: "github".into(),
                tool: "create_issue".into(),
            })
        );
        assert_eq!(parse_tool_name("read"), None);
    }

    #[test]
    fn matches_raw_names_by_normalized_shape() {
        assert!(normalized_name_matches("Create Issue!", "create_issue"));
        assert!(!normalized_name_matches(
            "Create Pull Request",
            "create_issue"
        ));
    }
}
