//! AGENTS.md 贡献者。
//!
//! 从两个位置加载 AGENTS.md 规则文件：
//! - 用户级：`~/.astrcode/AGENTS.md`（适用于所有项目）
//! - 项目级：`<working_dir>/AGENTS.md`（仅适用于当前项目）
//!
//! 两个文件同时存在时都会被包含到 prompt 中，分别作为 UserRules 和 ProjectRules block。

use std::{
    fs,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use log::warn;

use super::shared::{cache_marker_for_path, user_astrcode_file_path};
use crate::{BlockKind, BlockSpec, PromptContext, PromptContribution, PromptContributor};

/// AGENTS.md 贡献者。
///
/// 同时加载用户级和项目级 AGENTS.md，分别映射到 `UserRules` 和 `ProjectRules` block。
/// 文件不存在时静默跳过，不阻塞整个 prompt 组装流程。
pub struct AgentsMdContributor;

pub fn user_agents_md_path() -> Option<PathBuf> {
    user_astrcode_file_path("AGENTS.md")
}

pub fn project_agents_md_path(working_dir: &str) -> PathBuf {
    PathBuf::from(working_dir).join("AGENTS.md")
}

pub fn load_agents_md(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }

    match fs::read_to_string(path) {
        Ok(content) => Some(content.trim().to_string()),
        Err(error) => {
            warn!("failed to read {}: {}", path.display(), error);
            None
        },
    }
}

#[async_trait]
impl PromptContributor for AgentsMdContributor {
    fn contributor_id(&self) -> &'static str {
        "agents-md"
    }

    fn cache_version(&self) -> u64 {
        3
    }

    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        let user_marker = user_agents_md_path()
            .map(|path| format!("{}={}", path.display(), cache_marker_for_path(&path)))
            .unwrap_or_else(|| "user=<unresolved>".to_string());
        let project_path = project_agents_md_path(&ctx.working_dir);
        let project_marker = format!(
            "{}={}",
            project_path.display(),
            cache_marker_for_path(&project_path)
        );

        format!("{user_marker}|{project_marker}")
    }

    async fn contribute(&self, ctx: &PromptContext) -> PromptContribution {
        let mut blocks = Vec::new();

        if let Some(path) = user_agents_md_path() {
            if let Some(content) = load_agents_md(&path) {
                blocks.push(
                    BlockSpec::system_text(
                        "user-agents-md",
                        BlockKind::UserRules,
                        "User Rules",
                        format!("User-wide instructions from {}:\n{content}", path.display()),
                    )
                    .with_origin(path.display().to_string()),
                );
            }
        }

        let project_path = project_agents_md_path(&ctx.working_dir);
        if let Some(content) = load_agents_md(&project_path) {
            blocks.push(
                BlockSpec::system_text(
                    "project-agents-md",
                    BlockKind::ProjectRules,
                    "Project Rules",
                    format!(
                        "Project-specific instructions from {}:\n{content}",
                        project_path.display()
                    ),
                )
                .with_origin(project_path.display().to_string()),
            );
        }

        PromptContribution {
            blocks,
            ..PromptContribution::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use astrcode_core::test_support::TestEnvGuard;

    use super::*;

    fn context(working_dir: String) -> PromptContext {
        PromptContext {
            working_dir,
            tool_names: vec!["shell".to_string()],
            capability_specs: Vec::new(),
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 0,
            turn_index: 0,
            vars: Default::default(),
        }
    }

    #[tokio::test]
    async fn returns_user_and_project_rules_blocks() {
        let guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let user_agents_path = guard.home_dir().join(".astrcode").join("AGENTS.md");
        fs::create_dir_all(user_agents_path.parent().expect("parent should exist"))
            .expect("user agents dir should be created");
        fs::write(&user_agents_path, "Follow user rule")
            .expect("user agents file should be written");
        fs::write(project.path().join("AGENTS.md"), "Follow project rule")
            .expect("project agents file should be written");
        let contributor = AgentsMdContributor;

        let contribution = contributor
            .contribute(&context(project.path().to_string_lossy().into_owned()))
            .await;

        assert_eq!(contribution.blocks.len(), 2);
        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.kind == BlockKind::UserRules)
        );
        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.kind == BlockKind::ProjectRules)
        );
    }
}
