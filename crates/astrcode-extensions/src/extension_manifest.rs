//! s5r 扩展握手 manifest 类型与解析。

use std::collections::BTreeSet;

use astrcode_extension_sdk::{
    builder::{ExtensionToolDefinition, manifest as extension_manifest},
    extension::{
        CompactEvent, ContinueAfterStopOptions, CustomEventDeclaration, CustomEventSubscription,
        ExtensionCapability, ExtensionHttpRoute, HookMode, LifecycleEvent, SlashCommand,
        internal::{fixed_hook_mode, hook_mode_is_supported},
    },
    s5r::HandlerId,
    tool::{ExecutionMode, ToolDefinition, ToolExecutionPolicy, ToolOrigin},
    wire::{
        FeatureName,
        manifest::{
            InitializeManifest, ManifestCommand, ManifestHook, ManifestHookEvent,
            ManifestHttpRoute, ManifestTool, ManifestToolMode,
        },
        protocol::PeerInfo,
    },
};

/// Host-normalized registration derived from a typed S5R initialize manifest.
#[derive(Debug, Clone)]
pub(crate) struct ExtensionRegistration {
    pub(crate) extension_id: String,
    pub(crate) version: String,
    pub(crate) required_transport_features:
        Vec<astrcode_extension_sdk::extension::TransportFeature>,
    pub(crate) capabilities: Vec<ExtensionCapability>,
    pub(crate) tools: Vec<ExtensionToolDefinition>,
    pub(crate) commands: Vec<SlashCommand>,
    pub(crate) subscriptions: Vec<HookSubscription>,
    pub(crate) http_routes: Vec<RegisteredHttpRoute>,
    pub(crate) custom_events: Vec<CustomEventDeclaration>,
    pub(crate) custom_event_subscriptions: Vec<CustomEventSubscription>,
}

#[derive(Debug, Clone)]
pub(crate) enum HookSubscription {
    Lifecycle {
        event: LifecycleEvent,
        mode: HookMode,
        priority: i32,
        options: ContinueAfterStopOptions,
    },
    Compact {
        event: CompactEvent,
        priority: i32,
    },
}

impl HookSubscription {
    pub(crate) fn event_name(&self) -> &'static str {
        match self {
            Self::Lifecycle { event, .. } => event.as_str(),
            Self::Compact { event, .. } => event.as_str(),
        }
    }

    pub(crate) fn priority(&self) -> i32 {
        match self {
            Self::Lifecycle { priority, .. } | Self::Compact { priority, .. } => *priority,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RegisteredHttpRoute {
    pub(crate) route: ExtensionHttpRoute,
    pub(crate) handler_id: HandlerId,
}

/// Normalize the typed worker declaration into the host registration model.
pub(crate) fn registration_from_s5r_manifest(
    peer: &PeerInfo,
    manifest: InitializeManifest,
) -> Result<ExtensionRegistration, String> {
    let version = peer
        .version
        .as_deref()
        .ok_or_else(|| "initialize peer is missing version".to_owned())?;
    registration_from_manifest(&peer.name, version, manifest)
}

pub(crate) fn validate_registration_features(
    registration: &ExtensionRegistration,
    negotiated_features: &BTreeSet<FeatureName>,
) -> Result<(), String> {
    let uses_custom_events = !registration.custom_events.is_empty()
        || !registration.custom_event_subscriptions.is_empty();
    if uses_custom_events && !negotiated_features.contains(&FeatureName::custom_event_v1()) {
        return Err(
            "initialize manifest declares custom events but custom_event_v1 was not negotiated"
                .into(),
        );
    }
    Ok(())
}

fn registration_from_manifest(
    extension_id: &str,
    version: &str,
    manifest: InitializeManifest,
) -> Result<ExtensionRegistration, String> {
    let identity = extension_manifest(extension_id)
        .version(version.trim())
        .build_checked()
        .map_err(|error| format!("invalid initialize manifest identity: {error}"))?;
    let extension_id = identity.id().to_owned();
    let version = identity.version().to_owned();

    let required_transport_features = manifest.required_transport_features;
    if required_transport_features
        .iter()
        .collect::<BTreeSet<_>>()
        .len()
        != required_transport_features.len()
    {
        return Err("initialize manifest contains duplicate required transport features".into());
    }
    let capabilities = manifest.capabilities;
    // workspace_sensitive_paths 是宿主信任特权:敏感路径绕过随 resource lease 生效,
    // 只授予第一方 bundled 扩展;磁盘 s5r manifest 声明它属于权限放大,直接拒绝。
    if capabilities.contains(&ExtensionCapability::WorkspaceSensitivePaths) {
        return Err(format!(
            "capability {} is reserved for bundled first-party extensions",
            ExtensionCapability::WorkspaceSensitivePaths.as_str()
        ));
    }

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
    let http_routes = manifest
        .http_routes
        .into_iter()
        .map(|route| normalize_http_route(&extension_id, route))
        .collect::<Result<_, _>>()?;
    let custom_events = manifest.custom_events;
    let custom_event_subscriptions = manifest.custom_event_subscriptions;

    Ok(ExtensionRegistration {
        extension_id,
        version,
        required_transport_features,
        capabilities,
        tools,
        commands,
        subscriptions,
        http_routes,
        custom_events,
        custom_event_subscriptions,
    })
}

/// 解析 `<extension_id>:<kind>:<name>` 形式的 handler 标识，校验归属与格式。
/// 格式本身由 [`HandlerId`] 单点定义；这里只补充「必须归属调用方扩展」的宿主约束。
fn parse_handler_id<'a>(
    extension_id: &str,
    handler_id: &'a HandlerId,
) -> Result<(astrcode_extension_sdk::wire::HandlerKind, &'a str), String> {
    let (owner, kind, name) = handler_id
        .parts()
        .ok_or_else(|| format!("invalid handler id {handler_id}"))?;
    if owner != extension_id {
        return Err(format!(
            "handler {handler_id} must be attributed to extension {extension_id}"
        ));
    }
    Ok((kind, name))
}

fn validate_handler_id_kind(
    extension_id: &str,
    handler_id: &HandlerId,
    expected_kind: astrcode_extension_sdk::wire::HandlerKind,
) -> Result<(), String> {
    let (kind, _) = parse_handler_id(extension_id, handler_id)?;
    if kind != expected_kind {
        return Err(format!(
            "handler {handler_id} has kind {}, expected {}",
            kind.as_str(),
            expected_kind.as_str()
        ));
    }
    Ok(())
}

fn normalize_tool(tool: ManifestTool) -> Result<ExtensionToolDefinition, String> {
    let execution_mode = match tool.mode {
        ManifestToolMode::Parallel => ExecutionMode::Parallel,
        ManifestToolMode::Sequential => ExecutionMode::Sequential,
    };
    let timeout = tool
        .timeout_ms
        .map(|timeout_ms| {
            if timeout_ms == 0 {
                Err("tool timeout_ms must be greater than zero".to_owned())
            } else {
                Ok(std::time::Duration::from_millis(timeout_ms))
            }
        })
        .transpose()?;
    Ok(ExtensionToolDefinition::from_definition(ToolDefinition {
        name: tool.name,
        description: tool.description,
        parameters: tool.parameters,
        strict: tool.strict,
        origin: ToolOrigin::Extension,
    })
    .with_execution_policy(ToolExecutionPolicy {
        mode: execution_mode,
        timeout,
    }))
}

fn normalize_command(command: ManifestCommand) -> SlashCommand {
    SlashCommand {
        name: command.name,
        description: command.description,
        args_schema: command.args_schema,
        requires_idle: command.requires_idle,
        argument_completions: command.argument_completions,
        priority: command.priority,
        availability: command.availability,
        execution: command.execution,
    }
}

fn normalize_hook(hook: ManifestHook) -> Result<HookSubscription, String> {
    let event_name = hook.on.as_str();
    let priority = hook.priority.unwrap_or(0);
    if priority < 0 {
        return Err(format!("{event_name} priority must be non-negative"));
    }
    if let ManifestHookEvent::Compact(event) = hook.on {
        if hook.mode != HookMode::Blocking {
            return Err(format!("{event_name} requires blocking mode"));
        }
        return Ok(HookSubscription::Compact { event, priority });
    }

    let ManifestHookEvent::Lifecycle(event) = hook.on else {
        unreachable!("compact hooks return above")
    };
    if s5r_unsupported_typed_hook(&event) {
        return Err(format!("{event_name} is not supported by s5r manifest"));
    }
    if !hook_mode_is_supported(&event, hook.mode) {
        return Err(match fixed_hook_mode(&event) {
            Some(required) => format!("{event_name} requires {} mode", required.as_str()),
            None => format!("{event_name} does not support {} mode", hook.mode.as_str()),
        });
    }
    Ok(HookSubscription::Lifecycle {
        event,
        mode: hook.mode,
        priority,
        options: ContinueAfterStopOptions {
            max_per_turn: hook
                .options
                .max_per_turn
                .unwrap_or(ContinueAfterStopOptions::default().max_per_turn),
        },
    })
}

fn normalize_http_route(
    extension_id: &str,
    route: ManifestHttpRoute,
) -> Result<RegisteredHttpRoute, String> {
    astrcode_extension_sdk::extension::internal::validate_extension_http_route(&route.route)?;
    validate_handler_id_kind(
        extension_id,
        &route.handler_id,
        astrcode_extension_sdk::wire::HandlerKind::Http,
    )?;
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
    use astrcode_extension_sdk::{tool::ExecutionMode, wire::manifest::ManifestHookOptions};
    use serde_json::json;

    use super::*;

    #[test]
    fn typed_s5r_manifest_normalizes_defaults_and_validates_identity() {
        let invalid_peers = [
            (
                PeerInfo {
                    name: "../escape".into(),
                    version: Some("test".into()),
                },
                "must start with an ASCII letter or digit",
            ),
            (
                PeerInfo {
                    name: "missing-version".into(),
                    version: None,
                },
                "missing version",
            ),
        ];
        for (peer, expected) in invalid_peers {
            let error =
                registration_from_s5r_manifest(&peer, InitializeManifest::default()).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }

        let manifest: InitializeManifest = serde_json::from_value(json!({
            "required_transport_features": ["authenticated_http"],
            "tools": [
                {
                    "name": "defaulted",
                    "description": "",
                    "parameters": {"type": "object"}
                },
                {
                    "name": "strict",
                    "description": "",
                    "parameters": {"type": "object"},
                    "strict": true
                },
                {
                    "name": "slow",
                    "description": "",
                    "parameters": {"type": "object"},
                    "timeout_ms": 300000
                }
            ],
            "commands": [{
                "name": "typed-command",
                "description": "Command metadata",
                "args_schema": null,
                "requires_idle": true,
                "argument_completions": false,
                "priority": 7,
                "availability": "all_transports",
                "execution": {"kind": "extension"}
            }],
            "hooks": [{"on": "turn_end", "mode": "non_blocking"}],
            "custom_events": [{
                "event_type": "defaulted.event",
                "schema_version": 1,
                "delivery": "session_durable",
                "max_payload_bytes": 65536
            }]
        }))
        .unwrap();
        let registration = registration_from_s5r_manifest(
            &PeerInfo {
                name: "defaults".into(),
                version: Some("test".into()),
            },
            manifest,
        )
        .expect("manifest should parse");

        assert_eq!(registration.extension_id, "defaults");
        assert_eq!(registration.version, "test");
        assert_eq!(
            registration.required_transport_features,
            [astrcode_extension_sdk::extension::TransportFeature::AuthenticatedHttp]
        );
        assert!(!registration.tools[0].strict);
        assert_eq!(
            registration.tools[0].execution_policy().mode,
            ExecutionMode::Sequential
        );
        assert!(registration.tools[1].strict);
        assert_eq!(registration.tools[0].execution_policy().timeout, None);
        assert_eq!(registration.tools[1].execution_policy().timeout, None);
        assert_eq!(
            registration.tools[2].execution_policy().timeout,
            Some(std::time::Duration::from_millis(300_000))
        );
        assert!(registration.capabilities.is_empty());
        assert!(registration.http_routes.is_empty());
        assert_eq!(registration.commands[0].description, "Command metadata");
        assert!(registration.commands[0].requires_idle);
        assert_eq!(registration.commands[0].priority, 7);
        assert!(matches!(
            &registration.subscriptions[0],
            HookSubscription::Lifecycle { mode, options, .. }
                if *mode == HookMode::NonBlocking
                    && *options == ContinueAfterStopOptions::default()
        ));
        assert_eq!(registration.custom_events[0].schema_version, 1);
        assert_eq!(
            registration.custom_events[0].delivery,
            astrcode_extension_sdk::extension::CustomEventDelivery::SessionDurable
        );
        assert_eq!(registration.custom_events[0].max_payload_bytes, 64 * 1024);
        assert!(
            validate_registration_features(&registration, &BTreeSet::new()).is_err(),
            "custom-event declarations require the negotiated feature"
        );
        assert!(
            validate_registration_features(
                &registration,
                &BTreeSet::from([FeatureName::custom_event_v1()])
            )
            .is_ok()
        );
    }

    #[test]
    fn s5r_tool_timeout_must_be_positive() {
        let tool: ManifestTool = serde_json::from_value(json!({
            "name": "invalid-timeout",
            "description": "",
            "parameters": {"type": "object"},
            "timeout_ms": 0
        }))
        .unwrap();
        let manifest = InitializeManifest {
            tools: vec![tool],
            ..InitializeManifest::default()
        };

        let error = registration_from_manifest("timeout-test", "test", manifest).unwrap_err();
        assert_eq!(error, "tool timeout_ms must be greater than zero");
    }

    #[test]
    fn s5r_manifest_rejects_bundled_only_sensitive_path_capability() {
        let manifest: InitializeManifest = serde_json::from_value(json!({
            "required_transport_features": [],
            "capabilities": ["workspace_read", "workspace_sensitive_paths"]
        }))
        .unwrap();

        let error = registration_from_manifest("sensitive-grab", "test", manifest).unwrap_err();
        assert_eq!(
            error,
            "capability workspace_sensitive_paths is reserved for bundled first-party extensions"
        );
    }

    #[test]
    fn s5r_hook_priority_defaults_to_zero_and_rejects_negative() {
        let hook = serde_json::from_value(
            json!({"on": "turn_end", "mode": "non_blocking", "priority": 5}),
        )
        .unwrap();
        assert_eq!(normalize_hook(hook).unwrap().priority(), 5);

        let hook =
            serde_json::from_value(json!({"on": "turn_end", "mode": "non_blocking"})).unwrap();
        assert_eq!(normalize_hook(hook).unwrap().priority(), 0);

        let hook =
            serde_json::from_value(json!({"on": "pre_compact", "mode": "blocking", "priority": 3}))
                .unwrap();
        assert!(matches!(
            normalize_hook(hook),
            Ok(HookSubscription::Compact {
                event: CompactEvent::PreCompact,
                priority: 3,
            })
        ));

        let hook = serde_json::from_value(
            json!({"on": "turn_end", "mode": "non_blocking", "priority": -1}),
        )
        .unwrap();
        assert_eq!(
            normalize_hook(hook).unwrap_err(),
            "turn_end priority must be non-negative"
        );
    }

    #[test]
    fn s5r_hook_modes_match_dispatch_contract() {
        let cases = [
            ("tool_input_transform", "blocking", None),
            (
                "tool_input_transform",
                "advisory",
                Some("tool_input_transform requires blocking mode"),
            ),
            ("pre_tool_use", "blocking", None),
            (
                "pre_tool_use",
                "non_blocking",
                Some("pre_tool_use requires blocking mode"),
            ),
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
            let hook = serde_json::from_value(json!({"on": on, "mode": mode})).unwrap();
            let result = normalize_hook(hook);
            match expected_error {
                Some(expected_error) => assert_eq!(result.unwrap_err(), expected_error),
                None => assert!(result.is_ok(), "{on}/{mode}: {result:?}"),
            }
        }

        assert!(matches!(
            normalize_hook(ManifestHook {
                on: CompactEvent::PreCompact.into(),
                mode: HookMode::Blocking,
                priority: None,
                options: ManifestHookOptions::default(),
            }),
            Ok(HookSubscription::Compact {
                event: CompactEvent::PreCompact,
                priority: 0,
            })
        ));
    }
}
