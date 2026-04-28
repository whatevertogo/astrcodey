//! Built-in prompt section fillers. Default prompt text source of truth.

use std::{
    fs,
    path::{Path, PathBuf},
};

use astrcode_core::prompt::{PromptContext, PromptSection, SystemPromptParts};
use astrcode_support::hostpaths::astrcode_dir;

// ─── Default identity (source of truth) ─────────────────────────────────

pub const DEFAULT_IDENTITY: &str = concat!(
    "你是 AstrCode，一位天才级工程师和团队负责人。代码是你的表达：正确、可维护。行动前先充分理解，\
     再精准执行；",
    "追求优雅、完善的最佳实践，定位根因而不是修补表象。面对复杂任务时，主动编排 agent 与工具协作，",
    "协调资源并推动项目成功。"
);

const MAX_IDENTITY_SIZE: usize = 4096;

const FEW_SHOT: &str = concat!(
    "示例模式：修改代码前，先检查相关文件并收集上下文。\n",
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

pub fn add_identity(parts: &mut SystemPromptParts) {
    let identity =
        load_identity_md(&user_identity_md_path()).unwrap_or_else(|| DEFAULT_IDENTITY.to_string());
    parts.set_identity(identity);
}

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

// ─── Fixed section fillers ──────────────────────────────────────────────

pub fn add_plugin_system(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(system_prompts) = ctx.custom.get("system_prompts") {
        parts.push(PromptSection::PluginSystem, system_prompts.clone());
    }
}

pub fn add_environment(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    parts.push(
        PromptSection::Environment,
        format!(
            "工作目录：{}\n操作系统：{}\nShell：{}\n日期：{}\n可用工具：{}",
            ctx.working_dir, ctx.os, ctx.shell, ctx.date, ctx.available_tools
        ),
    );
}

pub fn add_user_rules(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(rules) = ctx.custom.get("user_rules") {
        parts.push(PromptSection::UserRules, rules.clone());
    }
}

pub fn add_project_rules(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    let files = find_agents_files(Path::new(&ctx.working_dir));
    if files.is_empty() {
        return;
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

    parts.push(PromptSection::ProjectRules, content);
}

pub fn add_skills(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(skills) = ctx.custom.get("skills") {
        parts.push(PromptSection::Skills, skills.clone());
    }
}

pub fn add_agents(parts: &mut SystemPromptParts, ctx: &PromptContext) {
    if let Some(agents) = ctx.custom.get("agents") {
        parts.push(PromptSection::Agents, agents.clone());
    }
}

pub fn add_few_shot(parts: &mut SystemPromptParts) {
    parts.push(PromptSection::FewShot, FEW_SHOT);
}

pub fn add_response_style(parts: &mut SystemPromptParts) {
    parts.set_response_style(RESPONSE_STYLE);
}

fn find_agents_files(working_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(working_dir);
    while let Some(dir) = current {
        dirs.push(dir.to_path_buf());
        current = dir.parent();
    }

    // 从浅到深排列，后出现的更具体指令自然覆盖前面的通用指令。
    dirs.reverse();
    dirs.into_iter()
        .map(|dir| dir.join("AGENTS.md"))
        .filter(|path| path.is_file())
        .collect()
}
