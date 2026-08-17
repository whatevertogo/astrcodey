//! Unified-diff application behind the workspace Host boundary.

use std::sync::Arc;

use astrcode_core::tool::FileObservationStore;
use astrcode_extension_sdk::{
    host::{
        HostWorkspaceApplyPatchOutput, HostWorkspaceApplyPatchRequest, HostWorkspacePatchChange,
        HostWorkspacePatchChangeKind, normalize_unified_diff_path,
    },
    s5r::ErrorPayload,
};

use super::workspace::{
    ensure_observation_current, reject_sensitive_path, reject_symlink_target, remember_observation,
    resolve_existing_path, resolve_write_target, write_file_atomic,
};

#[derive(Debug)]
struct FilePatch {
    old_path: Option<String>,
    new_path: Option<String>,
    hunks: Vec<Hunk>,
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Add(String),
    Delete(String),
}

#[derive(Debug, Clone, Copy)]
enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

struct TextDocument {
    lines: Vec<String>,
    line_ending: LineEnding,
    has_trailing_newline: bool,
}

#[cfg(test)]
pub(super) fn apply_patch(
    root: &str,
    request: HostWorkspaceApplyPatchRequest,
    capability: &'static str,
    observations: Option<&Arc<dyn FileObservationStore>>,
) -> Result<HostWorkspaceApplyPatchOutput, ErrorPayload> {
    apply_patch_with_access(root, request, capability, observations, false)
}

pub(super) fn apply_patch_with_access(
    root: &str,
    request: HostWorkspaceApplyPatchRequest,
    capability: &'static str,
    observations: Option<&Arc<dyn FileObservationStore>>,
    allow_sensitive_paths: bool,
) -> Result<HostWorkspaceApplyPatchOutput, ErrorPayload> {
    let patches = parse_patch(&request.patch).map_err(invalid_patch)?;
    let changes = patches
        .iter()
        .map(|patch| apply_file_patch(root, capability, patch, observations, allow_sensitive_paths))
        .collect();
    Ok(HostWorkspaceApplyPatchOutput { changes })
}

fn parse_patch(patch: &str) -> Result<Vec<FilePatch>, String> {
    if patch.trim().is_empty() {
        return Err("patch must not be empty".into());
    }
    let lines = patch.lines().collect::<Vec<_>>();
    let mut patches = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        if is_metadata(line) || line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        let Some(old_path) = line.strip_prefix("--- ") else {
            return Err(format!(
                "patch format error: unexpected line '{}'",
                line.chars().take(40).collect::<String>()
            ));
        };
        let old_path = normalize_unified_diff_path(old_path).map_err(|error| error.to_string())?;
        index += 1;
        let new_path = lines
            .get(index)
            .and_then(|line| line.strip_prefix("+++ "))
            .ok_or_else(|| {
                "patch format error: expected '+++ new_path' after '--- old_path'".to_owned()
            })?;
        let new_path = normalize_unified_diff_path(new_path).map_err(|error| error.to_string())?;
        index += 1;
        let hunks = parse_hunks(&lines, &mut index)?;
        if hunks.is_empty() {
            return Err(format!("patch for {new_path} does not contain a hunk"));
        }
        patches.push(FilePatch {
            old_path: (old_path != "/dev/null").then_some(old_path),
            new_path: (new_path != "/dev/null").then_some(new_path),
            hunks,
        });
    }
    if patches.is_empty() {
        Err("patch does not contain any file changes".into())
    } else {
        Ok(patches)
    }
}

fn parse_hunks(lines: &[&str], index: &mut usize) -> Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    while *index < lines.len() {
        let line = lines[*index];
        if line.starts_with("--- ") || line.starts_with("diff ") {
            break;
        }
        if !line.starts_with("@@") {
            *index += 1;
            continue;
        }
        let old_start = parse_hunk_header(line)?;
        *index += 1;
        let mut hunk_lines = Vec::new();
        while *index < lines.len()
            && !lines[*index].starts_with("@@")
            && !lines[*index].starts_with("--- ")
            && !lines[*index].starts_with("diff ")
        {
            let line = lines[*index];
            match line.chars().next() {
                Some(' ') => hunk_lines.push(HunkLine::Context(line[1..].to_owned())),
                Some('+') => hunk_lines.push(HunkLine::Add(line[1..].to_owned())),
                Some('-') => hunk_lines.push(HunkLine::Delete(line[1..].to_owned())),
                Some('\\') => {},
                _ => hunk_lines.push(HunkLine::Context(line.to_owned())),
            }
            *index += 1;
        }
        hunks.push(Hunk {
            old_start,
            lines: hunk_lines,
        });
    }
    Ok(hunks)
}

fn parse_hunk_header(header: &str) -> Result<usize, String> {
    let body = header
        .strip_prefix("@@")
        .and_then(|header| header.rsplit_once("@@"))
        .map(|(body, _)| body.trim())
        .ok_or_else(|| format!("invalid hunk header: {header}"))?;
    let old = body
        .split_whitespace()
        .next()
        .and_then(|range| range.strip_prefix('-'))
        .ok_or_else(|| format!("invalid hunk header: {header}"))?;
    old.split(',')
        .next()
        .and_then(|start| start.parse().ok())
        .ok_or_else(|| format!("invalid old range in hunk header: {header}"))
}

fn apply_file_patch(
    root: &str,
    capability: &'static str,
    patch: &FilePatch,
    observations: Option<&Arc<dyn FileObservationStore>>,
    allow_sensitive_paths: bool,
) -> HostWorkspacePatchChange {
    let Some(label) = patch.new_path.as_ref().or(patch.old_path.as_ref()) else {
        return failed(
            HostWorkspacePatchChangeKind::Updated,
            "unknown",
            "patch specifies neither old nor new path",
        );
    };
    let kind = if patch.old_path.is_none() {
        HostWorkspacePatchChangeKind::Created
    } else if patch.new_path.is_none() {
        HostWorkspacePatchChangeKind::Deleted
    } else {
        HostWorkspacePatchChangeKind::Updated
    };
    if let (Some(old), Some(new)) = (&patch.old_path, &patch.new_path)
        && old != new
    {
        return failed(kind, label, "rename patches are not supported");
    }
    if !allow_sensitive_paths && let Err(error) = reject_sensitive_path(label) {
        return failed(kind, label, &error.message);
    }

    let existing = patch.old_path.is_some();
    if existing
        && std::path::Path::new(label).is_absolute()
        && let Err(error) = reject_symlink_target(std::path::Path::new(label), capability)
    {
        return failed(kind, label, &error.message);
    }
    let path = if existing {
        match resolve_existing_path(root, label, capability) {
            Ok(path) => path,
            Err(error) => return failed(kind, label, &error.message),
        }
    } else {
        match resolve_write_target(root, label, capability, true) {
            Ok((parent, file_name, _)) => parent.join(file_name),
            Err(error) => return failed(kind, label, &error.message),
        }
    };
    if !existing && path.exists() {
        return failed(kind, label, "create patch target already exists");
    }
    if existing && let Err(error) = ensure_observation_current(observations, &path) {
        return failed(kind, label, &error.message);
    }
    let original = if existing {
        match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => return failed(kind, label, &format!("read failed: {error}")),
        }
    } else {
        String::new()
    };
    let document = parse_document(&original);
    let lines = match apply_hunks(&document.lines, &patch.hunks) {
        Ok(lines) => lines,
        Err(error) => return failed(kind, label, &error),
    };

    if matches!(kind, HostWorkspacePatchChangeKind::Deleted) {
        if !lines.is_empty() {
            return failed(kind, label, "delete patch does not remove the full file");
        }
        if std::path::Path::new(label).is_absolute()
            && let Err(error) = reject_symlink_target(std::path::Path::new(label), capability)
        {
            return failed(kind, label, &error.message);
        }
        return match std::fs::remove_file(&path) {
            Ok(()) => succeeded(kind, label, format!("deleted {}", path.display())),
            Err(error) => failed(kind, label, &format!("delete failed: {error}")),
        };
    }

    let trailing_newline = if existing {
        document.has_trailing_newline
    } else {
        request_implies_trailing_newline(patch)
    };
    let content = render_document(&lines, document.line_ending, trailing_newline);
    if let Err(error) = write_file_atomic(&path, content.as_bytes()) {
        return failed(kind, label, &format!("write failed: {error}"));
    }
    if let Err(error) = remember_observation(observations, &path) {
        return failed(kind, label, &error.message);
    }
    let (added, removed) = patch_counts(patch);
    succeeded(
        kind,
        label,
        format!(
            "{} {} (+{added} -{removed})",
            kind_name(kind),
            path.display()
        ),
    )
}

fn apply_hunks(content: &[String], hunks: &[Hunk]) -> Result<Vec<String>, String> {
    let mut result = content.to_vec();
    let mut line_delta = 0isize;
    for (index, hunk) in hunks.iter().enumerate() {
        let base = hunk.old_start.saturating_sub(1);
        let anchor = (base as isize + line_delta).clamp(0, result.len() as isize) as usize;
        let position = find_match(&result, hunk, anchor).ok_or_else(|| {
            format!(
                "hunk #{} around line {} failed: context mismatch",
                index + 1,
                hunk.old_start
            )
        })?;
        let mut source = position;
        let mut replacement = Vec::new();
        for line in &hunk.lines {
            match line {
                HunkLine::Context(expected) => {
                    let actual = result
                        .get(source)
                        .ok_or_else(|| format!("expected context '{expected}' at end of file"))?;
                    if actual != expected {
                        return Err(format!("expected context '{expected}', got '{actual}'"));
                    }
                    replacement.push(actual.clone());
                    source += 1;
                },
                HunkLine::Delete(expected) => {
                    let actual = result
                        .get(source)
                        .ok_or_else(|| format!("expected deletion '{expected}' at end of file"))?;
                    if actual != expected {
                        return Err(format!("expected deletion '{expected}', got '{actual}'"));
                    }
                    source += 1;
                },
                HunkLine::Add(line) => replacement.push(line.clone()),
            }
        }
        result.splice(position..source, replacement);
        let (added, removed) = hunk_counts(hunk);
        line_delta += added as isize - removed as isize;
    }
    Ok(result)
}

fn find_match(content: &[String], hunk: &Hunk, anchor: usize) -> Option<usize> {
    let expected = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(line) | HunkLine::Delete(line) => Some(line.as_str()),
            HunkLine::Add(_) => None,
        })
        .collect::<Vec<_>>();
    if expected.is_empty() {
        return Some(anchor.min(content.len()));
    }
    if matches_at(content, &expected, anchor) {
        return Some(anchor);
    }
    let lower = anchor.saturating_sub(expected.len().max(10));
    (lower..anchor)
        .rev()
        .chain((anchor + 1)..=content.len().saturating_sub(expected.len()))
        .find(|position| matches_at(content, &expected, *position))
}

fn matches_at(content: &[String], expected: &[&str], position: usize) -> bool {
    position + expected.len() <= content.len()
        && expected
            .iter()
            .enumerate()
            .all(|(offset, expected)| content[position + offset] == *expected)
}

fn parse_document(content: &str) -> TextDocument {
    TextDocument {
        lines: if content.is_empty() {
            Vec::new()
        } else {
            content.lines().map(str::to_owned).collect()
        },
        line_ending: if content.contains("\r\n") {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        },
        has_trailing_newline: content.ends_with('\n'),
    }
}

fn render_document(lines: &[String], ending: LineEnding, trailing_newline: bool) -> String {
    let mut content = lines.join(ending.as_str());
    if trailing_newline && !lines.is_empty() {
        content.push_str(ending.as_str());
    }
    content
}

fn request_implies_trailing_newline(patch: &FilePatch) -> bool {
    patch.hunks.last().is_some_and(|hunk| {
        hunk.lines
            .last()
            .is_some_and(|line| matches!(line, HunkLine::Add(_) | HunkLine::Context(_)))
    })
}

fn patch_counts(patch: &FilePatch) -> (usize, usize) {
    patch
        .hunks
        .iter()
        .map(hunk_counts)
        .fold((0, 0), |(added, removed), (hunk_added, hunk_removed)| {
            (added + hunk_added, removed + hunk_removed)
        })
}

fn hunk_counts(hunk: &Hunk) -> (usize, usize) {
    hunk.lines
        .iter()
        .fold((0, 0), |(added, removed), line| match line {
            HunkLine::Add(_) => (added + 1, removed),
            HunkLine::Delete(_) => (added, removed + 1),
            HunkLine::Context(_) => (added, removed),
        })
}

fn is_metadata(line: &str) -> bool {
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

fn invalid_patch(error: String) -> ErrorPayload {
    ErrorPayload::new(
        astrcode_extension_sdk::wire::WireErrorCode::InvalidInput,
        error,
    )
}

fn succeeded(
    kind: HostWorkspacePatchChangeKind,
    path: &str,
    summary: String,
) -> HostWorkspacePatchChange {
    HostWorkspacePatchChange {
        kind,
        path: path.into(),
        applied: true,
        summary,
        error: None,
    }
}

fn failed(kind: HostWorkspacePatchChangeKind, path: &str, error: &str) -> HostWorkspacePatchChange {
    HostWorkspacePatchChange {
        kind,
        path: path.into(),
        applied: false,
        summary: error.into(),
        error: Some(error.into()),
    }
}

const fn kind_name(kind: HostWorkspacePatchChangeKind) -> &'static str {
    match kind {
        HostWorkspacePatchChangeKind::Created => "created",
        HostWorkspacePatchChangeKind::Updated => "updated",
        HostWorkspacePatchChangeKind::Deleted => "deleted",
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn applies_create_update_and_rejects_mismatched_delete() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().to_str().expect("utf-8 root");
        std::fs::write(workspace.path().join("update.txt"), "old\r\n").expect("seed update");
        std::fs::write(workspace.path().join("delete.txt"), "changed\n").expect("seed delete");
        std::fs::write(workspace.path().join("existing.txt"), "keep\n").expect("seed existing");
        let output = apply_patch(
            root,
            HostWorkspaceApplyPatchRequest {
                patch: concat!(
                    "--- /dev/null\n+++ b/new.txt \tgenerated\n@@ -0,0 +1 @@\n+new\n",
                    "--- a/update.txt\n+++ b/update.txt\n@@ -1 +1 @@\n-old\n+new\n",
                    "--- a/delete.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
                    "--- /dev/null\n+++ b/existing.txt\n@@ -0,0 +1 @@\n+replace\n",
                )
                .into(),
            },
            "workspace patch",
            None,
        )
        .expect("parse patch");

        assert_eq!(output.changes.len(), 4);
        assert!(output.changes[0].applied);
        assert!(output.changes[1].applied);
        assert!(!output.changes[2].applied);
        assert!(!output.changes[3].applied);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("update.txt")).expect("updated"),
            "new\r\n"
        );
        assert!(workspace.path().join("delete.txt").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("existing.txt"))
                .expect("existing unchanged"),
            "keep\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn absolute_delete_rejects_a_symlink_without_deleting_its_target() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let target = outside.path().join("target.txt");
        let link = outside.path().join("link.txt");
        std::fs::write(&target, "keep\n").expect("seed target");
        symlink(&target, &link).expect("create symlink");
        let link = link.to_str().expect("utf-8 link");

        let output = apply_patch(
            workspace.path().to_str().expect("utf-8 workspace"),
            HostWorkspaceApplyPatchRequest {
                patch: format!("--- {link}\n+++ /dev/null\n@@ -1 +0,0 @@\n-keep\n"),
            },
            "workspace patch",
            None,
        )
        .expect("parse patch");

        assert!(!output.changes[0].applied);
        assert!(std::path::Path::new(link).is_symlink());
        assert_eq!(
            std::fs::read_to_string(target).expect("target remains"),
            "keep\n"
        );
    }
}
