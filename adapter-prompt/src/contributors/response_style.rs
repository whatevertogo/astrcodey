//! 响应风格贡献者。
//!
//! 为模型补充稳定的用户沟通风格与收尾格式约束，
//! 避免输出退化成工具日志或未验证的结论。

use async_trait::async_trait;

use crate::{BlockKind, BlockSpec, PromptContext, PromptContribution, PromptContributor};

pub struct ResponseStyleContributor;

const RESPONSE_STYLE_GUIDANCE: &str =
    "\
Write for the user, not for a console log. Lead with the answer, action, or next step when it is \
     clear.\n\nWhen the task needs tools, multiple steps, or noticeable wait time:\n- Before the \
     first tool call, briefly state what you are going to do.\n- Give short progress updates when \
     you confirm something important, change direction, or make meaningful progress after a \
     stretch of silence.\n- Use complete sentences and enough context that the user can resume \
     cold.\n\nDo not present a guess, lead, or partial result as if it were confirmed. \
     Distinguish a suspicion from a supported finding, and distinguish both from the final \
     conclusion.\n\nPrefer clear prose over running debug-log narration. Use light structure only \
     when it improves readability.\n\nWhen closing out implementation work, briefly cover:\n- \
     what changed,\n- why this shape is correct,\n- what you verified,\n- any remaining risk or \
     next step if verification was partial.";

#[async_trait]
impl PromptContributor for ResponseStyleContributor {
    fn contributor_id(&self) -> &'static str {
        "response-style"
    }

    fn cache_version(&self) -> u64 {
        1
    }

    async fn contribute(&self, _ctx: &PromptContext) -> PromptContribution {
        PromptContribution {
            blocks: vec![
                BlockSpec::system_text(
                    "response-style",
                    BlockKind::SkillGuide,
                    "Response Style",
                    RESPONSE_STYLE_GUIDANCE,
                )
                .with_category("communication")
                .with_tag("source:builtin"),
            ],
            ..PromptContribution::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlockContent;

    #[tokio::test]
    async fn renders_response_style_guidance_block() {
        let contribution = ResponseStyleContributor
            .contribute(&PromptContext {
                working_dir: "/workspace/demo".to_string(),
                tool_names: Vec::new(),
                capability_specs: Vec::new(),
                system_prompt_instructions: Vec::new(),
                agent_profiles: Vec::new(),
                skills: Vec::new(),
                step_index: 0,
                turn_index: 0,
                vars: Default::default(),
            })
            .await;

        assert_eq!(contribution.blocks.len(), 1);
        assert_eq!(contribution.blocks[0].kind, BlockKind::SkillGuide);
        let BlockContent::Text(content) = &contribution.blocks[0].content else {
            panic!("response style should render as text");
        };
        assert!(content.contains("Before the first tool call"));
        assert!(content.contains("Do not present a guess"));
        assert!(content.contains("what changed"));
    }
}
