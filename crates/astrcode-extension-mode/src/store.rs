//! Mode state and plan artifact persistence.

use std::path::{Path, PathBuf};

use astrcode_extension_sdk::hostpaths;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PendingModeTransition {
    pub id: String,
    pub context: String,
}

/// Per-session mode state persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ModeState {
    pub current_mode: String,
    #[serde(default)]
    pub previous_mode: Option<String>,
    /// Transition context remains pending until its provider cycle is durably committed.
    pub pending_transition: Option<PendingModeTransition>,
    /// True if the user entered plan mode (slash command / keybinding).
    /// False if the LLM entered plan mode via `switchMode` tool call.
    /// Controls whether exiting plan mode requires user approval.
    #[serde(default)]
    pub user_initiated: bool,
}

impl ModeState {
    pub(crate) fn initial() -> Self {
        Self {
            current_mode: "code".into(),
            previous_mode: None,
            pending_transition: None,
            user_initiated: false,
        }
    }

    pub(crate) fn replace_pending_transition(&mut self, context: Option<String>) {
        self.pending_transition = context.map(|context| PendingModeTransition {
            id: uuid::Uuid::new_v4().to_string(),
            context,
        });
    }

    pub(crate) fn acknowledge_transition(&mut self, contribution_id: &str) -> bool {
        let matches = self
            .pending_transition
            .as_ref()
            .is_some_and(|pending| pending.id == contribution_id);
        if matches {
            self.pending_transition = None;
        }
        matches
    }
}

const MODE_STATE_FILE: &str = "mode-state.json";
const PLAN_FILE: &str = "plan.md";

/// Compute the mode state storage root from a known session base directory.
pub(crate) fn mode_dir_from_base(base: &Path) -> PathBuf {
    base.join("mode")
}

/// Compute the plan artifact directory from a known session base directory.
pub(crate) fn plan_dir_from_base(base: &Path) -> PathBuf {
    base.join("plan")
}

pub(crate) fn load_mode_state(root: &Path) -> Result<ModeState, String> {
    let path = root.join(MODE_STATE_FILE);
    Ok(hostpaths::read_json_state(&path)
        .map_err(|e| format!("read mode state: {e}"))?
        .unwrap_or_else(ModeState::initial))
}

pub(crate) fn save_mode_state(root: &Path, state: &ModeState) -> Result<(), String> {
    hostpaths::write_json_state(&root.join(MODE_STATE_FILE), state)
        .map_err(|e| format!("save mode state: {e}"))
}

pub(crate) fn acknowledge_mode_transition(
    root: &Path,
    contribution_id: &str,
) -> Result<(), String> {
    hostpaths::update_json_state(&root.join(MODE_STATE_FILE), |state: Option<ModeState>| {
        let Some(mut state) = state else {
            return Ok((None, ()));
        };
        if !state.acknowledge_transition(contribution_id) {
            return Ok((None, ()));
        }
        Ok((Some(state), ()))
    })
    .map_err(|error| format!("ack mode transition: {error}"))
}

pub(crate) fn plan_file_path(plan_dir: &Path) -> PathBuf {
    plan_dir.join(PLAN_FILE)
}

pub(crate) fn load_plan(plan_dir: &Path) -> Result<Option<String>, String> {
    let path = plan_file_path(plan_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read plan artifact: {e}")),
    }
}

pub(crate) fn save_plan(plan_dir: &Path, content: &str) -> Result<String, String> {
    std::fs::create_dir_all(plan_dir).map_err(|e| format!("create plan directory: {e}"))?;
    let path = plan_file_path(plan_dir);
    hostpaths::write_file_atomic(&path, content).map_err(|e| format!("save plan artifact: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("astrcode-mode-store-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn initial_state_is_code_mode() {
        let state = ModeState::initial();
        assert_eq!(state.current_mode, "code");
        assert!(state.previous_mode.is_none());
    }

    #[test]
    fn round_trip_mode_state() {
        let root = test_root("round-trip");
        let state = ModeState {
            current_mode: "plan".into(),
            previous_mode: Some("code".into()),
            pending_transition: Some(PendingModeTransition {
                id: "transition-1".into(),
                context: "entered plan".into(),
            }),
            user_initiated: false,
        };
        save_mode_state(&root, &state).unwrap();
        let loaded = load_mode_state(&root).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn transition_is_retryable_and_old_ack_cannot_clear_a_newer_transition() {
        let root = test_root("exact-transition-ack");
        let mut state = ModeState::initial();
        state.replace_pending_transition(Some("enter plan".into()));
        let first_id = state.pending_transition.as_ref().unwrap().id.clone();
        save_mode_state(&root, &state).unwrap();

        let prepared_again = load_mode_state(&root).unwrap();
        assert_eq!(
            prepared_again.pending_transition.as_ref().unwrap().id,
            first_id,
            "preparing again after failure must retain the same contribution"
        );

        state.replace_pending_transition(Some("return to code".into()));
        let newer = state.pending_transition.clone().unwrap();
        save_mode_state(&root, &state).unwrap();
        acknowledge_mode_transition(&root, &first_id).unwrap();
        assert_eq!(
            load_mode_state(&root).unwrap().pending_transition.as_ref(),
            Some(&newer)
        );
        acknowledge_mode_transition(&root, &newer.id).unwrap();
        assert!(load_mode_state(&root).unwrap().pending_transition.is_none());
    }

    #[test]
    fn save_and_load_plan_artifact() {
        let dir = test_root("plan-artifact");
        let content = "# Plan: test\n\n## Goal\n\nDo something.\n";
        let path = save_plan(&dir, content).unwrap();
        assert!(path.ends_with("plan.md"));

        let loaded = load_plan(&dir).unwrap();
        assert_eq!(loaded.unwrap(), content);
    }

    #[test]
    fn load_plan_returns_none_when_missing() {
        let dir = test_root("no-plan");
        assert!(load_plan(&dir).unwrap().is_none());
    }
}
