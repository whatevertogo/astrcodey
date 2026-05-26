//! Turn pipeline stage state shared by the turn runner.

use std::collections::HashSet;

use astrcode_core::tool::{ToolDefinition, ToolPromptMetadata, ToolResult};

use crate::deferred_tools::{
    ToolSnapshot, activate_deferred_tools, clone_tools_by_index, provider_visible_tool_indexes,
};

/// Mutable state carried across provider/tool iterations in a single turn.
pub(crate) struct TurnState {
    final_text: String,
    tool_results: Vec<ToolResult>,
    reactive_compact_used: bool,
    active_deferred_tools: HashSet<String>,
    all_tools: Vec<ToolSnapshot>,
    visible_tools: Vec<ToolSnapshot>,
}

impl TurnState {
    pub(crate) fn new(all_tools: Vec<(ToolDefinition, Option<ToolPromptMetadata>)>) -> Self {
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
            final_text: String::new(),
            tool_results: Vec::new(),
            reactive_compact_used: false,
            active_deferred_tools,
            all_tools,
            visible_tools,
        }
    }

    pub(crate) fn push_tool_result(&mut self, result: ToolResult) {
        self.tool_results.push(result);
    }

    pub(crate) fn append_final_text(&mut self, text: &str) {
        self.final_text.push_str(text);
    }

    pub(crate) fn final_text(&self) -> &str {
        &self.final_text
    }

    pub(crate) fn reactive_compact_used(&self) -> bool {
        self.reactive_compact_used
    }

    pub(crate) fn mark_reactive_compact_used(&mut self) {
        self.reactive_compact_used = true;
    }

    pub(crate) fn take_output_parts(self) -> (String, Vec<ToolResult>) {
        (self.final_text, self.tool_results)
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

pub(crate) struct PreparedProviderRequest {
    pub(crate) llm: std::sync::Arc<dyn astrcode_core::llm::LlmProvider>,
    pub(crate) messages: Vec<astrcode_core::llm::LlmMessage>,
}
