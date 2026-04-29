//! Prompt composer: fill fixed system prompt sections and render once.

use astrcode_core::prompt::{PromptContext, PromptPlan, PromptProvider, SystemPromptParts};

use crate::contributors;

pub struct PromptComposer;

impl PromptComposer {
    pub fn new() -> Self {
        Self
    }

    pub async fn assemble_impl(&self, context: &PromptContext) -> PromptPlan {
        let mut parts = SystemPromptParts::default();

        contributors::add_identity(&mut parts);
        contributors::add_environment(&mut parts, context);
        contributors::add_user_rules(&mut parts, context);
        contributors::add_project_rules(&mut parts, context);
        contributors::add_skills(&mut parts, context);
        contributors::add_agents(&mut parts, context);
        contributors::add_few_shot(&mut parts);
        contributors::add_plugin_system(&mut parts, context);
        contributors::add_response_style(&mut parts);

        PromptPlan::from_system_prompt(parts.render_system_prompt())
    }
}

impl Default for PromptComposer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl PromptProvider for PromptComposer {
    async fn assemble(&self, context: PromptContext) -> PromptPlan {
        self.assemble_impl(&context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PromptContext {
        PromptContext {
            working_dir: env!("CARGO_MANIFEST_DIR").to_string(),
            os: "windows".to_string(),
            shell: "powershell".to_string(),
            date: "2026-04-28".to_string(),
            skills: None,
            agents: None,
            user_rules: None,
            plugin_system_prompts: None,
            custom: std::collections::BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn assemble_returns_usable_prompt_plan() {
        let plan = PromptComposer::new().assemble_impl(&context()).await;

        assert!(plan.system_prompt.is_some());
        assert!(plan.prepend_messages.is_empty());
        assert!(plan.append_messages.is_empty());
        assert!(plan.extra_tools.is_empty());
    }
}
