//! Typed initialization declaration shared by S5R workers and the host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire::{
    ExtensionCapability, HandlerId,
    custom_event::{CustomEventDeclaration, CustomEventSubscription},
    extension_http::ExtensionHttpRoute,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeManifest {
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
    #[serde(default)]
    pub description: String,
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
    PreToolUse,
    PostToolUse,
    BeforeProviderRequest,
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
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::BeforeProviderRequest => "before_provider_request",
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
    fn initialize_manifest_preserves_hook_limits_and_rejects_unknown_values() {
        let manifest = InitializeManifest {
            capabilities: Vec::new(),
            tools: Vec::new(),
            hooks: vec![ManifestHook {
                on: LifecycleEvent::ContinueAfterStop.into(),
                mode: HookMode::Blocking,
                options: ManifestHookOptions {
                    max_per_turn: Some(ContinueAfterStopLimit::unlimited()),
                },
            }],
            commands: Vec::new(),
            http_routes: Vec::new(),
            custom_events: Vec::new(),
            custom_event_subscriptions: Vec::new(),
        };
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["hooks"][0]["options"]["max_per_turn"], -1);

        for invalid in [
            serde_json::json!({"future_field": true}),
            serde_json::json!({"hooks": [{"on": "unknown", "mode": "blocking"}]}),
            serde_json::json!({"hooks": [{"on": "turn_end", "mode": "unknown"}]}),
            serde_json::json!({"tools": [{
                "name": "tool",
                "description": "",
                "parameters": {},
                "mode": "unknown"
            }]}),
        ] {
            assert!(serde_json::from_value::<InitializeManifest>(invalid).is_err());
        }
    }
}
