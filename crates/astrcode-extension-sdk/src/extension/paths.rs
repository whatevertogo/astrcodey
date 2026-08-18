//! Extension-attributed persistence paths.

use std::path::{Path, PathBuf};

use astrcode_core::config::defaults::extension_data_dir;

use super::ExtensionError;

/// Persistence locations already namespaced to one extension.
///
/// Authors never provide an extension id when asking for a path. The runtime derives both
/// namespaces from the validated manifest identity, preventing accidental cross-extension state
/// access through a misspelled or copied id.
#[derive(Debug, Clone, Default)]
pub struct ExtensionPaths {
    global_data_dir: Option<PathBuf>,
    session_data_dir: Option<PathBuf>,
}

impl ExtensionPaths {
    pub(crate) fn from_runtime(
        extension_id: &str,
        global_store_dir: Option<&Path>,
        session_store_dir: Option<&Path>,
    ) -> Self {
        Self {
            global_data_dir: global_store_dir.map(|base| extension_data_dir(base, extension_id)),
            session_data_dir: session_store_dir.map(|base| extension_data_dir(base, extension_id)),
        }
    }

    pub fn global_data_dir(&self) -> Option<&Path> {
        self.global_data_dir.as_deref()
    }

    pub fn session_data_dir(&self) -> Result<&Path, ExtensionPathError> {
        self.session_data_dir
            .as_deref()
            .ok_or(ExtensionPathError::SessionContextUnavailable)
    }

    /// Return the session data directory, mapped to [`ExtensionError`] when absent.
    pub fn require_session_data_dir(&self) -> Result<&Path, ExtensionError> {
        self.session_data_dir().map_err(ExtensionError::from)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionPathError {
    #[error("session-scoped extension data is unavailable outside a session call context")]
    SessionContextUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_attribution_owns_namespaces_and_missing_session_is_explicit() {
        let paths = ExtensionPaths::from_runtime(
            "review-extension",
            Some(Path::new("/state")),
            Some(Path::new("/sessions/session-1")),
        );
        assert_eq!(
            paths.global_data_dir(),
            Some(Path::new("/state/extension_data/review-extension"))
        );
        assert_eq!(
            paths.session_data_dir().unwrap(),
            Path::new("/sessions/session-1/extension_data/review-extension")
        );

        let startup = ExtensionPaths::from_runtime("review-extension", None, None);
        assert_eq!(
            startup.session_data_dir().unwrap_err(),
            ExtensionPathError::SessionContextUnavailable
        );
    }
}
