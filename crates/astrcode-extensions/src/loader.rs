//! Extension loader — discovers and loads extensions from global and project dirs.

use std::path::PathBuf;
use std::sync::Arc;

use astrcode_core::extension::Extension;

/// Loads extensions from global and project-level directories.
pub struct ExtensionLoader;

impl ExtensionLoader {
    /// Load all extensions: global first, then project-level.
    ///
    /// Project-level extensions take priority (run first in event dispatch).
    pub async fn load_all(working_dir: Option<&str>) -> Vec<Arc<dyn Extension>> {
        let mut extensions: Vec<Arc<dyn Extension>> = Vec::new();

        // Load global extensions: ~/.astrcode/extensions/
        let global_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".astrcode")
            .join("extensions");
        if global_dir.exists() {
            let global = Self::load_from_dir(&global_dir).await;
            extensions.extend(global);
        }

        // Load project extensions (higher priority): .astrcode/extensions/
        if let Some(wd) = working_dir {
            let project_dir = PathBuf::from(wd).join(".astrcode").join("extensions");
            if project_dir.exists() {
                let project = Self::load_from_dir(&project_dir).await;
                // Project extensions run first
                extensions.splice(0..0, project);
            }
        }

        extensions
    }

    async fn load_from_dir(_dir: &PathBuf) -> Vec<Arc<dyn Extension>> {
        // TODO: Scan directory for extension manifest files
        // TODO: Load extension WASM or dynamic libraries
        // TODO: Parse extension manifests and instantiate extensions
        //
        // For now, extensions are registered programmatically by the server
        // at bootstrap time. Filesystem-based loading is a future feature.
        Vec::new()
    }
}

/// Document: Skills, agent profiles, custom behaviors — all are extensions.
///
/// The core only provides hooks, agent loop, compaction, and built-in tools.
/// Everything else — skill loading, agent profile management, custom slash
/// commands, domain-specific context — is loaded as an extension.
///
/// To build a skill as an extension:
/// 1. Implement the `Extension` trait
/// 2. In `on_event(SessionStart, ...)`, scan the skill directory
/// 3. Register skill tools via `tools()` return
/// 4. Inject skill summaries via `context_contributions()` return
/// 5. Handle `skillTool` invocations in `on_event(BeforeToolCall, ...)`
///
/// To build agent profiles as an extension:
/// 1. Implement the `Extension` trait
/// 2. In `on_event(SessionStart, ...)`, scan agent definition files
/// 3. Inject agent summaries via `context_contributions()` return
/// 4. Register slash commands for agent switching
pub mod documentation {}
