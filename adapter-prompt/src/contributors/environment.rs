//! 环境贡献者。
//!
//! 生成包含工作目录、操作系统、默认 shell、日期和可用工具列表的环境信息 block。
//! 使用模板渲染，在 composer 渲染阶段填充变量。

use async_trait::async_trait;

use crate::{BlockKind, BlockSpec, PromptContext, PromptContribution, PromptContributor};

pub struct EnvironmentContributor;

#[async_trait]
impl PromptContributor for EnvironmentContributor {
    fn contributor_id(&self) -> &'static str {
        "environment"
    }

    async fn contribute(&self, _ctx: &PromptContext) -> PromptContribution {
        PromptContribution {
            blocks: vec![BlockSpec::system_template(
                "environment",
                BlockKind::Environment,
                "Environment",
                "Working directory: {{project.working_dir}}\nOS: {{env.os}}\nShell: \
                 {{env.shell}}\nDate: {{run.date}}\nAvailable tools: {{tools.names}}",
            )],
            ..PromptContribution::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::test_support::TestEnvGuard;

    use super::*;
    use crate::PromptComposer;

    #[tokio::test]
    async fn includes_working_dir_os_shell_date_and_tool_names() {
        let _guard = TestEnvGuard::new();
        let composer = PromptComposer::with_defaults();
        let ctx = PromptContext {
            working_dir: "/workspace/demo".to_string(),
            tool_names: vec!["shell".to_string(), "readFile".to_string()],
            capability_specs: Vec::new(),
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 0,
            turn_index: 0,
            vars: Default::default(),
        };

        let output = composer.build(&ctx).await.expect("build should succeed");
        let block = output
            .plan
            .system_blocks
            .iter()
            .find(|block| block.id == "environment")
            .expect("environment block should exist");
        assert_eq!(block.kind, BlockKind::Environment);
        assert!(block.content.contains("Working directory: /workspace/demo"));
        assert!(
            block
                .content
                .contains(&format!("OS: {}", std::env::consts::OS))
        );
        assert!(block.content.contains("Shell: "));
        assert!(block.content.contains("Date: "));
        assert!(block.content.contains("Available tools: shell, readFile"));
    }
}
