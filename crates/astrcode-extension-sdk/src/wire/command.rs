//! Command wire types shared by the host, bundled extensions, and S5R workers.

use serde::{Deserialize, Serialize};

/// Slash command declared by an extension.
///
/// This is both the authoring type for bundled extensions and the wire
/// declaration carried by an S5R [`crate::wire::manifest::InitializeManifest`].
/// The SDK authoring surface re-exports it from `extension::hooks::commands`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlashCommand {
    /// Command name without the leading slash `/`.
    ///
    /// Canonicalized at registration: lowercase and matching `[a-z][a-z0-9_-]*`
    /// (see [`crate::extension::registration_validation::canonicalize_command_name`]).
    pub name: String,
    /// Human-readable command description.
    pub description: String,
    /// JSON Schema definition of the arguments.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub args_schema: Option<serde_json::Value>,
    /// Whether the current session must be idle.
    pub requires_idle: bool,
    /// Whether argument completion is provided.
    pub argument_completions: bool,
    /// Priority when commands with the same name conflict; higher values win.
    pub priority: i32,
    /// Whether the command should only appear in transports with an interactive UI.
    pub availability: CommandAvailability,
    /// Responsibility boundary for command execution.
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
///
/// `Host` commands are executed entirely by the host behind its session
/// operation gate; the declaring extension supplies no handler. `Extension`
/// commands dispatch to the extension's [`crate::extension::CommandHandler`].
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

/// Normalize a slash command name for matching: strip a leading `/`, trim,
/// and lowercase.
///
/// Unlike registration-side canonicalization this never rejects input:
/// unregistered or malformed slash text falls through to the model as a plain
/// prompt, so matching must stay total.
pub fn normalize_slash_command_name(name: &str) -> String {
    name.trim().trim_start_matches('/').to_ascii_lowercase()
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
