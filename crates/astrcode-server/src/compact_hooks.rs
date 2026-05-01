use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    config::ModelSelection,
    extension::{
        CompactTrigger, ExtensionError, ExtensionEvent, PostCompactInput, PreCompactInput,
    },
    tool::ToolDefinition,
};
use astrcode_extensions::{context::ServerExtensionContext, runner::ExtensionRunner};

pub(crate) struct CompactHookContext<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) working_dir: &'a str,
    pub(crate) model_id: &'a str,
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) trigger: CompactTrigger,
    pub(crate) message_count: usize,
}

pub(crate) async fn collect_compact_instructions(
    extension_runner: &ExtensionRunner,
    input: CompactHookContext<'_>,
) -> Result<Vec<String>, ExtensionError> {
    let mut ctx = compact_extension_context(&input);
    ctx.set_pre_compact_input(PreCompactInput {
        trigger: input.trigger,
        message_count: input.message_count,
    });

    let contributions = extension_runner.collect_compact_contributions(&ctx).await?;
    Ok(clean_compact_instructions(contributions.instructions))
}

pub(crate) async fn dispatch_post_compact(
    extension_runner: &ExtensionRunner,
    input: CompactHookContext<'_>,
    compaction: &CompactResult,
) -> Result<(), ExtensionError> {
    let mut ctx = compact_extension_context(&input);
    ctx.set_post_compact_input(PostCompactInput {
        trigger: input.trigger,
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        messages_removed: compaction.messages_removed,
        summary: compaction.summary.clone(),
    });
    extension_runner
        .dispatch(ExtensionEvent::PostCompact, &ctx)
        .await
}

pub(crate) fn compact_trigger_name(trigger: CompactTrigger) -> &'static str {
    match trigger {
        CompactTrigger::AutoThreshold => "auto_threshold",
        CompactTrigger::PromptTooLongRetry => "prompt_too_long_retry",
        CompactTrigger::ManualCommand => "manual_command",
    }
}

fn compact_extension_context(input: &CompactHookContext<'_>) -> ServerExtensionContext {
    let mut ctx = ServerExtensionContext::new(
        input.session_id.to_string(),
        input.working_dir.to_string(),
        ModelSelection {
            profile_name: String::new(),
            model: input.model_id.to_string(),
            provider_kind: String::new(),
        },
    );
    ctx.set_tools(
        input
            .tools
            .iter()
            .cloned()
            .map(|tool| (tool.name.clone(), tool))
            .collect(),
    );
    ctx
}

fn clean_compact_instructions(instructions: Vec<String>) -> Vec<String> {
    instructions
        .into_iter()
        .map(|instruction| instruction.trim().to_string())
        .filter(|instruction| !instruction.is_empty())
        .collect()
}
