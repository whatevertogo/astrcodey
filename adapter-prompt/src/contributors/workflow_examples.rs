//! 工作流示例贡献者。
//!
//! 提供 few-shot 示例对话，教导模型"先收集上下文再修改代码"的行为模式。
//! 仅在第一步（step_index == 0）时生效，以 prepend 方式插入到对话消息中。
//!
//! 子 Agent 协作指导现在由上游治理声明注入；本 contributor 只保留 few-shot 示例。
use async_trait::async_trait;

use crate::{
    BlockCondition, BlockKind, BlockSpec, PromptContext, PromptContribution, PromptContributor,
    RenderTarget,
};

pub struct WorkflowExamplesContributor;

#[async_trait]
impl PromptContributor for WorkflowExamplesContributor {
    fn contributor_id(&self) -> &'static str {
        "workflow-examples"
    }

    async fn contribute(&self, ctx: &PromptContext) -> PromptContribution {
        let mut blocks = vec![
            BlockSpec::message_text(
                "few-shot-user",
                BlockKind::FewShotExamples,
                "Few Shot User",
                "Before changing code, inspect the relevant files and gather context first. If \
                 you only know a filename pattern or glob, use `findFiles`. Use `grep` only when \
                 you have both a content pattern and a search path. Use `shell` for directory \
                 inspection commands.",
                RenderTarget::PrependUser,
            )
            .with_condition(BlockCondition::FirstStepOnly)
            .with_priority(700),
            BlockSpec::message_text(
                "few-shot-assistant",
                BlockKind::FewShotExamples,
                "Few Shot Assistant",
                "I will inspect the relevant files and gather context before making changes. I \
                 will use `findFiles` to discover candidate paths, then use `grep` with both \
                 `pattern` and `path` when I need content search inside those files or \
                 directories. I will use `shell` for directory inspection when needed.",
                RenderTarget::PrependAssistant,
            )
            .with_condition(BlockCondition::FirstStepOnly)
            .depends_on("few-shot-user")
            .with_priority(701),
        ];

        if should_add_tool_search_example(ctx) {
            blocks.push(
                BlockSpec::message_text(
                    "tool-search-few-shot-user",
                    BlockKind::FewShotExamples,
                    "Tool Search Few Shot User",
                    "A visible external `mcp__...` tool looks relevant, but its parameters are \
                     unclear. Do not guess argument names or call it with an empty object. Use \
                     `tool_search` first to inspect the external tool schema.",
                    RenderTarget::PrependUser,
                )
                .with_condition(BlockCondition::FirstStepOnly)
                .with_priority(702),
            );
            blocks.push(
                BlockSpec::message_text(
                    "tool-search-few-shot-assistant",
                    BlockKind::FewShotExamples,
                    "Tool Search Few Shot Assistant",
                    "I will not guess parameters for the external tool. I will call `tool_search` \
                     first with part of the tool name or task purpose, for example `{ \"query\": \
                     \"webReader\" }` or `{ \"query\": \"github repo structure\" }`, read the \
                     returned `inputSchema`, and only then call the matching `mcp__...` tool with \
                     the documented arguments.",
                    RenderTarget::PrependAssistant,
                )
                .with_condition(BlockCondition::FirstStepOnly)
                .depends_on("tool-search-few-shot-user")
                .with_priority(703),
            );
        }

        PromptContribution {
            blocks,
            ..PromptContribution::default()
        }
    }
}

fn should_add_tool_search_example(ctx: &PromptContext) -> bool {
    has_tool_search(ctx) && has_external_tools(ctx)
}

fn has_tool_search(ctx: &PromptContext) -> bool {
    ctx.tool_names
        .iter()
        .any(|tool_name| tool_name == "tool_search")
}

fn has_external_tools(ctx: &PromptContext) -> bool {
    ctx.capability_specs.iter().any(|spec| {
        spec.kind.is_tool()
            && spec
                .tags
                .iter()
                .any(|tag| tag == "source:mcp" || tag == "source:plugin")
    })
}

#[cfg(test)]
mod tests {
    use astrcode_core::{LlmMessage, test_support::TestEnvGuard};

    use super::*;
    use crate::{PromptComposer, PromptComposerOptions, ValidationLevel};

    #[tokio::test]
    async fn adds_first_step_examples() {
        let _guard = TestEnvGuard::new();
        let composer = PromptComposer::with_options(PromptComposerOptions {
            validation_level: ValidationLevel::Strict,
            ..PromptComposerOptions::default()
        });

        let ctx = PromptContext {
            working_dir: "/workspace/demo".to_string(),
            tool_names: vec!["shell".to_string()],
            capability_specs: Vec::new(),
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 0,
            turn_index: 0,
            vars: Default::default(),
        };

        let output = composer.build(&ctx).await.expect("build should succeed");

        assert_eq!(output.plan.prepend_messages.len(), 2);
        assert!(
            output
                .plan
                .system_blocks
                .iter()
                .all(|block| block.id != "child-collaboration-guidance")
        );
        match &output.plan.prepend_messages[0] {
            LlmMessage::User { content, .. } => assert!(content.contains("findFiles")),
            other => panic!("expected prepended user message, got {other:?}"),
        }
        match &output.plan.prepend_messages[1] {
            LlmMessage::Assistant { content, .. } => assert!(content.contains("pattern")),
            other => panic!("expected prepended assistant message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn adds_tool_search_examples_when_external_tools_are_available() {
        let _guard = TestEnvGuard::new();
        let composer = PromptComposer::with_options(PromptComposerOptions {
            validation_level: ValidationLevel::Strict,
            ..PromptComposerOptions::default()
        });

        let ctx = PromptContext {
            working_dir: "/workspace/demo".to_string(),
            tool_names: vec![
                "tool_search".to_string(),
                "mcp__web-reader__webReader".to_string(),
            ],
            capability_specs: vec![
                astrcode_core::CapabilitySpec::builder(
                    "mcp__web-reader__webReader",
                    astrcode_core::CapabilityKind::Tool,
                )
                .description("Fetch and Convert URL to Large Model Friendly Input.")
                .schema(
                    serde_json::json!({"type": "object"}),
                    serde_json::json!({"type": "string"}),
                )
                .tags(["source:mcp"])
                .build()
                .expect("spec should build"),
            ],
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 0,
            turn_index: 0,
            vars: Default::default(),
        };

        let output = composer.build(&ctx).await.expect("build should succeed");

        assert_eq!(output.plan.prepend_messages.len(), 4);
        match &output.plan.prepend_messages[2] {
            LlmMessage::User { content, .. } => {
                assert!(content.contains("Do not guess argument names"));
                assert!(content.contains("`tool_search`"));
            },
            other => panic!("expected prepended user message, got {other:?}"),
        }
        match &output.plan.prepend_messages[3] {
            LlmMessage::Assistant { content, .. } => {
                assert!(content.contains("{ \"query\": \"webReader\" }"));
                assert!(content.contains("`inputSchema`"));
            },
            other => panic!("expected prepended assistant message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn does_not_add_collaboration_guidance_directly() {
        let _guard = TestEnvGuard::new();
        let composer = PromptComposer::with_options(PromptComposerOptions {
            validation_level: ValidationLevel::Strict,
            ..PromptComposerOptions::default()
        });

        let ctx = PromptContext {
            working_dir: "/workspace/demo".to_string(),
            tool_names: vec![
                "shell".to_string(),
                "spawn".to_string(),
                "observe".to_string(),
            ],
            capability_specs: Vec::new(),
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 0,
            turn_index: 0,
            vars: Default::default(),
        };

        let output = composer.build(&ctx).await.expect("build should succeed");

        assert!(
            output
                .plan
                .system_blocks
                .iter()
                .all(|block| block.id != "child-collaboration-guidance")
        );
    }
}
