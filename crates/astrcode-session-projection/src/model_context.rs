//! Provider 可见上下文、system prompt、usage 与 compact rewrite 投影。

use std::collections::HashSet;

use astrcode_core::{
    compaction::CompactStrategy,
    event::{
        DurableEvent, DurableEventPayload, StoredEvent, SystemPromptSource,
        TranscriptRewriteReason, transcript_prefix_fingerprint,
    },
    llm::{
        LlmContent, LlmMessage, LlmRole, TURN_ABORTED_SOURCE, provider_transcript,
        turn_aborted_context_message,
    },
    types::ToolCallId,
};
use serde::{Deserialize, Serialize};

use crate::ProjectionError;

/// 一次 compaction 的投影元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionView {
    pub trigger: String,
    pub pre_tokens: usize,
    pub post_tokens: usize,
    pub summary: String,
    pub transcript_path: Option<String>,
    pub seq: u64,
    pub source_seq: u64,
    pub strategy: CompactStrategy,
}

/// 会话读模型里带有 durable seq 的 provider 消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SequencedLlmMessage {
    pub message: LlmMessage,
    /// 普通消息记录最近更新它的事件 seq；rewrite 输出锚定到其 source seq。
    pub updated_seq: u64,
    /// provider 不可见的消息来源标记。
    #[serde(default)]
    pub source: Option<String>,
}

impl SequencedLlmMessage {
    pub(crate) fn plain(message: LlmMessage, updated_seq: u64) -> Self {
        Self {
            message,
            updated_seq,
            source: None,
        }
    }
}

pub const TOOL_CALL_FAILED_SOURCE: &str = "tool_call_failed";
pub const TOOL_CALL_CANCELLED_SOURCE: &str = "tool_call_cancelled";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredToolCall {
    pub call_id: String,
    pub tool_name: String,
}

/// Provider 用量覆盖的 model-context 前缀。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextUsageView {
    pub context_tokens: usize,
    pub model_context_window: usize,
    pub covered_message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSystemPrompt {
    pub text: String,
    pub extra: Option<String>,
    pub fingerprint: String,
    pub source: SystemPromptSource,
}

/// 当前发送给 provider 的有序上下文及其压缩、用量元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionModelContext {
    pub messages: Vec<SequencedLlmMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ContextUsageView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compactions: Vec<CompactionView>,
}

pub(crate) struct ModelContextValidationState {
    system_prompt: String,
    messages: Vec<SequencedLlmMessage>,
}

impl ModelContextValidationState {
    pub(crate) fn new(system_prompt: &SessionSystemPrompt, context: &SessionModelContext) -> Self {
        Self {
            system_prompt: system_prompt.text.clone(),
            messages: context.messages.clone(),
        }
    }

    pub(crate) fn validate_and_apply(
        &mut self,
        event: &StoredEvent,
    ) -> Result<(), ProjectionError> {
        validate_rewrite_fingerprint_against(&event.event, &self.system_prompt, &self.messages)?;
        apply_provider_event(event, &mut self.system_prompt, &mut self.messages);
        Ok(())
    }
}

pub(crate) fn validate_rewrite_fingerprint(
    event: &DurableEvent,
    system_prompt: &SessionSystemPrompt,
    context: &SessionModelContext,
) -> Result<(), ProjectionError> {
    validate_rewrite_fingerprint_against(event, &system_prompt.text, &context.messages)
}

fn validate_rewrite_fingerprint_against(
    event: &DurableEvent,
    system_prompt: &str,
    messages: &[SequencedLlmMessage],
) -> Result<(), ProjectionError> {
    let DurableEventPayload::TranscriptRewritten {
        source_seq,
        source_fingerprint: expected,
        ..
    } = &event.payload
    else {
        return Ok(());
    };
    let prefix = provider_transcript(
        messages
            .iter()
            .filter(|message| message.updated_seq <= *source_seq)
            .map(|message| message.message.clone())
            .collect(),
    );
    let actual = transcript_prefix_fingerprint(system_prompt, &prefix);
    if actual == *expected {
        return Ok(());
    }
    Err(
        ProjectionError::TranscriptRewriteSourceFingerprintMismatch {
            source_seq: *source_seq,
            expected: expected.clone(),
            actual,
        },
    )
}

pub(crate) fn apply_event(
    event: &StoredEvent,
    system_prompt: &mut SessionSystemPrompt,
    context: &mut SessionModelContext,
) {
    apply_provider_event(event, &mut system_prompt.text, &mut context.messages);

    match &event.payload {
        DurableEventPayload::ModelIdChanged { .. }
        | DurableEventPayload::SessionToolsConfigured { .. }
        | DurableEventPayload::SessionForked { .. } => context.usage = None,
        DurableEventPayload::SystemPromptConfigured {
            fingerprint,
            extra_system_prompt,
            source,
            ..
        } => {
            system_prompt.extra = extra_system_prompt.clone();
            system_prompt.fingerprint = fingerprint.clone();
            system_prompt.source = *source;
            context.usage = None;
        },
        DurableEventPayload::TranscriptRewritten {
            source_seq, reason, ..
        } => {
            context.usage = None;
            match reason {
                TranscriptRewriteReason::Compaction(details) => {
                    context.compactions.push(CompactionView {
                        trigger: details.trigger.clone(),
                        pre_tokens: details.pre_tokens,
                        post_tokens: details.post_tokens,
                        summary: details.summary.clone(),
                        transcript_path: details.transcript_path.clone(),
                        seq: event.seq,
                        source_seq: *source_seq,
                        strategy: details.strategy,
                    });
                },
            }
        },
        DurableEventPayload::TokenUsageRecorded {
            usage,
            model_context_window,
        } => {
            if let Some(context_tokens) = usage
                .context_tokens_after_response()
                .and_then(|tokens| usize::try_from(tokens).ok())
            {
                context.usage = Some(ContextUsageView {
                    context_tokens,
                    model_context_window: *model_context_window,
                    covered_message_count: context.messages.len(),
                });
            }
        },
        _ => {},
    }
}

fn apply_provider_event(
    event: &StoredEvent,
    system_prompt: &mut String,
    messages: &mut Vec<SequencedLlmMessage>,
) {
    let event_seq = event.seq;
    match &event.payload {
        DurableEventPayload::SystemPromptConfigured { text, .. } => {
            text.clone_into(system_prompt);
        },
        DurableEventPayload::UserMessage {
            text, attachments, ..
        } => messages.push(SequencedLlmMessage::plain(
            LlmMessage::user_with_attachments(text, attachments),
            event_seq,
        )),
        DurableEventPayload::TurnAbortedContext => messages.push(SequencedLlmMessage {
            message: turn_aborted_context_message(),
            updated_seq: event_seq,
            source: Some(TURN_ABORTED_SOURCE.into()),
        }),
        DurableEventPayload::AssistantMessageCompleted {
            text,
            reasoning_content,
            ..
        } => {
            let mut message = LlmMessage::assistant(text);
            message.reasoning_content = reasoning_content.clone();
            messages.push(SequencedLlmMessage::plain(message, event_seq));
        },
        DurableEventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
            raw_arguments,
        } => {
            let tool_call = LlmContent::ToolCall {
                call_id: call_id.to_string(),
                name: tool_name.clone(),
                arguments: arguments.clone(),
                raw_arguments: raw_arguments.clone(),
            };
            match messages.last_mut() {
                Some(last) if last.message.role == LlmRole::Assistant => {
                    last.message.content.push(tool_call);
                    last.updated_seq = event_seq;
                },
                _ => messages.push(SequencedLlmMessage::plain(
                    LlmMessage {
                        role: LlmRole::Assistant,
                        content: vec![tool_call],
                        name: None,
                        reasoning_content: None,
                    },
                    event_seq,
                )),
            }
        },
        DurableEventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
            ..
        } => apply_tool_terminal(
            messages,
            call_id,
            tool_name,
            result.content.clone(),
            result.is_error,
            None,
            event_seq,
        ),
        DurableEventPayload::ToolCallFailed {
            call_id,
            tool_name,
            error,
            ..
        } => apply_tool_terminal(
            messages,
            call_id,
            tool_name,
            error.clone(),
            true,
            Some(TOOL_CALL_FAILED_SOURCE),
            event_seq,
        ),
        DurableEventPayload::ToolCallCancelled {
            call_id,
            tool_name,
            reason,
            ..
        } => apply_tool_terminal(
            messages,
            call_id,
            tool_name,
            format!("Tool cancelled: {reason}"),
            true,
            Some(TOOL_CALL_CANCELLED_SOURCE),
            event_seq,
        ),
        DurableEventPayload::TranscriptRewritten {
            source_seq,
            messages: rewritten,
            ..
        } => apply_transcript_rewrite(messages, rewritten, *source_seq),
        DurableEventPayload::SessionForked {
            messages: forked, ..
        } => {
            *messages = forked
                .iter()
                .cloned()
                .map(|message| SequencedLlmMessage::plain(message, event_seq))
                .collect();
        },
        _ => {},
    }
}

fn apply_transcript_rewrite(
    current: &mut Vec<SequencedLlmMessage>,
    rewritten: &[LlmMessage],
    source_seq: u64,
) {
    let tail = current
        .iter()
        .filter(|message| message.updated_seq > source_seq)
        .cloned();
    *current = rewritten
        .iter()
        .cloned()
        .map(|message| SequencedLlmMessage {
            message,
            updated_seq: source_seq,
            source: None,
        })
        .chain(tail)
        .collect();
}

fn apply_tool_terminal(
    messages: &mut Vec<SequencedLlmMessage>,
    call_id: &ToolCallId,
    tool_name: &str,
    content: String,
    is_error: bool,
    source: Option<&str>,
    event_seq: u64,
) {
    messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: call_id.to_string(),
                content,
                is_error,
            }],
            name: Some(tool_name.to_owned()),
            reasoning_content: None,
        },
        updated_seq: event_seq,
        source: source.map(str::to_owned),
    });
}

impl SessionModelContext {
    pub(crate) fn tool_calls_needing_interruption(
        &self,
        pending_tool_calls: &HashSet<ToolCallId>,
    ) -> Vec<UnansweredToolCall> {
        let mut pending = self.pending_requested_tool_calls(pending_tool_calls);
        let mut seen = pending
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<HashSet<_>>();

        for call in self.tail_unanswered_tool_calls() {
            if seen.insert(call.call_id.clone()) {
                pending.push(call);
            }
        }
        pending
    }

    fn pending_requested_tool_calls(
        &self,
        pending_tool_calls: &HashSet<ToolCallId>,
    ) -> Vec<UnansweredToolCall> {
        let mut seen = HashSet::new();
        let mut pending = Vec::new();
        for message in &self.messages {
            if message.message.role != LlmRole::Assistant {
                continue;
            }
            for content in &message.message.content {
                let LlmContent::ToolCall { call_id, name, .. } = content else {
                    continue;
                };
                if pending_tool_calls.contains(call_id.as_str()) && seen.insert(call_id.as_str()) {
                    pending.push(UnansweredToolCall {
                        call_id: call_id.clone(),
                        tool_name: name.clone(),
                    });
                }
            }
        }
        pending
    }

    fn tail_unanswered_tool_calls(&self) -> Vec<UnansweredToolCall> {
        let Some(last_assistant_index) = self.messages.iter().rposition(|message| {
            message.message.role == LlmRole::Assistant
                && message
                    .message
                    .content
                    .iter()
                    .any(|content| matches!(content, LlmContent::ToolCall { .. }))
        }) else {
            return Vec::new();
        };
        let assistant = &self.messages[last_assistant_index].message;
        let mut answered = HashSet::new();
        for message in self.messages.iter().skip(last_assistant_index + 1) {
            if message.message.role != LlmRole::Tool {
                return Vec::new();
            }
            for content in &message.message.content {
                if let LlmContent::ToolResult { tool_call_id, .. } = content {
                    answered.insert(tool_call_id.as_str());
                }
            }
        }
        let mut seen = HashSet::new();
        assistant
            .content
            .iter()
            .filter_map(|content| match content {
                LlmContent::ToolCall { call_id, name, .. }
                    if !answered.contains(call_id.as_str()) && seen.insert(call_id.as_str()) =>
                {
                    Some(UnansweredToolCall {
                        call_id: call_id.clone(),
                        tool_name: name.clone(),
                    })
                },
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{DurableEvent, DurableEventPayload, StoredEvent, SystemPromptSource},
        llm::LlmTokenUsage,
        types::{SessionId, new_message_id},
    };

    use super::{SessionModelContext, SessionSystemPrompt, apply_event};

    #[test]
    fn usage_remains_anchored_to_covered_messages_until_context_identity_changes() {
        let session_id = SessionId::new("session-context-usage");
        let mut system_prompt = SessionSystemPrompt {
            text: "system".into(),
            extra: None,
            fingerprint: "fingerprint".into(),
            source: SystemPromptSource::Native,
        };
        let mut context = SessionModelContext::default();
        let stored = |seq, payload| {
            StoredEvent::new(seq, DurableEvent::session(session_id.clone(), payload))
        };

        apply_event(
            &stored(
                1,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: "first".into(),
                    attachments: vec![],
                    accepted_seq: None,
                },
            ),
            &mut system_prompt,
            &mut context,
        );
        apply_event(
            &stored(
                2,
                DurableEventPayload::TokenUsageRecorded {
                    usage: LlmTokenUsage {
                        total_tokens: Some(655_859),
                        ..Default::default()
                    },
                    model_context_window: 1_000_000,
                },
            ),
            &mut system_prompt,
            &mut context,
        );
        assert_eq!(
            context.usage.as_ref().map(|usage| (
                usage.context_tokens,
                usage.model_context_window,
                usage.covered_message_count,
            )),
            Some((655_859, 1_000_000, 1))
        );

        apply_event(
            &stored(
                3,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: "tail".into(),
                    attachments: vec![],
                    accepted_seq: None,
                },
            ),
            &mut system_prompt,
            &mut context,
        );
        assert_eq!(
            context
                .usage
                .as_ref()
                .map(|usage| usage.covered_message_count),
            Some(1)
        );

        apply_event(
            &stored(
                4,
                DurableEventPayload::ModelIdChanged {
                    model_id: "model-b".into(),
                },
            ),
            &mut system_prompt,
            &mut context,
        );
        assert!(context.usage.is_none());
    }
}
