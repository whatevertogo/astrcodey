//! Pure unified-diff path analysis shared by tool planners and Host lease enforcement.

use std::fmt::{Display, Formatter};

/// Old and new paths named by one file section in a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePatchPaths {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePatchPathError {
    message: String,
}

impl WorkspacePatchPathError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for WorkspacePatchPathError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkspacePatchPathError {}

/// Extracts every file path pair from a unified diff without performing I/O.
///
/// This is intentionally narrower than a patch parser: planners and the Host authorization gate
/// need an identical, side-effect-free view of the resources named by the final patch text. The
/// Host still parses and validates every hunk again before it writes anything.
pub fn analyze_unified_diff_paths(
    patch: &str,
) -> Result<Vec<WorkspacePatchPaths>, WorkspacePatchPathError> {
    if patch.trim().is_empty() {
        return Err(WorkspacePatchPathError::new("patch must not be empty"));
    }

    let lines = patch.lines().collect::<Vec<_>>();
    let mut paths = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        if is_patch_metadata(line) || line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        let Some(old_path) = line.strip_prefix("--- ") else {
            index += 1;
            continue;
        };
        index += 1;
        let new_path = lines
            .get(index)
            .and_then(|line| line.strip_prefix("+++ "))
            .ok_or_else(|| {
                WorkspacePatchPathError::new(
                    "patch format error: expected '+++ new_path' after '--- old_path'",
                )
            })?;

        let old_path = normalize_unified_diff_path(old_path)?;
        let new_path = normalize_unified_diff_path(new_path)?;
        let pair = WorkspacePatchPaths {
            old_path: (old_path != "/dev/null").then_some(old_path),
            new_path: (new_path != "/dev/null").then_some(new_path),
        };
        if pair.old_path.is_none() && pair.new_path.is_none() {
            return Err(WorkspacePatchPathError::new(
                "patch file section cannot use /dev/null for both paths",
            ));
        }
        paths.push(pair);
        index += 1;
    }

    if paths.is_empty() {
        return Err(WorkspacePatchPathError::new(
            "patch does not contain any file sections",
        ));
    }
    Ok(paths)
}

/// Normalizes one path from a unified-diff file header.
///
/// Planners, lease enforcement, and the Host patch executor must use this exact function so the
/// authorized path cannot differ from the path that is eventually written.
pub fn normalize_unified_diff_path(value: &str) -> Result<String, WorkspacePatchPathError> {
    let value = value.split('\t').next().unwrap_or(value).trim();
    if value.is_empty() {
        return Err(WorkspacePatchPathError::new(
            "patch file path must not be empty",
        ));
    }
    if value == "/dev/null" {
        return Ok(value.into());
    }
    Ok(value
        .strip_prefix("a/")
        .or_else(|| value.strip_prefix("b/"))
        .unwrap_or(value)
        .to_owned())
}

fn is_patch_metadata(line: &str) -> bool {
    [
        "diff ",
        "index ",
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
        "rename ",
        "similarity ",
        "copy ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzes_create_update_and_delete_paths_strictly() {
        let patch = concat!(
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
            "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+new\n",
            "--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
        );
        assert_eq!(
            analyze_unified_diff_paths(patch).expect("valid paths"),
            vec![
                WorkspacePatchPaths {
                    old_path: Some("src/lib.rs".into()),
                    new_path: Some("src/lib.rs".into()),
                },
                WorkspacePatchPaths {
                    old_path: None,
                    new_path: Some("new.txt".into()),
                },
                WorkspacePatchPaths {
                    old_path: Some("old.txt".into()),
                    new_path: None,
                },
            ]
        );
        assert!(analyze_unified_diff_paths("--- a/file\n").is_err());
        assert!(analyze_unified_diff_paths("\n").is_err());
    }
}
