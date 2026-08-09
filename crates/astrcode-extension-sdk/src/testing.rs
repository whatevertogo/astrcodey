//! Minimal host-safe fixtures for extension author tests.

mod harnesses;

use std::{path::PathBuf, sync::Arc};

use astrcode_core::{
    compaction::CompactTrigger, message_attachment::MessageAttachment, types::ToolCallId,
};
use async_trait::async_trait;
pub use harnesses::*;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    config::ModelSelection,
    extension::{
        CommandCompletionContext, CommandContext, CompactContext, ContinueAfterStopContext,
        CustomEventEmitter, ExchangeSummary, ExtensionCallContext, ExtensionCapability,
        ExtensionHttpRequest, ExtensionHttpRoute, ExtensionPaths, ExtensionTasks, HttpContext,
        LifecycleContext, PostToolUseContext, PreToolUseContext, PromptBuildContext,
        ProviderContext, RuntimeCompactContext, RuntimeContinueAfterStopContext,
        RuntimeHookCallContext, RuntimeLifecycleContext, RuntimePostToolUseContext,
        RuntimePreToolUseContext, RuntimePromptBuildContext, RuntimeProviderContext,
        RuntimeUserMessageEnvelopeContext, ToolContext, UserMessageEnvelopeContext,
    },
    host::{
        ExtensionHost, HostError, HostOperation,
        internal::{HostInvoker, HostScope, extension_host},
    },
    llm::LlmMessage,
    permission::ApprovalMode,
    tool::{ToolDefinition, ToolResult},
    types::SessionId,
};

struct CallContextBuilder {
    extension_id: String,
    session_id: Option<SessionId>,
    turn_id: Option<String>,
    working_dir: Option<PathBuf>,
    global_store_dir: Option<PathBuf>,
    session_store_dir: Option<PathBuf>,
    grants: Vec<ExtensionCapability>,
    host: Option<ExtensionHost>,
    events: Option<CustomEventEmitter>,
    tasks: Option<ExtensionTasks>,
    cancellation: CancellationToken,
}

impl CallContextBuilder {
    fn new(extension_id: impl Into<String>) -> Self {
        Self {
            extension_id: extension_id.into(),
            session_id: None,
            turn_id: None,
            working_dir: None,
            global_store_dir: None,
            session_store_dir: None,
            grants: Vec::new(),
            host: None,
            events: None,
            tasks: None,
            cancellation: CancellationToken::new(),
        }
    }

    fn session(
        mut self,
        session_id: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        session_store_dir: Option<PathBuf>,
    ) -> Self {
        self.session_id = Some(SessionId::new(session_id));
        self.working_dir = Some(working_dir.into());
        self.session_store_dir = session_store_dir;
        self
    }

    fn workspace(mut self, working_dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(working_dir.into());
        self
    }

    fn turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    fn global_store_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.global_store_dir = Some(path.into());
        self
    }

    fn capability(mut self, capability: ExtensionCapability) -> Self {
        if !self.grants.contains(&capability) {
            self.grants.push(capability);
        }
        self
    }

    fn host(mut self, host: ExtensionHost) -> Self {
        self.host = Some(host);
        self
    }

    fn events(mut self, events: CustomEventEmitter) -> Self {
        self.events = Some(events);
        self
    }

    fn tasks(mut self, tasks: ExtensionTasks) -> Self {
        self.tasks = Some(tasks);
        self
    }

    fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    fn build(self) -> ExtensionCallContext {
        let session_available = self.session_id.is_some();
        let workspace_available = self.working_dir.is_some();
        let host = self.host.unwrap_or_else(|| {
            extension_host(
                Arc::new(UnavailableHost),
                HostScope::new(
                    self.grants,
                    std::iter::empty::<HostOperation>(),
                    session_available,
                    workspace_available,
                ),
            )
        });
        let paths = ExtensionPaths::from_runtime(
            &self.extension_id,
            self.global_store_dir.as_deref(),
            self.session_store_dir.as_deref(),
        );
        let tasks = self
            .tasks
            .unwrap_or_else(|| ExtensionTasks::new(self.extension_id.as_str()));
        ExtensionCallContext::from_runtime(
            self.extension_id,
            self.session_id,
            self.turn_id,
            self.working_dir,
            paths,
            host,
            self.events.unwrap_or_default(),
            tasks,
            self.cancellation,
        )
    }
}

macro_rules! call_context_builder_methods {
    () => {
        pub fn session(
            mut self,
            session_id: impl Into<String>,
            working_dir: impl Into<PathBuf>,
            session_store_dir: Option<PathBuf>,
        ) -> Self {
            self.call = self
                .call
                .session(session_id, working_dir, session_store_dir);
            self
        }

        pub fn workspace(mut self, working_dir: impl Into<PathBuf>) -> Self {
            self.call = self.call.workspace(working_dir);
            self
        }

        pub fn global_store_dir(mut self, path: impl Into<PathBuf>) -> Self {
            self.call = self.call.global_store_dir(path);
            self
        }

        pub fn capability(mut self, capability: ExtensionCapability) -> Self {
            self.call = self.call.capability(capability);
            self
        }

        pub fn host(mut self, host: ExtensionHost) -> Self {
            self.call = self.call.host(host);
            self
        }

        pub fn events(mut self, events: CustomEventEmitter) -> Self {
            self.call = self.call.events(events);
            self
        }

        pub fn tasks(mut self, tasks: ExtensionTasks) -> Self {
            self.call = self.call.tasks(tasks);
            self
        }

        pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
            self.call = self.call.cancellation(cancellation);
            self
        }
    };
}

/// Builder for an attributed [`CommandContext`] used only by extension tests.
///
/// Like the production command dispatcher, it shares the common extension call context. The
/// default fixture has no session, workspace, capability, or host backend.
pub struct CommandContextBuilder {
    call: CallContextBuilder,
    command_name: String,
    argument: String,
    model: ModelSelection,
}

impl CommandContextBuilder {
    pub fn new(extension_id: impl Into<String>, command_name: impl Into<String>) -> Self {
        Self {
            call: CallContextBuilder::new(extension_id),
            command_name: command_name.into(),
            argument: String::new(),
            model: ModelSelection::simple("test-model"),
        }
    }

    call_context_builder_methods!();

    pub fn argument(mut self, argument: impl Into<String>) -> Self {
        self.argument = argument.into();
        self
    }

    pub fn model(mut self, model: ModelSelection) -> Self {
        self.model = model;
        self
    }

    pub fn build(self) -> CommandContext {
        let call = self.call.build();
        CommandContext::from_runtime(call, self.model, self.command_name, self.argument)
    }

    pub fn build_completion(self, cursor: usize) -> CommandCompletionContext {
        CommandCompletionContext::for_runtime(self.build(), cursor)
    }
}

/// Builder for an attributed [`HttpContext`] used only by extension tests.
///
/// HTTP fixtures start without a session, workspace, caller, capability, or backend. Tests opt in
/// to the scope their route is expected to receive.
pub struct HttpContextBuilder {
    call: CallContextBuilder,
    route: ExtensionHttpRoute,
    request: ExtensionHttpRequest,
    caller_extension_id: Option<String>,
}

impl HttpContextBuilder {
    pub fn new(
        extension_id: impl Into<String>,
        route: ExtensionHttpRoute,
        request: ExtensionHttpRequest,
    ) -> Self {
        Self {
            call: CallContextBuilder::new(extension_id),
            route,
            request,
            caller_extension_id: None,
        }
    }

    call_context_builder_methods!();

    pub fn caller_extension_id(mut self, extension_id: impl Into<String>) -> Self {
        self.caller_extension_id = Some(extension_id.into());
        self
    }

    pub fn build(self) -> HttpContext {
        let call = self.call.build();
        HttpContext::from_runtime(call, self.route, self.request, self.caller_extension_id)
    }
}

/// Builder for host-attributed hook contexts used only by extension tests.
///
/// Common call facts default to no session, turn, workspace, capability, event sink, or host
/// backend. Each terminal `build_*` method requires the hook-specific input instead of inventing
/// plausible production values.
pub struct HookContextBuilder {
    call: CallContextBuilder,
    model: ModelSelection,
}

impl HookContextBuilder {
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self {
            call: CallContextBuilder::new(extension_id),
            model: ModelSelection::simple("test-model"),
        }
    }

    call_context_builder_methods!();

    pub fn turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.call = self.call.turn_id(turn_id);
        self
    }

    pub fn model(mut self, model: ModelSelection) -> Self {
        self.model = model;
        self
    }

    pub fn build_continue_after_stop(
        self,
        assistant_text: impl Into<String>,
        finish_reason: impl Into<String>,
        continuations_this_turn: u32,
    ) -> ContinueAfterStopContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimeContinueAfterStopContext::new(
            runtime_call,
            assistant_text,
            finish_reason,
            continuations_this_turn,
        );
        ContinueAfterStopContext::from_runtime(call, &input)
    }

    pub fn build_user_message_envelope(
        self,
        text: impl Into<String>,
        attachments: Vec<MessageAttachment>,
    ) -> UserMessageEnvelopeContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimeUserMessageEnvelopeContext::new(runtime_call, text, attachments);
        UserMessageEnvelopeContext::from_runtime(call, &input)
    }

    pub fn build_pre_tool_use(
        self,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_input: Value,
        approval_mode: ApprovalMode,
        available_tools: Vec<ToolDefinition>,
    ) -> PreToolUseContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimePreToolUseContext::new(
            runtime_call,
            ToolCallId::new(call_id),
            tool_name,
            tool_input,
            approval_mode,
            available_tools,
        );
        PreToolUseContext::from_runtime(call, &input)
    }

    pub fn build_post_tool_use(
        self,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_input: Value,
        tool_result: ToolResult,
    ) -> PostToolUseContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimePostToolUseContext::new(
            runtime_call,
            ToolCallId::new(call_id),
            tool_name,
            tool_input,
            tool_result,
        );
        PostToolUseContext::from_runtime(call, &input)
    }

    pub fn build_provider(self, messages: Vec<LlmMessage>) -> ProviderContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimeProviderContext::new(runtime_call, messages);
        ProviderContext::from_runtime(call, &input)
    }

    pub fn build_prompt(self, tools: Vec<ToolDefinition>) -> PromptBuildContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimePromptBuildContext::new(runtime_call, tools);
        PromptBuildContext::from_runtime(call, &input)
    }

    pub fn build_compact(
        self,
        trigger: CompactTrigger,
        message_count: usize,
        pre_tokens: Option<usize>,
        post_tokens: Option<usize>,
        summary: Option<String>,
    ) -> CompactContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimeCompactContext::new(
            runtime_call,
            trigger,
            message_count,
            pre_tokens,
            post_tokens,
            summary,
        );
        CompactContext::from_runtime(call, &input)
    }

    pub fn build_lifecycle(
        self,
        last_exchange: Option<ExchangeSummary>,
        mid_turn_user_messages_synced: u32,
    ) -> LifecycleContext {
        let (call, runtime_call) = self.into_parts();
        let input = RuntimeLifecycleContext::new(runtime_call, last_exchange)
            .for_step_start(mid_turn_user_messages_synced);
        LifecycleContext::from_runtime(call, &input)
    }

    fn into_parts(self) -> (ExtensionCallContext, RuntimeHookCallContext) {
        let session_store_dir = self.call.session_store_dir.clone();
        let call = self.call.build();
        let session_id = call
            .session_id()
            .map(ToString::to_string)
            .unwrap_or_default();
        let working_dir = call
            .working_dir()
            .map(|path| path.to_path_buf())
            .unwrap_or_default();
        let mut runtime_call =
            RuntimeHookCallContext::new(session_id, working_dir, self.model, session_store_dir)
                .with_cancellation(call.cancellation().clone());
        if let Some(turn_id) = call.turn_id() {
            runtime_call = runtime_call.with_turn_id(turn_id.to_owned());
        }
        (call, runtime_call)
    }
}

/// Builder for a host-attributed [`ToolContext`] used only by extension tests.
///
/// It starts without a session, workspace, capability grant, event sink, or host backend. Tests
/// must opt into every fact relevant to the behavior they exercise.
pub struct ToolContextBuilder {
    call: CallContextBuilder,
    tool_name: String,
    call_id: Option<String>,
    arguments: Value,
    main_model_id: Option<String>,
    small_model_id: Option<String>,
    available_tools: Vec<ToolDefinition>,
}

impl ToolContextBuilder {
    pub fn new(extension_id: impl Into<String>, tool_name: impl Into<String>) -> Self {
        Self {
            call: CallContextBuilder::new(extension_id),
            tool_name: tool_name.into(),
            call_id: None,
            arguments: Value::Null,
            main_model_id: None,
            small_model_id: None,
            available_tools: Vec::new(),
        }
    }

    call_context_builder_methods!();

    pub fn turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.call = self.call.turn_id(turn_id);
        self
    }

    pub fn call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    pub fn arguments(mut self, arguments: Value) -> Self {
        self.arguments = arguments;
        self
    }

    pub fn main_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.main_model_id = Some(model_id.into());
        self
    }

    pub fn small_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.small_model_id = Some(model_id.into());
        self
    }

    pub fn available_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.available_tools = tools;
        self
    }

    pub fn build(self) -> ToolContext {
        let call = self.call.build();
        ToolContext::from_runtime(
            call,
            self.tool_name,
            self.call_id,
            self.arguments,
            self.main_model_id,
            self.small_model_id,
            self.available_tools,
        )
    }
}

struct UnavailableHost;

#[async_trait]
impl HostInvoker for UnavailableHost {
    async fn invoke(&self, operation: HostOperation, _input: Value) -> Result<Value, HostError> {
        Err(HostError::new(
            "backend_unavailable",
            format!(
                "{} backend is unavailable in this test context",
                operation.wire_name()
            ),
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::builder::tool;

    fn scoped_hook_builder() -> HookContextBuilder {
        HookContextBuilder::new("hook-fixture")
            .session(
                "session-1",
                "/workspace",
                Some(PathBuf::from("/sessions/session-1")),
            )
            .turn_id("turn-1")
            .model(ModelSelection::simple("model-1"))
            .global_store_dir("/state")
    }

    fn visible_tool() -> ToolDefinition {
        tool("visible").description("Visible tool").build().into()
    }

    #[test]
    fn hook_context_builder_covers_author_contexts_and_safe_defaults() {
        let cancellation = CancellationToken::new();
        let pre_tool = scoped_hook_builder()
            .cancellation(cancellation.clone())
            .build_pre_tool_use(
                "call-1",
                "shell",
                json!({ "command": "pwd" }),
                ApprovalMode::Manual,
                vec![visible_tool()],
            );
        cancellation.cancel();

        assert_eq!(pre_tool.extension_id(), "hook-fixture");
        assert_eq!(
            pre_tool.session_id().map(SessionId::as_str),
            Some("session-1")
        );
        assert_eq!(pre_tool.turn_id(), Some("turn-1"));
        assert_eq!(pre_tool.working_dir(), Some(Path::new("/workspace")));
        assert_eq!(pre_tool.model().model, "model-1");
        assert_eq!(
            pre_tool.paths().global_data_dir(),
            Some(Path::new("/state/extension_data/hook-fixture"))
        );
        assert_eq!(
            pre_tool.paths().session_data_dir().unwrap(),
            Path::new("/sessions/session-1/extension_data/hook-fixture")
        );
        assert_eq!(pre_tool.call_id().as_str(), "call-1");
        assert_eq!(pre_tool.tool_name(), "shell");
        assert_eq!(pre_tool.tool_input(), &json!({ "command": "pwd" }));
        assert_eq!(pre_tool.approval_mode(), ApprovalMode::Manual);
        assert_eq!(pre_tool.available_tools()[0].name, "visible");
        assert!(pre_tool.cancellation().is_cancelled());

        let continuation = scoped_hook_builder().build_continue_after_stop("done", "stop", 2);
        assert_eq!(continuation.assistant_text(), "done");
        assert_eq!(continuation.finish_reason(), "stop");
        assert_eq!(continuation.continuations_this_turn(), 2);

        let envelope = scoped_hook_builder().build_user_message_envelope(
            "hello",
            vec![MessageAttachment::image_png("screen.png", "encoded")],
        );
        assert_eq!(envelope.text(), "hello");
        assert_eq!(envelope.attachments()[0].filename, "screen.png");

        let post_tool = scoped_hook_builder().build_post_tool_use(
            "call-2",
            "shell",
            json!({ "command": "false" }),
            ToolResult::error("failed"),
        );
        assert_eq!(post_tool.call_id().as_str(), "call-2");
        assert!(post_tool.tool_result().is_error);

        let provider =
            scoped_hook_builder().build_provider(vec![LlmMessage::user("provider input")]);
        assert_eq!(provider.messages().len(), 1);

        let prompt = scoped_hook_builder().build_prompt(vec![visible_tool()]);
        assert_eq!(prompt.tools()[0].name, "visible");

        let compact = scoped_hook_builder().build_compact(
            CompactTrigger::ManualCommand,
            12,
            Some(100),
            Some(40),
            Some("summary".into()),
        );
        assert_eq!(compact.trigger(), CompactTrigger::ManualCommand);
        assert_eq!(compact.message_count(), 12);
        assert_eq!(compact.pre_tokens(), Some(100));
        assert_eq!(compact.post_tokens(), Some(40));
        assert_eq!(compact.summary(), Some("summary"));

        let lifecycle = HookContextBuilder::new("hook-fixture").build_lifecycle(
            Some(ExchangeSummary {
                user_message: "question".into(),
                assistant_message: "answer".into(),
            }),
            3,
        );
        assert_eq!(lifecycle.extension_id(), "hook-fixture");
        assert!(lifecycle.session_id().is_none());
        assert!(lifecycle.working_dir().is_none());
        assert_eq!(lifecycle.model().model, "test-model");
        assert!(lifecycle.paths().global_data_dir().is_none());
        assert!(lifecycle.paths().session_data_dir().is_err());
        assert!(!lifecycle.cancellation().is_cancelled());
        assert_eq!(
            lifecycle
                .last_exchange()
                .map(|exchange| exchange.assistant_message.as_str()),
            Some("answer")
        );
        assert_eq!(lifecycle.mid_turn_user_messages_synced(), 3);
    }
}
