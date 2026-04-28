//! Agent discovery + parsing — Claude Code compatible Markdown / YAML frontmatter.
//!
//! Supports both Claude CSV tools format (`tools: Read, Grep`) and
//! YAML list format (`tools: ["Read", "Grep"]`).
//! Scan directories: `~/.astrcode/agents/`, `.astrcode/agents/`,
//!                    `~/.claude/agents/`,   `.claude/agents/`

use std::path::PathBuf;

/// Parsed agent configuration (Claude-compatible).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    /// When this agent should be selected.
    pub description: String,
    /// Optional tool allowlist parsed from Claude-compatible frontmatter.
    pub tools: Vec<String>,
    /// Optional model preference parsed from Claude-compatible frontmatter.
    pub model: Option<String>,
    /// System prompt body (markdown body or systemPrompt/prompt frontmatter).
    pub body: String,
}

// ─── Built-in agents ─────────────────────────────────────────────────────

struct BuiltinAgent {
    path: &'static str,
    content: &'static str,
}

pub fn builtin_agents() -> Vec<AgentConfig> {
    let builtins: &[BuiltinAgent] = &[
        BuiltinAgent {
            path: "builtin://explore.md",
            content: include_str!("builtin_agents/explore.md"),
        },
        BuiltinAgent {
            path: "builtin://reviewer.md",
            content: include_str!("builtin_agents/reviewer.md"),
        },
        BuiltinAgent {
            path: "builtin://execute.md",
            content: include_str!("builtin_agents/execute.md"),
        },
    ];
    builtins
        .iter()
        .filter_map(|b| parse(b.path, b.content).ok())
        .collect()
}

// ─── Discovery ───────────────────────────────────────────────────────────

/// Discover agents from all sources. Priority (low to high):
/// 1. Built-in agents
/// 2. User: `~/.claude/agents/` + `~/.astrcode/agents/`
/// 3. Project: `.claude/agents/` + `.astrcode/agents/`
pub fn discover_agents(working_dir: Option<&str>) -> Vec<AgentConfig> {
    let mut agents = builtin_agents();

    if let Some(home) = home_dir() {
        for d in &[
            home.join(".claude").join("agents"),
            home.join(".astrcode").join("agents"),
        ] {
            merge_dir(&mut agents, d, false);
        }
    }

    if let Some(wd) = working_dir {
        let wd = PathBuf::from(wd);
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut cur = Some(wd.as_path());
        while let Some(d) = cur {
            ancestors.push(d.to_path_buf());
            cur = d.parent();
        }
        ancestors.reverse();
        for a in &ancestors {
            for d in &[
                a.join(".claude").join("agents"),
                a.join(".astrcode").join("agents"),
            ] {
                merge_dir(&mut agents, d, true);
            }
        }
    }

    agents
}

fn merge_dir(agents: &mut Vec<AgentConfig>, dir: &std::path::Path, override_existing: bool) {
    if !dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_agent_file(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(agent) = parse(&path.to_string_lossy(), &content) else {
            continue;
        };
        if override_existing {
            agents.retain(|a| a.id != agent.id);
        }
        agents.push(agent);
    }
}

fn is_agent_file(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "yml" | "yaml")
    )
}

// ─── Parsing ─────────────────────────────────────────────────────────────

fn parse(path: &str, content: &str) -> Result<AgentConfig, String> {
    let text = content.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_start_matches('\u{feff}');

    if path.ends_with(".md") || path.ends_with(".markdown") {
        let (fm, body) =
            split_frontmatter(text).map_err(|_| format!("{path}: missing YAML frontmatter"))?;
        build(path, &fm, Some(&body))
    } else {
        build(path, text, None)
    }
}

fn build(path: &str, yaml_text: &str, markdown_body: Option<&str>) -> Result<AgentConfig, String> {
    let root: serde_yaml::Value =
        serde_yaml::from_str(yaml_text).map_err(|e| format!("{path}: parse YAML: {e}"))?;
    let m = root
        .as_mapping()
        .ok_or_else(|| format!("{path}: expected YAML mapping"))?;

    let fallback = PathBuf::from(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agent")
        .to_string();
    let name = str_val(m, "name").unwrap_or(fallback);
    let id = normalize_id(&name);

    let description =
        str_val(m, "description").ok_or_else(|| format!("{path}: description is required"))?;

    let tools = parse_tools(m);
    let model = str_val(m, "model").filter(|s| s != "inherit" && !s.is_empty());

    let body = markdown_body
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .or_else(|| str_val(m, "systemPrompt"))
        .or_else(|| str_val(m, "prompt"))
        .unwrap_or_default();

    Ok(AgentConfig {
        id,
        name,
        description,
        tools,
        model,
        body,
    })
}

fn parse_tools(m: &serde_yaml::Mapping) -> Vec<String> {
    let key = serde_yaml::Value::String("tools".into());
    let Some(v) = m.get(&key) else {
        return Vec::new();
    };
    match v {
        serde_yaml::Value::String(s) => s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn str_val(m: &serde_yaml::Mapping, key: &str) -> Option<String> {
    let v = m.get(serde_yaml::Value::String(key.into()))?;
    v.as_str().map(String::from)
}

// ─── Frontmatter splitting ───────────────────────────────────────────────

fn split_frontmatter(content: &str) -> Result<(String, String), ()> {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return Err(());
    }
    let rest: Vec<&str> = lines.collect();
    for (i, line) in rest.iter().enumerate() {
        if (*line == "---" || *line == "...")
            && rest
                .get(i + 1)
                .is_none_or(|next| !next.starts_with(' ') && !next.starts_with('\t'))
        {
            let fm = rest[..i].join("\n");
            let body = rest[i + 1..].join("\n");
            return Ok((fm, body));
        }
    }
    Err(())
}

fn normalize_id(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            out.push('-');
            last_sep = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_agents_load() {
        let agents = builtin_agents();
        assert!(agents.iter().any(|a| a.id == "explore"));
        assert!(agents.iter().any(|a| a.id == "reviewer"));
        assert!(agents.iter().any(|a| a.id == "execute"));
    }

    #[test]
    fn parse_csv_tools_format() {
        let md = r#"---
name: test-agent
description: A test agent
tools: Read, Grep, Bash
---
Body text."#;
        let agent = parse("test.md", md).unwrap();
        assert_eq!(agent.tools, vec!["Read", "Grep", "Bash"]);
    }

    #[test]
    fn parse_list_tools_format() {
        let md = r#"---
name: test-agent
description: A test agent
tools: ["Read", "Grep", "Bash"]
---
Body text."#;
        let agent = parse("test.md", md).unwrap();
        assert_eq!(agent.tools, vec!["Read", "Grep", "Bash"]);
    }


    #[test]
    fn system_prompt_from_body() {
        let md = r#"---
name: test-agent
description: A test agent
---
This is the system prompt."#;
        let agent = parse("test.md", md).unwrap();
        assert_eq!(agent.body, "This is the system prompt.");
    }

    #[test]
    fn normalizes_agent_id() {
        assert_eq!(normalize_id("Code Reviewer"), "code-reviewer");
        assert_eq!(normalize_id("my_agent!"), "my-agent");
    }
}
