use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use astrcode_support::hostpaths;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpConfig {
    pub(crate) servers: Vec<McpServerConfig>,
    pub(crate) diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerConfig {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RawMcpConfig {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, RawMcpServerConfig>,
}

#[derive(Debug, Deserialize)]
struct RawMcpServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
}

pub(crate) fn load_config(working_dir: &str) -> McpConfig {
    load_config_from_paths(
        &hostpaths::mcp_config_path(),
        &hostpaths::project_mcp_config_path(working_dir),
        working_dir,
        project_mcp_enabled(),
    )
}

fn project_mcp_enabled() -> bool {
    std::env::var("ASTRCODE_ENABLE_PROJECT_MCP")
        .map(|value| value == "1")
        .unwrap_or(false)
}

pub(crate) fn load_config_from_paths(
    global_path: &Path,
    project_path: &Path,
    working_dir: &str,
    project_enabled: bool,
) -> McpConfig {
    let mut merged = BTreeMap::new();
    let mut diagnostics = Vec::new();

    load_one_config(
        global_path,
        ConfigScope::Global,
        working_dir,
        &mut merged,
        &mut diagnostics,
    );

    if project_path.exists() {
        if project_enabled {
            load_one_config(
                project_path,
                ConfigScope::Project,
                working_dir,
                &mut merged,
                &mut diagnostics,
            );
        } else {
            diagnostics.push(format!(
                "project MCP config {} ignored; set ASTRCODE_ENABLE_PROJECT_MCP=1 to enable",
                project_path.display()
            ));
        }
    }

    McpConfig {
        servers: merged.into_values().collect(),
        diagnostics,
    }
}

#[derive(Debug, Clone, Copy)]
enum ConfigScope {
    Global,
    Project,
}

fn load_one_config(
    path: &Path,
    scope: ConfigScope,
    working_dir: &str,
    merged: &mut BTreeMap<String, McpServerConfig>,
    diagnostics: &mut Vec<String>,
) {
    if !path.exists() {
        return;
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            diagnostics.push(format!("read MCP config {}: {error}", path.display()));
            return;
        },
    };
    let raw = match serde_json::from_slice::<RawMcpConfig>(&bytes) {
        Ok(raw) => raw,
        Err(error) => {
            diagnostics.push(format!("parse MCP config {}: {error}", path.display()));
            return;
        },
    };

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for (name, server) in raw.mcp_servers {
        let Some(config) = validate_server(name, server, base_dir, scope, working_dir, diagnostics)
        else {
            continue;
        };
        merged.insert(config.name.clone(), config);
    }
}

fn validate_server(
    name: String,
    raw: RawMcpServerConfig,
    base_dir: &Path,
    scope: ConfigScope,
    working_dir: &str,
    diagnostics: &mut Vec<String>,
) -> Option<McpServerConfig> {
    if name.trim().is_empty() {
        diagnostics.push("skip MCP server with empty name".into());
        return None;
    }
    if raw.command.trim().is_empty() {
        diagnostics.push(format!("skip MCP server {name}: command is required"));
        return None;
    }

    let cwd = match raw.cwd {
        Some(cwd) if cwd.trim().is_empty() => None,
        Some(cwd) => {
            let resolved = hostpaths::resolve_path(base_dir, Path::new(&cwd));
            if matches!(scope, ConfigScope::Project)
                && !hostpaths::is_path_within(&resolved, Path::new(working_dir))
            {
                diagnostics.push(format!(
                    "skip MCP server {name}: cwd {} is outside workspace {}",
                    resolved.display(),
                    working_dir
                ));
                return None;
            }
            Some(resolved)
        },
        None => None,
    };

    Some(McpServerConfig {
        name,
        command: raw.command,
        args: raw.args,
        env: raw.env,
        cwd,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn missing_config_returns_empty_servers() {
        let root = unique_temp_dir("missing");
        let config = load_config_from_paths(
            &root.join("global.json"),
            &root.join("project.json"),
            &root.to_string_lossy(),
            false,
        );

        assert!(config.servers.is_empty());
        assert!(config.diagnostics.is_empty());
    }

    #[test]
    fn project_overrides_global_when_enabled() {
        let root = unique_temp_dir("override");
        fs::create_dir_all(&root).unwrap();
        let global = root.join("global.json");
        let project = root.join("project.json");
        fs::write(
            &global,
            r#"{"mcpServers":{"same":{"command":"global","args":["a"]}}}"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{"mcpServers":{"same":{"command":"project","args":["b"]}}}"#,
        )
        .unwrap();

        let config = load_config_from_paths(&global, &project, &root.to_string_lossy(), true);

        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].command, "project");
        assert_eq!(config.servers[0].args, vec!["b"]);
    }

    #[test]
    fn project_config_is_gated() {
        let root = unique_temp_dir("gated");
        fs::create_dir_all(&root).unwrap();
        let project = root.join("project.json");
        fs::write(
            &project,
            r#"{"mcpServers":{"project":{"command":"project"}}}"#,
        )
        .unwrap();

        let config = load_config_from_paths(
            &root.join("global.json"),
            &project,
            &root.to_string_lossy(),
            false,
        );

        assert!(config.servers.is_empty());
        assert!(config.diagnostics[0].contains("ignored"));
    }

    #[test]
    fn malformed_config_is_diagnostic_not_panic() {
        let root = unique_temp_dir("malformed");
        fs::create_dir_all(&root).unwrap();
        let global = root.join("global.json");
        fs::write(&global, "{not-json").unwrap();

        let config = load_config_from_paths(
            &global,
            &root.join("project.json"),
            &root.to_string_lossy(),
            false,
        );

        assert!(config.servers.is_empty());
        assert!(config.diagnostics[0].contains("parse MCP config"));
    }

    #[test]
    fn invalid_servers_are_skipped() {
        let root = unique_temp_dir("invalid");
        fs::create_dir_all(&root).unwrap();
        let global = root.join("global.json");
        fs::write(
            &global,
            r#"{"mcpServers":{"empty":{"command":""},"ok":{"command":"node"}}}"#,
        )
        .unwrap();

        let config = load_config_from_paths(
            &global,
            &root.join("project.json"),
            &root.to_string_lossy(),
            false,
        );

        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "ok");
        assert!(config.diagnostics[0].contains("command is required"));
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("astrcode-mcp-{name}-{suffix}"))
    }
}
