//! Host-only construction and mutation seam for author-facing extension contexts.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use astrcode_core::{
    event::{EventDeliveryReceipt, EventSendError},
    message_attachment::MessageAttachment,
    types::{EventId, SessionId, ToolCallId},
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{
    CommandCompletionContext, CommandContext, CommandDiscoveryContext, CompactPayload,
    ContinueAfterStopPayload, CustomEventContext, CustomEventDeclaration, CustomEventEmitter,
    ExchangeSummary, ExtensionCallContext, ExtensionConfig, ExtensionHttpRequest,
    ExtensionHttpRoute, ExtensionPaths, ExtensionStartContext, ExtensionStopContext,
    ExtensionTasks, HookContext, HttpContext, LifecyclePayload, PostToolUsePayload,
    PreToolUsePayload, PromptBuildPayload, ProviderContributionId, ProviderPayload,
    ProviderRequestId, ProviderSettlementPayload, StopReason, ToolContext, ToolDiscoveryContext,
    ToolPlanContext, UserMessageEnvelopePayload,
};
pub use super::{
    hooks::{
        HookInput, RuntimeCompactContext, RuntimeContinueAfterStopContext, RuntimeHookCallContext,
        RuntimeLifecycleContext, RuntimePostToolUseContext, RuntimePreToolUseContext,
        RuntimePromptBuildContext, RuntimeProviderContext, RuntimeProviderSettlementContext,
        RuntimeUserMessageEnvelopeContext,
    },
    registration_validation::{
        canonical_registration_name, custom_event_subscription_matches,
        extension_http_route_patterns_conflict, fixed_hook_mode, has_duplicate_registration_name,
        hook_mode_is_supported, match_extension_http_route, normalize_custom_event_subscription,
        validate_custom_event_subscription, validate_extension_http_route,
    },
};
use crate::{
    config::ModelSelection,
    host::ExtensionHost,
    llm::LlmMessage,
    permission::ApprovalMode,
    tool::{ToolDefinition, ToolResult},
};

/// Host-bound event ingress. Extension authors emit through [`CustomEventEmitter`].
#[async_trait]
pub trait CustomEventSink: Send + Sync {
    async fn emit(
        &self,
        event_type: &str,
        schema_version: u32,
        durable: bool,
        payload: serde_json::Value,
    ) -> Result<EventDeliveryReceipt, EventSendError>;

    fn try_emit(
        &self,
        event_type: &str,
        schema_version: u32,
        durable: bool,
        payload: serde_json::Value,
    ) -> Result<(), EventSendError>;
}

pub fn extension_paths(
    extension_id: &str,
    global_store_dir: Option<&Path>,
    session_store_dir: Option<&Path>,
) -> ExtensionPaths {
    ExtensionPaths::from_runtime(extension_id, global_store_dir, session_store_dir)
}

pub fn extension_config(
    extension_id: impl Into<String>,
    value: serde_json::Value,
) -> ExtensionConfig {
    ExtensionConfig::from_runtime(extension_id, value)
}

pub fn extension_config_value(config: &ExtensionConfig) -> &serde_json::Value {
    config.value()
}

pub const fn extension_stop_context(reason: StopReason) -> ExtensionStopContext {
    ExtensionStopContext::from_runtime(reason)
}

pub fn extension_tasks(extension_id: impl Into<String>) -> ExtensionTasks {
    ExtensionTasks::new(extension_id)
}

pub fn suspended_extension_tasks(extension_id: impl Into<String>) -> ExtensionTasks {
    ExtensionTasks::new_suspended(extension_id)
}

pub fn activate_extension_tasks(tasks: &ExtensionTasks) {
    tasks.activate();
}

pub fn cancel_extension_tasks(tasks: &ExtensionTasks) {
    tasks.cancel();
}

pub async fn wait_extension_tasks(tasks: &ExtensionTasks, timeout: Duration) -> bool {
    tasks.wait(timeout).await
}

pub fn extension_call_context(
    extension_id: impl Into<String>,
    paths: ExtensionPaths,
    host: ExtensionHost,
    events: CustomEventEmitter,
    tasks: ExtensionTasks,
    cancellation: CancellationToken,
) -> ExtensionCallContext {
    ExtensionCallContext::from_runtime(extension_id, paths, host, events, tasks, cancellation)
}

pub fn retain_call_cancellation(context: ExtensionCallContext) -> ExtensionCallContext {
    context.retain_cancellation_after_context_drop()
}

pub fn extension_start_context(
    call: ExtensionCallContext,
    config: ExtensionConfig,
    startup_working_dir: Option<PathBuf>,
) -> ExtensionStartContext {
    ExtensionStartContext::from_runtime(call, config, startup_working_dir)
}

pub fn custom_event_emitter(
    declarations: impl IntoIterator<Item = CustomEventDeclaration>,
    sink: Option<Arc<dyn CustomEventSink>>,
) -> CustomEventEmitter {
    CustomEventEmitter::from_runtime(declarations, sink)
}

#[allow(clippy::too_many_arguments)]
pub fn custom_event_context(
    call: ExtensionCallContext,
    session_id: SessionId,
    turn_id: Option<String>,
    event_id: EventId,
    seq: Option<u64>,
    source_extension_id: String,
    event_type: String,
    schema_version: u32,
    causation_id: Option<EventId>,
    cascade_depth: u8,
    payload: serde_json::Value,
) -> CustomEventContext {
    CustomEventContext::from_runtime(
        call,
        session_id,
        turn_id,
        event_id,
        seq,
        source_extension_id,
        event_type,
        schema_version,
        causation_id,
        cascade_depth,
        payload,
    )
}

pub fn http_context(
    call: ExtensionCallContext,
    route: ExtensionHttpRoute,
    request: ExtensionHttpRequest,
    caller_extension_id: Option<String>,
) -> HttpContext {
    HttpContext::from_runtime(call, route, request, caller_extension_id)
}

pub fn author_hook_context<P>(call: ExtensionCallContext, input: &HookInput<P>) -> HookContext<P>
where
    P: Clone,
{
    HookContext::from_runtime(call, input)
}

pub fn runtime_continue_after_stop_context(
    call: RuntimeHookCallContext,
    assistant_text: impl Into<String>,
    finish_reason: impl Into<String>,
    continuations_this_turn: u32,
) -> RuntimeContinueAfterStopContext {
    HookInput::new(
        call,
        ContinueAfterStopPayload::new(assistant_text, finish_reason, continuations_this_turn),
    )
}

pub fn runtime_user_message_envelope_context(
    call: RuntimeHookCallContext,
    text: impl Into<String>,
    attachments: Vec<MessageAttachment>,
) -> RuntimeUserMessageEnvelopeContext {
    HookInput::new(call, UserMessageEnvelopePayload::new(text, attachments))
}

pub fn replace_user_message_text(context: &mut RuntimeUserMessageEnvelopeContext, text: String) {
    context.replace_text(text);
}

pub fn append_user_message_text(context: &mut RuntimeUserMessageEnvelopeContext, text: &str) {
    context.append_text(text);
}

pub fn runtime_pre_tool_use_context(
    call: RuntimeHookCallContext,
    call_id: ToolCallId,
    tool_name: impl Into<String>,
    tool_input: serde_json::Value,
    approval_mode: ApprovalMode,
    available_tools: Vec<ToolDefinition>,
) -> RuntimePreToolUseContext {
    HookInput::new(
        call,
        PreToolUsePayload::new(
            call_id,
            tool_name,
            tool_input,
            approval_mode,
            available_tools,
        ),
    )
}

pub fn replace_pre_tool_input(
    context: &mut RuntimePreToolUseContext,
    tool_input: serde_json::Value,
) {
    context.replace_tool_input(tool_input);
}

pub fn runtime_post_tool_use_context(
    call: RuntimeHookCallContext,
    call_id: ToolCallId,
    tool_name: impl Into<String>,
    tool_input: serde_json::Value,
    tool_result: ToolResult,
) -> RuntimePostToolUseContext {
    HookInput::new(
        call,
        PostToolUsePayload::new(call_id, tool_name, tool_input, tool_result),
    )
}

pub fn replace_post_tool_result(context: &mut RuntimePostToolUseContext, content: String) {
    context.replace_result_content(content);
}

pub fn runtime_provider_context(
    call: RuntimeHookCallContext,
    request_id: ProviderRequestId,
    messages: Vec<LlmMessage>,
) -> RuntimeProviderContext {
    HookInput::new(call, ProviderPayload::new(request_id, messages))
}

pub fn runtime_provider_settlement_context(
    call: RuntimeHookCallContext,
    request_id: ProviderRequestId,
) -> RuntimeProviderSettlementContext {
    RuntimeProviderSettlementContext::new(call, request_id)
}

pub fn author_provider_settlement_context(
    call: ExtensionCallContext,
    runtime: &RuntimeProviderSettlementContext,
    contribution_id: ProviderContributionId,
) -> super::ProviderSettlementContext {
    let input = HookInput::new(
        runtime.call().clone(),
        ProviderSettlementPayload::new(runtime.request_id().clone(), contribution_id),
    );
    HookContext::from_runtime(call, &input)
}

pub fn replace_provider_messages(context: &mut RuntimeProviderContext, messages: Vec<LlmMessage>) {
    context.replace_messages(messages);
}

pub fn append_provider_messages(context: &mut RuntimeProviderContext, messages: Vec<LlmMessage>) {
    context.append_messages(messages);
}

pub fn runtime_prompt_build_context(
    call: RuntimeHookCallContext,
    tools: Vec<ToolDefinition>,
) -> RuntimePromptBuildContext {
    HookInput::new(call, PromptBuildPayload::new(tools))
}

pub fn runtime_compact_context(
    call: RuntimeHookCallContext,
    trigger: astrcode_core::compaction::CompactTrigger,
    message_count: usize,
    pre_tokens: Option<usize>,
    post_tokens: Option<usize>,
    summary: Option<String>,
) -> RuntimeCompactContext {
    HookInput::new(
        call,
        CompactPayload::new(trigger, message_count, pre_tokens, post_tokens, summary),
    )
}

pub fn runtime_lifecycle_context(
    call: RuntimeHookCallContext,
    last_exchange: Option<ExchangeSummary>,
    mid_turn_user_messages_synced: u32,
) -> RuntimeLifecycleContext {
    HookInput::new(
        call,
        LifecyclePayload::new(last_exchange).for_step_start(mid_turn_user_messages_synced),
    )
}

pub fn runtime_lifecycle_for_step_start(
    context: RuntimeLifecycleContext,
    mid_turn_user_messages_synced: u32,
) -> RuntimeLifecycleContext {
    context.map_payload(|payload| payload.for_step_start(mid_turn_user_messages_synced))
}

pub fn tool_discovery_context(
    call: ExtensionCallContext,
    working_dir: impl Into<PathBuf>,
    generation: u64,
) -> ToolDiscoveryContext {
    ToolDiscoveryContext::from_runtime(call, working_dir, generation)
}

pub fn command_discovery_context(
    call: ExtensionCallContext,
    working_dir: impl Into<PathBuf>,
    generation: u64,
) -> CommandDiscoveryContext {
    CommandDiscoveryContext::from_runtime(call, working_dir, generation)
}

pub fn command_context(
    call: ExtensionCallContext,
    session_id: SessionId,
    turn_id: Option<String>,
    working_dir: PathBuf,
    model: ModelSelection,
    command_name: impl Into<String>,
    argument: impl Into<String>,
) -> CommandContext {
    CommandContext::from_runtime(
        super::SessionCallContext::from_runtime(call, session_id, turn_id),
        working_dir,
        model,
        command_name,
        argument,
    )
}

pub fn command_completion_context(
    command: CommandContext,
    cursor: usize,
) -> CommandCompletionContext {
    CommandCompletionContext::for_runtime(command, cursor)
}

#[allow(clippy::too_many_arguments)]
pub fn tool_context(
    call: ExtensionCallContext,
    session_id: SessionId,
    turn_id: Option<String>,
    working_dir: PathBuf,
    tool_name: impl Into<String>,
    call_id: Option<String>,
    arguments: serde_json::Value,
    main_model_id: Option<String>,
    small_model_id: Option<String>,
    available_tools: Vec<ToolDefinition>,
) -> ToolContext {
    ToolContext::from_runtime(
        super::SessionCallContext::from_runtime(call, session_id, turn_id),
        working_dir,
        tool_name,
        call_id,
        arguments,
        main_model_id,
        small_model_id,
        available_tools,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn tool_plan_context(
    extension_id: impl Into<String>,
    session_id: SessionId,
    turn_id: Option<String>,
    working_dir: PathBuf,
    tool_name: impl Into<String>,
    call_id: Option<String>,
    arguments: serde_json::Value,
    cancellation: CancellationToken,
) -> ToolPlanContext {
    ToolPlanContext::from_runtime(
        extension_id,
        session_id,
        working_dir,
        tool_name,
        arguments,
        cancellation,
    )
    .with_turn_id(turn_id)
    .with_call_id(call_id)
}
