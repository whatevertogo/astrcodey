//! Mode types, catalog, and built-in mode definitions.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

/// Mode identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ModeId(String);

impl ModeId {
    pub(crate) fn code() -> Self {
        Self("code".into())
    }

    pub(crate) fn plan() -> Self {
        Self("plan".into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_raw(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Display for ModeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Tools blocked in plan mode.
const PLAN_RESTRICTED_TOOLS: &[&str] = &["write", "edit", "patch", "shell", "shell_poll"];

/// Declarative definition of an agent running mode.
#[derive(Debug, Clone)]
pub(crate) struct ModeSpec {
    pub id: ModeId,
    pub name: String,
    /// Tool names that are blocked in this mode.
    pub restricted_tools: HashSet<String>,
    /// Mode IDs this mode can transition to.
    pub allowed_transitions: Vec<ModeId>,
    /// Whether this mode requires a plan artifact to exist before allowing exit.
    pub requires_plan_artifact: bool,
}

/// Registry of available modes with lookup by ID.
#[derive(Clone)]
pub(crate) struct ModeCatalog {
    modes: Vec<ModeSpec>,
    index: BTreeMap<String, usize>,
}

impl ModeCatalog {
    pub(crate) fn new(modes: Vec<ModeSpec>) -> Self {
        let index = modes
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.as_str().to_string(), i))
            .collect();
        Self { modes, index }
    }

    pub(crate) fn get(&self, id: &ModeId) -> Option<&ModeSpec> {
        self.index.get(id.as_str()).map(|&i| &self.modes[i])
    }
}

/// Validates whether transitioning from one mode to another is allowed.
pub(crate) fn validate_transition(
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

fn builtin_mode_specs() -> Vec<ModeSpec> {
    let transitions = vec![ModeId::code(), ModeId::plan()];
    vec![
        ModeSpec {
            id: ModeId::code(),
            name: "Code".into(),
            restricted_tools: HashSet::new(),
            allowed_transitions: transitions.clone(),
            requires_plan_artifact: false,
        },
        ModeSpec {
            id: ModeId::plan(),
            name: "Plan".into(),
            restricted_tools: PLAN_RESTRICTED_TOOLS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            allowed_transitions: transitions,
            requires_plan_artifact: true,
        },
    ]
}

pub(crate) fn builtin_catalog() -> ModeCatalog {
    ModeCatalog::new(builtin_mode_specs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_mode_restricts_write_tools() {
        let catalog = builtin_catalog();
        let plan = catalog.get(&ModeId::plan()).unwrap();
        assert!(plan.restricted_tools.contains("write"));
        assert!(plan.restricted_tools.contains("shell"));
        assert!(plan.restricted_tools.contains("shell_poll"));
    }

    #[test]
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
