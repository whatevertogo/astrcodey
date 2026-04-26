//! 子 Agent profile 摘要贡献者。
//!
//! 当 `spawn` 工具可用时，生成当前可委派 profile 的动态索引块。
//! 这样可把动态 profile 列表放在 prompt 层，而不是固化进 tool definition。

use async_trait::async_trait;

use crate::{
    BlockKind, BlockSpec, PromptAgentProfileSummary, PromptContext, PromptContribution,
    PromptContributor,
};

pub struct AgentProfileSummaryContributor;

const SPAWN_AGENT_TOOL_NAME: &str = "spawn";
const MAX_PROFILE_SUMMARY_CHARS: usize = 120;

#[async_trait]
impl PromptContributor for AgentProfileSummaryContributor {
    fn contributor_id(&self) -> &'static str {
        "agent-profile-summary"
    }

    fn cache_version(&self) -> u64 {
        4
    }

    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        ctx.contributor_cache_fingerprint()
    }

    async fn contribute(&self, ctx: &PromptContext) -> PromptContribution {
        if !ctx
            .tool_names
            .iter()
            .any(|tool_name| tool_name == SPAWN_AGENT_TOOL_NAME)
        {
            return PromptContribution::default();
        }

        let mut profiles = ctx.agent_profiles.clone();
        profiles.sort_by(|left, right| left.id.cmp(&right.id));
        if profiles.is_empty() {
            return PromptContribution::default();
        }

        let mut content = String::from(
            "Use `spawn` only when a task benefits from a dedicated sub-agent with an isolated \
             task scope. Prefer doing the work locally when the task is tiny, the answer is \
             needed immediately for the next step, or no context isolation is \
             needed.\n\nAvailable child behavior templates:\n",
        );
        for profile in profiles {
            append_profile_line(&mut content, &profile);
        }
        content.push_str(
            "\nChoose `type` by the kind of responsibility you want the child to own. Omit `type` \
             to use the default `explore` template. These entries describe behavior and \
             when-to-use scope only. They do not declare static tool ownership or launch-time \
             capability limits.",
        );

        PromptContribution {
            blocks: vec![
                BlockSpec::system_text(
                    "agent-profile-summary",
                    BlockKind::ToolGuide,
                    "Child Behavior Templates",
                    content.trim_end().to_string(),
                )
                .with_category("agents")
                .with_tag("source:agent-profile-index"),
            ],
            ..PromptContribution::default()
        }
    }
}

fn append_profile_line(content: &mut String, profile: &PromptAgentProfileSummary) {
    let summary = compact_profile_description(&profile.description);
    content.push_str(&format!("- {}: {}\n", profile.id, summary));
}

fn compact_profile_description(description: &str) -> String {
    let normalized = normalize_profile_description(description);
    let sentence = first_sentence(&normalized).unwrap_or(normalized.as_str());
    truncate_chars(sentence, MAX_PROFILE_SUMMARY_CHARS)
}

fn normalize_profile_description(description: &str) -> String {
    description
        .replace("\\r", " ")
        .replace("\\n", " ")
        .replace(['\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_sentence(description: &str) -> Option<&str> {
    description
        .char_indices()
        .find_map(|(index, ch)| {
            matches!(ch, '.' | '。' | '!' | '！' | '?' | '？' | ';' | '；')
                .then_some(index + ch.len_utf8())
        })
        .map(|end| description[..end].trim())
        .filter(|sentence| !sentence.is_empty())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::test_support::TestEnvGuard;

    use super::*;
    use crate::{BlockContent, PromptContext};

    #[tokio::test]
    async fn renders_agent_profile_listing_when_spawn_agent_is_available() {
        let _guard = TestEnvGuard::new();
        let contribution = AgentProfileSummaryContributor
            .contribute(&PromptContext {
                working_dir: "/workspace/demo".to_string(),
                tool_names: vec!["shell".to_string(), "spawn".to_string()],
                capability_specs: Vec::new(),
                system_prompt_instructions: Vec::new(),
                agent_profiles: vec![PromptAgentProfileSummary::new("reviewer", "多视角代码审查")],
                skills: Vec::new(),
                step_index: 0,
                turn_index: 0,
                vars: Default::default(),
            })
            .await;

        assert_eq!(contribution.blocks.len(), 1);
        let BlockContent::Text(content) = &contribution.blocks[0].content else {
            panic!("agent profile summary should render as text");
        };
        assert!(content.contains("Available child behavior templates"));
        assert!(content.contains("- reviewer: 多视角代码审查"));
    }

    #[test]
    fn compact_profile_description_keeps_only_a_bounded_summary() {
        let summary = compact_profile_description(
            "Use this agent when code review is needed across security, correctness, tests, \
             architecture, regression risk, release readiness, and broader integration boundaries \
             before every commit or pull request in critical code \
             paths.\\n\\nExamples:\\n\\n<example>...</example>",
        );

        assert!(summary.starts_with("Use this agent when code review is needed"));
        assert!(!summary.contains("Examples"));
        assert!(summary.ends_with('…'));
        assert!(summary.chars().count() <= MAX_PROFILE_SUMMARY_CHARS + 1);
    }

    #[tokio::test]
    async fn behavior_template_catalog_does_not_claim_capability_authority() {
        let _guard = TestEnvGuard::new();
        let contribution = AgentProfileSummaryContributor
            .contribute(&PromptContext {
                working_dir: "/workspace/demo".to_string(),
                tool_names: vec!["spawn".to_string()],
                capability_specs: Vec::new(),
                system_prompt_instructions: Vec::new(),
                agent_profiles: vec![PromptAgentProfileSummary::new(
                    "explore",
                    "适合先收集上下文、梳理代码路径，再把发现整理给父级。",
                )],
                skills: Vec::new(),
                step_index: 0,
                turn_index: 0,
                vars: Default::default(),
            })
            .await;

        let BlockContent::Text(content) = &contribution.blocks[0].content else {
            panic!("agent profile summary should render as text");
        };
        assert!(content.contains("Available child behavior templates"));
        assert!(content.contains("describe behavior and when-to-use scope only"));
        assert!(content.contains("do not declare static tool ownership"));
    }

    #[tokio::test]
    async fn skips_listing_when_spawn_agent_is_unavailable() {
        let _guard = TestEnvGuard::new();
        let contribution = AgentProfileSummaryContributor
            .contribute(&PromptContext {
                working_dir: "/workspace/demo".to_string(),
                tool_names: vec!["shell".to_string()],
                capability_specs: Vec::new(),
                system_prompt_instructions: Vec::new(),
                agent_profiles: vec![PromptAgentProfileSummary::new("reviewer", "多视角代码审查")],
                skills: Vec::new(),
                step_index: 0,
                turn_index: 0,
                vars: Default::default(),
            })
            .await;

        assert!(contribution.blocks.is_empty());
    }
}
