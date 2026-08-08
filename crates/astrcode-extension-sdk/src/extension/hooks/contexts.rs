//! Host-constructed contexts for extension hook handlers.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::{
    compaction::CompactTrigger,
    message_attachment::MessageAttachment,
    types::{SessionId, ToolCallId},
};
use tokio_util::sync::CancellationToken;

use super::types::ExchangeSummary;
use crate::{
    config::ModelSelection,
    extension::{ExtensionCallContext, ExtensionEventEmitter, ExtensionPaths, ExtensionTasks},
    host::ExtensionHost,
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

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

#[derive(Clone)]
struct HookCallContext {
    call: ExtensionCallContext,
    model: ModelSelection,
}

impl HookCallContext {
    fn new(call: ExtensionCallContext, model: ModelSelection) -> Self {
        Self { call, model }
    }
}

macro_rules! hook_context_accessors {
    () => {
        pub fn call(&self) -> &ExtensionCallContext {
            &self.common.call
        }

        pub fn extension_id(&self) -> &str {
            self.common.call.extension_id()
        }

        pub fn session_id(&self) -> Option<&SessionId> {
            self.common.call.session_id()
        }

        pub fn turn_id(&self) -> Option<&str> {
            self.common.call.turn_id()
        }

        pub fn working_dir(&self) -> Option<&Path> {
            self.common.call.working_dir()
        }

        pub fn model(&self) -> &ModelSelection {
            &self.common.model
        }

        pub fn paths(&self) -> &ExtensionPaths {
            self.common.call.paths()
        }

        pub fn host(&self) -> &ExtensionHost {
            self.common.call.host()
        }

        pub fn events(&self) -> &ExtensionEventEmitter {
            self.common.call.events()
        }

        pub fn tasks(&self) -> &ExtensionTasks {
            self.common.call.tasks()
        }

        pub fn cancellation(&self) -> &CancellationToken {
            self.common.call.cancellation()
        }
    };
}

/// Runtime input for an LLM stop continuation hook.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeContinueAfterStopContext {
    call: RuntimeHookCallContext,
    assistant_text: String,
    finish_reason: String,
    continuations_this_turn: u32,
}

impl RuntimeContinueAfterStopContext {
    pub fn new(
        call: RuntimeHookCallContext,
        assistant_text: impl Into<String>,
        finish_reason: impl Into<String>,
        continuations_this_turn: u32,
    ) -> Self {
        Self {
            call,
            assistant_text: assistant_text.into(),
            finish_reason: finish_reason.into(),
            continuations_this_turn,
        }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub fn continuations_this_turn(&self) -> u32 {
        self.continuations_this_turn
    }
}

/// LLM 自然结束后的扩展决策钩子上下文。
#[derive(Clone)]
pub struct ContinueAfterStopContext {
    common: HookCallContext,
    assistant_text: Arc<str>,
    finish_reason: Arc<str>,
    continuations_this_turn: u32,
}

impl ContinueAfterStopContext {
    #[doc(hidden)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        input: &RuntimeContinueAfterStopContext,
    ) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            assistant_text: Arc::from(input.assistant_text.as_str()),
            finish_reason: Arc::from(input.finish_reason.as_str()),
            continuations_this_turn: input.continuations_this_turn,
        }
    }

    hook_context_accessors!();

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

/// Runtime input for user-message envelope hooks.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeUserMessageEnvelopeContext {
    call: RuntimeHookCallContext,
    text: String,
    attachments: Vec<MessageAttachment>,
}

impl RuntimeUserMessageEnvelopeContext {
    pub fn new(
        call: RuntimeHookCallContext,
        text: impl Into<String>,
        attachments: Vec<MessageAttachment>,
    ) -> Self {
        Self {
            call,
            text: text.into(),
            attachments,
        }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn replace_text(&mut self, text: String) {
        self.text = text;
    }

    pub fn append_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
    }
}

/// 用户消息写入 transcript 前的扩展变换上下文。
#[derive(Clone)]
pub struct UserMessageEnvelopeContext {
    common: HookCallContext,
    text: Arc<str>,
    attachments: Arc<[MessageAttachment]>,
}

impl UserMessageEnvelopeContext {
    #[doc(hidden)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        input: &RuntimeUserMessageEnvelopeContext,
    ) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            text: Arc::from(input.text.as_str()),
            attachments: input.attachments.clone().into(),
        }
    }

    hook_context_accessors!();

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn attachments(&self) -> &[MessageAttachment] {
        &self.attachments
    }
}

/// Runtime input for pre-tool hooks.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimePreToolUseContext {
    call: RuntimeHookCallContext,
    call_id: ToolCallId,
    tool_name: String,
    tool_input: serde_json::Value,
    approval_mode: crate::permission::ApprovalMode,
    available_tools: Vec<ToolDefinition>,
}

impl RuntimePreToolUseContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        call: RuntimeHookCallContext,
        call_id: ToolCallId,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        approval_mode: crate::permission::ApprovalMode,
        available_tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            call,
            call_id,
            tool_name: tool_name.into(),
            tool_input,
            approval_mode,
            available_tools,
        }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn tool_input(&self) -> &serde_json::Value {
        &self.tool_input
    }

    pub fn replace_tool_input(&mut self, tool_input: serde_json::Value) {
        self.tool_input = tool_input;
    }
}

/// PreToolUse 钩子上下文。
#[derive(Clone)]
pub struct PreToolUseContext {
    common: HookCallContext,
    call_id: ToolCallId,
    tool_name: Arc<str>,
    tool_input: serde_json::Value,
    approval_mode: crate::permission::ApprovalMode,
    available_tools: Arc<[ToolDefinition]>,
}

impl PreToolUseContext {
    #[doc(hidden)]
    pub fn from_runtime(call: ExtensionCallContext, input: &RuntimePreToolUseContext) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            call_id: input.call_id.clone(),
            tool_name: Arc::from(input.tool_name.as_str()),
            tool_input: input.tool_input.clone(),
            approval_mode: input.approval_mode,
            available_tools: input.available_tools.clone().into(),
        }
    }

    hook_context_accessors!();

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
}

impl std::fmt::Debug for PreToolUseContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreToolUseContext")
            .field("extension_id", &self.extension_id())
            .field("session_id", &self.session_id())
            .field("call_id", &self.call_id)
            .field("tool_name", &self.tool_name)
            .finish_non_exhaustive()
    }
}

/// Runtime input for post-tool hooks.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimePostToolUseContext {
    call: RuntimeHookCallContext,
    call_id: ToolCallId,
    tool_name: String,
    tool_input: serde_json::Value,
    tool_result: ToolResult,
}

impl RuntimePostToolUseContext {
    pub fn new(
        call: RuntimeHookCallContext,
        call_id: ToolCallId,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        tool_result: ToolResult,
    ) -> Self {
        Self {
            call,
            call_id,
            tool_name: tool_name.into(),
            tool_input,
            tool_result,
        }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn tool_result(&self) -> &ToolResult {
        &self.tool_result
    }

    pub fn replace_result_content(&mut self, content: String) {
        self.tool_result.error = self.tool_result.is_error.then(|| content.clone());
        self.tool_result.content = content;
    }
}

/// PostToolUse 钩子上下文。
#[derive(Clone)]
pub struct PostToolUseContext {
    common: HookCallContext,
    call_id: ToolCallId,
    tool_name: Arc<str>,
    tool_input: serde_json::Value,
    tool_result: ToolResult,
}

impl PostToolUseContext {
    #[doc(hidden)]
    pub fn from_runtime(call: ExtensionCallContext, input: &RuntimePostToolUseContext) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            call_id: input.call_id.clone(),
            tool_name: Arc::from(input.tool_name.as_str()),
            tool_input: input.tool_input.clone(),
            tool_result: input.tool_result.clone(),
        }
    }

    hook_context_accessors!();

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
}

impl std::fmt::Debug for PostToolUseContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostToolUseContext")
            .field("extension_id", &self.extension_id())
            .field("session_id", &self.session_id())
            .field("call_id", &self.call_id)
            .field("tool_name", &self.tool_name)
            .field("is_error", &self.tool_result.is_error)
            .finish_non_exhaustive()
    }
}

/// Runtime input for provider hooks.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeProviderContext {
    call: RuntimeHookCallContext,
    messages: Vec<crate::llm::LlmMessage>,
}

impl RuntimeProviderContext {
    pub fn new(call: RuntimeHookCallContext, messages: Vec<crate::llm::LlmMessage>) -> Self {
        Self { call, messages }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub fn messages(&self) -> &[crate::llm::LlmMessage] {
        &self.messages
    }

    pub fn replace_messages(&mut self, messages: Vec<crate::llm::LlmMessage>) {
        self.messages = messages;
    }

    pub fn append_messages(&mut self, messages: Vec<crate::llm::LlmMessage>) {
        self.messages.extend(messages);
    }
}

/// Provider 钩子上下文。
#[derive(Clone)]
pub struct ProviderContext {
    common: HookCallContext,
    messages: Arc<[crate::llm::LlmMessage]>,
}

impl ProviderContext {
    #[doc(hidden)]
    pub fn from_runtime(call: ExtensionCallContext, input: &RuntimeProviderContext) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            messages: input.messages.clone().into(),
        }
    }

    hook_context_accessors!();

    pub fn messages(&self) -> &[crate::llm::LlmMessage] {
        &self.messages
    }
}

/// Runtime input for prompt-build hooks.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimePromptBuildContext {
    call: RuntimeHookCallContext,
    tools: Vec<ToolDefinition>,
}

impl RuntimePromptBuildContext {
    pub fn new(call: RuntimeHookCallContext, tools: Vec<ToolDefinition>) -> Self {
        Self { call, tools }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }
}

/// PromptBuild 钩子上下文。
///
/// 首次构建发生在 `SessionStarted` 持久化之前。处理器应只依赖此上下文和扩展自身
/// 状态，不应假定 `session_id` 已能通过 session-history host API 查询。
#[derive(Clone)]
pub struct PromptBuildContext {
    common: HookCallContext,
    tools: Arc<[ToolDefinition]>,
}

impl PromptBuildContext {
    #[doc(hidden)]
    pub fn from_runtime(call: ExtensionCallContext, input: &RuntimePromptBuildContext) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            tools: input.tools.clone().into(),
        }
    }

    hook_context_accessors!();

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }
}

/// Runtime input for compact hooks.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeCompactContext {
    call: RuntimeHookCallContext,
    trigger: CompactTrigger,
    message_count: usize,
    pre_tokens: Option<usize>,
    post_tokens: Option<usize>,
    summary: Option<String>,
}

impl RuntimeCompactContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        call: RuntimeHookCallContext,
        trigger: CompactTrigger,
        message_count: usize,
        pre_tokens: Option<usize>,
        post_tokens: Option<usize>,
        summary: Option<String>,
    ) -> Self {
        Self {
            call,
            trigger,
            message_count,
            pre_tokens,
            post_tokens,
            summary,
        }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }
}

/// Compact 钩子上下文。
#[derive(Clone)]
pub struct CompactContext {
    common: HookCallContext,
    trigger: CompactTrigger,
    message_count: usize,
    pre_tokens: Option<usize>,
    post_tokens: Option<usize>,
    summary: Option<Arc<str>>,
}

impl CompactContext {
    #[doc(hidden)]
    pub fn from_runtime(call: ExtensionCallContext, input: &RuntimeCompactContext) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            trigger: input.trigger,
            message_count: input.message_count,
            pre_tokens: input.pre_tokens,
            post_tokens: input.post_tokens,
            summary: input.summary.as_deref().map(Arc::from),
        }
    }

    hook_context_accessors!();

    pub fn trigger(&self) -> CompactTrigger {
        self.trigger
    }

    pub fn message_count(&self) -> usize {
        self.message_count
    }

    pub fn pre_tokens(&self) -> Option<usize> {
        self.pre_tokens
    }

    pub fn post_tokens(&self) -> Option<usize> {
        self.post_tokens
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }
}

/// Runtime input for lifecycle hooks.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeLifecycleContext {
    call: RuntimeHookCallContext,
    last_exchange: Option<ExchangeSummary>,
    mid_turn_user_messages_synced: u32,
}

impl RuntimeLifecycleContext {
    pub fn new(call: RuntimeHookCallContext, last_exchange: Option<ExchangeSummary>) -> Self {
        Self {
            call,
            last_exchange,
            mid_turn_user_messages_synced: 0,
        }
    }

    pub fn call(&self) -> &RuntimeHookCallContext {
        &self.call
    }

    pub fn for_step_start(mut self, mid_turn_user_messages_synced: u32) -> Self {
        self.mid_turn_user_messages_synced = mid_turn_user_messages_synced;
        self
    }

    pub fn mid_turn_user_messages_synced(&self) -> u32 {
        self.mid_turn_user_messages_synced
    }
}

/// 通用生命周期钩子上下文。
#[derive(Clone)]
pub struct LifecycleContext {
    common: HookCallContext,
    last_exchange: Option<ExchangeSummary>,
    mid_turn_user_messages_synced: u32,
}

impl LifecycleContext {
    #[doc(hidden)]
    pub fn from_runtime(call: ExtensionCallContext, input: &RuntimeLifecycleContext) -> Self {
        Self {
            common: HookCallContext::new(call, input.call.model.clone()),
            last_exchange: input.last_exchange.clone(),
            mid_turn_user_messages_synced: input.mid_turn_user_messages_synced,
        }
    }

    hook_context_accessors!();

    pub fn last_exchange(&self) -> Option<&ExchangeSummary> {
        self.last_exchange.as_ref()
    }

    pub fn mid_turn_user_messages_synced(&self) -> u32 {
        self.mid_turn_user_messages_synced
    }
}

impl std::fmt::Debug for LifecycleContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LifecycleContext")
            .field("extension_id", &self.extension_id())
            .field("session_id", &self.session_id())
            .field("last_exchange", &self.last_exchange)
            .field(
                "mid_turn_user_messages_synced",
                &self.mid_turn_user_messages_synced,
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct DiscoveryCallContext {
    call: ExtensionCallContext,
    generation: u64,
}

macro_rules! discovery_context {
    ($name:ident) => {
        /// Host-attributed context for one workspace-scoped discovery pass.
        #[derive(Clone)]
        pub struct $name {
            common: DiscoveryCallContext,
        }

        impl $name {
            #[doc(hidden)]
            pub fn from_runtime(call: ExtensionCallContext, generation: u64) -> Self {
                Self {
                    common: DiscoveryCallContext { call, generation },
                }
            }

            pub fn call(&self) -> &ExtensionCallContext {
                &self.common.call
            }

            pub fn extension_id(&self) -> &str {
                self.common.call.extension_id()
            }

            pub fn working_dir(&self) -> Option<&Path> {
                self.common.call.working_dir()
            }

            pub fn generation(&self) -> u64 {
                self.common.generation
            }

            pub fn paths(&self) -> &ExtensionPaths {
                self.common.call.paths()
            }

            pub fn host(&self) -> &ExtensionHost {
                self.common.call.host()
            }

            pub fn events(&self) -> &ExtensionEventEmitter {
                self.common.call.events()
            }

            pub fn tasks(&self) -> &ExtensionTasks {
                self.common.call.tasks()
            }

            pub fn cancellation(&self) -> &CancellationToken {
                self.common.call.cancellation()
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("call", &self.common.call)
                    .field("generation", &self.common.generation)
                    .finish()
            }
        }
    };
}

discovery_context!(ToolDiscoveryContext);
discovery_context!(CommandDiscoveryContext);

#[derive(Clone)]
struct CommandCallContext {
    call: ExtensionCallContext,
    model: ModelSelection,
    command_name: String,
    argument: String,
}

impl CommandCallContext {
    fn new(
        call: ExtensionCallContext,
        model: ModelSelection,
        command_name: impl Into<String>,
        argument: impl Into<String>,
    ) -> Self {
        Self {
            call,
            model,
            command_name: command_name.into(),
            argument: argument.into(),
        }
    }
}

macro_rules! command_context_accessors {
    () => {
        pub fn call(&self) -> &ExtensionCallContext {
            &self.call.call
        }

        pub fn extension_id(&self) -> &str {
            self.call.call.extension_id()
        }

        pub fn session_id(&self) -> Option<&SessionId> {
            self.call.call.session_id()
        }

        pub fn working_dir(&self) -> Option<&Path> {
            self.call.call.working_dir()
        }

        pub fn model(&self) -> &ModelSelection {
            &self.call.model
        }

        pub fn paths(&self) -> &ExtensionPaths {
            self.call.call.paths()
        }

        pub fn host(&self) -> &ExtensionHost {
            self.call.call.host()
        }

        pub fn events(&self) -> &ExtensionEventEmitter {
            self.call.call.events()
        }

        pub fn tasks(&self) -> &ExtensionTasks {
            self.call.call.tasks()
        }

        pub fn cancellation(&self) -> &CancellationToken {
            self.call.call.cancellation()
        }

        pub fn command_name(&self) -> &str {
            &self.call.command_name
        }

        pub fn argument(&self) -> &str {
            &self.call.argument
        }
    };
}

/// Context for executing one extension slash command.
///
/// Values are attributed and constructed by the host. Extension code reads them through
/// accessors so the runtime can extend this context without breaking author implementations.
#[derive(Clone)]
pub struct CommandContext {
    call: CommandCallContext,
}

impl CommandContext {
    #[doc(hidden)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        model: ModelSelection,
        command_name: impl Into<String>,
        argument: impl Into<String>,
    ) -> Self {
        Self {
            call: CommandCallContext::new(call, model, command_name, argument),
        }
    }

    command_context_accessors!();
}

/// Context for completing the argument of one extension slash command.
#[derive(Clone)]
pub struct CommandCompletionContext {
    call: CommandCallContext,
    cursor: usize,
}

impl CommandCompletionContext {
    #[doc(hidden)]
    pub fn for_runtime(command: CommandContext, cursor: usize) -> Self {
        Self {
            call: command.call,
            cursor,
        }
    }

    command_context_accessors!();

    pub fn cursor(&self) -> usize {
        self.cursor
    }
}

impl std::fmt::Debug for CommandContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandContext")
            .field("call", &self.call.call)
            .field("model", &self.call.model)
            .field("command_name", &self.call.command_name)
            .field("argument", &self.call.argument)
            .finish()
    }
}

impl std::fmt::Debug for CommandCompletionContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandCompletionContext")
            .field("call", &self.call.call)
            .field("model", &self.call.model)
            .field("command_name", &self.call.command_name)
            .field("argument", &self.call.argument)
            .field("cursor", &self.cursor)
            .finish()
    }
}
