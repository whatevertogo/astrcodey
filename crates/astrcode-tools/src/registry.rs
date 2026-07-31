//! Built-in tool catalog implementation.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use astrcode_core::{config::defaults::DEFAULT_SHELL_TIMEOUT_SECS, tool::Tool};
use astrcode_extension_sdk::{
    extension::ExtensionError,
    runtime_ports::{ToolCatalogProvider, ToolCatalogScope, ToolCatalogSnapshot},
};

/// First-party file, shell, and terminal tool catalog.
pub struct BuiltinToolCatalog {
    shell_timeout_secs: Arc<AtomicU64>,
    observed_config: Mutex<ObservedToolCatalogConfig>,
}

#[derive(Clone, Copy)]
struct ObservedToolCatalogConfig {
    shell_timeout_secs: u64,
    version: u64,
}

impl BuiltinToolCatalog {
    pub fn new(shell_timeout_secs: u64) -> Self {
        Self {
            shell_timeout_secs: Arc::new(AtomicU64::new(shell_timeout_secs)),
            observed_config: Mutex::new(ObservedToolCatalogConfig {
                shell_timeout_secs,
                version: 0,
            }),
        }
    }

    pub fn with_shell_timeout_source(shell_timeout_secs: Arc<AtomicU64>) -> Self {
        let initial_timeout = shell_timeout_secs.load(Ordering::Acquire);
        Self {
            shell_timeout_secs,
            observed_config: Mutex::new(ObservedToolCatalogConfig {
                shell_timeout_secs: initial_timeout,
                version: 0,
            }),
        }
    }

    pub fn set_shell_timeout_secs(&self, shell_timeout_secs: u64) {
        self.shell_timeout_secs
            .store(shell_timeout_secs, Ordering::Release);
    }

    fn observe_config(&self) -> ObservedToolCatalogConfig {
        let mut observed = self
            .observed_config
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let shell_timeout_secs = self.shell_timeout_secs.load(Ordering::Acquire);
        if observed.shell_timeout_secs != shell_timeout_secs {
            observed.shell_timeout_secs = shell_timeout_secs;
            observed.version = observed.version.wrapping_add(1);
        }
        *observed
    }
}

impl Default for BuiltinToolCatalog {
    fn default() -> Self {
        Self::new(DEFAULT_SHELL_TIMEOUT_SECS)
    }
}

#[async_trait::async_trait]
impl ToolCatalogProvider for BuiltinToolCatalog {
    fn revision(&self) -> u64 {
        self.observe_config().version
    }

    async fn tool_catalog(
        &self,
        scope: &ToolCatalogScope,
    ) -> Result<ToolCatalogSnapshot, ExtensionError> {
        let config = self.observe_config();
        Ok(ToolCatalogSnapshot::complete(
            config.version,
            builtin_tools(PathBuf::from(&scope.working_dir), config.shell_timeout_secs),
        ))
    }
}

/// Build the default built-in tool set for one session working directory.
pub fn builtin_tools(working_dir: PathBuf, timeout_secs: u64) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(super::files::ReadFileTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::WriteFileTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::EditFileTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::ApplyPatchTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::GlobTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::GrepTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::shell_tool::ShellTool {
            working_dir: working_dir.clone(),
            timeout_secs,
        }) as Arc<dyn Tool>,
        Arc::new(super::terminal_tool::TerminalTool { working_dir }) as Arc<dyn Tool>,
    ]
}

pub fn default_tool_catalog() -> Arc<dyn ToolCatalogProvider> {
    Arc::new(BuiltinToolCatalog::default())
}

pub fn default_tool_catalog_with_shell_timeout_source(
    shell_timeout_secs: Arc<AtomicU64>,
) -> Arc<dyn ToolCatalogProvider> {
    Arc::new(BuiltinToolCatalog::with_shell_timeout_source(
        shell_timeout_secs,
    ))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use astrcode_core::tool::{
        ExecutionMode,
        access::{FileOperation, ResourceAccess},
    };

    use super::*;

    #[test]
    fn builtins_expose_expected_contract() {
        let catalog = BuiltinToolCatalog::new(30);
        let initial_revision = catalog.revision();
        catalog.set_shell_timeout_secs(60);
        assert!(catalog.revision() > initial_revision);

        let tools = builtin_tools(PathBuf::from("."), 30);
        let definitions = tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        let names = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert!(
            ["patch", "edit", "shell"]
                .iter()
                .all(|name| names.contains(name))
        );
        assert!(definitions.iter().all(|definition| {
            definition.origin == astrcode_core::tool::ToolOrigin::Builtin && definition.strict
        }));

        let read = tools
            .iter()
            .find(|tool| tool.definition().name == "read")
            .unwrap();
        let read_access = read
            .resource_accesses(&serde_json::json!({"path": "src/main.rs"}), Path::new("."))
            .unwrap();
        assert_eq!(read_access.len(), 1);
        assert!(matches!(
            read_access[0],
            ResourceAccess::File {
                operation: FileOperation::Read,
                ..
            }
        ));

        let shell = tools
            .iter()
            .find(|tool| tool.definition().name == "shell")
            .unwrap();
        let shell_access = shell
            .resource_accesses(&serde_json::json!({"command": "echo hi"}), Path::new("."))
            .unwrap();
        assert_eq!(shell_access, vec![ResourceAccess::all()]);

        let modes = definitions
            .into_iter()
            .map(|definition| (definition.name, definition.execution_mode))
            .collect::<BTreeMap<_, _>>();

        for name in ["glob", "grep", "read"] {
            assert_eq!(modes[name], ExecutionMode::Parallel);
        }
        for name in ["edit", "patch", "shell", "terminal", "write"] {
            assert_eq!(modes[name], ExecutionMode::Sequential);
        }
    }
}
