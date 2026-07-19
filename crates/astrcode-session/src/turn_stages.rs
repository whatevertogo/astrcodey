//! Turn pipeline stage state shared by the turn runner.

use std::collections::HashSet;

use astrcode_core::{
    llm::{LlmContent, LlmMessage, LlmRole, provider_visible_messages},
    tool::{ToolDefinition, ToolPromptMetadata, ToolResult},
};

use crate::{
    deferred_tools::{ToolSnapshot, activate_deferred_tools, provider_visible_tools},
    tool_deduplicator::ToolCallDeduplicator,
    tool_types::StreamedToolCall,
};

/// Turn-local transcript facts that are produced after the durable read model snapshot.
///
/// Durable projection remains the cross-turn SSOT; this builder owns the in-flight turn facts so
/// runner, hooks, and tool commit do not each assemble assistant/tool messages differently.
#[derive(Default)]
pub(crate) struct TurnTranscript {
    output_text: String,
    tool_results: Vec<ToolResult>,
    latest_provider_response: Option<LlmMessage>,
}

impl TurnTranscript {
    pub(crate) fn record_assistant_text(&mut self, text: &str, reasoning_content: Option<String>) {
        self.output_text.push_str(text);
        self.record_assistant_message(
            vec![LlmContent::Text {
                text: text.to_string(),
            }],
            reasoning_content,
        );
    }

    pub(crate) fn record_assistant_tool_calls(
        &mut self,
        text: &str,
        reasoning_content: Option<String>,
        tool_calls: &[StreamedToolCall],
    ) {
        self.output_text.push_str(text);
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(LlmContent::Text {
                text: text.to_string(),
            });
        }
        content.extend(tool_calls.iter().map(|tool_call| LlmContent::ToolCall {
            call_id: tool_call.call_id.clone(),
            name: tool_call.name.clone(),
            arguments:
                serde_json::from_str(&tool_call.arguments).unwrap_or(serde_json::Value::Null),
        }));
        self.record_assistant_message(content, reasoning_content);
    }

    fn record_assistant_message(
        &mut self,
        content: Vec<LlmContent>,
        reasoning_content: Option<String>,
    ) {
        let message = LlmMessage {
            role: LlmRole::Assistant,
            content,
            name: None,
            reasoning_content,
        };
        if message.has_provider_visible_content() {
            self.latest_provider_response = Some(message);
        }
    }

    pub(crate) fn record_tool_result(&mut self, result: ToolResult) {
        self.tool_results.push(result);
    }

    pub(crate) fn append_output_text(&mut self, text: &str) {
        self.output_text.push_str(text);
    }

    pub(crate) fn output_text(&self) -> &str {
        &self.output_text
    }

    pub(crate) fn set_output_text(&mut self, text: String) {
        self.output_text = text;
    }

    pub(crate) fn take_output_parts(&mut self) -> (String, Vec<ToolResult>) {
        (
            std::mem::take(&mut self.output_text),
            std::mem::take(&mut self.tool_results),
        )
    }

    pub(crate) fn provider_response_messages(
        &self,
        mut request_messages: Vec<LlmMessage>,
    ) -> Vec<LlmMessage> {
        if let Some(message) = &self.latest_provider_response {
            request_messages.push(message.clone());
        }
        provider_visible_messages(request_messages)
    }
}

/// Mutable state carried across provider/tool iterations in a single turn.
pub(crate) struct TurnState {
    transcript: TurnTranscript,
    reactive_compact_used: bool,
    continue_after_stop_count: u32,
    /// 已计入上下文的非合成 user 消息数（用于 steer flush 检测）。
    tracked_user_message_count: usize,
    active_deferred_tools: HashSet<String>,
    all_tools: Vec<ToolSnapshot>,
    visible_tools: Vec<ToolSnapshot>,
    tool_deduplicator: ToolCallDeduplicator,
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
        let visible_tools = provider_visible_tools(&all_tools, &active_deferred_tools);

        Self {
            transcript: TurnTranscript::default(),
            reactive_compact_used: false,
            continue_after_stop_count: 0,
            tracked_user_message_count: 0,
            active_deferred_tools,
            all_tools,
            visible_tools,
            tool_deduplicator: ToolCallDeduplicator::new(),
        }
    }

    pub(crate) fn tool_deduplicator(&self) -> &ToolCallDeduplicator {
        &self.tool_deduplicator
    }

    pub(crate) fn tool_deduplicator_mut(&mut self) -> &mut ToolCallDeduplicator {
        &mut self.tool_deduplicator
    }

    pub(crate) fn continue_after_stop_count(&self) -> u32 {
        self.continue_after_stop_count
    }

    pub(crate) fn record_continue_after_stop(&mut self) {
        self.continue_after_stop_count = self.continue_after_stop_count.saturating_add(1);
    }

    pub(crate) fn tracked_user_message_count(&self) -> usize {
        self.tracked_user_message_count
    }

    pub(crate) fn set_tracked_user_message_count(&mut self, count: usize) {
        self.tracked_user_message_count = count;
    }

    pub(crate) fn record_tool_result(&mut self, result: ToolResult) {
        self.transcript.record_tool_result(result);
    }

    pub(crate) fn append_final_text(&mut self, text: &str) {
        self.transcript.append_output_text(text);
    }

    pub(crate) fn final_text(&self) -> &str {
        self.transcript.output_text()
    }

    pub(crate) fn set_final_text(&mut self, text: String) {
        self.transcript.set_output_text(text);
    }

    pub(crate) fn record_assistant_text(&mut self, text: &str, reasoning_content: Option<String>) {
        self.transcript
            .record_assistant_text(text, reasoning_content);
    }

    pub(crate) fn record_assistant_tool_calls(
        &mut self,
        text: &str,
        reasoning_content: Option<String>,
        tool_calls: &[StreamedToolCall],
    ) {
        self.transcript
            .record_assistant_tool_calls(text, reasoning_content, tool_calls);
    }

    pub(crate) fn provider_response_messages(
        &self,
        request_messages: Vec<LlmMessage>,
    ) -> Vec<LlmMessage> {
        self.transcript.provider_response_messages(request_messages)
    }

    pub(crate) fn reactive_compact_used(&self) -> bool {
        self.reactive_compact_used
    }

    pub(crate) fn mark_reactive_compact_used(&mut self) {
        self.reactive_compact_used = true;
    }

    pub(crate) fn take_output_parts(&mut self) -> (String, Vec<ToolResult>) {
        self.transcript.take_output_parts()
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
            self.visible_tools =
                provider_visible_tools(&self.all_tools, &self.active_deferred_tools);
        }
        changed
    }
}

pub(crate) struct PreparedProviderRequest {
    pub(crate) llm: std::sync::Arc<dyn astrcode_core::llm::LlmProvider>,
    pub(crate) messages: Vec<astrcode_core::llm::LlmMessage>,
}
