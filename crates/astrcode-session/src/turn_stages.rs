//! Turn pipeline stage state shared by the turn runner.

use std::collections::HashSet;

use astrcode_context::prompt_engine::system_messages_from_prompt;
use astrcode_core::{
    llm::{LlmContent, LlmMessage, LlmRole},
    tool::{ToolDefinition, ToolPromptMetadata, ToolResult},
};

use crate::deferred_tools::{
    ToolSnapshot, activate_deferred_tools, clone_tools_by_index, provider_visible_tool_indexes,
};

/// Mutable state carried across provider/tool iterations in a single turn.
pub(crate) struct TurnState {
    pub(crate) messages: Vec<LlmMessage>,
    pub(crate) final_text: String,
    pub(crate) tool_results: Vec<ToolResult>,
    pub(crate) reactive_compact_used: bool,
    active_deferred_tools: HashSet<String>,
    all_tools: Vec<ToolSnapshot>,
    visible_tools: Vec<ToolSnapshot>,
}

impl TurnState {
    pub(crate) fn new(
        initial_history: Vec<LlmMessage>,
        system_prompt: &str,
        user_text: &str,
        all_tools: Vec<(ToolDefinition, Option<ToolPromptMetadata>)>,
    ) -> Self {
        let mut messages = Vec::with_capacity(initial_history.len() + 4);
        // KV 缓存分组：将系统提示词按 Static/SemiStatic/Dynamic 拆成多条 system message，
        // 让 Anthropic 和 OpenAI 的前缀缓存机制自然生效。
        messages.extend(system_messages_from_prompt(system_prompt));
        messages.extend(
            initial_history
                .into_iter()
                .filter(|message| message.role != LlmRole::System),
        );
        if !last_message_is_user_text(&messages, user_text) {
            messages.push(LlmMessage::user(user_text));
        }

        let all_tools = all_tools
            .into_iter()
            .map(|(definition, prompt_metadata)| ToolSnapshot {
                definition,
                prompt_metadata,
            })
            .collect::<Vec<_>>();
        let active_deferred_tools = HashSet::new();
        let tool_indexes = provider_visible_tool_indexes(&all_tools, &active_deferred_tools);
        let visible_tools = clone_tools_by_index(&all_tools, &tool_indexes);

        Self {
            messages,
            final_text: String::new(),
            tool_results: Vec::new(),
            reactive_compact_used: false,
            active_deferred_tools,
            all_tools,
            visible_tools,
        }
    }

    pub(crate) fn all_tool_snapshots(&self) -> &[ToolSnapshot] {
        &self.all_tools
    }

    pub(crate) fn visible_tools(&self) -> Vec<ToolDefinition> {
        ToolSnapshot::definitions(&self.visible_tools)
    }

    pub(crate) fn active_deferred_tools(&self) -> &HashSet<String> {
        &self.active_deferred_tools
    }

    pub(crate) fn activate_deferred_tools(&mut self, discovered_tools: Vec<String>) -> bool {
        let changed = activate_deferred_tools(
            &mut self.active_deferred_tools,
            &self.all_tools,
            discovered_tools,
        );
        if changed {
            let tool_indexes =
                provider_visible_tool_indexes(&self.all_tools, &self.active_deferred_tools);
            self.visible_tools = clone_tools_by_index(&self.all_tools, &tool_indexes);
        }
        changed
    }
}

fn last_message_is_user_text(messages: &[LlmMessage], text: &str) -> bool {
    messages.last().is_some_and(|message| {
        message.role == LlmRole::User
            && message.content.len() == 1
            && matches!(&message.content[0], LlmContent::Text { text: value } if value == text)
    })
}

pub(crate) struct PreparedProviderRequest {
    pub(crate) llm: std::sync::Arc<dyn astrcode_core::llm::LlmProvider>,
    pub(crate) messages: Vec<LlmMessage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_state_does_not_duplicate_already_persisted_user_message() {
        let state = TurnState::new(
            vec![LlmMessage::user("current")],
            "system",
            "current",
            Vec::new(),
        );

        let user_messages = state
            .messages
            .iter()
            .filter(|message| message.role == LlmRole::User)
            .count();
        assert_eq!(user_messages, 1);
    }
}
