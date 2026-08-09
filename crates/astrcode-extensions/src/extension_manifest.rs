//! s5r 扩展握手 manifest 类型与解析。

use std::collections::HashSet;

use astrcode_extension_sdk::{
    builder::manifest as extension_manifest,
    extension::{
        CompactEvent, ContinueAfterStopOptions, CustomEventDeclaration, CustomEventSubscription,
        ExtensionCapability, ExtensionHttpRoute, HookMode, LifecycleEvent, SlashCommand,
        fixed_hook_mode, hook_mode_is_supported,
    },
    s5r::{
        HandlerDescriptor, HandlerId, capability_from_wire, compact_event_from_name,
        compact_event_to_name, event_from_name, event_to_name,
        manifest::{
            InitializeManifest, ManifestCommand, ManifestHook, ManifestHttpRoute, ManifestTool,
        },
        mode_from_name, mode_to_name,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin},
};
use serde_json::{Value, json};

use crate::remote_manifest::handler_id;

/// `Initialize.metadata` 解析出的注册信息。
#[derive(Debug, Clone)]
pub(crate) struct ExtensionRegistration {
    extension_id: String,
    version: String,
    capabilities: Vec<ExtensionCapability>,
    tools: Vec<ToolDefinition>,
    commands: Vec<SlashCommand>,
    subscriptions: Vec<HookSubscription>,
    continuation_hooks: Vec<String>,
    http_routes: Vec<RegisteredHttpRoute>,
    custom_events: Vec<CustomEventDeclaration>,
    custom_event_subscriptions: Vec<CustomEventSubscription>,
}

#[derive(Debug, Clone)]
pub(crate) enum HookSubscription {
    Lifecycle {
        event: LifecycleEvent,
        mode: HookMode,
        options: ContinueAfterStopOptions,
    },
    Compact(CompactEvent),
}

impl HookSubscription {
    pub(crate) fn event_name(&self) -> &'static str {
        match self {
            Self::Lifecycle { event, .. } => event_to_name(event),
            Self::Compact(event) => compact_event_to_name(*event),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RegisteredHttpRoute {
    pub(crate) route: ExtensionHttpRoute,
    pub(crate) handler_id: String,
}

impl ExtensionRegistration {
    pub(crate) fn extension_id(&self) -> &str {
        &self.extension_id
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn capabilities(&self) -> &[ExtensionCapability] {
        &self.capabilities
    }

    pub(crate) fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub(crate) fn commands(&self) -> &[SlashCommand] {
        &self.commands
    }

    pub(crate) fn subscriptions(&self) -> &[HookSubscription] {
        &self.subscriptions
    }

    pub(crate) fn expected_handler_descriptors(&self) -> Result<Vec<HandlerDescriptor>, String> {
        let mut descriptors = Vec::new();
        let mut handler_ids = HashSet::new();
        let mut push = |descriptor: HandlerDescriptor| {
            if !handler_ids.insert(descriptor.handler_id.clone()) {
                return Err(format!(
                    "initialize manifest declares duplicate handler {}",
                    descriptor.handler_id
                ));
            }
            descriptors.push(descriptor);
            Ok(())
        };

        for tool in &self.tools {
            push(HandlerDescriptor {
                handler_id: handler_id(&self.extension_id, "tool", &tool.name),
                description: tool.description.clone(),
                input_schema: tool.parameters.clone(),
            })?;
        }
        for subscription in &self.subscriptions {
            let event_name = subscription.event_name();
            push(HandlerDescriptor {
                handler_id: handler_id(&self.extension_id, "hook", event_name),
                description: format!("hook {event_name}"),
                input_schema: json!({"type": "object"}),
            })?;
        }
        for hook in &self.continuation_hooks {
            push(HandlerDescriptor {
                handler_id: handler_id(&self.extension_id, "hook", hook),
                description: format!("continuation hook {hook}"),
                input_schema: json!({"type": "object"}),
            })?;
        }
        for command in &self.commands {
            push(HandlerDescriptor {
                handler_id: handler_id(&self.extension_id, "command", &command.name),
                description: command.description.clone(),
                input_schema: json!({"type": "object"}),
            })?;
        }
        for route in &self.http_routes {
            validate_handler_id_kind(&self.extension_id, &route.handler_id, "http")?;
            push(HandlerDescriptor {
                handler_id: route.handler_id.clone(),
                description: route.route.description.clone(),
                input_schema: json!({"type": "object"}),
            })?;
        }
        for subscription in &self.custom_event_subscriptions {
            push(HandlerDescriptor {
                handler_id: handler_id(&self.extension_id, "event", &subscription.id),
                description: format!("custom event {}", subscription.event_type),
                input_schema: json!({"type": "object"}),
            })?;
        }
        Ok(descriptors)
    }

    pub(crate) fn http_routes(&self) -> &[RegisteredHttpRoute] {
        &self.http_routes
    }

    pub(crate) fn custom_events(&self) -> &[CustomEventDeclaration] {
        &self.custom_events
    }

    pub(crate) fn custom_event_subscriptions(&self) -> &[CustomEventSubscription] {
        &self.custom_event_subscriptions
    }
}

/// 从 s5r `InitializeMessage.metadata` 解析注册信息。
pub(crate) fn registration_from_s5r_metadata(
    metadata: &Value,
    expected_s5r_version: &str,
) -> Result<ExtensionRegistration, String> {
    let manifest: InitializeManifest = serde_json::from_value(metadata.clone())
        .map_err(|error| format!("invalid initialize manifest: {error}"))?;
    if manifest.protocol.s5r != expected_s5r_version {
        return Err(format!(
            "initialize metadata protocol.s5r must be \"{expected_s5r_version}\""
        ));
    }
    let required_wire_feature = astrcode_extension_contract::protocol::FEATURE_NESTED_INVOKE_V1;
    if !manifest
        .wire_features
        .iter()
        .any(|feature| feature == required_wire_feature)
    {
        return Err(format!(
            "S5R {expected_s5r_version} requires wire feature {required_wire_feature}"
        ));
    }
    registration_from_manifest(manifest)
}

fn registration_from_manifest(
    manifest: InitializeManifest,
) -> Result<ExtensionRegistration, String> {
    let identity = extension_manifest(manifest.extension_id.clone())
        .version(manifest.version.trim())
        .build_checked()
        .map_err(|error| format!("invalid initialize manifest identity: {error}"))?;
    let extension_id = identity.id().to_owned();
    let version = identity.version().to_owned();

    let capabilities = manifest
        .capabilities
        .into_iter()
        .map(|name| {
            capability_from_wire(&name)
                .ok_or_else(|| format!("initialize manifest has unknown capability \"{name}\""))
        })
        .collect::<Result<_, _>>()?;

    let tools = manifest
        .tools
        .into_iter()
        .map(normalize_tool)
        .collect::<Result<_, _>>()?;
    let commands = manifest
        .commands
        .into_iter()
        .map(normalize_command)
        .collect();
    let subscriptions = manifest
        .hooks
        .into_iter()
        .map(normalize_hook)
        .collect::<Result<_, _>>()?;
    let continuation_hooks = manifest.continuation_hooks;
    let http_routes = manifest
        .http_routes
        .into_iter()
        .map(normalize_http_route)
        .collect::<Result<_, _>>()?;
    let custom_events = manifest.custom_events;
    let custom_event_subscriptions = manifest.custom_event_subscriptions;

    Ok(ExtensionRegistration {
        extension_id,
        version,
        capabilities,
        tools,
        commands,
        subscriptions,
        continuation_hooks,
        http_routes,
        custom_events,
        custom_event_subscriptions,
    })
}

/// 解析 `<extension_id>:<kind>:<name>` 形式的 handler 标识，校验归属与格式。
/// 格式本身由 [`HandlerId`] 单点定义；这里只补充「必须归属调用方扩展」的宿主约束。
pub(crate) fn parse_handler_id<'a>(
    extension_id: &str,
    handler_id: &'a str,
) -> Result<(&'a str, &'a str), String> {
    let parsed = HandlerId::split(handler_id)
        .ok_or_else(|| format!("handler {handler_id} must use <extension>:<kind>:<name>"))?;
    let (owner, kind, name) = parsed;
    if owner != extension_id {
        return Err(format!(
            "handler {handler_id} must be attributed to extension {extension_id}"
        ));
    }
    Ok((kind.as_str(), name))
}

fn validate_handler_id_kind(
    extension_id: &str,
    handler_id: &str,
    expected_kind: &str,
) -> Result<(), String> {
    let (kind, _) = parse_handler_id(extension_id, handler_id)?;
    if kind != expected_kind {
        return Err(format!(
            "handler {handler_id} has kind {kind}, expected {expected_kind}"
        ));
    }
    Ok(())
}

fn normalize_tool(tool: ManifestTool) -> Result<ToolDefinition, String> {
    let execution_mode = match tool.mode.as_str() {
        "parallel" => ExecutionMode::Parallel,
        "sequential" => ExecutionMode::Sequential,
        _ => {
            return Err(format!(
                "unknown tool execution mode in manifest: {}",
                tool.mode
            ));
        },
    };
    Ok(ToolDefinition {
        name: tool.name,
        description: tool.description,
        parameters: tool.parameters,
        strict: tool.strict,
        origin: ToolOrigin::Extension,
        execution_mode,
    })
}

fn normalize_command(command: ManifestCommand) -> SlashCommand {
    SlashCommand {
        name: command.name,
        description: command.description,
        args_schema: None,
        requires_idle: false,
        argument_completions: false,
        priority: 0,
    }
}

fn normalize_hook(hook: ManifestHook) -> Result<HookSubscription, String> {
    if let Some(event) = compact_event_from_name(&hook.on) {
        let mode = normalize_hook_mode(&hook.mode)?;
        if mode != HookMode::Blocking {
            return Err(format!("{} requires blocking mode", hook.on));
        }
        return Ok(HookSubscription::Compact(event));
    }

    let event = event_from_name(&hook.on)
        .ok_or_else(|| format!("unknown hook event in manifest: {}", hook.on))?;
    let mode = normalize_hook_mode(&hook.mode)?;
    if s5r_unsupported_typed_hook(&event) {
        return Err(format!("{} is not supported by s5r manifest", hook.on));
    }
    if !hook_mode_is_supported(&event, mode) {
        return Err(match fixed_hook_mode(&event) {
            Some(required) => format!("{} requires {} mode", hook.on, mode_to_name(required)),
            None => format!("{} does not support {} mode", hook.on, hook.mode),
        });
    }
    Ok(HookSubscription::Lifecycle {
        event,
        mode,
        options: ContinueAfterStopOptions {
            max_per_turn: hook
                .options
                .max_per_turn
                .unwrap_or(ContinueAfterStopOptions::default().max_per_turn),
        },
    })
}

fn normalize_hook_mode(mode: &str) -> Result<HookMode, String> {
    mode_from_name(mode).ok_or_else(|| format!("unknown hook mode in manifest: {mode}"))
}

fn normalize_http_route(route: ManifestHttpRoute) -> Result<RegisteredHttpRoute, String> {
    route.route.validate()?;
    if route.handler_id.trim().is_empty() {
        return Err(format!(
            "HTTP route {} is missing handler_id",
            route.route.path
        ));
    }
    Ok(RegisteredHttpRoute {
        route: route.route,
        handler_id: route.handler_id,
    })
}

fn s5r_unsupported_typed_hook(event: &LifecycleEvent) -> bool {
    matches!(event, LifecycleEvent::UserMessageEnvelope)
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::{s5r::manifest::ManifestHookOptions, tool::ExecutionMode};
    use serde_json::json;

    use super::*;

    #[test]
    fn s5r_initialize_manifest_is_forward_compatible_and_validates_known_fields() {
        let invalid_manifests = [
            (
                json!({
                    "extension_id": "../escape",
                    "version": "test",
                    "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION}
                }),
                "must start with an ASCII letter or digit",
            ),
            (
                json!({
                    "extension_id": "bad-known-field",
                    "version": "test",
                    "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION},
                    "tools": [{
                        "name": "tool",
                        "description": "",
                        "parameters": {"type": "object"},
                        "strict": "yes"
                    }]
                }),
                "invalid type",
            ),
            (
                json!({
                    "extension_id": "bad-capability",
                    "version": "test",
                    "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION},
                    "capabilities": ["not_a_capability"]
                }),
                "unknown capability",
            ),
        ];
        for (mut manifest, expected) in invalid_manifests {
            manifest["wire_features"] =
                json!([astrcode_extension_contract::protocol::FEATURE_NESTED_INVOKE_V1]);
            let error =
                registration_from_s5r_metadata(&manifest, astrcode_extension_sdk::s5r::S5R_VERSION)
                    .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }

        let registration = registration_from_s5r_metadata(
            &json!({
                "extension_id": "defaults",
                "version": "test",
                "protocol": {
                    "s5r": astrcode_extension_sdk::s5r::S5R_VERSION,
                    "future_protocol_field": true
                },
                "wire_codec": "json",
                "wire_features": [
                    astrcode_extension_contract::protocol::FEATURE_NESTED_INVOKE_V1,
                    "future_feature"
                ],
                "future_manifest_field": {"enabled": true},
                "tools": [
                    {
                        "name": "defaulted",
                        "description": "",
                        "parameters": {"type": "object"},
                        "future_tool_field": "ignored"
                    },
                    {
                        "name": "strict",
                        "description": "",
                        "parameters": {"type": "object"},
                        "strict": true
                    }
                ],
                "commands": [{"name": "defaulted-command"}],
                "hooks": [{"on": "turn_end", "mode": "non_blocking"}],
                "custom_events": [{"eventType": "defaulted.event"}]
            }),
            astrcode_extension_sdk::s5r::S5R_VERSION,
        )
        .expect("manifest should parse");

        assert!(!registration.tools()[0].strict);
        assert_eq!(
            registration.tools()[0].execution_mode,
            ExecutionMode::Sequential
        );
        assert!(registration.tools()[1].strict);
        assert!(registration.capabilities().is_empty());
        assert!(registration.http_routes().is_empty());
        assert_eq!(registration.commands()[0].description, "");
        assert!(matches!(
            &registration.subscriptions()[0],
            HookSubscription::Lifecycle { mode, options, .. }
                if *mode == HookMode::NonBlocking
                    && *options == ContinueAfterStopOptions::default()
        ));
        assert_eq!(registration.custom_events()[0].schema_version, 1);
        assert!(registration.custom_events()[0].durable);
        assert_eq!(registration.custom_events()[0].max_payload_bytes, 64 * 1024);
    }

    #[test]
    fn s5r_hook_modes_match_dispatch_contract() {
        let cases = [
            ("pre_tool_use", "blocking", None),
            ("post_tool_use", "advisory", None),
            ("before_provider_request", "non_blocking", None),
            ("turn_start", "blocking", None),
            ("user_prompt_submit", "blocking", None),
            ("turn_end", "advisory", None),
            (
                "turn_end",
                "blocking",
                Some("turn_end does not support blocking mode"),
            ),
            ("after_provider_response", "advisory", None),
            (
                "after_provider_response",
                "blocking",
                Some("after_provider_response requires advisory mode"),
            ),
            ("prompt_build", "blocking", None),
            (
                "prompt_build",
                "non_blocking",
                Some("prompt_build requires blocking mode"),
            ),
            ("pre_compact", "blocking", None),
            (
                "pre_compact",
                "advisory",
                Some("pre_compact requires blocking mode"),
            ),
            ("post_compact", "blocking", None),
            (
                "post_compact",
                "non_blocking",
                Some("post_compact requires blocking mode"),
            ),
            ("continue_after_stop", "blocking", None),
            (
                "continue_after_stop",
                "advisory",
                Some("continue_after_stop requires blocking mode"),
            ),
            (
                "user_message_envelope",
                "blocking",
                Some("user_message_envelope is not supported by s5r manifest"),
            ),
        ];

        for (on, mode, expected_error) in cases {
            let result = normalize_hook(ManifestHook {
                on: on.into(),
                mode: mode.into(),
                options: ManifestHookOptions::default(),
            });
            match expected_error {
                Some(expected_error) => assert_eq!(result.unwrap_err(), expected_error),
                None => assert!(result.is_ok(), "{on}/{mode}: {result:?}"),
            }
        }

        assert!(matches!(
            normalize_hook(ManifestHook {
                on: "pre_compact".into(),
                mode: "blocking".into(),
                options: ManifestHookOptions::default(),
            }),
            Ok(HookSubscription::Compact(CompactEvent::PreCompact))
        ));
    }
}
