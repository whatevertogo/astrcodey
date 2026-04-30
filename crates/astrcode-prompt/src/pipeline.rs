//! Pipeline：纯函数式 system prompt 组装。
//!
//! `build_system_prompt()` 接收结构化输入，组装固定顺序的 section 后直接
//! 返回完整字符串。扩展通过 `PromptBuild` 事件追加内容到固定 section。

use std::{
    fs,
    path::{Path, PathBuf},
};

use astrcode_core::prompt::{ExtensionPromptBlock, ExtensionSection, SystemPromptInput};
use astrcode_support::hostpaths::astrcode_dir;

// ─── 内置常量 ──────────────────────────────────────────────────────────

pub const DEFAULT_IDENTITY: &str = concat!(
    "你是 AstrCode，一位天才级工程师。代码是你的表达：正确、可维护。行动前先充分理解上下文，\
     再精准执行；",
    "追求优雅、完善的最佳实践，定位根因而不是修补表象。面对复杂任务时，主动编排 agent 与工具协作，",
    "协调资源并推动项目成功。",
    "你习惯于边思考边总结",
);

const MAX_IDENTITY_SIZE: usize = 4096;

const FEW_SHOT: &str = concat!(
    "示例：修改代码前，先检查相关文件并收集上下文。\n",
    "如果只知道文件名模式或 glob，用 `findFiles` 发现候选路径；需要在已知路径内搜索内容时，用带 \
     `pattern` 和 `path` 的 `grep`；",
    "需要目录检查或运行命令时，用 `shell`。\n\n",
    "User：修复这个仓库里的失败行为。\n",
    "Assistant：我会先阅读相关文件和调用点，定位根因后做最小正确修改，运行聚焦验证，\
     然后报告修改文件和验证缺口。"
);

const RESPONSE_STYLE: &str = concat!(
    "为用户写内容，不要写成控制台日志。清楚时，先给答案、动作或下一步。\n\n",
    "当任务需要工具、多步骤或明显等待时：\n",
    "- 第一次调用工具前，简短说明你要做什么。\n",
    "- 当你确认了重要事实、改变方向，或沉默一段时间后取得实质进展时，给出简短进度更新。\n",
    "- 使用完整句子和足够上下文，让用户即使中途回来也能接上。\n\n",
    "不要把猜测、线索或阶段性结果说成已确认结论。区分猜想、有证据支持的发现和最终结论。\n\n",
    "优先使用清晰自然的文字；只有在能提升可读性时才使用结构化列表。\n\n",
    "收尾实现类任务时，简短覆盖：\n",
    "- 改了什么，\n",
    "- 为什么这种形态是正确的，\n",
    "- 验证了什么，\n",
    "- 如果验证不完整，还有什么剩余风险或下一步。\n\n",
    "代码标识符、文件路径、命令和 API 名称保持原始拼写。"
);

// ─── Identity 加载 ─────────────────────────────────────────────────────

pub fn user_identity_md_path() -> PathBuf {
    astrcode_dir().join("IDENTITY.md")
}

pub fn load_identity_md(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let identity = if trimmed.len() > MAX_IDENTITY_SIZE {
        truncate_to_char_boundary(trimmed, MAX_IDENTITY_SIZE)
    } else {
        trimmed
    };
    Some(identity.to_string())
}

fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ─── 核心构建函数 ──────────────────────────────────────────────────────

/// 根据结构化输入构建完整的 system prompt 字符串。
///
/// 纯函数，无副作用。section 顺序固定，不可配置。
pub fn build_system_prompt(input: &SystemPromptInput) -> String {
    let mut sections: Vec<String> = Vec::new();

    // 1. Identity
    let identity = input
        .identity
        .as_deref()
        .unwrap_or(DEFAULT_IDENTITY);
    sections.push(format!("# Identity\n\n{}", identity.trim()));

    // 2. Environment
    sections.push(format!(
        "# Environment\n\n工作目录：{}\n操作系统：{}\nShell：{}\n日期：{}",
        input.working_dir, input.os, input.shell, input.date
    ));

    // 3. User Rules
    if let Some(rules) = &input.user_rules {
        sections.push(format!("# User Rules\n\n{}", rules.trim()));
    }

    // 4. Project Rules (AGENTS.md)
    if let Some(project_rules) = &input.project_rules {
        sections.push(format!("# Project Rules\n\n{}", project_rules.trim()));
    }

    // 5. Tools
    if !input.tools.is_empty() {
        let tools_text = input
            .tools
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("# Tools\n\n{}", tools_text));
    }

    // 6. Extension blocks in order: PlatformInstructions, Skills, Agents
    push_extension_section(
        &mut sections,
        "Platform Instructions",
        &input.extension_blocks,
        ExtensionSection::PlatformInstructions,
    );
    push_extension_section(
        &mut sections,
        "Skills",
        &input.extension_blocks,
        ExtensionSection::Skills,
    );
    push_extension_section(
        &mut sections,
        "Agents",
        &input.extension_blocks,
        ExtensionSection::Agents,
    );

    // 6. Few Shot
    sections.push(format!("# Few Shot\n\n{}", FEW_SHOT));

    // 7. Response Style
    sections.push(format!("# Response Style\n\n{}", RESPONSE_STYLE));

    // 8. Extra instructions (子会话等)
    if let Some(extra) = &input.extra_instructions {
        sections.push(extra.trim().to_string());
    }

    sections.join("\n\n")
}

/// 从扩展块中过滤指定 section 的内容，非空时追加到 sections 列表。
fn push_extension_section(
    sections: &mut Vec<String>,
    title: &str,
    blocks: &[ExtensionPromptBlock],
    kind: ExtensionSection,
) {
    let body = blocks
        .iter()
        .filter(|b| b.section == kind)
        .map(|b| b.content.trim())
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !body.is_empty() {
        sections.push(format!("# {}\n\n{}", title, body));
    }
}

// ─── AGENTS.md 加载（供 bootstrap 使用） ────────────────────────────────

/// 从 working_dir 向上遍历查找所有 AGENTS.md，由浅到深排序。
pub fn find_agents_files(working_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(working_dir);
    while let Some(dir) = current {
        dirs.push(dir.to_path_buf());
        current = dir.parent();
    }

    dirs.reverse();
    dirs.into_iter()
        .map(|dir| dir.join("AGENTS.md"))
        .filter(|path| path.is_file())
        .collect()
}

/// 读取并合并 AGENTS.md 文件为一段 project rules 文本。
pub fn load_project_rules(working_dir: &Path) -> Option<String> {
    let files = find_agents_files(working_dir);
    if files.is_empty() {
        return None;
    }

    let mut content = String::from(
        "以下内容来自 AGENTS.md。必须遵守；如果规则冲突，目录更深的 AGENTS.md 优先。\n",
    );
    for path in files {
        if let Ok(text) = fs::read_to_string(&path) {
            content.push_str("\n--- ");
            content.push_str(&path.display().to_string());
            content.push_str(" ---\n");
            content.push_str(&text);
            if !text.ends_with('\n') {
                content.push('\n');
            }
        }
    }

    non_empty_string(content)
}

fn non_empty_string(text: String) -> Option<String> {
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn build_renders_all_sections_in_order() {
        let input = SystemPromptInput {
            working_dir: "/test".into(),
            os: "linux".into(),
            shell: "bash".into(),
            date: "2026-04-29".into(),
            identity: Some("custom identity".into()),
            user_rules: Some("test rules".into()),
            project_rules: Some("project rules content".into()),
            extension_blocks: vec![
                ExtensionPromptBlock {
                    section: ExtensionSection::Skills,
                    content: "skill a".into(),
                },
                ExtensionPromptBlock {
                    section: ExtensionSection::Agents,
                    content: "agent x".into(),
                },
                ExtensionPromptBlock {
                    section: ExtensionSection::PlatformInstructions,
                    content: "extra hint".into(),
                },
            ],
            extra_instructions: Some("extra body".into()),
            template_vars: BTreeMap::new(),
            tools: vec![],
        };

        let prompt = build_system_prompt(&input);

        // All sections present
        assert!(prompt.contains("# Identity\n\ncustom identity"));
        assert!(prompt.contains("# Environment\n\n工作目录：/test"));
        assert!(prompt.contains("# User Rules\n\ntest rules"));
        assert!(prompt.contains("# Project Rules\n\nproject rules content"));
        assert!(prompt.contains("# Platform Instructions\n\nextra hint"));
        assert!(prompt.contains("# Skills\n\nskill a"));
        assert!(prompt.contains("# Agents\n\nagent x"));
        assert!(prompt.contains("# Few Shot"));
        assert!(prompt.contains("# Response Style"));
        assert!(prompt.contains("extra body"));

        // Ordering: Identity before Environment, Environment before Skills, etc.
        let identity = prompt.find("# Identity").unwrap();
        let env = prompt.find("# Environment").unwrap();
        let user_rules = prompt.find("# User Rules").unwrap();
        let project_rules = prompt.find("# Project Rules").unwrap();
        let platform = prompt.find("# Platform Instructions").unwrap();
        let skills = prompt.find("# Skills").unwrap();
        let agents = prompt.find("# Agents").unwrap();
        let few_shot = prompt.find("# Few Shot").unwrap();
        let style = prompt.find("# Response Style").unwrap();

        assert!(identity < env);
        assert!(env < user_rules);
        assert!(user_rules < project_rules);
        assert!(project_rules < platform);
        assert!(platform < skills);
        assert!(skills < agents);
        assert!(agents < few_shot);
        assert!(few_shot < style);
    }

    #[test]
    fn empty_optionals_are_skipped() {
        let input = SystemPromptInput {
            working_dir: "/test".into(),
            os: "linux".into(),
            shell: "bash".into(),
            date: "2026-04-29".into(),
            identity: None,
            user_rules: None,
            project_rules: None,
            extension_blocks: vec![],
            extra_instructions: None,
            template_vars: BTreeMap::new(),
            tools: vec![],
        };

        let prompt = build_system_prompt(&input);

        // Should have Identity (fallback to default), Environment, Few Shot, Response Style
        assert!(prompt.contains("# Identity\n\n"));
        assert!(prompt.contains("# Environment"));
        assert!(prompt.contains("# Few Shot"));
        assert!(prompt.contains("# Response Style"));
        // Should NOT have empty sections
        assert!(!prompt.contains("# User Rules"));
        assert!(!prompt.contains("# Project Rules"));
        assert!(!prompt.contains("# Platform Instructions"));
        assert!(!prompt.contains("# Skills"));
        assert!(!prompt.contains("# Agents"));
    }
}
