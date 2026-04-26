//! Skill 摘要贡献者。
//!
//! 当 `Skill` tool 在可用工具列表中时，生成 skill 索引摘要 block。
//! 这是两阶段 skill 模型的第一阶段：仅暴露 skill 名称和描述，
//! 完整指南通过 `Skill` tool 按需加载。

use async_trait::async_trait;

use crate::{BlockKind, BlockSpec, PromptContext, PromptContribution, PromptContributor};

pub struct SkillSummaryContributor;

const SKILL_TOOL_NAME: &str = "Skill";

#[async_trait]
impl PromptContributor for SkillSummaryContributor {
    fn contributor_id(&self) -> &'static str {
        "skill-summary"
    }

    fn cache_version(&self) -> u64 {
        1
    }

    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        ctx.contributor_cache_fingerprint()
    }

    async fn contribute(&self, ctx: &PromptContext) -> PromptContribution {
        if !ctx
            .tool_names
            .iter()
            .any(|tool_name| tool_name == SKILL_TOOL_NAME)
        {
            return PromptContribution::default();
        }

        // ctx.skills 已由 turn_runner 通过 SkillCatalog.resolve_for_working_dir 解析完毕，
        // 此处直接使用，避免重复解析。
        let mut skills = ctx.skills.clone();
        skills.sort_by(|left, right| left.id.cmp(&right.id));
        if skills.is_empty() {
            return PromptContribution::default();
        }

        let mut content = String::from(
            "Execute a skill within the main conversation.\n\nWhen a task matches one of the \
             available skills, call the `Skill` tool before continuing. Do not mention a skill \
             without calling `Skill`.\n\nAvailable skills:\n",
        );
        for skill in skills {
            content.push_str(&format!("- {}: {}\n", skill.id, skill.description.trim()));
        }

        PromptContribution {
            blocks: vec![
                BlockSpec::system_text(
                    "skill-summary",
                    BlockKind::Skill,
                    "Skill Summary",
                    content.trim_end().to_string(),
                )
                .with_category("skills")
                .with_tag("source:skill-index"),
            ],
            ..PromptContribution::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::test_support::TestEnvGuard;

    use super::*;
    use crate::{BlockContent, PromptContext, PromptSkillSummary};

    #[tokio::test]
    async fn renders_skill_listing_when_skill_tool_is_available() {
        let _guard = TestEnvGuard::new();
        let contribution = SkillSummaryContributor
            .contribute(&PromptContext {
                working_dir: "/workspace/demo".to_string(),
                tool_names: vec!["shell".to_string(), "Skill".to_string()],
                capability_specs: Vec::new(),
                system_prompt_instructions: Vec::new(),
                agent_profiles: Vec::new(),
                skills: vec![PromptSkillSummary::new(
                    "git-commit",
                    "Use this skill when the user asks you to write and run a git commit.",
                )],
                step_index: 0,
                turn_index: 0,
                vars: Default::default(),
            })
            .await;

        assert_eq!(contribution.blocks.len(), 1);
        let BlockContent::Text(content) = &contribution.blocks[0].content else {
            panic!("skill summary should render as text");
        };
        assert!(content.contains("call the `Skill` tool"));
        assert!(content.contains("- git-commit:"));
    }

    #[tokio::test]
    async fn skips_listing_when_skill_tool_is_unavailable() {
        let _guard = TestEnvGuard::new();
        let contribution = SkillSummaryContributor
            .contribute(&PromptContext {
                working_dir: "/workspace/demo".to_string(),
                tool_names: vec!["shell".to_string()],
                capability_specs: Vec::new(),
                system_prompt_instructions: Vec::new(),
                agent_profiles: Vec::new(),
                skills: Vec::new(),
                step_index: 0,
                turn_index: 0,
                vars: Default::default(),
            })
            .await;

        assert!(contribution.blocks.is_empty());
    }
}
