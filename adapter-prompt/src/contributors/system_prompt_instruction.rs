//! System prompt 指令贡献者。
//!
//! 承接需要进入分层 system prompt blocks 的指令，动态 mode/child contract 继续走
//! `LlmMessage::System` 对话流，避免破坏 system prompt block 缓存。

use async_trait::async_trait;

use crate::{BlockKind, BlockSpec, PromptContext, PromptContribution, PromptContributor};

pub struct SystemPromptInstructionContributor;

#[async_trait]
impl PromptContributor for SystemPromptInstructionContributor {
    fn contributor_id(&self) -> &'static str {
        "system-prompt-instruction"
    }

    fn cache_version(&self) -> u64 {
        1
    }

    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        serde_json::to_string(&ctx.system_prompt_instructions)
            .expect("system prompt instructions should serialize")
    }

    async fn contribute(&self, ctx: &PromptContext) -> PromptContribution {
        let mut instructions = ctx.system_prompt_instructions.clone();
        instructions.sort_by(|left, right| {
            left.priority_hint
                .unwrap_or(580)
                .cmp(&right.priority_hint.unwrap_or(580))
                .then_with(|| left.block_id.cmp(&right.block_id))
        });

        PromptContribution {
            blocks: instructions
                .into_iter()
                .map(|instruction| {
                    let mut block = BlockSpec::system_text(
                        instruction.block_id,
                        BlockKind::ExtensionInstruction,
                        instruction.title,
                        instruction.content,
                    )
                    .with_layer(instruction.layer)
                    .with_category("extensions")
                    .with_tag("source:runtime-system-prompt");
                    if let Some(priority_hint) = instruction.priority_hint {
                        block = block.with_priority(priority_hint);
                    }
                    if let Some(origin) = instruction.origin {
                        block = block.with_origin(origin);
                    }
                    block
                })
                .collect(),
            ..PromptContribution::default()
        }
    }
}
