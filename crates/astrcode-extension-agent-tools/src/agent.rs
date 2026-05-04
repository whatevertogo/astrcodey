//! Agent 发现与解析 — 兼容 Claude Code 的 Markdown / YAML frontmatter 格式。
//!
//! 支持两种工具列表格式：
//! - CSV 工具格式 (`tools: read, grep`)
//! - YAML 列表格式 (`tools: ["read", "grep"]`)
//!
//! 扫描目录顺序：
//! - `~/.astrcode/agents/`、`.astrcode/agents/`
//! - `~/.claude/agents/`、`.claude/agents/`

use std::path::PathBuf;

use astrcode_support::{frontmatter, hostpaths};

/// 解析后的 Agent 配置（兼容 Claude 格式）。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Agent 唯一标识（由名称标准化生成）
    pub id: String,
    /// Agent 显示名称
    pub name: String,
    /// 描述何时应选择此 Agent
    pub description: String,
    /// 可选的工具白名单，从兼容 Claude 的 frontmatter 中解析
    pub tools: Vec<String>,
    /// 可选的模型偏好，从兼容 Claude 的 frontmatter 中解析
    pub model: Option<String>,
    /// 系统提示词正文（markdown 正文或 systemPrompt/prompt frontmatter 字段）
    pub body: String,
}

// ─── 内置 Agent ─────────────────────────────────────────────────────

/// 内置 Agent 定义
struct BuiltinAgent {
    /// 内置路径标识
    path: &'static str,
    /// Agent markdown 内容
    content: &'static str,
}

/// 返回所有内置 Agent 配置。
///
/// 内置 Agent 包括 explore（探索）、reviewer（审查）和 execute（执行）。
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

// ─── 发现 ───────────────────────────────────────────────────────────

/// 从所有来源发现 Agent。优先级（从低到高）：
/// 1. 内置 Agent
/// 2. 用户级: `~/.claude/agents/` + `~/.astrcode/agents/`
/// 3. 项目级: `.claude/agents/` + `.astrcode/agents/`
pub fn discover_agents(working_dir: Option<&str>) -> Vec<AgentConfig> {
    let mut agents = builtin_agents();

    // 扫描用户主目录下的 Agent
    {
        let home = hostpaths::resolve_home_dir();
        for d in &[
            home.join(".claude").join("agents"),
            home.join(".astrcode").join("agents"),
        ] {
            merge_dir(&mut agents, d, false);
        }
    }

    // 扫描项目目录及其所有祖先目录下的 Agent（项目级可覆盖用户级）
    if let Some(wd) = working_dir {
        let wd = PathBuf::from(wd);
        // 收集从根到当前目录的所有祖先路径
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut cur = Some(wd.as_path());
        while let Some(d) = cur {
            ancestors.push(d.to_path_buf());
            cur = d.parent();
        }
        // 反转：从根目录开始扫描，确保更近的目录优先级更高
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

/// 将目录中的 Agent 合并到列表中。
///
/// # 参数
/// - `agents`: 现有 Agent 列表
/// - `dir`: 要扫描的目录
/// - `override_existing`: 如果为 true，同名 Agent 会覆盖已有条目
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
            // 移除同 ID 的旧 Agent，实现覆盖
            agents.retain(|a| a.id != agent.id);
        }
        agents.push(agent);
    }
}

/// 判断文件是否为 Agent 定义文件（支持 .md/.markdown/.yml/.yaml 扩展名）
fn is_agent_file(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "yml" | "yaml")
    )
}

// ─── 解析 ─────────────────────────────────────────────────────────────

/// 解析 Agent 配置文件。
///
/// Markdown 文件需要包含 YAML frontmatter；YAML 文件直接解析。
fn parse(path: &str, content: &str) -> Result<AgentConfig, String> {
    // 统一换行符并移除 BOM
    let text = content.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_start_matches('\u{feff}');

    if path.ends_with(".md") || path.ends_with(".markdown") {
        let (fm, body) = frontmatter::split_frontmatter(text)
            .ok_or_else(|| format!("{path}: missing YAML frontmatter"))?;
        build(path, fm, Some(body))
    } else {
        build(path, text, None)
    }
}

/// 从 YAML 文本和可选的 Markdown 正文构建 AgentConfig。
fn build(path: &str, yaml_text: &str, markdown_body: Option<&str>) -> Result<AgentConfig, String> {
    let root: serde_yaml::Value =
        serde_yaml::from_str(yaml_text).map_err(|e| format!("{path}: parse YAML: {e}"))?;
    let m = root
        .as_mapping()
        .ok_or_else(|| format!("{path}: expected YAML mapping"))?;

    // 使用文件名作为名称的回退值
    let fallback = PathBuf::from(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agent")
        .to_string();
    let name = mapping_str(m, "name").unwrap_or(fallback);
    let id = normalize_id(&name);

    let description =
        mapping_str(m, "description").ok_or_else(|| format!("{path}: description is required"))?;

    let tools =
        frontmatter::yaml_parse_tools_list(m.get(serde_yaml::Value::String("tools".into())));
    // "inherit" 和空字符串表示继承父级模型设置
    let model = mapping_str(m, "model").filter(|s| s != "inherit" && !s.is_empty());

    // 系统提示词优先级: markdown 正文 > systemPrompt 字段 > prompt 字段 > 空
    let body = markdown_body
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .or_else(|| mapping_str(m, "systemPrompt"))
        .or_else(|| mapping_str(m, "prompt"))
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

/// 从 YAML 映射中获取字符串值。
fn mapping_str(m: &serde_yaml::Mapping, key: &str) -> Option<String> {
    let v = m.get(serde_yaml::Value::String(key.into()))?;
    v.as_str().map(String::from)
}

/// 将 Agent 名称标准化为 ID 格式。
///
/// 将非字母数字字符替换为 `-`，并转换为小写。
/// 连续的非字母数字字符合并为一个 `-`。
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

// ─── 测试 ───────────────────────────────────────────────────────────

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
tools: read, grep, shell
---
Body text."#;
        let agent = parse("test.md", md).unwrap();
        assert_eq!(agent.tools, vec!["read", "grep", "shell"]);
    }

    #[test]
    fn parse_list_tools_format() {
        let md = r#"---
name: test-agent
description: A test agent
tools: ["read", "grep", "shell"]
---
Body text."#;
        let agent = parse("test.md", md).unwrap();
        assert_eq!(agent.tools, vec!["read", "grep", "shell"]);
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
