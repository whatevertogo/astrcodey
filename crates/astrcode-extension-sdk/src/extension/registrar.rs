use std::{collections::HashSet, sync::Arc};

use serde::{Deserialize, Serialize};

use super::{
    CommandDiscoveryHandler, CommandHandler, CompactEvent, CompactHandler,
    ContinueAfterStopHandler, ContinueAfterStopOptions, ContinueAfterStopRegistration,
    ExtensionCapability, ExtensionEvent, ExtensionEventDecl, ExtensionHttpAccess,
    ExtensionHttpHandler, ExtensionHttpRoute, ExtensionHttpRouteRegistration, ExtensionManifest,
    HookMode, LifecycleHandler, MAX_EXTENSION_EVENT_PAYLOAD_BYTES, PostToolUseHandler,
    PreToolUseHandler, PromptBuildHandler, ProviderEvent, ProviderHandler, SlashCommand,
    ToolDiscoveryHandler, ToolHandler, ToolHookRegistration, ToolHookTarget,
    UserMessageEnvelopeHandler, UserMessageEnvelopeRegistration,
    extension_http_route_patterns_conflict, lifecycle_event_allows_blocking,
};
use crate::{
    builder::ExtensionToolDefinition,
    tool::{ToolDefinition, ToolPromptMetadata},
};

// ─── Registrar ───────────────────────────────────────────────────

/// 扩展能力注册器。
///
/// 在 `Extension::register()` 调用期间有效，扩展通过它声明自己提供的能力。
///
/// 字段全部私有，外部只能通过 `tool` / `command` / `on_pre_tool_use` 等
/// 写入方法和 `tools()` / `commands()` 等读取 accessor 访问。这样保证：
/// 1. 扩展作者只能用受控 API 注册能力，无法旁路构造非法状态；
/// 2. 字段重构（合并、增加索引）不会破坏外部代码；
/// 3. `Registrar` 只在 `Extension::register()` 生命周期内有效，私有字段
///    阻止外部把它当成长寿数据持有。
#[derive(Default)]
pub struct Registrar {
    registrations: ExtensionRegistrations,
}

/// Immutable declaration and handler aggregate produced by [`Registrar::finish`].
///
/// Runtime indexes must be derived from this value instead of maintaining registration-family
/// vectors alongside it.
#[derive(Default)]
pub struct ExtensionRegistrations {
    tools: Vec<ToolRegistration>,
    tool_discovery: Vec<Arc<dyn ToolDiscoveryHandler>>,
    commands: Vec<(SlashCommand, Arc<dyn CommandHandler>)>,
    command_discovery: Vec<Arc<dyn CommandDiscoveryHandler>>,
    http_routes: Vec<ExtensionHttpRouteRegistration>,
    keybindings: Vec<Keybinding>,
    status_items: Vec<StatusItem>,
    pre_tool_use: Vec<ToolHookRegistration<dyn PreToolUseHandler>>,
    post_tool_use: Vec<ToolHookRegistration<dyn PostToolUseHandler>>,
    provider: Vec<(ProviderEvent, HookMode, i32, Arc<dyn ProviderHandler>)>,
    prompt_build: Vec<(i32, Arc<dyn PromptBuildHandler>)>,
    compact: Vec<(CompactEvent, i32, Arc<dyn CompactHandler>)>,
    continue_after_stop: Vec<ContinueAfterStopRegistration<dyn ContinueAfterStopHandler>>,
    user_message_envelope: Vec<UserMessageEnvelopeRegistration<dyn UserMessageEnvelopeHandler>>,
    lifecycle: Vec<(ExtensionEvent, HookMode, i32, Arc<dyn LifecycleHandler>)>,
    extension_event_decls: Vec<ExtensionEventDecl>,
}

#[derive(Clone)]
pub struct ToolRegistration {
    definition: ToolDefinition,
    prompt: ToolPromptMetadata,
    handler: Arc<dyn ToolHandler>,
}

impl ToolRegistration {
    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    pub fn prompt(&self) -> &ToolPromptMetadata {
        &self.prompt
    }

    pub fn handler(&self) -> &Arc<dyn ToolHandler> {
        &self.handler
    }
}

impl Registrar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool(
        &mut self,
        definition: impl Into<ExtensionToolDefinition>,
        handler: Arc<dyn ToolHandler>,
    ) {
        let (mut definition, prompt) = definition.into().into_parts();
        definition.name = definition.name.trim().to_owned();
        self.registrations.tools.push(ToolRegistration {
            definition,
            prompt,
            handler,
        });
    }

    pub fn tool_discovery(&mut self, handler: Arc<dyn ToolDiscoveryHandler>) {
        self.registrations.tool_discovery.push(handler);
    }

    pub fn command(&mut self, mut cmd: SlashCommand, handler: Arc<dyn CommandHandler>) {
        cmd.name = cmd.name.trim().to_owned();
        self.registrations.commands.push((cmd, handler));
    }

    pub fn command_discovery(&mut self, handler: Arc<dyn CommandDiscoveryHandler>) {
        self.registrations.command_discovery.push(handler);
    }

    pub fn http_route(
        &mut self,
        route: ExtensionHttpRoute,
        handler: Arc<dyn ExtensionHttpHandler>,
    ) {
        self.registrations
            .http_routes
            .push(ExtensionHttpRouteRegistration { route, handler });
    }

    pub fn keybinding(&mut self, mut binding: Keybinding) {
        binding.key = binding.key.trim().to_owned();
        binding.command = binding.command.trim().to_owned();
        self.registrations.keybindings.push(binding);
    }

    pub fn status_item(&mut self, mut item: StatusItem) {
        item.id = item.id.trim().to_owned();
        self.registrations.status_items.push(item);
    }

    pub fn declare_event(&mut self, mut declaration: ExtensionEventDecl) {
        declaration.event_type = declaration.event_type.trim().to_owned();
        self.registrations.extension_event_decls.push(declaration);
    }

    pub fn on_pre_tool_use(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PreToolUseHandler>,
    ) {
        self.on_pre_tool_use_for(ToolHookTarget::All, mode, priority, handler);
    }

    pub fn on_pre_tool_use_for(
        &mut self,
        target: ToolHookTarget,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PreToolUseHandler>,
    ) {
        self.registrations.pre_tool_use.push(ToolHookRegistration {
            mode,
            priority,
            target,
            handler,
        });
    }

    pub fn on_post_tool_use(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PostToolUseHandler>,
    ) {
        self.on_post_tool_use_for(ToolHookTarget::All, mode, priority, handler);
    }

    pub fn on_post_tool_use_for(
        &mut self,
        target: ToolHookTarget,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn PostToolUseHandler>,
    ) {
        self.registrations.post_tool_use.push(ToolHookRegistration {
            mode,
            priority,
            target,
            handler,
        });
    }

    /// 注册 provider request hook。
    ///
    /// Request 阶段允许 `Blocking` handler 阻断请求或改写 messages。
    pub fn on_before_provider_request(
        &mut self,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn ProviderHandler>,
    ) {
        self.registrations
            .provider
            .push((ProviderEvent::BeforeRequest, mode, priority, handler));
    }

    /// 注册 provider response observer。
    ///
    /// Response 阶段只观察结果，不允许阻断或改写后续流程。
    pub fn on_after_provider_response(&mut self, priority: i32, handler: Arc<dyn ProviderHandler>) {
        self.registrations.provider.push((
            ProviderEvent::AfterResponse,
            HookMode::Advisory,
            priority,
            handler,
        ));
    }

    pub fn on_prompt_build(&mut self, priority: i32, handler: Arc<dyn PromptBuildHandler>) {
        self.registrations.prompt_build.push((priority, handler));
    }

    pub fn on_compact(
        &mut self,
        event: CompactEvent,
        priority: i32,
        handler: Arc<dyn CompactHandler>,
    ) {
        self.registrations.compact.push((event, priority, handler));
    }

    pub fn on_continue_after_stop(
        &mut self,
        priority: i32,
        options: ContinueAfterStopOptions,
        handler: Arc<dyn ContinueAfterStopHandler>,
    ) {
        self.registrations
            .continue_after_stop
            .push(ContinueAfterStopRegistration {
                priority,
                options,
                handler,
            });
    }

    pub fn on_user_message_envelope(
        &mut self,
        priority: i32,
        handler: Arc<dyn UserMessageEnvelopeHandler>,
    ) {
        self.registrations
            .user_message_envelope
            .push(UserMessageEnvelopeRegistration { priority, handler });
    }

    pub fn on_lifecycle(
        &mut self,
        event: ExtensionEvent,
        mode: HookMode,
        priority: i32,
        handler: Arc<dyn LifecycleHandler>,
    ) {
        self.registrations
            .lifecycle
            .push((event, mode, priority, handler));
    }

    #[doc(hidden)]
    pub fn finish(
        self,
        manifest: ExtensionManifest,
    ) -> Result<(ExtensionManifest, ExtensionRegistrations), RegistrationError> {
        manifest
            .validate()
            .map_err(|error| invalid_registration(manifest.id(), error.to_string()))?;
        self.registrations.validate(&manifest)?;
        Ok((manifest, self.registrations))
    }
}

impl ExtensionRegistrations {
    pub fn tools(&self) -> &[ToolRegistration] {
        &self.tools
    }

    pub fn tool_discoveries(&self) -> &[Arc<dyn ToolDiscoveryHandler>] {
        &self.tool_discovery
    }

    pub fn commands(&self) -> &[(SlashCommand, Arc<dyn CommandHandler>)] {
        &self.commands
    }

    pub fn command_discoveries(&self) -> &[Arc<dyn CommandDiscoveryHandler>] {
        &self.command_discovery
    }

    pub fn http_routes(&self) -> &[ExtensionHttpRouteRegistration] {
        &self.http_routes
    }

    pub fn pre_tool_use(&self) -> &[ToolHookRegistration<dyn PreToolUseHandler>] {
        &self.pre_tool_use
    }

    pub fn post_tool_use(&self) -> &[ToolHookRegistration<dyn PostToolUseHandler>] {
        &self.post_tool_use
    }

    pub fn provider(&self) -> &[(ProviderEvent, HookMode, i32, Arc<dyn ProviderHandler>)] {
        &self.provider
    }

    pub fn prompt_build(&self) -> &[(i32, Arc<dyn PromptBuildHandler>)] {
        &self.prompt_build
    }

    pub fn compact(&self) -> &[(CompactEvent, i32, Arc<dyn CompactHandler>)] {
        &self.compact
    }

    pub fn continue_after_stop(
        &self,
    ) -> &[ContinueAfterStopRegistration<dyn ContinueAfterStopHandler>] {
        &self.continue_after_stop
    }

    pub fn user_message_envelope(
        &self,
    ) -> &[UserMessageEnvelopeRegistration<dyn UserMessageEnvelopeHandler>] {
        &self.user_message_envelope
    }

    pub fn lifecycle(&self) -> &[(ExtensionEvent, HookMode, i32, Arc<dyn LifecycleHandler>)] {
        &self.lifecycle
    }

    pub fn keybindings(&self) -> &[Keybinding] {
        &self.keybindings
    }

    pub fn status_items(&self) -> &[StatusItem] {
        &self.status_items
    }

    pub fn extension_event_decls(&self) -> &[ExtensionEventDecl] {
        &self.extension_event_decls
    }

    fn validate(&self, manifest: &ExtensionManifest) -> Result<(), RegistrationError> {
        let extension_id = manifest.id();
        let capabilities = manifest.capabilities();

        require_capability(
            extension_id,
            capabilities,
            !self.extension_event_decls.is_empty(),
            "event",
            ExtensionCapability::EmitEvents,
        )?;
        require_capability(
            extension_id,
            capabilities,
            !self.compact.is_empty(),
            "compact",
            ExtensionCapability::SessionHistory,
        )?;
        require_capability(
            extension_id,
            capabilities,
            !self.user_message_envelope.is_empty(),
            "user_message_envelope",
            ExtensionCapability::ProviderRequest,
        )?;
        require_capability(
            extension_id,
            capabilities,
            !self.provider.is_empty(),
            "provider",
            ExtensionCapability::ProviderRequest,
        )?;
        require_capability(
            extension_id,
            capabilities,
            self.pre_tool_use
                .iter()
                .any(|registration| registration.mode == HookMode::Blocking),
            "pre_tool_use",
            ExtensionCapability::ToolIntercept,
        )?;
        require_capability(
            extension_id,
            capabilities,
            self.post_tool_use
                .iter()
                .any(|registration| registration.mode == HookMode::Blocking),
            "post_tool_use",
            ExtensionCapability::ToolIntercept,
        )?;
        require_capability(
            extension_id,
            capabilities,
            !self.continue_after_stop.is_empty(),
            "continue_after_stop",
            ExtensionCapability::TurnContinuationControl,
        )?;

        for (event, mode, _, _) in &self.lifecycle {
            if *mode == HookMode::Blocking && !lifecycle_event_allows_blocking(event) {
                return Err(RegistrationError::InvalidLifecycleMode {
                    extension_id: extension_id.to_owned(),
                    event: event.clone(),
                });
            }
        }

        let mut tool_names = HashSet::new();
        for registration in &self.tools {
            let name = registration.definition.name.as_str();
            if name.is_empty() {
                return Err(invalid_registration(
                    extension_id,
                    "tool name cannot be empty",
                ));
            }
            if !tool_names.insert(name) {
                return Err(invalid_registration(
                    extension_id,
                    format!("duplicate tool `{name}`"),
                ));
            }
        }
        let mut command_names = HashSet::new();
        for (command, handler) in &self.commands {
            let name = command.name.as_str();
            if name.is_empty() {
                return Err(invalid_registration(
                    extension_id,
                    "command name cannot be empty",
                ));
            }
            if !command_names.insert(name) {
                return Err(invalid_registration(
                    extension_id,
                    format!("duplicate command `{name}`"),
                ));
            }
            if command.argument_completions && !handler.supports_argument_completions() {
                return Err(invalid_registration(
                    extension_id,
                    format!(
                        "command `{name}` declares argument completions, but its handler does not \
                         support them"
                    ),
                ));
            }
        }
        for binding in &self.keybindings {
            if binding.key.trim().is_empty() {
                return Err(invalid_registration(
                    extension_id,
                    "keybinding key cannot be empty",
                ));
            }
            if !command_names.contains(binding.command.as_str())
                && self.command_discovery.is_empty()
            {
                return Err(invalid_registration(
                    extension_id,
                    format!(
                        "keybinding `{}` targets unknown static command `{}`",
                        binding.key, binding.command
                    ),
                ));
            }
        }

        let mut status_ids = HashSet::new();
        for item in &self.status_items {
            let id = item.id.as_str();
            if id.is_empty() || !status_ids.insert(id) {
                return Err(invalid_registration(
                    extension_id,
                    format!("invalid or duplicate status item id `{id}`"),
                ));
            }
        }

        let mut event_types = HashSet::new();
        for event in &self.extension_event_decls {
            let event_type = event.event_type.as_str();
            if event_type.is_empty() || !event_types.insert(event_type) {
                return Err(invalid_registration(
                    extension_id,
                    format!("invalid or duplicate extension event `{event_type}`"),
                ));
            }
            if event.schema_version == 0 {
                return Err(invalid_registration(
                    extension_id,
                    format!("extension event `{event_type}` requires a non-zero schema version"),
                ));
            }
            if !(1..=MAX_EXTENSION_EVENT_PAYLOAD_BYTES).contains(&event.max_payload_bytes) {
                return Err(invalid_registration(
                    extension_id,
                    format!(
                        "extension event `{event_type}` payload limit must be between 1 and \
                         {MAX_EXTENSION_EVENT_PAYLOAD_BYTES} bytes"
                    ),
                ));
            }
        }

        for (index, registration) in self.http_routes.iter().enumerate() {
            let route = &registration.route;
            route
                .validate()
                .map_err(|reason| invalid_registration(extension_id, reason))?;
            let capability = match route.access {
                ExtensionHttpAccess::Public => ExtensionCapability::PublicHttp,
                ExtensionHttpAccess::Authenticated => ExtensionCapability::AuthenticatedHttp,
            };
            require_capability(extension_id, capabilities, true, "http_route", capability)?;
            if self.http_routes[..index].iter().any(|existing| {
                existing.route.access == route.access
                    && existing.route.method == route.method
                    && extension_http_route_patterns_conflict(&existing.route.path, &route.path)
            }) {
                return Err(invalid_registration(
                    extension_id,
                    format!("conflicting HTTP route `{}`", route.path),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    #[error("extension {extension_id} registered {registration} without declaring {capability:?}")]
    MissingCapability {
        extension_id: String,
        registration: &'static str,
        capability: ExtensionCapability,
    },
    #[error(
        "extension {extension_id} registered lifecycle {event:?} with blocking mode, but the \
         event is observe-only"
    )]
    InvalidLifecycleMode {
        extension_id: String,
        event: ExtensionEvent,
    },
    #[error("extension {extension_id} has an invalid registration: {reason}")]
    Invalid {
        extension_id: String,
        reason: String,
    },
}

impl From<RegistrationError> for super::ExtensionError {
    fn from(error: RegistrationError) -> Self {
        match error {
            RegistrationError::MissingCapability {
                extension_id,
                registration,
                capability,
            } => Self::MissingCapability {
                extension_id,
                hook: registration,
                capability,
            },
            RegistrationError::InvalidLifecycleMode {
                extension_id,
                event,
            } => Self::InvalidLifecycleMode {
                extension_id,
                event,
            },
            RegistrationError::Invalid {
                extension_id,
                reason,
            } => Self::InvalidRegistration {
                extension_id,
                reason,
            },
        }
    }
}

fn require_capability(
    extension_id: &str,
    capabilities: &[ExtensionCapability],
    registration_present: bool,
    registration: &'static str,
    capability: ExtensionCapability,
) -> Result<(), RegistrationError> {
    if registration_present && !capabilities.contains(&capability) {
        return Err(RegistrationError::MissingCapability {
            extension_id: extension_id.to_owned(),
            registration,
            capability,
        });
    }
    Ok(())
}

fn invalid_registration(extension_id: &str, reason: impl Into<String>) -> RegistrationError {
    RegistrationError::Invalid {
        extension_id: extension_id.to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        builder::{command, command_handler, manifest, tool, tool_handler},
        extension::{
            CommandDiscovery, CommandDiscoveryContext, ExtensionCommandResult, ExtensionError,
        },
        tool::ToolResult,
    };

    struct EmptyCommandDiscovery;

    #[async_trait::async_trait]
    impl CommandDiscoveryHandler for EmptyCommandDiscovery {
        async fn discover(
            &self,
            _ctx: CommandDiscoveryContext,
        ) -> Result<CommandDiscovery, ExtensionError> {
            Ok(CommandDiscovery::new(Vec::new()))
        }
    }

    fn finish_with_event_limit(max_payload_bytes: usize) -> Result<(), RegistrationError> {
        let mut registrar = Registrar::new();
        registrar.declare_event(ExtensionEventDecl {
            event_type: "test.completed".into(),
            schema_version: 1,
            durable: true,
            max_payload_bytes,
        });
        registrar
            .finish(
                manifest("event-limit-test")
                    .version("1.0.0")
                    .capability(ExtensionCapability::EmitEvents)
                    .build(),
            )
            .map(|_| ())
    }

    #[test]
    fn event_payload_limit_accepts_host_ceiling_only() {
        for (max_payload_bytes, accepted) in [
            (0, false),
            (MAX_EXTENSION_EVENT_PAYLOAD_BYTES, true),
            (MAX_EXTENSION_EVENT_PAYLOAD_BYTES + 1, false),
        ] {
            assert_eq!(
                finish_with_event_limit(max_payload_bytes).is_ok(),
                accepted,
                "unexpected validation result for {max_payload_bytes} bytes"
            );
        }
    }

    #[test]
    fn registration_names_are_stored_in_canonical_form() {
        let mut registrar = Registrar::new();
        registrar.tool(
            tool("  review  ")
                .parameters(serde_json::json!({"type": "object"}))
                .build(),
            tool_handler(|_| async { Ok(ToolResult::success("ok")) }),
        );
        registrar.command(
            command("  inspect  ").build(),
            command_handler(|_| async { Ok(ExtensionCommandResult::handled("ok")) }),
        );
        registrar.keybinding(Keybinding {
            key: "  ctrl+i  ".into(),
            command: "  inspect  ".into(),
            arguments: String::new(),
            description: String::new(),
        });
        registrar.status_item(StatusItem {
            id: "  state  ".into(),
            text: String::new(),
            priority: 0,
            tooltip: None,
        });
        registrar.declare_event(ExtensionEventDecl {
            event_type: "  review.completed  ".into(),
            schema_version: 1,
            durable: true,
            max_payload_bytes: 1024,
        });

        let (_, registrations) = registrar
            .finish(
                manifest("canonical-registration-test")
                    .version("1.0.0")
                    .capability(ExtensionCapability::EmitEvents)
                    .build(),
            )
            .expect("canonical registrations");

        assert_eq!(registrations.tools()[0].definition().name, "review");
        assert_eq!(registrations.commands()[0].0.name, "inspect");
        assert_eq!(registrations.keybindings()[0].key, "ctrl+i");
        assert_eq!(registrations.keybindings()[0].command, "inspect");
        assert_eq!(registrations.status_items()[0].id, "state");
        assert_eq!(
            registrations.extension_event_decls()[0].event_type,
            "review.completed"
        );
    }

    #[test]
    fn keybinding_may_target_a_dynamically_discovered_command() {
        let mut registrar = Registrar::new();
        registrar.command_discovery(Arc::new(EmptyCommandDiscovery));
        registrar.keybinding(Keybinding {
            key: "ctrl+d".into(),
            command: "dynamic-command".into(),
            arguments: String::new(),
            description: String::new(),
        });

        registrar
            .finish(manifest("dynamic-keybinding-test").version("1.0.0").build())
            .expect("dynamic command target is validated at discovery time");
    }
}

// ─── Keybinding ──────────────────────────────────────────────────────────

/// 插件注册的快捷键绑定。
///
/// 当用户按下对应组合键时，TUI 将执行关联的斜杠命令（如同用户输入该命令）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    /// 快捷键描述（如 "shift+tab", "ctrl+p"）。
    pub key: String,
    /// 按下时执行的斜杠命令名（不含 `/`）。
    pub command: String,
    /// 可选的命令参数。
    #[serde(default)]
    pub arguments: String,
    /// 人类可读描述（用于帮助/UI 展示）。
    pub description: String,
}

// ─── Status Item ─────────────────────────────────────────────────────────

/// 插件注册的状态栏项。
///
/// 显示在 TUI footer 和前端状态栏中。插件可以通过 `StatusItemUpdate`
/// 通知动态更新内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItem {
    /// 唯一标识符（如 "mode"、"git-branch"）。
    pub id: String,
    /// 初始显示文本。
    pub text: String,
    /// 排序优先级（越小越靠左）。
    #[serde(default)]
    pub priority: i32,
    /// 可选的 tooltip 描述。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}
