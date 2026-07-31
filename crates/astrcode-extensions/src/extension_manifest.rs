//! s5r 扩展握手 manifest 类型与解析。

use astrcode_extension_sdk::{
    extension::{
        ContinueAfterStopOptions, ExtensionCapability, ExtensionEvent, ExtensionEventDecl,
        ExtensionHttpRoute, HookMode, SlashCommand,
    },
    s5r::{
        capability_from_wire, event_from_name,
        manifest::{
            InitializeManifest, ManifestCommand, ManifestHook, ManifestHttpRoute, ManifestTool,
        },
        mode_from_name,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin},
};
use serde_json::Value;

/// `Initialize.metadata` 解析出的注册信息。
#[derive(Debug, Clone)]
pub(crate) struct ExtensionRegistration {
    extension_id: String,
    capabilities: Vec<ExtensionCapability>,
    tools: Vec<ToolDefinition>,
    commands: Vec<SlashCommand>,
    subscriptions: Vec<(ExtensionEvent, HookMode, ContinueAfterStopOptions)>,
    http_routes: Vec<RegisteredHttpRoute>,
    extension_events: Vec<ExtensionEventDecl>,
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

    pub(crate) fn capabilities(&self) -> &[ExtensionCapability] {
        &self.capabilities
    }

    pub(crate) fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub(crate) fn commands(&self) -> &[SlashCommand] {
        &self.commands
    }

    pub(crate) fn subscriptions(&self) -> &[(ExtensionEvent, HookMode, ContinueAfterStopOptions)] {
        &self.subscriptions
    }

    pub(crate) fn http_routes(&self) -> &[RegisteredHttpRoute] {
        &self.http_routes
    }

    pub(crate) fn extension_events(&self) -> &[ExtensionEventDecl] {
        &self.extension_events
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
    registration_from_manifest(manifest)
}

fn registration_from_manifest(
    manifest: InitializeManifest,
) -> Result<ExtensionRegistration, String> {
    let extension_id = manifest.extension_id.trim();
    if extension_id.is_empty() {
        return Err("initialize manifest missing extension_id".into());
    }
    let extension_id = extension_id.to_owned();

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
    let http_routes = manifest
        .http_routes
        .into_iter()
        .map(normalize_http_route)
        .collect::<Result<_, _>>()?;
    let extension_events = manifest
        .extension_events
        .into_iter()
        .map(ExtensionEventDecl::from)
        .collect();

    Ok(ExtensionRegistration {
        extension_id,
        capabilities,
        tools,
        commands,
        subscriptions,
        http_routes,
        extension_events,
    })
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

fn normalize_hook(
    hook: ManifestHook,
) -> Result<(ExtensionEvent, HookMode, ContinueAfterStopOptions), String> {
    let event = event_from_name(&hook.on)
        .ok_or_else(|| format!("unknown hook event in manifest: {}", hook.on))?;
    let mode = mode_from_name(&hook.mode)
        .ok_or_else(|| format!("unknown hook mode in manifest: {}", hook.mode))?;
    if s5r_unsupported_typed_hook(&event) {
        return Err(format!("{} is not supported by s5r manifest", hook.on));
    }
    if event == ExtensionEvent::ContinueAfterStop && mode != HookMode::Blocking {
        return Err(format!("{} is a blocking-only hook", hook.on));
    }
    Ok((
        event,
        mode,
        ContinueAfterStopOptions {
            max_per_turn: hook
                .options
                .max_per_turn
                .unwrap_or(ContinueAfterStopOptions::default().max_per_turn),
        },
    ))
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

fn s5r_unsupported_typed_hook(event: &ExtensionEvent) -> bool {
    matches!(event, ExtensionEvent::UserMessageEnvelope)
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::tool::ExecutionMode;
    use serde_json::json;

    use super::*;

    #[test]
    fn s5r_initialize_manifest_is_forward_compatible_and_validates_known_fields() {
        let invalid_manifests = [
            (
                json!({
                    "extension_id": "bad-known-field",
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
                    "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION},
                    "capabilities": ["not_a_capability"]
                }),
                "unknown capability",
            ),
        ];
        for (manifest, expected) in invalid_manifests {
            let error =
                registration_from_s5r_metadata(&manifest, astrcode_extension_sdk::s5r::S5R_VERSION)
                    .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }

        let registration = registration_from_s5r_metadata(
            &json!({
                "extension_id": "legacy-defaults",
                "protocol": {
                    "s5r": astrcode_extension_sdk::s5r::S5R_VERSION,
                    "future_protocol_field": true
                },
                "wire_codec": "json",
                "future_manifest_field": {"enabled": true},
                "tools": [
                    {
                        "name": "legacy",
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
                "commands": [{"name": "legacy-command"}],
                "hooks": [{"on": "turn_end", "mode": "non_blocking"}],
                "extension_events": [{"event_type": "legacy.event"}]
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
        assert_eq!(
            registration.subscriptions()[0].2,
            ContinueAfterStopOptions::default()
        );
        assert_eq!(registration.extension_events()[0].schema_version, 1);
        assert!(registration.extension_events()[0].durable);
        assert_eq!(
            registration.extension_events()[0].max_payload_bytes,
            64 * 1024
        );
    }
}
