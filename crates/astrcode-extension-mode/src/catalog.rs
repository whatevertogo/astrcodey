//! Mode types, catalog, and built-in mode definitions.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

/// Mode identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModeId(String);

impl ModeId {
    pub fn code() -> Self {
        Self("code".into())
    }

    pub fn plan() -> Self {
        Self("plan".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_raw(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Display for ModeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Required headings for the plan artifact.
pub const PLAN_REQUIRED_HEADINGS: &[&str] = &[
    "Context",
    "Goal",
    "Scope",
    "Non-Goals",
    "Existing Code to Reuse",
    "Implementation Steps",
    "Verification",
    "Dependencies and Risks",
    "Assumptions",
];

/// Review checklist shown to the LLM during the exit gate.
pub const EXIT_REVIEW_CHECKLIST: &[&str] = &[
    "Are all assumptions in the plan verified against the actual code?",
    "Are edge cases and affected files identified?",
    "Are verification steps concrete and sufficient?",
    "Is the plan executable as-is?",
];

/// Tools blocked in plan mode.
const PLAN_RESTRICTED_TOOLS: &[&str] = &[];

/// Declarative definition of an agent running mode.
#[derive(Debug, Clone)]
pub struct ModeSpec {
    pub id: ModeId,
    pub name: String,
    pub description: String,
    /// Tool names that are blocked in this mode.
    pub restricted_tools: HashSet<String>,
    /// Mode IDs this mode can transition to.
    pub allowed_transitions: Vec<ModeId>,
    /// Whether this mode requires a plan artifact to exist before allowing exit.
    pub requires_plan_artifact: bool,
    /// Number of review passes required before exiting this mode.
    pub exit_review_passes: u32,
}

/// Registry of available modes with lookup by ID.
#[derive(Clone)]
pub struct ModeCatalog {
    modes: Vec<ModeSpec>,
    index: BTreeMap<String, usize>,
}

impl ModeCatalog {
    pub fn new(modes: Vec<ModeSpec>) -> Self {
        let index = modes
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.as_str().to_string(), i))
            .collect();
        Self { modes, index }
    }

    pub fn get(&self, id: &ModeId) -> Option<&ModeSpec> {
        self.index.get(id.as_str()).map(|&i| &self.modes[i])
    }

    pub fn list(&self) -> &[ModeSpec] {
        &self.modes
    }
}

/// Validates whether transitioning from one mode to another is allowed.
pub fn validate_transition(
    catalog: &ModeCatalog,
    from: &ModeId,
    to: &ModeId,
) -> Result<(), String> {
    let from_spec = catalog
        .get(from)
        .ok_or_else(|| format!("unknown source mode '{}'", from))?;
    catalog
        .get(to)
        .ok_or_else(|| format!("unknown target mode '{}'", to))?;
    if !from_spec.allowed_transitions.iter().any(|t| t == to) {
        return Err(format!(
            "transition from '{}' to '{}' is not allowed",
            from, to
        ));
    }
    Ok(())
}

pub fn builtin_mode_specs() -> Vec<ModeSpec> {
    let transitions = vec![ModeId::code(), ModeId::plan()];
    vec![
        ModeSpec {
            id: ModeId::code(),
            name: "Code".into(),
            description: "Default execution mode with full capabilities.".into(),
            restricted_tools: HashSet::new(),
            allowed_transitions: transitions.clone(),
            requires_plan_artifact: false,
            exit_review_passes: 0,
        },
        ModeSpec {
            id: ModeId::plan(),
            name: "Plan".into(),
            description: "Planning mode with full tool access for producing a structured plan."
                .into(),
            restricted_tools: PLAN_RESTRICTED_TOOLS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            allowed_transitions: transitions,
            requires_plan_artifact: true,
            exit_review_passes: 1,
        },
    ]
}

pub fn builtin_catalog() -> ModeCatalog {
    ModeCatalog::new(builtin_mode_specs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_contains_code_and_plan() {
        let catalog = builtin_catalog();
        assert!(catalog.get(&ModeId::code()).is_some());
        assert!(catalog.get(&ModeId::plan()).is_some());
        assert_eq!(catalog.list().len(), 2);
    }

    #[test]
    fn plan_mode_has_no_restricted_tools() {
        let catalog = builtin_catalog();
        let plan = catalog.get(&ModeId::plan()).unwrap();
        assert!(plan.restricted_tools.is_empty());
    }

    #[test]
    // TODO: a better way to enforce that critical tools like "agent" are not accidentally
    // restricted by mode definitions.
    fn plan_mode_does_not_restrict_agent_tool() {
        let catalog = builtin_catalog();
        assert!(
            !catalog
                .get(&ModeId::plan())
                .unwrap()
                .restricted_tools
                .contains("agent")
        );
    }

    #[test]
    fn code_mode_does_not_restrict_agent_tool() {
        let catalog = builtin_catalog();
        assert!(
            !catalog
                .get(&ModeId::code())
                .unwrap()
                .restricted_tools
                .contains("agent")
        );
    }

    #[test]
    fn transition_code_to_plan_is_allowed() {
        let catalog = builtin_catalog();
        assert!(validate_transition(&catalog, &ModeId::code(), &ModeId::plan()).is_ok());
    }

    #[test]
    fn transition_plan_to_code_is_allowed() {
        let catalog = builtin_catalog();
        assert!(validate_transition(&catalog, &ModeId::plan(), &ModeId::code()).is_ok());
    }

    #[test]
    fn transition_to_unknown_mode_is_rejected() {
        let catalog = builtin_catalog();
        let err = validate_transition(&catalog, &ModeId::code(), &ModeId::from_raw("unknown"))
            .unwrap_err();
        assert!(err.contains("unknown target mode"));
    }
}
