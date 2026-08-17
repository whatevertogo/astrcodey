# astrcodey 扩展系统对外 API 签名清单

> 来源:crates/ 下源码逐文件照抄(函数体已去除,默认实现以 `{ ... }` 标注)。
> 两条作者路径:进程内 bundled 扩展(`astrcode_extension_sdk::prelude`)与磁盘 s5r 子进程扩展(`astrcode_extension_worker::worker_prelude`),共享同一套领域 API。

## 1. `astrcode-extension-sdk/src/extension/lifecycle.rs` — Extension trait

```rust
#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    fn manifest(&self) -> ExtensionManifest;
    fn register(&self, _registrar: &mut Registrar) {}
    fn validate_config(&self, _config: &ExtensionConfig) -> Result<(), ExtensionError> { Ok(()) }
    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> { Ok(()) }
    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> { Ok(()) }
    async fn health(&self) -> Result<(), ExtensionError> { Ok(()) }
}
```

### StartContext / StopContext(实际定义在 `extension/call_context.rs` 与 `extension/runtime.rs`)

```rust
// call_context.rs
pub struct ExtensionStartContext { /* call, tasks, config, startup_working_dir 均私有 */ }

impl ExtensionStartContext {
    pub fn config(&self) -> &ExtensionConfig;
    pub fn tasks(&self) -> &ExtensionTasks;
    pub fn startup_working_dir(&self) -> Option<&Path>;
}
impl ExtensionCall for ExtensionStartContext {
    fn call(&self) -> &ExtensionCallContext;
}

// runtime.rs
pub struct ExtensionStopContext {
    reason: StopReason,
}

impl ExtensionStopContext {
    pub const fn reason(self) -> StopReason;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Reload,
    Disabled,
    Shutdown,
    StartupFailed,
}
```

## 2. `astrcode-extension-sdk/src/extension/registrar.rs` — Registrar

```rust
#[derive(Default)]
pub struct Registrar { /* 字段私有 */ }

impl Registrar {
    pub fn new() -> Self;
    pub fn tool(&mut self, definition: impl Into<ExtensionToolDefinition>, handler: Arc<dyn ToolHandler>);
    pub fn tool_discovery(&mut self, handler: Arc<dyn ToolDiscoveryHandler>);
    pub fn command(&mut self, mut cmd: SlashCommand, handler: Arc<dyn CommandHandler>);
    pub fn command_discovery(&mut self, handler: Arc<dyn CommandDiscoveryHandler>);
    pub fn http_route(&mut self, route: ExtensionHttpRoute, handler: Arc<dyn ExtensionHttpHandler>);
    pub fn keybinding(&mut self, mut binding: Keybinding);
    pub fn status_item(&mut self, mut item: StatusItem);
    pub fn declare_custom_event(&mut self, mut declaration: CustomEventDeclaration);
    pub fn on_custom_event(&mut self, mut subscription: CustomEventSubscription, priority: i32, handler: Arc<dyn CustomEventHandler>);
    pub fn on_tool_input_transform(&mut self, priority: i32, handler: Arc<dyn ToolInputTransformHandler>);
    pub fn on_tool_input_transform_for(&mut self, target: ToolHookTarget, priority: i32, handler: Arc<dyn ToolInputTransformHandler>);
    pub fn on_pre_tool_use(&mut self, priority: i32, handler: Arc<dyn PreToolUseHandler>);
    pub fn on_pre_tool_use_for(&mut self, target: ToolHookTarget, priority: i32, handler: Arc<dyn PreToolUseHandler>);
    pub fn on_post_tool_use(&mut self, mode: HookMode, priority: i32, handler: Arc<dyn PostToolUseHandler>);
    pub fn on_post_tool_use_for(&mut self, target: ToolHookTarget, mode: HookMode, priority: i32, handler: Arc<dyn PostToolUseHandler>);
    pub fn on_before_provider_request(&mut self, mode: HookMode, priority: i32, handler: Arc<dyn ProviderHandler>);
    pub fn on_after_provider_response(&mut self, priority: i32, handler: Arc<dyn ProviderHandler>);
    pub fn on_provider_contribution(&mut self, priority: i32, handler: Arc<dyn ProviderContributionHandler>);
    pub fn on_prompt_build(&mut self, priority: i32, handler: Arc<dyn PromptBuildHandler>);
    pub fn on_pre_compact(&mut self, priority: i32, handler: Arc<dyn PreCompactHandler>);
    pub fn on_post_compact(&mut self, priority: i32, handler: Arc<dyn PostCompactHandler>);
    pub fn on_continue_after_stop(&mut self, priority: i32, options: ContinueAfterStopOptions, handler: Arc<dyn ContinueAfterStopHandler>);
    pub fn on_user_message_envelope(&mut self, priority: i32, handler: Arc<dyn UserMessageEnvelopeHandler>);
    pub fn on_lifecycle(&mut self, event: LifecycleEvent, mode: HookMode, priority: i32, handler: Arc<dyn LifecycleHandler>);

    #[doc(hidden)]
    pub fn finish(self, manifest: ExtensionManifest) -> Result<(ExtensionManifest, ExtensionRegistrations), RegistrationError>;
}
```

`finish` 的校验逻辑(`ExtensionRegistrations::validate`)强制:

- 按注册族要求对应 capability(custom events → `EmitCustomEvents`/`ConsumeCustomEvents`;compact → `SessionHistory`;user_message_envelope / provider → `ProviderRequest`;input transform / pre_tool_use / blocking post_tool_use → `ToolIntercept`;continue_after_stop → `TurnContinuationControl`;host command → `SessionCommand`;http_route 按 access → `PublicHttp`/`AuthenticatedHttp`)。
- observe-only 的 lifecycle event 拒绝 `HookMode::Blocking`。
- tool/command/status item/custom event/subscription id 非空且不重复;声明了 `argument_completions` 的 command 其 handler 必须 `supports_argument_completions()`;keybinding 必须指向已注册静态 command(除非有 command_discovery);custom event 的 `schema_version != 0`、`max_payload_bytes` 在 `1..=MAX_CUSTOM_EVENT_PAYLOAD_BYTES`;HTTP 路由同 access+method 下 path pattern 不得冲突。

### ExtensionRegistrations(finish 产物的只读 accessor)

```rust
#[derive(Default)]
pub struct ExtensionRegistrations { /* 字段私有 */ }

impl ExtensionRegistrations {
    pub fn tools(&self) -> &[ToolRegistration];
    pub fn tool_discoveries(&self) -> &[Arc<dyn ToolDiscoveryHandler>];
    pub fn commands(&self) -> &[(SlashCommand, Arc<dyn CommandHandler>)];
    pub fn command_discoveries(&self) -> &[Arc<dyn CommandDiscoveryHandler>];
    pub fn http_routes(&self) -> &[ExtensionHttpRouteRegistration];
    pub fn tool_input_transforms(&self) -> &[ToolUseRegistration<dyn ToolInputTransformHandler>];
    pub fn pre_tool_use(&self) -> &[ToolUseRegistration<dyn PreToolUseHandler>];
    pub fn post_tool_use(&self) -> &[ToolHookRegistration<dyn PostToolUseHandler>];
    pub fn provider(&self) -> &[(ProviderEvent, HookMode, i32, Arc<dyn ProviderHandler>)];
    pub fn provider_contributions(&self) -> &[(i32, Arc<dyn ProviderContributionHandler>)];
    pub fn prompt_build(&self) -> &[(i32, Arc<dyn PromptBuildHandler>)];
    pub fn pre_compact(&self) -> &[(i32, Arc<dyn PreCompactHandler>)];
    pub fn post_compact(&self) -> &[(i32, Arc<dyn PostCompactHandler>)];
    pub fn continue_after_stop(&self) -> &[ContinueAfterStopRegistration<dyn ContinueAfterStopHandler>];
    pub fn user_message_envelope(&self) -> &[UserMessageEnvelopeRegistration<dyn UserMessageEnvelopeHandler>];
    pub fn lifecycle(&self) -> &[(LifecycleEvent, HookMode, i32, Arc<dyn LifecycleHandler>)];
    pub fn keybindings(&self) -> &[Keybinding];
    pub fn status_items(&self) -> &[StatusItem];
    pub fn custom_event_declarations(&self) -> &[CustomEventDeclaration];
    pub fn custom_event_subscriptions(&self) -> &[CustomEventRegistration];
}
```

### 同文件辅助类型

```rust
#[derive(Clone)]
pub struct CustomEventRegistration {
    pub subscription: CustomEventSubscription,
    pub priority: i32,
    pub handler: Arc<dyn CustomEventHandler>,
}

#[derive(Clone)]
pub struct ToolRegistration { /* 字段私有 */ }
impl ToolRegistration {
    pub fn definition(&self) -> &ToolDefinition;
    pub fn prompt(&self) -> &ToolPromptMetadata;
    pub fn handler(&self) -> &Arc<dyn ToolHandler>;
}

#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    MissingCapability { extension_id: String, registration: &'static str, capability: ExtensionCapability },
    InvalidLifecycleMode { extension_id: String, event: LifecycleEvent },
    Invalid { extension_id: String, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    pub key: String,
    pub command: String,
    #[serde(default)]
    pub arguments: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}
```

## 3. `astrcode-extension-sdk/src/extension/hooks/handlers.rs` — Handler traits

```rust
#[async_trait::async_trait]
pub trait ToolInputTransformHandler: Send + Sync {
    async fn transform(&self, ctx: PreToolUseContext) -> Result<ToolInputTransformResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait PreToolUseHandler: Send + Sync {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait PostToolUseHandler: Send + Sync {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait ProviderHandler: Send + Sync {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait ProviderContributionHandler: Send + Sync {
    async fn prepare(&self, ctx: ProviderContext) -> Result<Option<PreparedProviderContribution>, ExtensionError>;
    async fn acknowledge(&self, ctx: ProviderSettlementContext) -> Result<(), ExtensionError>;
}

#[async_trait::async_trait]
pub trait PromptBuildHandler: Send + Sync {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<super::types::PromptContributions, ExtensionError>;
}

#[async_trait::async_trait]
pub trait PreCompactHandler: Send + Sync {
    async fn handle(&self, ctx: PreCompactContext) -> Result<PreCompactResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait PostCompactHandler: Send + Sync {
    async fn handle(&self, ctx: PostCompactContext) -> Result<(), ExtensionError>;
}

#[async_trait::async_trait]
pub trait LifecycleHandler: Send + Sync {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait ContinueAfterStopHandler: Send + Sync {
    async fn handle(&self, ctx: ContinueAfterStopContext) -> Result<ContinueAfterStopResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait UserMessageEnvelopeHandler: Send + Sync {
    async fn handle(&self, ctx: UserMessageEnvelopeContext) -> Result<UserMessageEnvelopeResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    async fn plan(&self, ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError>;
    async fn execute(&self, ctx: ToolContext) -> Result<ToolExecutionResult, ExtensionError>;
}

#[async_trait::async_trait]
pub trait CommandHandler: Send + Sync {
    async fn execute(&self, ctx: CommandContext) -> Result<ExtensionCommandResult, ExtensionError>;
    async fn complete(&self, _ctx: CommandCompletionContext) -> Result<CommandCompletions, ExtensionError> { Ok(CommandCompletions::default()) }
    fn supports_argument_completions(&self) -> bool { false }
}

#[async_trait::async_trait]
pub trait ToolDiscoveryHandler: Send + Sync {
    async fn discover(&self, ctx: ToolDiscoveryContext) -> Result<ToolDiscovery, ExtensionError>;
}

#[async_trait::async_trait]
pub trait CommandDiscoveryHandler: Send + Sync {
    async fn discover(&self, ctx: CommandDiscoveryContext) -> Result<CommandDiscovery, ExtensionError>;
}
```

同文件的 discovery 结果类型:

```rust
pub struct DiscoveredTool { /* 私有 */ }
impl DiscoveredTool {
    pub fn new(definition: ToolDefinition, handler: Arc<dyn ToolHandler>) -> Self;
    pub fn prompt_metadata(mut self, metadata: ToolPromptMetadata) -> Self;
    pub fn definition(&self) -> &ToolDefinition;
    pub fn handler(&self) -> &Arc<dyn ToolHandler>;
    pub fn prompt(&self) -> Option<&ToolPromptMetadata>;
    pub fn into_parts(self) -> (ToolDefinition, Arc<dyn ToolHandler>, Option<ToolPromptMetadata>);
}

pub struct ToolDiscovery { /* 私有 */ }
impl ToolDiscovery {
    pub fn new(tools: Vec<DiscoveredTool>) -> Self;
    pub fn tools(&self) -> &[DiscoveredTool];
    pub fn into_tools(self) -> Vec<DiscoveredTool>;
}
impl From<Vec<DiscoveredTool>> for ToolDiscovery;

pub struct DiscoveredCommand { /* 私有 */ }
impl DiscoveredCommand {
    pub fn new(command: SlashCommand, handler: Arc<dyn CommandHandler>) -> Self;
    pub fn command(&self) -> &SlashCommand;
    pub fn handler(&self) -> &Arc<dyn CommandHandler>;
    pub fn into_parts(self) -> (SlashCommand, Arc<dyn CommandHandler>);
}

pub struct CommandDiscovery { /* 私有 */ }
impl CommandDiscovery {
    pub fn new(commands: Vec<DiscoveredCommand>) -> Self;
    pub fn commands(&self) -> &[DiscoveredCommand];
    pub fn into_commands(self) -> Vec<DiscoveredCommand>;
}
impl From<Vec<DiscoveredCommand>> for CommandDiscovery;
```

## 4. Context 类型

### `extension/hooks/contexts.rs`

所有 hook context 都是 `HookContext<P>` 的别名;`P` 的 getter 通过 `Deref` 直达。

```rust
pub struct HookContext<P> { /* 私有 */ }
impl<P> HookContext<P> {
    pub fn model(&self) -> &ModelSelection;
    pub fn session_id(&self) -> &SessionId;
    pub fn turn_id(&self) -> Option<&str>;
    pub fn working_dir(&self) -> &Path;
}
impl<P> Deref for HookContext<P> { type Target = P; }
impl<P> ExtensionCall for HookContext<P> { fn call(&self) -> &ExtensionCallContext; }
```

各 Context 别名及其 payload 的 pub 方法:

```rust
pub type ContinueAfterStopContext = HookContext<ContinueAfterStopPayload>;
impl ContinueAfterStopPayload {
    pub fn assistant_text(&self) -> &str;
    pub fn finish_reason(&self) -> &str;
    pub fn continuations_this_turn(&self) -> u32;
}

pub type UserMessageEnvelopeContext = HookContext<UserMessageEnvelopePayload>;
impl UserMessageEnvelopePayload {
    pub fn text(&self) -> &str;
    pub fn attachments(&self) -> &[MessageAttachment];
}

pub type PreToolUseContext = HookContext<PreToolUsePayload>;
impl PreToolUsePayload {
    pub fn call_id(&self) -> &ToolCallId;
    pub fn tool_name(&self) -> &str;
    pub fn tool_input(&self) -> &serde_json::Value;
    pub fn approval_mode(&self) -> crate::permission::ApprovalMode;
    pub fn available_tools(&self) -> &[ToolDefinition];
}

pub type PostToolUseContext = HookContext<PostToolUsePayload>;
impl PostToolUsePayload {
    pub fn call_id(&self) -> &ToolCallId;
    pub fn tool_name(&self) -> &str;
    pub fn tool_input(&self) -> &serde_json::Value;
    pub fn tool_result(&self) -> &ToolResult;
}

pub type ProviderContext = HookContext<ProviderPayload>;
impl ProviderPayload {
    pub fn request_id(&self) -> &ProviderRequestId;
    pub fn shared_messages(&self) -> &[Arc<crate::llm::LlmMessage>];
    #[deprecated(note = "按值复制全量消息;改用 `shared_messages()` 零拷贝访问。本访问器保留一个版本后移除")]
    pub fn messages(&self) -> Vec<crate::llm::LlmMessage>;
}

pub type ProviderSettlementContext = HookContext<ProviderSettlementPayload>;
impl ProviderSettlementPayload {
    pub fn request_id(&self) -> &ProviderRequestId;
    pub fn contribution_id(&self) -> &ProviderContributionId;
}

pub type PromptBuildContext = HookContext<PromptBuildPayload>;
impl PromptBuildPayload {
    pub fn tools(&self) -> &[ToolDefinition];
}

pub type PreCompactContext = HookContext<PreCompactPayload>;
impl PreCompactPayload {
    pub fn trigger(&self) -> CompactTrigger;
    pub fn message_count(&self) -> usize;
    pub fn source_messages(&self) -> &[crate::llm::LlmMessage];
    pub fn retained_file_limit(&self) -> usize;
}

pub type PostCompactContext = HookContext<PostCompactPayload>;
impl PostCompactPayload {
    pub fn trigger(&self) -> CompactTrigger;
    pub fn message_count(&self) -> usize;
    pub fn pre_tokens(&self) -> usize;
    pub fn post_tokens(&self) -> usize;
    pub fn summary(&self) -> &str;
}

pub type LifecycleContext = HookContext<LifecyclePayload>;
impl LifecyclePayload {
    pub fn last_exchange(&self) -> Option<&ExchangeSummary>;
    pub fn mid_turn_user_messages_synced(&self) -> u32;
}

pub struct ToolDiscoveryContext { /* 私有 */ }
impl ToolDiscoveryContext {
    pub fn generation(&self) -> u64;
    pub fn working_dir(&self) -> &Path;
}

pub struct CommandDiscoveryContext { /* 私有 */ }
impl CommandDiscoveryContext {
    pub fn generation(&self) -> u64;
    pub fn working_dir(&self) -> &Path;
}

pub struct CommandContext { /* 私有 */ }
impl CommandContext {
    pub fn model(&self) -> &ModelSelection;
    pub fn command_name(&self) -> &str;
    pub fn argument(&self) -> &str;
    pub fn session_id(&self) -> &SessionId;
    pub fn turn_id(&self) -> Option<&str>;
    pub fn working_dir(&self) -> &Path;
}

pub struct CommandCompletionContext { /* 私有 */ }
impl CommandCompletionContext {
    pub fn model(&self) -> &ModelSelection;
    pub fn command_name(&self) -> &str;
    pub fn argument(&self) -> &str;
    pub fn cursor(&self) -> usize;
    pub fn session_id(&self) -> &SessionId;
    pub fn turn_id(&self) -> Option<&str>;
    pub fn working_dir(&self) -> &Path;
}
```

以上类型均 `impl ExtensionCall`(提供 `call()` 及默认方法 `extension_id()`/`paths()`/`host()`/`events()`/`cancellation()`)。

### `extension/call_context.rs`(所有 context 共享的基座)

```rust
pub struct ExtensionCallContext { /* 私有 */ }
impl ExtensionCallContext {
    pub fn extension_id(&self) -> &str;
    pub fn paths(&self) -> &ExtensionPaths;
    pub fn host(&self) -> &ExtensionHost;
    pub fn events(&self) -> &CustomEventEmitter;
    pub fn cancellation(&self) -> &CancellationToken;
}

pub trait ExtensionCall {
    fn call(&self) -> &ExtensionCallContext;
    fn extension_id(&self) -> &str;
    fn paths(&self) -> &ExtensionPaths;
    fn host(&self) -> &ExtensionHost;
    fn events(&self) -> &CustomEventEmitter;
    fn cancellation(&self) -> &CancellationToken;
}

pub struct WorkspaceCallContext { /* 私有 */ }
impl WorkspaceCallContext {
    pub fn working_dir(&self) -> &Path;
}

pub struct SessionCallContext { /* 私有 */ }
impl SessionCallContext {
    pub fn session_id(&self) -> &SessionId;
    pub fn turn_id(&self) -> Option<&str>;
}
```

### `extension/tool_context.rs` / `tool_plan_context.rs` / `http.rs`

```rust
pub struct ToolContext { /* 私有 */ }
impl ToolContext {
    pub fn tool_name(&self) -> &str;
    pub fn call_id(&self) -> Option<&str>;
    pub fn require_call_id(&self) -> Result<&str, HostError>;
    pub fn raw_arguments(&self) -> &Value;
    pub fn arguments<T: DeserializeOwned>(&self) -> Result<T, ExtensionError>;
    pub fn main_model_id(&self) -> Option<&str>;
    pub fn small_model_id(&self) -> Option<&str>;
    pub fn available_tools(&self) -> &[ToolDefinition];
    pub fn session_id(&self) -> &SessionId;
    pub fn turn_id(&self) -> Option<&str>;
    pub fn working_dir(&self) -> &Path;
}

pub struct ToolPlanContext { /* 私有 */ }
impl ToolPlanContext {
    pub fn extension_id(&self) -> &str;
    pub fn session_id(&self) -> &SessionId;
    pub fn turn_id(&self) -> Option<&str>;
    pub fn working_dir(&self) -> &Path;
    pub fn tool_name(&self) -> &str;
    pub fn call_id(&self) -> Option<&str>;
    pub fn raw_arguments(&self) -> &Value;
    pub fn arguments<T: DeserializeOwned>(&self) -> Result<T, ExtensionError>;
    pub fn cancellation(&self) -> &CancellationToken;
}

pub struct HttpContext { /* 私有 */ }
impl HttpContext {
    pub fn caller_extension_id(&self) -> Option<&str>;
    pub fn route(&self) -> &ExtensionHttpRoute;
    pub fn request(&self) -> &ExtensionHttpRequest;
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, ExtensionError>;
}

#[async_trait::async_trait]
pub trait ExtensionHttpHandler: Send + Sync {
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError>;
}
```

## 5. `host/` 领域 client(`host/domain_client.rs`,泛型 `T: HostClientTransport`,错误为 `T::Error`)

传输 trait:

```rust
#[async_trait]
pub trait HostClientTransport: Clone + Send + Sync {
    type Error;
    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, Self::Error>;
    async fn invoke_stream(&self, operation: HostOperation, _input: Value) -> Result<ModelStream, Self::Error>;
    fn client_error(code: WireErrorCode, message: String) -> Self::Error;
    fn payload_error(error: ErrorPayload) -> Self::Error;
}
```

```rust
impl<T: HostClientTransport> ModelClient<T> {
    pub async fn main_chat(&self, messages: Vec<LlmMessage>) -> Result<HostLlmChatOutput, T::Error>;
    pub async fn main_chat_request(&self, request: HostLlmChatRequest) -> Result<HostLlmChatOutput, T::Error>;
    pub async fn small_chat(&self, messages: Vec<LlmMessage>) -> Result<HostLlmChatOutput, T::Error>;
    pub async fn small_chat_request(&self, request: HostLlmChatRequest) -> Result<HostLlmChatOutput, T::Error>;
    pub async fn main_chat_events(&self, messages: Vec<LlmMessage>) -> Result<ModelStream, T::Error>;
    pub async fn main_chat_events_request(&self, request: HostLlmChatRequest) -> Result<ModelStream, T::Error>;
    pub async fn small_chat_events(&self, messages: Vec<LlmMessage>) -> Result<ModelStream, T::Error>;
    pub async fn small_chat_events_request(&self, request: HostLlmChatRequest) -> Result<ModelStream, T::Error>;
    pub async fn main_chat_collected(&self, messages: Vec<LlmMessage>) -> Result<HostLlmChatOutput, T::Error>;
    pub async fn main_chat_collected_request(&self, request: HostLlmChatRequest) -> Result<HostLlmChatOutput, T::Error>;
    pub async fn small_chat_collected(&self, messages: Vec<LlmMessage>) -> Result<HostLlmChatOutput, T::Error>;
    pub async fn small_chat_collected_request(&self, request: HostLlmChatRequest) -> Result<HostLlmChatOutput, T::Error>;
}
// host/client.rs 中 in-process ModelClient 另有:
// pub fn main_available(&self) -> Result<bool, HostError>;
// pub fn small_available(&self) -> Result<bool, HostError>;

impl<T: HostClientTransport> EventClient<T> {
    pub async fn emit(&self, request: crate::host::HostEventEmitRequest) -> Result<HostEventEmitOutput, T::Error>;
}

impl<T: HostClientTransport> SessionControlClient<T> {
    pub async fn create_root(&self) -> Result<HostCreateSessionOutput, T::Error>;
    pub async fn submit_root_turn(&self, request: HostRootSubmitTurnRequest) -> Result<HostSubmitTurnOutput, T::Error>;
    pub async fn root_state(&self, request: HostSessionTargetRequest) -> Result<HostSessionStateOutput, T::Error>;
    pub async fn inject_or_start(&self, request: HostSessionInputRequest) -> Result<HostSessionDeliveryOutput, T::Error>;
    pub async fn interrupt_and_submit(&self, request: HostSessionInputRequest) -> Result<HostSessionDeliveryOutput, T::Error>;
    pub async fn cancel_turn(&self, request: HostSessionTargetRequest) -> Result<HostSessionCancelOutput, T::Error>;
    pub async fn execution_view(&self, request: HostSessionTargetRequest) -> Result<HostSessionExecutionView, T::Error>;
    pub async fn state(&self, request: HostSessionTargetRequest) -> Result<HostSessionStateOutput, T::Error>;
    pub async fn reactivate(&self, request: HostSessionTargetRequest) -> Result<HostSessionReactivateOutput, T::Error>;
    pub async fn create_child(&self, request: HostCreateSessionRequest) -> Result<HostCreateSessionOutput, T::Error>;
    pub async fn submit_turn(&self, request: HostSubmitTurnRequest) -> Result<HostSubmitTurnOutput, T::Error>;
    pub async fn configure_tools(&self, request: HostConfigureSessionToolsRequest) -> Result<HostConfigureSessionToolsOutput, T::Error>;
    pub async fn recycle(&self, request: HostRecycleSessionRequest) -> Result<(), T::Error>;
}

impl<T: HostClientTransport> SessionHistoryClient<T> {
    pub async fn list_summaries(&self) -> Result<HostSessionSummariesOutput, T::Error>;
    pub async fn transcript(&self, request: HostSessionTargetRequest) -> Result<HostSessionTranscript, T::Error>;
    pub async fn provider_messages(&self, request: HostSessionTargetRequest) -> Result<HostSessionProviderMessagesOutput, T::Error>;
    pub async fn token_usage(&self, request: HostSessionTargetRequest) -> Result<HostSessionTokenUsageOutput, T::Error>;
    pub async fn events_page(&self, request: HostSessionEventsPageRequest) -> Result<HostSessionEventsPageOutput, T::Error>;
    pub async fn snapshot(&self, request: HostSessionTargetRequest) -> Result<SessionHistorySnapshotOutput, T::Error>;
}

impl<T: HostClientTransport> SessionStateClient<T> {
    pub async fn read(&self, request: HostSessionStateReadRequest) -> Result<HostSessionStateReadOutput, T::Error>;
    pub async fn write(&self, request: HostSessionStateWriteRequest) -> Result<(), T::Error>;
}

impl<T: HostClientTransport> SessionInspectClient<T> {
    pub async fn list(&self) -> Result<SessionInspectListOutput, T::Error>;
    pub async fn snapshot(&self, session_id: &str) -> Result<SessionInspectSnapshotOutput, T::Error>;
    pub async fn read_model(&self, session_id: &str) -> Result<SessionInspectReadModelOutput, T::Error>;
    pub async fn provider_messages(&self, session_id: &str) -> Result<SessionInspectProviderMessagesOutput, T::Error>;
}

impl<T: HostClientTransport> ToolResultClient<T> {
    pub async fn read(&self, request: HostToolResultReadRequest) -> Result<HostToolResultReadOutput, T::Error>;
}

impl<T: HostClientTransport> WorkspaceClient<T> {
    pub async fn apply_patch(&self, request: HostWorkspaceApplyPatchRequest) -> Result<HostWorkspaceApplyPatchOutput, T::Error>;
    pub async fn read(&self, request: HostWorkspaceReadRequest) -> Result<HostWorkspaceReadOutput, T::Error>;
    pub async fn write(&self, request: HostWorkspaceWriteRequest) -> Result<HostWorkspaceWriteOutput, T::Error>;
    pub async fn edit(&self, request: HostWorkspaceEditRequest) -> Result<HostWorkspaceEditOutput, T::Error>;
    pub async fn list(&self, request: HostWorkspaceListRequest) -> Result<HostWorkspaceListOutput, T::Error>;
    pub async fn grep(&self, request: HostWorkspaceGrepRequest) -> Result<HostWorkspaceGrepOutput, T::Error>;
    pub async fn glob(&self, request: HostWorkspaceGlobRequest) -> Result<HostWorkspaceGlobOutput, T::Error>;
}

impl<T: HostClientTransport> ProcessClient<T> {
    pub async fn spawn(&self, request: HostProcessRequest) -> Result<HostProcessOutput, T::Error>;
    pub async fn start(&self, request: HostProcessStartRequest) -> Result<HostProcessHandleOutput, T::Error>;
    pub async fn read(&self, request: HostProcessReadRequest) -> Result<HostProcessReadOutput, T::Error>;
    pub async fn write(&self, id: impl Into<String>, input: impl Into<String>) -> Result<(), T::Error>;
    pub async fn close_stdin(&self, id: impl Into<String>) -> Result<(), T::Error>;
    pub async fn status(&self, request: HostProcessTargetRequest) -> Result<HostProcessStatusOutput, T::Error>;
    pub async fn promote(&self, request: HostProcessTargetRequest) -> Result<(), T::Error>;
    pub async fn kill(&self, request: HostProcessTargetRequest) -> Result<(), T::Error>;
    pub async fn list(&self) -> Result<HostProcessListOutput, T::Error>;
}

impl<T: HostClientTransport> NetworkClient<T> {
    pub async fn send(&self, request: HostNetworkRequest) -> Result<HostNetworkResponse, T::Error>;
}

impl<T: HostClientTransport> ExtensionHttpClient<T> {
    pub async fn dispatch_public(&self, request: ExtensionHttpDispatchRequest) -> Result<ExtensionHttpResponse, T::Error>;
}
```

`host/client.rs` 将以上 client 特化为 `ExtensionHost`(in-process),入口在 `host/mod.rs`:

```rust
impl ExtensionHost {
    pub fn models(&self) -> ModelClient;
    pub fn session_control(&self) -> Result<SessionControlClient, HostError>;
    pub fn session_history(&self) -> Result<SessionHistoryClient, HostError>;
    pub fn session_state(&self) -> Result<SessionStateClient, HostError>;
    pub fn session_inspect(&self) -> Result<SessionInspectClient, HostError>;
    pub fn workspace(&self) -> Result<WorkspaceClient, HostError>;
    pub fn tool_results(&self) -> Result<ToolResultClient, HostError>;
    pub fn process(&self) -> Result<ProcessClient, HostError>;
    pub fn network(&self) -> Result<NetworkClient, HostError>;
    pub fn extension_http(&self) -> Result<ExtensionHttpClient, HostError>;
}
```

## 6. events / runtime / paths

### `extension/events.rs`

```rust
pub struct CustomEventContext { /* 私有 */ }
impl CustomEventContext {
    pub fn event_id(&self) -> &EventId;
    pub fn session_id(&self) -> &SessionId;
    pub fn turn_id(&self) -> Option<&str>;
    pub fn seq(&self) -> Option<u64>;
    pub fn source_extension_id(&self) -> &str;
    pub fn event_type(&self) -> &str;
    pub fn schema_version(&self) -> u32;
    pub fn causation_id(&self) -> Option<&EventId>;
    pub fn cascade_depth(&self) -> u8;
    pub fn is_durable(&self) -> bool;
    pub fn payload(&self) -> &serde_json::Value;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomEventDisposition {
    Ack,
    Retry { reason: String },
    DeadLetter { reason: String },
}
impl CustomEventDisposition {
    pub fn retry(reason: impl Into<String>) -> Self;
    pub fn dead_letter(reason: impl Into<String>) -> Self;
}

#[async_trait]
pub trait CustomEventHandler: Send + Sync {
    async fn handle(&self, ctx: CustomEventContext) -> Result<CustomEventDisposition, ExtensionError>;
}

#[derive(Clone, Default)]
pub struct CustomEventEmitter { /* 私有 */ }
impl CustomEventEmitter {
    pub async fn emit<T: Serialize + ?Sized>(&self, event_type: &str, payload: &T) -> Result<EventDeliveryReceipt, CustomEventEmitError>;
    pub fn try_emit<T: Serialize + ?Sized>(&self, event_type: &str, payload: &T) -> Result<(), CustomEventEmitError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CustomEventEmitError {
    Undeclared { event_type: String },
    ContextUnavailable,
    InvalidPayload { event_type: String, message: String },
    PayloadTooLarge { event_type: String, actual_bytes: usize, max_bytes: usize },
    QueueFull { event_type: String },
    IngressClosed { event_type: String },
    Publication { event_type: String, message: String },
}

// re-export:
pub use crate::wire::custom_event::{
    CustomEventDeclaration, CustomEventDelivery, CustomEventSourceFilter, CustomEventSubscription,
    DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES, DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION,
    MAX_CUSTOM_EVENT_PAYLOAD_BYTES, MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN,
};
pub use crate::wire::manifest::LifecycleEvent;
```

### `extension/runtime.rs`

```rust
#[derive(Clone, Debug)]
pub struct ExtensionConfig { /* 私有 */ }
impl ExtensionConfig {
    pub fn deserialize<T: serde::de::DeserializeOwned>(&self) -> Result<T, ExtensionConfigError>;
    pub fn deserialize_or_default<T>(&self) -> Result<T, ExtensionConfigError>
    where T: serde::de::DeserializeOwned + Default;
    pub fn is_empty(&self) -> bool;
}

#[derive(Debug, thiserror::Error)]
pub struct ExtensionConfigError { /* 私有字段 */ }
impl ExtensionConfigError {
    pub fn extension_id(&self) -> &str;
    pub fn path(&self) -> &str;
}

#[derive(Clone)]
pub struct ExtensionTasks { /* 私有 */ }
impl ExtensionTasks {
    pub fn cancellation(&self) -> CancellationToken;
    pub fn spawn<F>(&self, name: impl Into<String>, future: F)
    where F: Future<Output = ()> + Send + 'static;
    pub async fn run_to_completion<F, T>(&self, name: impl Into<String>, future: F) -> Result<T, ExtensionTaskError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionTaskError {
    ShuttingDown { extension_id: String, task: String },
    Panicked { extension_id: String, task: String },
    RuntimeStopped { extension_id: String, task: String },
}
```

### `extension/paths.rs`

```rust
#[derive(Debug, Clone, Default)]
pub struct ExtensionPaths { /* 私有 */ }
impl ExtensionPaths {
    pub fn global_data_dir(&self) -> Option<&Path>;
    pub fn session_data_dir(&self) -> Result<&Path, ExtensionPathError>;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionPathError {
    SessionContextUnavailable,
}
```

## 7. `wire/capability.rs` 与 `wire/manifest.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionCapability {
    SessionControl,        // "session_control"
    SessionCommand,        // "session_command"
    SessionInspect,        // "session_inspect"
    PublicHttp,            // "public_http"
    AuthenticatedHttp,     // "authenticated_http"
    PublicHttpDispatch,    // "public_http_dispatch"
    MainModel,             // "main_model"
    SmallModel,            // "small_model"
    SessionHistory,        // "session_history"
    EmitCustomEvents,      // "emit_custom_events"
    ConsumeCustomEvents,   // "consume_custom_events"
    WorkspaceRead,         // "workspace_read"
    WorkspaceWrite,        // "workspace_write"
    ToolResultRead,        // "tool_result_read"
    ProcessSpawn,          // "process_spawn"
    NetworkClient,         // "network_client"
    ProviderRequest,       // "provider_request"
    InputDelivery,         // "input_delivery"
    ToolIntercept,         // "tool_intercept"
    TurnContinuationControl, // "turn_continuation_control"
    LiveConversation,      // "live_conversation"
}
impl ExtensionCapability {
    pub const fn as_str(self) -> &'static str;
    pub fn parse(name: &str) -> Option<Self>;
}
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    Blocking,
    NonBlocking,
    Advisory,
}
impl HookMode {
    pub const fn as_str(self) -> &'static str;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
    SessionStart,
    SessionResume,
    SessionShutdown,
    TurnStart,
    TurnEnd,
    TurnAborted,
    StepStart,
    StepEnd,
    ToolInputTransform,
    PreToolUse,
    PostToolUse,
    BeforeProviderRequest,
    ProviderContribution,
    AfterProviderResponse,
    ContinueAfterStop,
    UserPromptSubmit,
    UserMessageEnvelope,
    PromptBuild,
    PostRecap,
}
impl LifecycleEvent {
    pub const fn as_str(&self) -> &'static str;
}

// 同文件相关枚举:
pub enum CompactEvent { PreCompact, PostCompact }
pub enum ManifestToolMode { Parallel, #[default] Sequential }
pub enum CommandAvailability { AllTransports, InteractiveOnly }
pub enum CommandExecution { Extension, Host(SessionCommandKind) }
pub enum SessionCommandKind { CompactSession, SelectModel }
pub enum ContinueAfterStopLimit { Limited { max_per_turn: u32 }, Unlimited }
//   with: pub const fn limited(max_per_turn: u32) -> Self;
//         pub const fn unlimited() -> Self;
//         pub const fn allows(self, continuations_this_turn: u32) -> bool;
```

## 8. `builder.rs` — builder

入口函数:

```rust
pub fn manifest(id: impl Into<String>) -> ExtensionManifestBuilder;
pub fn command(name: impl Into<String>) -> SlashCommandBuilder;
pub fn http_route(method: ExtensionHttpMethod, path: impl Into<String>) -> ExtensionHttpRouteBuilder;
pub fn keybinding(key: impl Into<String>, command: impl Into<String>) -> KeybindingBuilder;
pub fn status_item(id: impl Into<String>, text: impl Into<String>) -> StatusItemBuilder;
pub fn custom_event(event_type: impl Into<String>) -> CustomEventDeclarationBuilder;
pub fn tool(name: impl Into<String>) -> ToolDefinitionBuilder;
pub fn worker_tool(name: impl Into<String>) -> ToolDefinitionBuilder; // ToolOrigin::Extension
```

各 builder 的方法(方法名 + 作用):

- `ExtensionManifestBuilder`:`name`(显示名)、`version`、`description`、`capability`(追加 capability,去重)、`requires_transport`(声明 TransportFeature 依赖,去重)、`build` → `ExtensionManifest`、`build_checked` → `Result<ExtensionManifest, ExtensionManifestError>`。
- `SlashCommandBuilder`:`description`、`arguments`(args JSON schema)、`requires_idle`、`argument_completions`、`priority`、`availability`(`CommandAvailability`)、`host_command`(声明为宿主执行的 `SessionCommandKind`)、`build` → `SlashCommand`。
- `ExtensionHttpRouteBuilder`(默认 authenticated):`public`、`authenticated`、`description`、`max_body_bytes`、`build` → `ExtensionHttpRoute`。
- `KeybindingBuilder`:`arguments`、`description`、`build` → `Keybinding`。
- `StatusItemBuilder`:`priority`、`tooltip`、`build` → `StatusItem`。
- `CustomEventDeclarationBuilder`:`schema_version`、`delivery`(`CustomEventDelivery`)、`max_payload_bytes`、`build` → `CustomEventDeclaration`。
- `ToolDefinitionBuilder`:`description`、`parameters`(JSON schema)、`strict`(provider 侧严格 schema 约束)、`non_strict`(默认)、`execution_mode`(`ExecutionMode`)、`prompt`(`ToolPromptMetadata`)、`build` → `ExtensionToolDefinition`。
- `ExtensionToolDefinition`:`from_definition`、`with_prompt`、`definition()`、`prompt()`、`into_parts()`;`Deref<Target = ToolDefinition>`;与 `ToolDefinition` 双向 `From`。

闭包适配器:

```rust
pub fn command_handler<F, Fut>(f: F) -> Arc<dyn CommandHandler>
where
    F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExtensionCommandResult, ExtensionError>> + Send + 'static;

pub fn http_handler<F, Fut>(f: F) -> Arc<dyn ExtensionHttpHandler>
where
    F: Fn(HttpContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExtensionHttpResponse, ExtensionError>> + Send + 'static;

pub fn tool_handler<P, PlanFut, F, Fut, R>(planner: P, f: F) -> Arc<dyn ToolHandler>
where
    P: Fn(ToolPlanContext) -> PlanFut + Send + Sync + 'static,
    PlanFut: Future<Output = Result<ToolPlan, ExtensionError>> + Send + 'static,
    F: Fn(ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ExtensionError>> + Send + 'static,
    R: Into<ToolExecutionResult> + Send + 'static;

pub fn tool_handler_args<A, P, PlanFut, F, Fut, R>(planner: P, f: F) -> Arc<dyn ToolHandler>
where
    A: DeserializeOwned + Send + 'static,
    P: Fn(A, ToolPlanContext) -> PlanFut + Send + Sync + 'static,
    PlanFut: Future<Output = Result<ToolPlan, ExtensionError>> + Send + 'static,
    F: Fn(A, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ExtensionError>> + Send + 'static,
    R: Into<ToolExecutionResult> + Send + 'static;

pub fn continue_after_stop_handler_fn<F, Fut>(f: F) -> Arc<dyn crate::extension::ContinueAfterStopHandler>
where
    F: Fn(ContinueAfterStopContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ContinueAfterStopResult, ExtensionError>> + Send + 'static;
```

## 9. `astrcode-extension-worker`

### `worker/mod.rs` — Worker 注册方法

```rust
pub struct Worker { /* version, registry, activation 私有 */ }

impl Worker {
    pub fn new(extension_id: impl Into<String>, version: impl Into<String>) -> Self;

    pub fn on_activate<F, Fut>(&mut self, handler: F) -> &mut Self
    where
        F: FnOnce(serde_json::Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), ErrorPayload>> + Send + 'static;

    pub fn capability(&mut self, cap: ExtensionCapability) -> &mut Self;
    pub fn require_transport(&mut self, feature: TransportFeature) -> &mut Self;
    pub fn custom_event(&mut self, event: crate::extension::CustomEventDeclaration) -> &mut Self;

    pub fn on_custom_event(&mut self, subscription: CustomEventSubscription, handler: CustomEventHandlerFn) -> Result<&mut Self, ErrorPayload>;

    pub fn tool(&mut self, def: impl Into<ToolDefinition>, planner: ToolPlannerFn, handler: ToolHandlerFn) -> Result<&mut Self, ErrorPayload>;

    pub fn hook(&mut self, on: LifecycleEvent, mode: HookMode, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;
    pub fn on_tool_input_transform(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;   // 固定 blocking
    pub fn on_pre_tool_use(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;          // 固定 blocking
    pub fn on_after_provider_response(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>; // 固定 advisory
    pub fn on_provider_contribution(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;
    pub fn on_prompt_build(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;
    pub fn on_pre_compact(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;
    pub fn on_post_compact(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;
    pub fn continuation_hook_handler(&mut self, on: impl Into<String>, handler: ContinuationHandlerFn) -> Result<&mut Self, ErrorPayload>;
    pub fn on_continue_after_stop(&mut self, options: ContinueAfterStopOptions, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload>;

    pub fn command(&mut self, command: crate::extension::SlashCommand, handler: CommandHandlerFn) -> Result<&mut Self, ErrorPayload>;
    pub fn http_route(&mut self, route: crate::extension::ExtensionHttpRoute, handler: HttpHandlerFn) -> Result<&mut Self, ErrorPayload>;

    pub async fn run_stdio(mut self) -> Result<(), ErrorPayload>;
}

pub fn tool_text(content: impl Into<String>, is_error: bool) -> HandlerResult;
```

worker 的 builder 适配器(`worker/builder.rs`,经 `worker/mod.rs` re-export):

```rust
pub fn parse_tool_arguments<T: DeserializeOwned>(arguments: Value) -> Result<T, ErrorPayload>;
pub fn parse_hook_input<T: DeserializeOwned>(event: &Value) -> Result<T, ErrorPayload>;
pub fn tool_handler<F, Fut>(f: F) -> ToolHandlerFn            where F: Fn(WorkerInvocationContext) -> Fut ...;
pub fn tool_handler_args<A, F, Fut>(f: F) -> ToolHandlerFn    where A: DeserializeOwned ..., F: Fn(A, WorkerInvocationContext) -> Fut ...;
pub fn tool_planner<F, Fut>(f: F) -> ToolPlannerFn            where F: Fn(WorkerToolPlanContext) -> Fut<Output = Result<ToolPlan, ErrorPayload>> ...;
pub fn tool_planner_args<A, F, Fut>(f: F) -> ToolPlannerFn    where A: DeserializeOwned ..., F: Fn(A, WorkerToolPlanContext) -> Fut ...;
pub fn hook_handler<F, Fut>(f: F) -> HookHandlerFn            where F: Fn(WorkerInvocationContext) -> Fut ...;
pub fn hook_handler_args<A, F, Fut>(f: F) -> HookHandlerFn    where A: DeserializeOwned ..., F: Fn(A, WorkerInvocationContext) -> Fut ...;
pub fn command_handler<F, Fut>(f: F) -> CommandHandlerFn      where F: Fn(WorkerCommandContext) -> Fut ...;
pub fn continuation_handler<F, Fut>(f: F) -> ContinuationHandlerFn       where F: Fn(WorkerCallContext) -> Fut ...;
pub fn continuation_handler_args<A, F, Fut>(f: F) -> ContinuationHandlerFn where A: DeserializeOwned ..., F: Fn(A, WorkerCallContext) -> Fut ...;
pub fn custom_event_handler<F, Fut>(f: F) -> CustomEventHandlerFn        where F: Fn(WorkerCustomEventContext) -> Fut ...;
pub fn custom_event_handler_args<A, F, Fut>(f: F) -> CustomEventHandlerFn where A: DeserializeOwned ..., F: Fn(A, WorkerCustomEventContext) -> Fut ...;
pub fn http_handler<F, Fut>(f: F) -> HttpHandlerFn            where F: Fn(ExtensionHttpRequest, WorkerCallContext) -> Fut<Output = Result<ExtensionHttpResponse, ErrorPayload>> ...;
```

(以上所有 `Fut` 的完整约束为 `Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static`,`F` 均 `+ Send + Sync + 'static`。)

### `worker/host.rs` — HostClient 入口

```rust
pub struct HostClient;

impl HostClient {
    pub fn host_supports(operation: HostOperation) -> Result<bool, ErrorPayload>;
    pub const fn events() -> EventClient;
    pub const fn models() -> ModelClient;
    pub const fn session_control() -> SessionControlClient;
    pub const fn session_history() -> SessionHistoryClient;
    pub const fn session_state() -> SessionStateClient;
    pub const fn session_inspect() -> SessionInspectClient;
    pub const fn workspace() -> WorkspaceClient;
    pub const fn tool_results() -> ToolResultClient;
    pub const fn process() -> ProcessClient;
    pub const fn network() -> NetworkClient;
    pub const fn extension_http() -> ExtensionHttpClient;
}
```

这些 client 都是 `Typed*Client<WorkerHostTransport>` 的别名,方法与第 5 节领域 client 完全一致(`T::Error = ErrorPayload`)。`HostClient` 只在处理一次入站 invocation 的 task-local 作用域内可用,脱离作用域调用返回 `WireErrorCode::ContextUnavailable`。

## 10. SDK 顶层 re-export(`src/lib.rs`)

顶层模块:`discovery`、`extension`、`frontmatter`、`hostpaths`、`shell`、`builder`、`host`、`manifest`、`model_stream`、`runtime_ports`、`s5r`、`session`、`transport`、`wire`,以及命名空间模块:

```rust
pub mod config { pub use astrcode_core::config::ModelSelection; }
pub mod llm { pub use astrcode_core::llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRequest, LlmRole, LlmTokenUsage, ModelLimits, collect_stream_text}; }
pub mod event { pub use astrcode_core::event::{Event, EventDeliveryReceipt, EventPayload, EventSendError, EventSender}; }
pub mod tool { pub use astrcode_core::tool::{ExecutionMode, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPromptMetadata, ToolPromptTag, ToolResult, access::{FileOperation, HostResource, ResourceAccess, ToolPlan}, read_image::ReadToolInlinePayload, tool_metadata}; pub use crate::extension::{ToolContext, ToolPlanContext}; }
pub mod types { pub use astrcode_core::types::{SessionId, ToolCallId, project_key_from_path}; }
pub mod permission { pub use astrcode_core::permission::{ApprovalDecision, ApprovalMode}; }
pub use wire::WireErrorCode;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
```

`prelude`(in-process bundled extension 编写面)导出 `builder::*` 全部 builder 与闭包适配器、`extension::*` 全部 trait/context/结果类型、`host::*` 全部领域 client 与 request/output 类型、`model_stream::{ModelStream, ModelStreamEvent}`、`session::*` 与 `wire::session_inspect::*` 的 DTO、`tool::*` 与 `types::{SessionId, ToolCallId}`。

## 备注与边界

- `hooks/results.rs`(`PreToolUseResult`、`PostToolUseResult`、`HookResult` 等返回类型的构造器)与 `hooks/commands.rs`(`SlashCommand`、`ExtensionCommandResult`、`CommandCompletions`)、`extension/http.rs` 的 `ExtensionHttpRequest/Response/Route` 未逐字段展开,需要可再补。
- `RuntimeHookCallContext`、`HookInput` 标注了 `#[doc(hidden)]`,属于 runtime 内部接缝,上面只列了 `HookContext` 的公开面。
