//! Typed initialization declaration shared by S5R workers and the host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire::{
    ExtensionCapability, HandlerId,
    custom_event::{CustomEventDeclaration, CustomEventSubscription},
    extension_http::ExtensionHttpRoute,
    transport::TransportFeature,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeManifest {
    pub required_transport_features: Vec<TransportFeature>,
    #[serde(default)]
    pub capabilities: Vec<ExtensionCapability>,
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
    #[serde(default)]
    pub hooks: Vec<ManifestHook>,
    #[serde(default)]
    pub commands: Vec<ManifestCommand>,
    #[serde(default)]
    pub http_routes: Vec<ManifestHttpRoute>,
    #[serde(default)]
    pub custom_events: Vec<CustomEventDeclaration>,
    #[serde(default)]
    pub custom_event_subscriptions: Vec<CustomEventSubscription>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
    #[serde(default)]
    pub mode: ManifestToolMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestToolMode {
    Parallel,
    #[default]
    Sequential,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCommand {
    pub name: String,
    pub description: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub args_schema: Option<Value>,
    pub requires_idle: bool,
    pub argument_completions: bool,
    pub priority: i32,
    pub availability: CommandAvailability,
    pub execution: CommandExecution,
}

/// Slash command visibility across transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAvailability {
    AllTransports,
    InteractiveOnly,
}

/// Declares whether an extension handler or the host owns execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "command", rename_all = "snake_case")]
pub enum CommandExecution {
    Extension,
    Host(SessionCommandKind),
}

/// Privileged session commands implemented by the host behind its operation gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCommandKind {
    CompactSession,
    SelectModel,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestHook {
    pub on: ManifestHookEvent,
    pub mode: HookMode,
    #[serde(default, skip_serializing_if = "ManifestHookOptions::is_empty")]
    pub options: ManifestHookOptions,
}

/// Hook event encoded by an S5R manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ManifestHookEvent {
    Lifecycle(LifecycleEvent),
    Compact(CompactEvent),
}

impl ManifestHookEvent {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Lifecycle(event) => event.as_str(),
            Self::Compact(event) => event.as_str(),
        }
    }
}

impl From<LifecycleEvent> for ManifestHookEvent {
    fn from(event: LifecycleEvent) -> Self {
        Self::Lifecycle(event)
    }
}

impl From<CompactEvent> for ManifestHookEvent {
    fn from(event: CompactEvent) -> Self {
        Self::Compact(event)
    }
}

/// Extension hook execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    Blocking,
    NonBlocking,
    Advisory,
}

impl HookMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::NonBlocking => "non_blocking",
            Self::Advisory => "advisory",
        }
    }
}

/// Core lifecycle event available to extensions.
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
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionResume => "session_resume",
            Self::SessionShutdown => "session_shutdown",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::TurnAborted => "turn_aborted",
            Self::StepStart => "step_start",
            Self::StepEnd => "step_end",
            Self::ToolInputTransform => "tool_input_transform",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::BeforeProviderRequest => "before_provider_request",
            Self::ProviderContribution => "provider_contribution",
            Self::AfterProviderResponse => "after_provider_response",
            Self::ContinueAfterStop => "continue_after_stop",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::UserMessageEnvelope => "user_message_envelope",
            Self::PromptBuild => "prompt_build",
            Self::PostRecap => "post_recap",
        }
    }
}

/// Compact hook event available to extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactEvent {
    PreCompact,
    PostCompact,
}

impl CompactEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestHookOptions {
    #[serde(default)]
    pub max_per_turn: Option<ContinueAfterStopLimit>,
}

impl ManifestHookOptions {
    fn is_empty(&self) -> bool {
        self.max_per_turn.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestHttpRoute {
    pub route: ExtensionHttpRoute,
    pub handler_id: HandlerId,
}

/// Per-turn limit for one `continue_after_stop` hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub enum ContinueAfterStopLimit {
    Limited { max_per_turn: u32 },
    Unlimited,
}

impl ContinueAfterStopLimit {
    pub const fn limited(max_per_turn: u32) -> Self {
        Self::Limited { max_per_turn }
    }

    pub const fn unlimited() -> Self {
        Self::Unlimited
    }

    pub const fn allows(self, continuations_this_turn: u32) -> bool {
        match self {
            Self::Limited { max_per_turn } => continuations_this_turn < max_per_turn,
            Self::Unlimited => true,
        }
    }
}

impl TryFrom<i64> for ContinueAfterStopLimit {
    type Error = String;

    fn try_from(max_per_turn: i64) -> Result<Self, Self::Error> {
        match max_per_turn {
            -1 => Ok(Self::Unlimited),
            value if (0..=i64::from(u32::MAX)).contains(&value) => Ok(Self::limited(value as u32)),
            _ => {
                Err("continue_after_stop max_per_turn must be -1 or a non-negative integer".into())
            },
        }
    }
}

impl From<ContinueAfterStopLimit> for i64 {
    fn from(limit: ContinueAfterStopLimit) -> Self {
        match limit {
            ContinueAfterStopLimit::Limited { max_per_turn } => i64::from(max_per_turn),
            ContinueAfterStopLimit::Unlimited => -1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_manifest_preserves_required_shapes_and_rejects_unknown_values() {
        let manifest = InitializeManifest {
            required_transport_features: vec![TransportFeature::AuthenticatedHttp],
            capabilities: Vec::new(),
            tools: Vec::new(),
            hooks: vec![ManifestHook {
                on: LifecycleEvent::ContinueAfterStop.into(),
                mode: HookMode::Blocking,
                options: ManifestHookOptions {
                    max_per_turn: Some(ContinueAfterStopLimit::unlimited()),
                },
            }],
            commands: vec![ManifestCommand {
                name: "inspect".into(),
                description: "Inspect session state".into(),
                args_schema: Some(serde_json::json!({ "type": "string" })),
                requires_idle: true,
                argument_completions: true,
                priority: 17,
                availability: CommandAvailability::InteractiveOnly,
                execution: CommandExecution::Host(SessionCommandKind::SelectModel),
            }],
            http_routes: Vec::new(),
            custom_events: Vec::new(),
            custom_event_subscriptions: Vec::new(),
        };
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["hooks"][0]["options"]["max_per_turn"], -1);
        let command = &value["commands"][0];
        assert_eq!(
            command["args_schema"],
            serde_json::json!({ "type": "string" })
        );
        assert_eq!(command["requires_idle"], true);
        assert_eq!(command["argument_completions"], true);
        assert_eq!(command["priority"], 17);
        assert_eq!(command["availability"], "interactive_only");
        assert_eq!(
            command["execution"],
            serde_json::json!({"kind": "host", "command": "select_model"})
        );
        let decoded = serde_json::from_value::<InitializeManifest>(value.clone()).unwrap();
        assert_eq!(
            decoded.required_transport_features,
            [TransportFeature::AuthenticatedHttp]
        );

        let mut missing_transport_requirements = value;
        missing_transport_requirements
            .as_object_mut()
            .unwrap()
            .remove("required_transport_features");
        assert!(
            serde_json::from_value::<InitializeManifest>(missing_transport_requirements).is_err()
        );

        for invalid in [
            serde_json::json!({"required_transport_features": [], "future_field": true}),
            serde_json::json!({
                "required_transport_features": [],
                "hooks": [{"on": "unknown", "mode": "blocking"}]
            }),
            serde_json::json!({
                "required_transport_features": [],
                "hooks": [{"on": "turn_end", "mode": "unknown"}]
            }),
            serde_json::json!({"required_transport_features": [], "tools": [{
                "name": "tool",
                "description": "",
                "parameters": {},
                "mode": "unknown"
            }]}),
        ] {
            assert!(serde_json::from_value::<InitializeManifest>(invalid).is_err());
        }

        let command = serde_json::json!({
            "name": "inspect",
            "description": "Inspect session state",
            "args_schema": null,
            "requires_idle": false,
            "argument_completions": false,
            "priority": 0,
            "availability": "all_transports",
            "execution": {"kind": "extension"}
        });
        for required_field in [
            "name",
            "description",
            "args_schema",
            "requires_idle",
            "argument_completions",
            "priority",
            "availability",
            "execution",
        ] {
            let mut incomplete = command.clone();
            incomplete.as_object_mut().unwrap().remove(required_field);
            assert!(
                serde_json::from_value::<InitializeManifest>(serde_json::json!({
                    "required_transport_features": [],
                    "commands": [incomplete]
                }))
                .is_err(),
                "missing command field {required_field} must be rejected"
            );
        }
    }
}
