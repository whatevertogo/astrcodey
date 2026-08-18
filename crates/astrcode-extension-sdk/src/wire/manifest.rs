//! Typed initialization declaration shared by S5R workers and the host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire::{
    ExtensionCapability, HandlerId, SlashCommand,
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
    pub commands: Vec<SlashCommand>,
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
    /// Per-tool invoke timeout in milliseconds; absent means the host default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestToolMode {
    Parallel,
    #[default]
    Sequential,
}

/// Hook subscription declaration in an S5R manifest.
///
/// The event name is carried by the variant tag (the `on` field); payloads differ per family:
/// only tool hooks carry `tools`, and only continue_after_stop carries `options`. Invalid field
/// combinations are rejected at deserialization time and are unrepresentable in the type; legal
/// payloads have the same JSON shape as the old flat format, so old and new host/worker builds
/// can be deployed interchangeably.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum ManifestHook {
    ToolInputTransform(ToolHookManifest),
    PreToolUse(ToolHookManifest),
    PostToolUse(ToolHookManifest),
    ContinueAfterStop(ContinueAfterStopHookManifest),
    PreCompact(CommonHookManifest),
    PostCompact(CommonHookManifest),
    SessionStart(CommonHookManifest),
    SessionResume(CommonHookManifest),
    SessionShutdown(CommonHookManifest),
    TurnStart(CommonHookManifest),
    TurnEnd(CommonHookManifest),
    TurnAborted(CommonHookManifest),
    StepStart(CommonHookManifest),
    StepEnd(CommonHookManifest),
    BeforeProviderRequest(CommonHookManifest),
    ProviderContribution(CommonHookManifest),
    AfterProviderResponse(CommonHookManifest),
    UserPromptSubmit(CommonHookManifest),
    UserMessageEnvelope(CommonHookManifest),
    PromptBuild(CommonHookManifest),
    PostRecap(CommonHookManifest),
}

/// Declaration payload for most hooks: mode and an optional priority only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommonHookManifest {
    pub mode: HookMode,
    /// Optional scheduling priority, defaulting to 0; the host dispatches in descending order,
    /// and equal priorities keep registration order. When absent it is omitted from
    /// serialization, so old hosts' field validation is unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

/// Declaration payload for tool hooks: additionally carries an exact tool-name filter; by
/// default matches all tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHookManifest {
    pub mode: HookMode,
    /// Same semantics as [`CommonHookManifest::priority`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Exact tool-name filter; when absent it is omitted from serialization, with the same
    /// compatibility semantics as `priority`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

/// Declaration payload for continue_after_stop: additionally carries the per-turn continuation
/// limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueAfterStopHookManifest {
    pub mode: HookMode,
    /// Same semantics as [`CommonHookManifest::priority`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(default, skip_serializing_if = "ManifestHookOptions::is_empty")]
    pub options: ManifestHookOptions,
}

impl ManifestHook {
    /// Construct a declaration for a semantic event. `options` is only retained for
    /// ContinueAfterStop; the registry passes a non-default value only for that event and
    /// ignores it for all others.
    pub fn new(on: ManifestHookEvent, mode: HookMode, options: ManifestHookOptions) -> Self {
        fn common(mode: HookMode) -> CommonHookManifest {
            CommonHookManifest {
                mode,
                priority: None,
            }
        }
        fn tool(mode: HookMode) -> ToolHookManifest {
            ToolHookManifest {
                mode,
                priority: None,
                tools: None,
            }
        }
        match on {
            ManifestHookEvent::Compact(CompactEvent::PreCompact) => Self::PreCompact(common(mode)),
            ManifestHookEvent::Compact(CompactEvent::PostCompact) => {
                Self::PostCompact(common(mode))
            },
            ManifestHookEvent::Lifecycle(event) => match event {
                LifecycleEvent::ToolInputTransform => Self::ToolInputTransform(tool(mode)),
                LifecycleEvent::PreToolUse => Self::PreToolUse(tool(mode)),
                LifecycleEvent::PostToolUse => Self::PostToolUse(tool(mode)),
                LifecycleEvent::ContinueAfterStop => {
                    Self::ContinueAfterStop(ContinueAfterStopHookManifest {
                        mode,
                        priority: None,
                        options,
                    })
                },
                LifecycleEvent::SessionStart => Self::SessionStart(common(mode)),
                LifecycleEvent::SessionResume => Self::SessionResume(common(mode)),
                LifecycleEvent::SessionShutdown => Self::SessionShutdown(common(mode)),
                LifecycleEvent::TurnStart => Self::TurnStart(common(mode)),
                LifecycleEvent::TurnEnd => Self::TurnEnd(common(mode)),
                LifecycleEvent::TurnAborted => Self::TurnAborted(common(mode)),
                LifecycleEvent::StepStart => Self::StepStart(common(mode)),
                LifecycleEvent::StepEnd => Self::StepEnd(common(mode)),
                LifecycleEvent::BeforeProviderRequest => Self::BeforeProviderRequest(common(mode)),
                LifecycleEvent::ProviderContribution => Self::ProviderContribution(common(mode)),
                LifecycleEvent::AfterProviderResponse => Self::AfterProviderResponse(common(mode)),
                LifecycleEvent::UserPromptSubmit => Self::UserPromptSubmit(common(mode)),
                LifecycleEvent::UserMessageEnvelope => Self::UserMessageEnvelope(common(mode)),
                LifecycleEvent::PromptBuild => Self::PromptBuild(common(mode)),
                LifecycleEvent::PostRecap => Self::PostRecap(common(mode)),
            },
        }
    }

    /// The semantic event corresponding to this variant tag.
    pub fn event(&self) -> ManifestHookEvent {
        let event = match self {
            Self::ToolInputTransform(_) => LifecycleEvent::ToolInputTransform,
            Self::PreToolUse(_) => LifecycleEvent::PreToolUse,
            Self::PostToolUse(_) => LifecycleEvent::PostToolUse,
            Self::ContinueAfterStop(_) => LifecycleEvent::ContinueAfterStop,
            Self::PreCompact(_) => return CompactEvent::PreCompact.into(),
            Self::PostCompact(_) => return CompactEvent::PostCompact.into(),
            Self::SessionStart(_) => LifecycleEvent::SessionStart,
            Self::SessionResume(_) => LifecycleEvent::SessionResume,
            Self::SessionShutdown(_) => LifecycleEvent::SessionShutdown,
            Self::TurnStart(_) => LifecycleEvent::TurnStart,
            Self::TurnEnd(_) => LifecycleEvent::TurnEnd,
            Self::TurnAborted(_) => LifecycleEvent::TurnAborted,
            Self::StepStart(_) => LifecycleEvent::StepStart,
            Self::StepEnd(_) => LifecycleEvent::StepEnd,
            Self::BeforeProviderRequest(_) => LifecycleEvent::BeforeProviderRequest,
            Self::ProviderContribution(_) => LifecycleEvent::ProviderContribution,
            Self::AfterProviderResponse(_) => LifecycleEvent::AfterProviderResponse,
            Self::UserPromptSubmit(_) => LifecycleEvent::UserPromptSubmit,
            Self::UserMessageEnvelope(_) => LifecycleEvent::UserMessageEnvelope,
            Self::PromptBuild(_) => LifecycleEvent::PromptBuild,
            Self::PostRecap(_) => LifecycleEvent::PostRecap,
        };
        event.into()
    }

    pub fn event_name(&self) -> &'static str {
        self.event().as_str()
    }

    pub fn mode(&self) -> HookMode {
        match self {
            Self::ToolInputTransform(p) | Self::PreToolUse(p) | Self::PostToolUse(p) => p.mode,
            Self::ContinueAfterStop(p) => p.mode,
            Self::PreCompact(p)
            | Self::PostCompact(p)
            | Self::SessionStart(p)
            | Self::SessionResume(p)
            | Self::SessionShutdown(p)
            | Self::TurnStart(p)
            | Self::TurnEnd(p)
            | Self::TurnAborted(p)
            | Self::StepStart(p)
            | Self::StepEnd(p)
            | Self::BeforeProviderRequest(p)
            | Self::ProviderContribution(p)
            | Self::AfterProviderResponse(p)
            | Self::UserPromptSubmit(p)
            | Self::UserMessageEnvelope(p)
            | Self::PromptBuild(p)
            | Self::PostRecap(p) => p.mode,
        }
    }

    pub fn priority(&self) -> Option<i32> {
        match self {
            Self::ToolInputTransform(p) | Self::PreToolUse(p) | Self::PostToolUse(p) => p.priority,
            Self::ContinueAfterStop(p) => p.priority,
            Self::PreCompact(p)
            | Self::PostCompact(p)
            | Self::SessionStart(p)
            | Self::SessionResume(p)
            | Self::SessionShutdown(p)
            | Self::TurnStart(p)
            | Self::TurnEnd(p)
            | Self::TurnAborted(p)
            | Self::StepStart(p)
            | Self::StepEnd(p)
            | Self::BeforeProviderRequest(p)
            | Self::ProviderContribution(p)
            | Self::AfterProviderResponse(p)
            | Self::UserPromptSubmit(p)
            | Self::UserMessageEnvelope(p)
            | Self::PromptBuild(p)
            | Self::PostRecap(p) => p.priority,
        }
    }

    pub fn set_priority(&mut self, priority: Option<i32>) {
        let slot = match self {
            Self::ToolInputTransform(p) | Self::PreToolUse(p) | Self::PostToolUse(p) => {
                &mut p.priority
            },
            Self::ContinueAfterStop(p) => &mut p.priority,
            Self::PreCompact(p)
            | Self::PostCompact(p)
            | Self::SessionStart(p)
            | Self::SessionResume(p)
            | Self::SessionShutdown(p)
            | Self::TurnStart(p)
            | Self::TurnEnd(p)
            | Self::TurnAborted(p)
            | Self::StepStart(p)
            | Self::StepEnd(p)
            | Self::BeforeProviderRequest(p)
            | Self::ProviderContribution(p)
            | Self::AfterProviderResponse(p)
            | Self::UserPromptSubmit(p)
            | Self::UserMessageEnvelope(p)
            | Self::PromptBuild(p)
            | Self::PostRecap(p) => &mut p.priority,
        };
        *slot = priority;
    }

    /// Only tool hooks return `Some`; other families have no `tools` field in the type.
    pub fn tool_target_mut(&mut self) -> Option<&mut Option<Vec<String>>> {
        match self {
            Self::ToolInputTransform(p) | Self::PreToolUse(p) | Self::PostToolUse(p) => {
                Some(&mut p.tools)
            },
            _ => None,
        }
    }

    /// Set the tool filter; non-tool hooks have no such field in the type and return `false`.
    pub fn set_tool_target(&mut self, tools: Vec<String>) -> bool {
        match self {
            Self::ToolInputTransform(p) | Self::PreToolUse(p) | Self::PostToolUse(p) => {
                p.tools = Some(tools);
                true
            },
            _ => false,
        }
    }
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
    pub fn is_empty(&self) -> bool {
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
    use crate::wire::command::{CommandAvailability, CommandExecution, SessionCommandKind};

    #[test]
    fn manifest_hook_priority_is_optional_and_omitted_when_default() {
        let without: ManifestHook =
            serde_json::from_value(serde_json::json!({"on": "turn_end", "mode": "non_blocking"}))
                .unwrap();
        assert_eq!(without.priority(), None);
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("priority")
                .is_none(),
            "default priority must stay absent for old hosts"
        );

        let with: ManifestHook = serde_json::from_value(
            serde_json::json!({"on": "turn_end", "mode": "non_blocking", "priority": 9}),
        )
        .unwrap();
        assert_eq!(with.priority(), Some(9));
        assert_eq!(serde_json::to_value(&with).unwrap()["priority"], 9);
    }

    #[test]
    fn manifest_hook_tools_lives_only_on_tool_hook_variants() {
        let without: ManifestHook =
            serde_json::from_value(serde_json::json!({"on": "pre_tool_use", "mode": "blocking"}))
                .unwrap();
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("tools")
                .is_none(),
            "default tools must stay absent for old hosts"
        );

        let with: ManifestHook = serde_json::from_value(
            serde_json::json!({"on": "pre_tool_use", "mode": "blocking", "tools": ["shell"]}),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&with).unwrap()["tools"][0], "shell");

        for invalid in [
            serde_json::json!({"on": "turn_end", "mode": "non_blocking", "tools": ["shell"]}),
            serde_json::json!({"on": "pre_compact", "mode": "blocking", "tools": ["shell"]}),
            serde_json::json!({"on": "turn_end", "mode": "non_blocking", "options": {"max_per_turn": 2}}),
            serde_json::json!({"on": "pre_tool_use", "mode": "blocking", "options": {"max_per_turn": 2}}),
        ] {
            assert!(
                serde_json::from_value::<ManifestHook>(invalid.clone()).is_err(),
                "family-foreign fields must fail deserialization: {invalid}"
            );
        }
    }

    #[test]
    fn initialize_manifest_preserves_required_shapes_and_rejects_unknown_values() {
        let manifest = InitializeManifest {
            required_transport_features: vec![TransportFeature::AuthenticatedHttp],
            capabilities: Vec::new(),
            tools: Vec::new(),
            hooks: vec![ManifestHook::new(
                LifecycleEvent::ContinueAfterStop.into(),
                HookMode::Blocking,
                ManifestHookOptions {
                    max_per_turn: Some(ContinueAfterStopLimit::unlimited()),
                },
            )],
            commands: vec![SlashCommand {
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
