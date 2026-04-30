//! Prompt composer: 薄包装，委托 pipeline 构建 system prompt。

use astrcode_core::prompt::{PromptPlan, PromptProvider, SystemPromptInput};

use crate::pipeline;

pub struct PromptComposer;

impl PromptComposer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PromptComposer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl PromptProvider for PromptComposer {
    async fn assemble(&self, input: SystemPromptInput) -> PromptPlan {
        let system_prompt = pipeline::build_system_prompt(&input);
        PromptPlan::from_system_prompt(system_prompt)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn input() -> SystemPromptInput {
        SystemPromptInput {
            working_dir: env!("CARGO_MANIFEST_DIR").to_string(),
            os: "windows".into(),
            shell: "powershell".into(),
            date: "2026-04-28".into(),
            identity: None,
            user_rules: None,
            project_rules: None,
            extension_blocks: vec![],
            extra_instructions: None,
            template_vars: BTreeMap::new(),
            tools: vec![],
        }
    }

    #[tokio::test]
    async fn assemble_returns_usable_prompt_plan() {
        let plan = PromptComposer::new().assemble(input()).await;
        assert!(plan.system_prompt.is_some());
        assert!(plan.extra_tools.is_empty());
    }
}
