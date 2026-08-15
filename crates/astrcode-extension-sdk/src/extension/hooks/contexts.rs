//! Host-constructed contexts for extension hook handlers.

use std::{
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::{
    compaction::CompactTrigger,
    llm::LlmProviderBindings,
    message_attachment::MessageAttachment,
    types::{SessionId, ToolCallId},
};
use tokio_util::sync::CancellationToken;

use super::types::{ExchangeSummary, ProviderContributionId, ProviderRequestId};
use crate::{
    config::ModelSelection,
    extension::{ExtensionCall, ExtensionCallContext, SessionCallContext, WorkspaceCallContext},
    tool::{ToolDefinition, ToolResult},
};

/// Runtime-only facts supplied to a turn hook dispatcher before an extension is selected.
///
/// This type is public only because the session runtime and extension runner are separate crates.
/// Extension handlers receive an attributed specialized context instead.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeHookCallContext {
    session_id: SessionId,
    turn_id: Option<String>,
    working_dir: PathBuf,
    model: ModelSelection,
    session_store_dir: Option<PathBuf>,
    event_tx: Option<crate::event::EventSender>,
    llm_providers: Option<LlmProviderBindings>,
    cancellation: CancellationToken,
}

impl RuntimeHookCallContext {
    pub fn new(
        session_id: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        model: ModelSelection,
        session_store_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            session_id: SessionId::new(session_id),
            turn_id: None,
            working_dir: working_dir.into(),
            model,
            session_store_dir,
            event_tx: None,
            llm_providers: None,
            cancellation: CancellationToken::new(),
        }
    }

    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    pub fn with_event_tx(mut self, event_tx: Option<crate::event::EventSender>) -> Self {
        self.event_tx = event_tx;
        self
    }

    pub fn with_llm_providers(mut self, llm_providers: LlmProviderBindings) -> Self {
        self.llm_providers = Some(llm_providers);
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    pub fn model(&self) -> &ModelSelection {
        &self.model
    }

    pub fn session_store_dir(&self) -> Option<&Path> {
        self.session_store_dir.as_deref()
    }

    pub fn event_tx(&self) -> Option<&crate::event::EventSender> {
        self.event_tx.as_ref()
    }

    pub fn llm_providers(&self) -> Option<&LlmProviderBindings> {
        self.llm_providers.as_ref()
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

/// Per-extension immutable view of one hook call.
///
/// `P` is the hook-specific payload; its inherent getters are reachable directly on the
/// context through `Deref`, so handler code reads `ctx.tool_name()` regardless of which hook
/// it implements. Contexts are attributed by the host — extension code cannot construct them.
#[derive(Clone)]
pub struct HookContext<P> {
    call: SessionCallContext,
    working_dir: PathBuf,
    model: ModelSelection,
    payload: P,
}

impl<P> HookContext<P> {
    /// Attributes a per-extension view of the dispatcher input at its current state.
    pub(crate) fn from_runtime(call: ExtensionCallContext, input: &HookInput<P>) -> Self
    where
        P: Clone,
    {
        Self {
            call: SessionCallContext::from_runtime(
                call,
                input.call.session_id().clone(),
                input.call.turn_id().map(str::to_owned),
            ),
            working_dir: input.call.working_dir().to_path_buf(),
            model: input.call.model().clone(),
            payload: input.payload.clone(),
        }
    }

    pub fn model(&self) -> &ModelSelection {
        &self.model
    }

    pub fn session_id(&self) -> &SessionId {
        self.call.session_id()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.call.turn_id()
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}

impl<P> Deref for HookContext<P> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.payload
    }
}

impl<P> ExtensionCall for HookContext<P> {
    fn call(&self) -> &ExtensionCallContext {
        self.call.call()
    }
}

impl<P: std::fmt::Debug> std::fmt::Debug for HookContext<P> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HookContext")
            .field("extension_id", &self.call.extension_id())
            .field("session_id", &self.call.session_id())
            .field("payload", &self.payload)
            .finish_non_exhaustive()
    }
}

/// Dispatcher input for one hook pass: host facts plus the payload the dispatcher
/// aggregates mutations into before attributing per-extension [`HookContext`] views.
///
/// Getters and mutators of `P` are reachable through `Deref`/`DerefMut`.
#[doc(hidden)]
#[derive(Clone)]
pub struct HookInput<P> {
    call: RuntimeHookCallContext,
    payload: P,
}

impl<P> HookInput<P> {
    pub(crate) fn new(call: RuntimeHookCallContext, payload: P) -> Self {
        Self { call, payload }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub(crate) fn map_payload(mut self, map: impl FnOnce(P) -> P) -> Self {
        self.payload = map(self.payload);
        self
    }
}

impl<P> Deref for HookInput<P> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.payload
    }
}

impl<P> DerefMut for HookInput<P> {
    fn deref_mut(&mut self) -> &mut P {
        &mut self.payload
    }
}

/// LLM 自然结束后的扩展决策钩子载荷。
#[derive(Clone, Debug)]
pub struct ContinueAfterStopPayload {
    assistant_text: Arc<str>,
    finish_reason: Arc<str>,
    continuations_this_turn: u32,
}

impl ContinueAfterStopPayload {
    pub(crate) fn new(
        assistant_text: impl Into<String>,
        finish_reason: impl Into<String>,
        continuations_this_turn: u32,
    ) -> Self {
        Self {
            assistant_text: Arc::from(assistant_text.into()),
            finish_reason: Arc::from(finish_reason.into()),
            continuations_this_turn,
        }
    }

    pub fn assistant_text(&self) -> &str {
        &self.assistant_text
    }

    pub fn finish_reason(&self) -> &str {
        &self.finish_reason
    }

    pub fn continuations_this_turn(&self) -> u32 {
        self.continuations_this_turn
    }
}

/// LLM 自然结束后的扩展决策钩子上下文。
pub type ContinueAfterStopContext = HookContext<ContinueAfterStopPayload>;

#[doc(hidden)]
pub type RuntimeContinueAfterStopContext = HookInput<ContinueAfterStopPayload>;

/// 用户消息写入 transcript 前的扩展变换载荷。
#[derive(Clone, Debug)]
pub struct UserMessageEnvelopePayload {
    text: Arc<str>,
    attachments: Arc<[MessageAttachment]>,
}

impl UserMessageEnvelopePayload {
    pub(crate) fn new(text: impl Into<String>, attachments: Vec<MessageAttachment>) -> Self {
        Self {
            text: Arc::from(text.into()),
            attachments: attachments.into(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn attachments(&self) -> &[MessageAttachment] {
        &self.attachments
    }

    pub(crate) fn replace_text(&mut self, text: String) {
        self.text = Arc::from(text);
    }

    pub(crate) fn append_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut merged = self.text.to_string();
        if !merged.is_empty() {
            merged.push_str("\n\n");
        }
        merged.push_str(text);
        self.text = Arc::from(merged);
    }
}

/// 用户消息写入 transcript 前的扩展变换上下文。
pub type UserMessageEnvelopeContext = HookContext<UserMessageEnvelopePayload>;

#[doc(hidden)]
pub type RuntimeUserMessageEnvelopeContext = HookInput<UserMessageEnvelopePayload>;

/// PreToolUse 钩子载荷。
#[derive(Clone, Debug)]
pub struct PreToolUsePayload {
    call_id: ToolCallId,
    tool_name: Arc<str>,
    tool_input: serde_json::Value,
    approval_mode: crate::permission::ApprovalMode,
    available_tools: Arc<[ToolDefinition]>,
}

impl PreToolUsePayload {
    pub(crate) fn new(
        call_id: ToolCallId,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        approval_mode: crate::permission::ApprovalMode,
        available_tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            call_id,
            tool_name: Arc::from(tool_name.into()),
            tool_input,
            approval_mode,
            available_tools: available_tools.into(),
        }
    }

    pub fn call_id(&self) -> &ToolCallId {
        &self.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn tool_input(&self) -> &serde_json::Value {
        &self.tool_input
    }

    pub fn approval_mode(&self) -> crate::permission::ApprovalMode {
        self.approval_mode
    }

    pub fn available_tools(&self) -> &[ToolDefinition] {
        &self.available_tools
    }

    pub(crate) fn replace_tool_input(&mut self, tool_input: serde_json::Value) {
        self.tool_input = tool_input;
    }
}

/// PreToolUse 钩子上下文。
pub type PreToolUseContext = HookContext<PreToolUsePayload>;

#[doc(hidden)]
pub type RuntimePreToolUseContext = HookInput<PreToolUsePayload>;

/// PostToolUse 钩子载荷。
#[derive(Clone, Debug)]
pub struct PostToolUsePayload {
    call_id: ToolCallId,
    tool_name: Arc<str>,
    tool_input: serde_json::Value,
    tool_result: ToolResult,
}

impl PostToolUsePayload {
    pub(crate) fn new(
        call_id: ToolCallId,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        tool_result: ToolResult,
    ) -> Self {
        Self {
            call_id,
            tool_name: Arc::from(tool_name.into()),
            tool_input,
            tool_result,
        }
    }

    pub fn call_id(&self) -> &ToolCallId {
        &self.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn tool_input(&self) -> &serde_json::Value {
        &self.tool_input
    }

    pub fn tool_result(&self) -> &ToolResult {
        &self.tool_result
    }

    pub(crate) fn replace_result_content(&mut self, content: String) {
        self.tool_result.error = self.tool_result.is_error.then(|| content.clone());
        self.tool_result.content = content;
    }
}

/// PostToolUse 钩子上下文。
pub type PostToolUseContext = HookContext<PostToolUsePayload>;

#[doc(hidden)]
pub type RuntimePostToolUseContext = HookInput<PostToolUsePayload>;

/// Provider 钩子载荷。
#[derive(Clone, Debug)]
pub struct ProviderPayload {
    request_id: ProviderRequestId,
    messages: Arc<[Arc<crate::llm::LlmMessage>]>,
}

impl ProviderPayload {
    pub(crate) fn new(
        request_id: ProviderRequestId,
        messages: Vec<Arc<crate::llm::LlmMessage>>,
    ) -> Self {
        Self {
            request_id,
            messages: messages.into(),
        }
    }

    pub fn request_id(&self) -> &ProviderRequestId {
        &self.request_id
    }

    /// 共享的消息存储：零拷贝只读访问。
    pub fn shared_messages(&self) -> &[Arc<crate::llm::LlmMessage>] {
        &self.messages
    }

    #[deprecated(
        note = "按值复制全量消息；改用 `shared_messages()` 零拷贝访问。本访问器保留一个版本后移除"
    )]
    pub fn messages(&self) -> Vec<crate::llm::LlmMessage> {
        self.shared_messages()
            .iter()
            .map(|message| (**message).clone())
            .collect()
    }

    pub(crate) fn replace_messages(&mut self, messages: Vec<crate::llm::LlmMessage>) {
        self.messages = messages.into_iter().map(Arc::new).collect();
    }

    pub(crate) fn append_messages(&mut self, messages: Vec<crate::llm::LlmMessage>) {
        let mut merged = self.messages.to_vec();
        merged.extend(messages.into_iter().map(Arc::new));
        self.messages = merged.into();
    }
}

/// Provider 钩子上下文。
pub type ProviderContext = HookContext<ProviderPayload>;

#[doc(hidden)]
pub type RuntimeProviderContext = HookInput<ProviderPayload>;

/// Durable-success acknowledgement for one exact prepared provider contribution.
#[derive(Clone, Debug)]
pub struct ProviderSettlementPayload {
    request_id: ProviderRequestId,
    contribution_id: ProviderContributionId,
}

impl ProviderSettlementPayload {
    pub(crate) fn new(
        request_id: ProviderRequestId,
        contribution_id: ProviderContributionId,
    ) -> Self {
        Self {
            request_id,
            contribution_id,
        }
    }

    pub fn request_id(&self) -> &ProviderRequestId {
        &self.request_id
    }

    pub fn contribution_id(&self) -> &ProviderContributionId {
        &self.contribution_id
    }
}

/// Extension-attributed acknowledgement delivered after the provider request and its assistant
/// durable facts have committed.
pub type ProviderSettlementContext = HookContext<ProviderSettlementPayload>;

/// Runtime facts needed to attribute provider acknowledgements to their original handlers.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeProviderSettlementContext {
    call: RuntimeHookCallContext,
    request_id: ProviderRequestId,
}

impl RuntimeProviderSettlementContext {
    pub(crate) fn new(call: RuntimeHookCallContext, request_id: ProviderRequestId) -> Self {
        Self { call, request_id }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub fn request_id(&self) -> &ProviderRequestId {
        &self.request_id
    }
}

/// PromptBuild 钩子载荷。
#[derive(Clone, Debug)]
pub struct PromptBuildPayload {
    tools: Arc<[ToolDefinition]>,
}

impl PromptBuildPayload {
    pub(crate) fn new(tools: Vec<ToolDefinition>) -> Self {
        Self {
            tools: tools.into(),
        }
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }
}

/// PromptBuild 钩子上下文。
///
/// 首次构建发生在 `SessionStarted` 持久化之前。处理器应只依赖此上下文和扩展自身
/// 状态，不应假定 `session_id` 已能通过 session-history host API 查询。
pub type PromptBuildContext = HookContext<PromptBuildPayload>;

#[doc(hidden)]
pub type RuntimePromptBuildContext = HookInput<PromptBuildPayload>;

/// Facts available while collecting PreCompact contributions.
#[derive(Clone, Debug)]
pub struct PreCompactPayload {
    trigger: CompactTrigger,
    source_messages: Arc<[crate::llm::LlmMessage]>,
    retained_file_limit: usize,
}

impl PreCompactPayload {
    pub(crate) fn new(
        trigger: CompactTrigger,
        source_messages: Vec<crate::llm::LlmMessage>,
        retained_file_limit: usize,
    ) -> Self {
        Self {
            trigger,
            source_messages: source_messages.into(),
            retained_file_limit,
        }
    }

    pub fn trigger(&self) -> CompactTrigger {
        self.trigger
    }

    pub fn message_count(&self) -> usize {
        self.source_messages.len()
    }

    pub fn source_messages(&self) -> &[crate::llm::LlmMessage] {
        &self.source_messages
    }

    pub fn retained_file_limit(&self) -> usize {
        self.retained_file_limit
    }
}

pub type PreCompactContext = HookContext<PreCompactPayload>;

#[doc(hidden)]
pub type RuntimePreCompactContext = HookInput<PreCompactPayload>;

/// Facts delivered after a compact rewrite is durably committed.
#[derive(Clone, Debug)]
pub struct PostCompactPayload {
    trigger: CompactTrigger,
    message_count: usize,
    pre_tokens: usize,
    post_tokens: usize,
    summary: Arc<str>,
}

impl PostCompactPayload {
    pub(crate) fn new(
        trigger: CompactTrigger,
        message_count: usize,
        pre_tokens: usize,
        post_tokens: usize,
        summary: String,
    ) -> Self {
        Self {
            trigger,
            message_count,
            pre_tokens,
            post_tokens,
            summary: Arc::from(summary),
        }
    }

    pub fn trigger(&self) -> CompactTrigger {
        self.trigger
    }

    pub fn message_count(&self) -> usize {
        self.message_count
    }

    pub fn pre_tokens(&self) -> usize {
        self.pre_tokens
    }

    pub fn post_tokens(&self) -> usize {
        self.post_tokens
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }
}

pub type PostCompactContext = HookContext<PostCompactPayload>;

#[doc(hidden)]
pub type RuntimePostCompactContext = HookInput<PostCompactPayload>;

/// 通用生命周期钩子载荷。
#[derive(Clone, Debug)]
pub struct LifecyclePayload {
    last_exchange: Option<ExchangeSummary>,
    mid_turn_user_messages_synced: u32,
}

impl LifecyclePayload {
    pub(crate) fn new(last_exchange: Option<ExchangeSummary>) -> Self {
        Self {
            last_exchange,
            mid_turn_user_messages_synced: 0,
        }
    }

    pub(crate) fn for_step_start(mut self, mid_turn_user_messages_synced: u32) -> Self {
        self.mid_turn_user_messages_synced = mid_turn_user_messages_synced;
        self
    }

    pub fn last_exchange(&self) -> Option<&ExchangeSummary> {
        self.last_exchange.as_ref()
    }

    pub fn mid_turn_user_messages_synced(&self) -> u32 {
        self.mid_turn_user_messages_synced
    }
}

/// 通用生命周期钩子上下文。
pub type LifecycleContext = HookContext<LifecyclePayload>;

#[doc(hidden)]
pub type RuntimeLifecycleContext = HookInput<LifecyclePayload>;

/// Host-attributed context for one workspace-scoped tool discovery pass.
#[derive(Clone)]
pub struct ToolDiscoveryContext {
    call: WorkspaceCallContext,
    generation: u64,
}

impl ToolDiscoveryContext {
    pub(crate) fn from_runtime(
        call: ExtensionCallContext,
        working_dir: impl Into<PathBuf>,
        generation: u64,
    ) -> Self {
        Self {
            call: WorkspaceCallContext::from_runtime(call, working_dir),
            generation,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn working_dir(&self) -> &Path {
        self.call.working_dir()
    }
}

impl ExtensionCall for ToolDiscoveryContext {
    fn call(&self) -> &ExtensionCallContext {
        self.call.call()
    }
}

impl std::fmt::Debug for ToolDiscoveryContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolDiscoveryContext")
            .field("call", &self.call)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Host-attributed context for one workspace-scoped command discovery pass.
#[derive(Clone)]
pub struct CommandDiscoveryContext {
    call: WorkspaceCallContext,
    generation: u64,
}

impl CommandDiscoveryContext {
    pub(crate) fn from_runtime(
        call: ExtensionCallContext,
        working_dir: impl Into<PathBuf>,
        generation: u64,
    ) -> Self {
        Self {
            call: WorkspaceCallContext::from_runtime(call, working_dir),
            generation,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn working_dir(&self) -> &Path {
        self.call.working_dir()
    }
}

impl ExtensionCall for CommandDiscoveryContext {
    fn call(&self) -> &ExtensionCallContext {
        self.call.call()
    }
}

impl std::fmt::Debug for CommandDiscoveryContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandDiscoveryContext")
            .field("call", &self.call)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Context for executing one extension slash command.
///
/// Values are attributed and constructed by the host. Extension code reads them through
/// accessors so the runtime can extend this context without breaking author implementations.
#[derive(Clone)]
pub struct CommandContext {
    call: SessionCallContext,
    working_dir: PathBuf,
    model: ModelSelection,
    command_name: String,
    argument: String,
}

impl CommandContext {
    pub(crate) fn from_runtime(
        call: SessionCallContext,
        working_dir: PathBuf,
        model: ModelSelection,
        command_name: impl Into<String>,
        argument: impl Into<String>,
    ) -> Self {
        Self {
            call,
            working_dir,
            model,
            command_name: command_name.into(),
            argument: argument.into(),
        }
    }

    pub fn model(&self) -> &ModelSelection {
        &self.model
    }

    pub fn command_name(&self) -> &str {
        &self.command_name
    }

    pub fn argument(&self) -> &str {
        &self.argument
    }

    pub fn session_id(&self) -> &SessionId {
        self.call.session_id()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.call.turn_id()
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}

impl ExtensionCall for CommandContext {
    fn call(&self) -> &ExtensionCallContext {
        self.call.call()
    }
}

/// Context for completing the argument of one extension slash command.
#[derive(Clone)]
pub struct CommandCompletionContext {
    call: SessionCallContext,
    working_dir: PathBuf,
    model: ModelSelection,
    command_name: String,
    argument: String,
    cursor: usize,
}

impl CommandCompletionContext {
    pub(crate) fn for_runtime(command: CommandContext, cursor: usize) -> Self {
        Self {
            call: command.call,
            working_dir: command.working_dir,
            model: command.model,
            command_name: command.command_name,
            argument: command.argument,
            cursor,
        }
    }

    pub fn model(&self) -> &ModelSelection {
        &self.model
    }

    pub fn command_name(&self) -> &str {
        &self.command_name
    }

    pub fn argument(&self) -> &str {
        &self.argument
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn session_id(&self) -> &SessionId {
        self.call.session_id()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.call.turn_id()
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}

impl ExtensionCall for CommandCompletionContext {
    fn call(&self) -> &ExtensionCallContext {
        self.call.call()
    }
}

impl std::fmt::Debug for CommandContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandContext")
            .field("call", &self.call)
            .field("model", &self.model)
            .field("command_name", &self.command_name)
            .field("argument", &self.argument)
            .finish()
    }
}

impl std::fmt::Debug for CommandCompletionContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandCompletionContext")
            .field("call", &self.call)
            .field("model", &self.model)
            .field("command_name", &self.command_name)
            .field("argument", &self.argument)
            .field("cursor", &self.cursor)
            .finish()
    }
}
