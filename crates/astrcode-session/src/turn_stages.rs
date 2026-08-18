//! Turn pipeline stage state shared by the turn runner.

use std::{collections::HashSet, sync::Arc};

use astrcode_core::{
    llm::{LlmContent, LlmMessage, LlmRole, provider_visible_shared_messages},
    tool::ToolDefinition,
};
use astrcode_session_projection::ActiveStepView;

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
    latest_provider_response: Option<LlmMessage>,
}

impl TurnTranscript {
    pub(crate) fn record_assistant_text(&mut self, text: &str, reasoning_content: Option<String>) {
        self.output_text.push_str(text);
        self.remember_latest_visible_response(
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
        content.extend(tool_calls.iter().map(|tool_call| {
            let (arguments, raw_arguments) = match serde_json::from_str(&tool_call.arguments) {
                Ok(arguments) => (arguments, None),
                Err(_) => (
                    serde_json::Value::String(tool_call.arguments.clone()),
                    Some(tool_call.arguments.clone()),
                ),
            };
            LlmContent::ToolCall {
                call_id: tool_call.call_id.clone(),
                name: tool_call.name.clone(),
                arguments,
                raw_arguments,
            }
        }));
        self.remember_latest_visible_response(content, reasoning_content);
    }

    /// 记录本 step 的 provider 可见 assistant 消息（供 AfterProviderResponse 钩子使用）。
    /// 只保留"可见"消息：空文本 step 不应覆盖/残留上一 step 的响应。
    fn remember_latest_visible_response(
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

    /// 每个 agent step 开始时调用，避免上一 step 的响应泄漏到本 step 的
    /// `AfterProviderResponse` 钩子（历史 bug：不可见输出 step 会重复携带旧消息）。
    pub(crate) fn reset_latest_provider_response(&mut self) {
        self.latest_provider_response = None;
    }

    pub(crate) fn append_final_text(&mut self, text: &str) {
        self.output_text.push_str(text);
    }

    pub(crate) fn final_text(&self) -> &str {
        &self.output_text
    }

    pub(crate) fn set_final_text(&mut self, text: String) {
        self.output_text = text;
    }

    pub(crate) fn take_output_text(&mut self) -> String {
        std::mem::take(&mut self.output_text)
    }

    pub(crate) fn provider_response_messages(
        &self,
        mut request_messages: Vec<Arc<LlmMessage>>,
    ) -> Vec<Arc<LlmMessage>> {
        if let Some(message) = &self.latest_provider_response {
            request_messages.push(Arc::new(message.clone()));
        }
        provider_visible_shared_messages(request_messages)
    }
}

/// Mutable state carried across provider/tool iterations in a single turn.
pub(crate) struct TurnState {
    transcript: TurnTranscript,
    reactive_compact_used: bool,
    continue_after_stop_count: u32,
    /// 最近一个 step 的 provider `input_tokens` 与连续相同计数(frozen 视图检测)。
    last_step_input_tokens: Option<u64>,
    same_input_tokens_streak: u32,
    active_deferred_tools: HashSet<String>,
    all_tools: Vec<ToolSnapshot>,
    visible_tools: Vec<ToolSnapshot>,
    /// `visible_tools` 定义序列化估算;可见集变更时重算。
    tools_token_estimate: usize,
    tool_deduplicator: ToolCallDeduplicator,
    next_step_index: u32,
    resumed_attempt: Option<u32>,
}

impl TurnState {
    pub(crate) fn new(
        all_tools: Vec<crate::tool_registry::DefinitionWithPromptMetadata>,
        active_step: Option<&ActiveStepView>,
    ) -> Self {
        let all_tools = all_tools
            .into_iter()
            .map(|tool| ToolSnapshot {
                definition: tool.definition,
                prompt_metadata: tool.prompt_metadata,
            })
            .collect::<Vec<_>>();
        let active_deferred_tools = HashSet::new();
        let visible_tools = provider_visible_tools(&all_tools, &active_deferred_tools);
        let tools_token_estimate =
            astrcode_context::token_estimate::estimate_tool_definition_tokens(
                &ToolSnapshot::definitions(&visible_tools),
            );

        Self {
            transcript: TurnTranscript::default(),
            reactive_compact_used: false,
            continue_after_stop_count: 0,
            last_step_input_tokens: None,
            same_input_tokens_streak: 0,
            active_deferred_tools,
            all_tools,
            visible_tools,
            tools_token_estimate,
            tool_deduplicator: ToolCallDeduplicator::new(),
            next_step_index: active_step.map_or(0, |step| {
                if step.completed {
                    step.step_index.saturating_add(1)
                } else {
                    step.step_index
                }
            }),
            resumed_attempt: active_step
                .filter(|step| !step.completed)
                .map(|step| step.attempt.saturating_add(1)),
        }
    }

    /// 每个 agent step 开始时调用：清空同 step 去重状态并重置 provider 响应快照。
    pub(crate) fn begin_step(&mut self) -> (u32, u32) {
        self.transcript.reset_latest_provider_response();
        self.tool_deduplicator.begin_step();
        let step_index = self.next_step_index;
        let attempt = self.resumed_attempt.take().unwrap_or(1);
        self.next_step_index = self.next_step_index.saturating_add(1);
        (step_index, attempt)
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

    /// 记录本 step 的 provider `input_tokens`,返回连续相同计数(含本次)。
    pub(crate) fn record_step_input_tokens(&mut self, input_tokens: u64) -> u32 {
        if self.last_step_input_tokens == Some(input_tokens) {
            self.same_input_tokens_streak = self.same_input_tokens_streak.saturating_add(1);
        } else {
            self.last_step_input_tokens = Some(input_tokens);
            self.same_input_tokens_streak = 1;
        }
        self.same_input_tokens_streak
    }

    pub(crate) fn append_final_text(&mut self, text: &str) {
        self.transcript.append_final_text(text);
    }

    pub(crate) fn final_text(&self) -> &str {
        self.transcript.final_text()
    }

    pub(crate) fn set_final_text(&mut self, text: String) {
        self.transcript.set_final_text(text);
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
        request_messages: Vec<Arc<LlmMessage>>,
    ) -> Vec<Arc<LlmMessage>> {
        self.transcript.provider_response_messages(request_messages)
    }

    pub(crate) fn reactive_compact_used(&self) -> bool {
        self.reactive_compact_used
    }

    pub(crate) fn mark_reactive_compact_used(&mut self) {
        self.reactive_compact_used = true;
    }

    pub(crate) fn take_output_text(&mut self) -> String {
        self.transcript.take_output_text()
    }

    pub(crate) fn all_tool_snapshots(&self) -> &[ToolSnapshot] {
        &self.all_tools
    }

    pub(crate) fn visible_tools(&self) -> Vec<ToolDefinition> {
        ToolSnapshot::definitions(&self.visible_tools)
    }

    /// 当前可见工具集的定义 token 估算,随可见集变更重算。
    ///
    /// 逐工具 schema 序列化是估算里最贵的部分;可见集在一个 step 内不变。
    pub(crate) fn tools_token_estimate(&self) -> usize {
        self.tools_token_estimate
    }

    pub(crate) fn active_deferred_tools(&self) -> &HashSet<String> {
        &self.active_deferred_tools
    }

    pub(crate) fn activate_deferred_tools(&mut self, discovered_tools: Vec<String>) {
        let changed = activate_deferred_tools(
            &mut self.active_deferred_tools,
            &self.all_tools,
            discovered_tools,
        );
        if changed {
            self.visible_tools =
                provider_visible_tools(&self.all_tools, &self.active_deferred_tools);
            self.tools_token_estimate =
                astrcode_context::token_estimate::estimate_tool_definition_tokens(
                    &ToolSnapshot::definitions(&self.visible_tools),
                );
        }
    }
}

pub(crate) struct PreparedProviderRequest {
    pub(crate) llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    pub(crate) request_id: astrcode_extension_sdk::extension::ProviderRequestId,
    pub(crate) messages: Vec<Arc<astrcode_core::llm::LlmMessage>>,
    pub(crate) max_output_tokens: usize,
    pub(crate) acknowledgements:
        astrcode_extension_sdk::runtime_ports::ProviderRequestAcknowledgements,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_marks_unparseable_tool_arguments_as_raw() {
        let raw = r#"{"query":"unfinished"#;
        let mut transcript = TurnTranscript::default();
        transcript.record_assistant_tool_calls(
            "",
            None,
            &[StreamedToolCall {
                call_id: "call-bad".into(),
                name: "search".into(),
                arguments: raw.into(),
            }],
        );

        let message = transcript
            .latest_provider_response
            .as_ref()
            .expect("tool call should produce an assistant message");
        let LlmContent::ToolCall {
            arguments,
            raw_arguments,
            ..
        } = &message.content[0]
        else {
            panic!("expected tool call content");
        };
        assert_eq!(arguments, &serde_json::Value::String(raw.into()));
        assert_eq!(raw_arguments.as_deref(), Some(raw));
    }
}
