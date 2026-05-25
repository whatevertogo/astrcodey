//! Compaction 协调 — 统一 auto / reactive compact 的共享准备逻辑。

use std::sync::Arc;

use astrcode_context::context_assembler::{ContextPrepareInput, PrepareMessagesOptions};
use astrcode_core::{
    extension::{CompactStrategy, CompactTrigger},
    llm::LlmMessage,
    types::TurnId,
};
use astrcode_extensions::runner::ExtensionRunner;

use crate::{
    compact::make_compact_request_fn,
    deferred_tools::append_deferred_tools_reminder,
    turn_runner::{CompactionStageMeta, TurnRunner},
    turn_stages::TurnState,
};

/// 一次 compaction 请求的参数。
pub(crate) struct CompactionRequest {
    pub trigger: CompactTrigger,
    pub strategy: CompactStrategy,
    pub allow_auto_compact: bool,
    pub force_compact: bool,
    pub base_event_seq: u64,
}

/// context assembler 输出，含是否实际执行了 compaction。
pub(crate) struct PreparedContextMessages {
    pub system_messages: Vec<LlmMessage>,
    pub context_messages: Vec<LlmMessage>,
    pub compaction_applied: bool,
}

impl TurnRunner {
    /// 调用 context assembler 准备消息，并在需要时执行 compaction 持久化。
    pub(crate) async fn prepare_context_messages(
        &mut self,
        extension_runner: &ExtensionRunner,
        state: &mut TurnState,
        turn_id: &TurnId,
        request: CompactionRequest,
    ) -> Result<PreparedContextMessages, crate::turn_context::TurnError> {
        let llm = Arc::clone(self.llm());
        let context_assembler = Arc::clone(self.session().caps().context_assembler());
        let custom_instructions = self
            .compact_instructions(extension_runner, state, request.trigger)
            .await;
        let (system_messages, visible_messages) = split_system_messages(state);
        let input = ContextPrepareInput {
            messages: visible_messages,
            system_prompt: Some(self.system_prompt()),
            model_limits: llm.model_limits(),
            custom_instructions,
        };
        let request_fn = make_compact_request_fn(Arc::clone(&llm));
        let mut prepared = context_assembler
            .prepare_messages_with_llm(
                input,
                PrepareMessagesOptions {
                    allow_auto_compact: request.allow_auto_compact,
                    force_compact: request.force_compact,
                    keep_recent_turns: context_assembler.settings().compact_keep_recent_turns,
                },
                request_fn,
            )
            .await;

        let mut compaction_applied = false;
        if let Some(ref mut compaction) = prepared.compaction {
            let persisted = self
                .handle_compaction_stage(
                    extension_runner,
                    state,
                    &mut compaction.result,
                    context_assembler.settings(),
                    turn_id,
                    CompactionStageMeta {
                        base_event_seq: request.base_event_seq,
                        trigger: request.trigger,
                        strategy: request.strategy,
                        llm_api_failed: compaction.llm_api_failed,
                    },
                )
                .await;
            if persisted {
                state.replace_messages(
                    [system_messages.clone(), prepared.messages.clone()].concat(),
                );
                compaction_applied = true;
            }
        }

        let mut context_messages = prepared.messages;
        append_deferred_tools_reminder(
            &mut context_messages,
            state.all_tool_snapshots(),
            state.active_deferred_tools(),
        );

        Ok(PreparedContextMessages {
            system_messages,
            context_messages,
            compaction_applied,
        })
    }
}

pub(crate) fn split_system_messages(state: &TurnState) -> (Vec<LlmMessage>, Vec<LlmMessage>) {
    state
        .messages()
        .iter()
        .cloned()
        .partition(|message| message.role == astrcode_core::llm::LlmRole::System)
}
