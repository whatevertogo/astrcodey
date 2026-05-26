//! Compaction 协调 — 统一 auto / reactive compact 的共享准备逻辑。

use std::sync::Arc;

use astrcode_context::context_assembler::{
    CompactIfNeededOutcome, CompactMessagesOptions, ContextPrepareInput,
};
use astrcode_core::{
    extension::{CompactStrategy, CompactTrigger},
    llm::LlmMessage,
    storage::SessionReadModel,
    types::TurnId,
};
use astrcode_extensions::runner::ExtensionRunner;

use crate::{
    compact::make_compact_request_fn,
    deferred_tools::append_deferred_tools_reminder,
    llm_request_history::visible_messages_for_assembler,
    turn_runner::{CompactionStageMeta, TurnRunner},
    turn_stages::TurnState,
};

/// 一次 compaction 请求的参数。
pub(crate) struct CompactionRequest {
    pub trigger: CompactTrigger,
    pub strategy: CompactStrategy,
    /// 是否执行 compact（阈值 + 配置，或 reactive force）。
    pub run_compact: bool,
    /// compact 时是否调用 LLM（断路器关闭时仍可做确定性 compact）。
    pub use_llm_for_compact: bool,
    pub force_compact: bool,
    pub base_event_seq: u64,
    pub keep_recent_turns: Option<usize>,
}

/// context assembler 输出，含是否实际执行了 compaction。
pub(crate) struct PreparedContextMessages {
    pub context_messages: Vec<LlmMessage>,
    pub compaction_applied: bool,
}

impl TurnRunner {
    /// 调用 context assembler 准备消息，并在需要时执行 compaction 持久化。
    pub(crate) async fn prepare_context_messages(
        &mut self,
        extension_runner: &ExtensionRunner,
        state: &TurnState,
        model: &SessionReadModel,
        turn_id: &TurnId,
        request: CompactionRequest,
    ) -> Result<PreparedContextMessages, crate::turn_context::TurnError> {
        let llm = Arc::clone(self.llm());
        let context_assembler = Arc::clone(self.session().caps().context_assembler());
        let custom_instructions = self
            .compact_instructions(extension_runner, model, request.trigger)
            .await;
        let messages_before_compact = visible_messages_for_assembler(model);
        let input = ContextPrepareInput {
            messages: messages_before_compact.clone(),
            system_prompt: Some(self.system_prompt()),
            model_limits: llm.model_limits(),
            custom_instructions: custom_instructions.clone(),
        };
        let request_fn = make_compact_request_fn(Arc::clone(&llm));
        let keep_recent_turns = request
            .keep_recent_turns
            .or_else(|| context_assembler.settings().compact_keep_recent_turns);
        let render_options = astrcode_context::compaction::CompactSummaryRenderOptions {
            custom_instructions: custom_instructions.clone(),
            ..Default::default()
        };

        let compact_outcome = context_assembler
            .compact_if_needed(
                input.messages,
                input.system_prompt,
                &custom_instructions,
                render_options,
                CompactMessagesOptions {
                    run: request.force_compact || request.run_compact,
                    use_llm: request.use_llm_for_compact,
                    keep_recent_turns,
                },
                request_fn,
            )
            .await;

        let (mut context_messages, compaction_applied) = match compact_outcome {
            CompactIfNeededOutcome::NotRun { messages }
            | CompactIfNeededOutcome::Skipped { messages } => (messages, false),
            CompactIfNeededOutcome::Applied {
                messages,
                compaction,
            } => {
                let mut compaction_result = compaction.result;
                let persisted = self
                    .handle_compaction_stage(
                        extension_runner,
                        state,
                        model,
                        &mut compaction_result,
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
                    (messages, true)
                } else {
                    tracing::warn!(
                        trigger = ?request.trigger,
                        "compaction generated but persist skipped; using pre-compact messages"
                    );
                    (messages_before_compact, false)
                }
            },
        };

        append_deferred_tools_reminder(
            &mut context_messages,
            state.all_tool_snapshots(),
            state.active_deferred_tools(),
        );

        Ok(PreparedContextMessages {
            context_messages,
            compaction_applied,
        })
    }
}
