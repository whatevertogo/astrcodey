//! Built-in tool pack implementation.

use std::{path::PathBuf, sync::Arc};

use astrcode_core::tool::Tool;
use astrcode_kernel::{ToolPack, ToolPackScope};

/// First-party file, shell, and terminal tools.
pub struct BuiltinToolPack;

impl ToolPack for BuiltinToolPack {
    fn tools(&self, scope: &ToolPackScope<'_>) -> Vec<Arc<dyn Tool>> {
        builtin_tools(PathBuf::from(scope.working_dir), scope.shell_timeout_secs)
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

pub fn default_tool_packs() -> Vec<Arc<dyn ToolPack>> {
    vec![Arc::new(BuiltinToolPack)]
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use astrcode_core::{
        tool::ExecutionMode,
        tool_access::{FileOperation, ResourceAccess},
    };
    use astrcode_kernel::ToolRegistry;

    use super::*;

    #[test]
    fn builtins_expose_patch_after_parser_is_wired() {
        let mut registry = ToolRegistry::new();
        for tool in builtin_tools(PathBuf::from("."), 30) {
            registry.register(tool);
        }

        let names = registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name == "patch"));
        assert!(names.iter().any(|name| name == "edit"));
        assert!(names.iter().any(|name| name == "shell"));
    }

    #[test]
    fn builtins_carry_builtin_origin() {
        let mut registry = ToolRegistry::new();
        for tool in builtin_tools(PathBuf::from("."), 30) {
            registry.register(tool);
        }

        assert!(
            registry
                .list_definitions()
                .iter()
                .all(|definition| definition.origin == astrcode_core::tool::ToolOrigin::Builtin)
        );
    }

    #[test]
    fn builtins_declare_resource_accesses() {
        let mut registry = ToolRegistry::new();
        for tool in builtin_tools(PathBuf::from("."), 30) {
            registry.register(tool);
        }

        let read_access = registry
            .resource_accesses(
                "read",
                &serde_json::json!({"path": "src/main.rs"}),
                Path::new("."),
            )
            .unwrap();
        assert_eq!(read_access.len(), 1);
        assert!(matches!(
            read_access[0],
            ResourceAccess::File {
                operation: FileOperation::Read,
                ..
            }
        ));

        let shell_access = registry
            .resource_accesses(
                "shell",
                &serde_json::json!({"command": "echo hi"}),
                Path::new("."),
            )
            .unwrap();
        assert_eq!(shell_access, vec![ResourceAccess::all()]);
    }

    #[test]
    fn builtins_use_read_parallel_write_sequential_modes() {
        let mut registry = ToolRegistry::new();
        for tool in builtin_tools(PathBuf::from("."), 30) {
            registry.register(tool);
        }

        let modes = registry
            .list_definitions()
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
