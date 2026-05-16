//! Mode state and plan artifact persistence.

use std::path::{Path, PathBuf};

use astrcode_support::hostpaths;
use serde::{Deserialize, Serialize};

/// Per-session mode state persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModeState {
    pub current_mode: String,
    #[serde(default)]
    pub previous_mode: Option<String>,
    /// Set when a transition just happened; cleared after injection.
    #[serde(default)]
    pub pending_transition_context: Option<String>,
    /// Exit gate: number of review passes completed during current plan mode session.
    #[serde(default)]
    pub exit_review_passes_completed: u32,
}

impl ModeState {
    pub fn initial() -> Self {
        Self {
            current_mode: "code".into(),
            previous_mode: None,
            pending_transition_context: None,
            exit_review_passes_completed: 0,
        }
    }
}

const MODE_STATE_FILE: &str = "mode-state.json";
const PLAN_FILE: &str = "plan.md";

/// Compute the mode state storage root for a session.
pub fn mode_store_root(session_id: &str, working_dir: &str) -> PathBuf {
    hostpaths::session_dir_for_project_path(&PathBuf::from(working_dir), session_id).join("mode")
}

/// Compute the plan artifact directory for a session.
pub fn plan_dir(session_id: &str, working_dir: &str) -> PathBuf {
    hostpaths::session_plan_dir_for_project_path(&PathBuf::from(working_dir), session_id)
}

pub fn load_mode_state(root: &Path) -> Result<ModeState, String> {
    let path = root.join(MODE_STATE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).map_err(|e| format!("parse mode state: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ModeState::initial()),
        Err(e) => Err(format!("read mode state: {e}")),
    }
}

pub fn save_mode_state(root: &Path, state: &ModeState) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("create mode directory: {e}"))?;
    let path = root.join(MODE_STATE_FILE);
    let tmp = root.join(format!("{MODE_STATE_FILE}.tmp"));
    let json =
        serde_json::to_string_pretty(state).map_err(|e| format!("serialize mode state: {e}"))?;
    std::fs::write(&tmp, json).map_err(|e| format!("write mode state: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("save mode state: {e}"))?;
    Ok(())
}

pub fn plan_file_path(plan_dir: &Path) -> PathBuf {
    plan_dir.join(PLAN_FILE)
}

pub fn load_plan(plan_dir: &Path) -> Result<Option<String>, String> {
    let path = plan_file_path(plan_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read plan artifact: {e}")),
    }
}

pub fn save_plan(plan_dir: &Path, content: &str) -> Result<String, String> {
    std::fs::create_dir_all(plan_dir).map_err(|e| format!("create plan directory: {e}"))?;
    let path = plan_file_path(plan_dir);
    let tmp = plan_dir.join("plan.md.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write plan artifact: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("save plan artifact: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

/// Validate that plan content contains all required headings.
pub fn validate_plan_headings(content: &str) -> Vec<String> {
    crate::catalog::PLAN_REQUIRED_HEADINGS
        .iter()
        .filter(|heading| {
            let pattern = format!("## {}", heading);
            !content.contains(&pattern)
        })
        .map(|s| (*s).to_string())
        .collect()
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
        assert_eq!(state.exit_review_passes_completed, 0);
    }

    #[test]
    fn round_trip_mode_state() {
        let root = test_root("round-trip");
        let state = ModeState {
            current_mode: "plan".into(),
            previous_mode: Some("code".into()),
            pending_transition_context: Some("entered plan".into()),
            exit_review_passes_completed: 1,
        };
        save_mode_state(&root, &state).unwrap();
        let loaded = load_mode_state(&root).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn validate_plan_headings_detects_missing() {
        let plan = "# Plan: test\n\n## Goal\n\n## Scope\n";
        let missing = validate_plan_headings(plan);
        assert!(missing.contains(&"Context".to_string()));
        assert!(missing.contains(&"Verification".to_string()));
        assert!(!missing.contains(&"Goal".to_string()));
    }

    #[test]
    fn validate_plan_headings_accepts_complete_plan() {
        let plan = crate::prompts::plan_template().replace("<title>", "test");
        let missing = validate_plan_headings(&plan);
        assert!(missing.is_empty());
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
