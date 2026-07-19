//! 工作区文件读写边界。

use std::{
    ffi::{OsStr, OsString},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use astrcode_extension_sdk::s5r::ErrorPayload;
use astrcode_support::hostpaths::resolve_under_workspace_root;
use globset::Glob;
use regex::Regex;
use serde_json::{Value, json};
use walkdir::{DirEntry, WalkDir};

use super::{capability::WorkspaceCapability, run_blocking_io};

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 5_000;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_SEARCH_MATCHES: usize = 1_000;
const MAX_SEARCH_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_LINE_CHARS: usize = 2_000;
const MAX_SEARCH_SCAN_BYTES: usize = 64 * 1024 * 1024;
const IGNORED_DIRECTORIES: &[&str] = &[".git", "node_modules"];

pub(super) struct WorkspaceGroup {
    default_working_dir: Option<String>,
}

impl WorkspaceGroup {
    pub(super) fn new(default_working_dir: Option<String>) -> Self {
        Self {
            default_working_dir,
        }
    }

    pub(super) fn invoke(
        &self,
        capability: WorkspaceCapability,
        input: &Value,
        working_dir: Option<&str>,
    ) -> Result<Value, ErrorPayload> {
        let root = self.root(working_dir)?;
        run_blocking_io(|| match capability {
            WorkspaceCapability::Read => read(root, input),
            WorkspaceCapability::List => list(root, input),
            WorkspaceCapability::Grep => grep(root, input),
            WorkspaceCapability::Glob => glob(root, input),
            WorkspaceCapability::Write => write(root, input),
            WorkspaceCapability::Edit => edit(root, input),
        })
    }

    fn root<'a>(&'a self, working_dir: Option<&'a str>) -> Result<&'a str, ErrorPayload> {
        working_dir
            .or(self.default_working_dir.as_deref())
            .ok_or_else(|| ErrorPayload::new("backend_unavailable", "working_dir not set"))
    }
}

fn read(root: &str, input: &Value) -> Result<Value, ErrorPayload> {
    let relative_path = required_string(input, "path")?;
    reject_sensitive_path(relative_path)?;
    let path = resolve_existing_path(root, relative_path, "workspace.read")?;
    let metadata = std::fs::metadata(&path)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    if !metadata.is_file() {
        return Err(ErrorPayload::new(
            "invalid_input",
            "workspace.read path must be a regular file",
        ));
    }
    let max_bytes = input
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_FILE_BYTES as u64)
        .min(MAX_FILE_BYTES as u64);
    if metadata.len() > max_bytes {
        return Err(ErrorPayload::new(
            "file_too_large",
            format!("file size {} exceeds max_bytes {max_bytes}", metadata.len()),
        ));
    }
    let content = read_bounded_file(&path, max_bytes as usize)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?
        .ok_or_else(|| {
            ErrorPayload::new(
                "file_too_large",
                format!("file size exceeds max_bytes {max_bytes}"),
            )
        })?;
    let content = String::from_utf8(content)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    Ok(json!({ "content": content }))
}

fn list(root: &str, input: &Value) -> Result<Value, ErrorPayload> {
    let relative_path = input.get("path").and_then(Value::as_str).unwrap_or(".");
    reject_sensitive_path(relative_path)?;
    let path = resolve_existing_path(root, relative_path, "workspace.list")?;
    if !path.is_dir() {
        return Err(ErrorPayload::new(
            "invalid_input",
            "workspace.list path must be a directory",
        ));
    }
    let depth = bounded_usize(input, "depth", 1, 1, 32);
    let limit = bounded_usize(input, "limit", MAX_LIST_ENTRIES, 1, MAX_LIST_ENTRIES);
    let canonical_root = canonical_root(root)?;
    let mut entries = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    for entry in WalkDir::new(&path)
        .min_depth(1)
        .max_depth(depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| traversable_entry(&canonical_root, entry, false))
    {
        let entry = entry.map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
        scanned += 1;
        if scanned > MAX_WALK_ENTRIES || entries.len() >= limit {
            truncated = true;
            break;
        }
        let file_type = entry.file_type();
        let kind = if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else if file_type.is_symlink() {
            "symlink"
        } else {
            "other"
        };
        let bytes = file_type
            .is_file()
            .then(|| entry.metadata().ok().map(|metadata| metadata.len()))
            .flatten();
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "path": relative_path_string(&canonical_root, entry.path()),
            "kind": kind,
            "bytes": bytes,
        }));
    }
    entries.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    Ok(json!({
        "path": relative_path_string(&canonical_root, &path),
        "entries": entries,
        "returned_entries": entries.len(),
        "truncated": truncated,
    }))
}

fn grep(root: &str, input: &Value) -> Result<Value, ErrorPayload> {
    let pattern = required_string(input, "pattern")?;
    let regex = Regex::new(pattern)
        .map_err(|error| ErrorPayload::new("invalid_input", format!("invalid regex: {error}")))?;
    let relative_path = input.get("path").and_then(Value::as_str).unwrap_or(".");
    reject_sensitive_path(relative_path)?;
    let search_root = resolve_existing_path(root, relative_path, "workspace.grep")?;
    let canonical_root = canonical_root(root)?;
    let max_matches = bounded_usize(input, "max_matches", 100, 1, MAX_SEARCH_MATCHES);
    let max_bytes = bounded_usize(input, "max_bytes", 64 * 1024, 1, MAX_SEARCH_OUTPUT_BYTES);
    let max_line_chars = bounded_usize(input, "max_line_chars", 500, 1, MAX_SEARCH_LINE_CHARS);
    let searchable = searchable_files_with_limit(&canonical_root, &search_root, MAX_WALK_ENTRIES)?;
    let mut matches = Vec::new();
    let mut output_bytes = 0usize;
    let mut output_truncated = false;
    let mut scanned_bytes = 0usize;
    let mut scan_truncated = false;
    for path in searchable.files {
        let content = match read_bounded_file(&path, MAX_FILE_BYTES) {
            Ok(Some(content)) => content,
            Ok(None) => {
                scan_truncated = true;
                continue;
            },
            Err(_) => continue,
        };
        if scanned_bytes.saturating_add(content.len()) > MAX_SEARCH_SCAN_BYTES {
            scan_truncated = true;
            break;
        }
        scanned_bytes += content.len();
        let Ok(content) = String::from_utf8(content) else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            let (line, line_truncated) = truncate_chars(line, max_line_chars);
            if matches.len() >= max_matches || output_bytes.saturating_add(line.len()) > max_bytes {
                output_truncated = true;
                break;
            }
            output_bytes += line.len();
            matches.push(json!({
                "path": relative_path_string(&canonical_root, &path),
                "line_number": index + 1,
                "line": line,
                "line_truncated": line_truncated,
            }));
        }
        if output_truncated {
            break;
        }
    }
    Ok(json!({
        "pattern": pattern,
        "root": relative_path_string(&canonical_root, &search_root),
        "matches": matches,
        "truncated": searchable.truncated || scan_truncated || output_truncated,
    }))
}

fn glob(root: &str, input: &Value) -> Result<Value, ErrorPayload> {
    let pattern = required_string(input, "pattern")?;
    if Path::new(pattern).is_absolute() {
        return Err(ErrorPayload::new(
            "permission_denied",
            "glob pattern must be relative to the workspace",
        ));
    }
    let matcher = Glob::new(pattern)
        .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?
        .compile_matcher();
    let relative_root = input.get("root").and_then(Value::as_str).unwrap_or(".");
    if is_overly_broad_glob(pattern, relative_root) {
        return Err(ErrorPayload::new(
            "invalid_input",
            "Use workspace.list to inspect workspace structure; workspace.glob is for targeted \
             file discovery (for example **/*.rs or crates/astrcode-core/**)",
        ));
    }
    reject_sensitive_path(relative_root)?;
    let search_root = resolve_existing_path(root, relative_root, "workspace.glob")?;
    if !search_root.is_dir() {
        return Err(ErrorPayload::new(
            "invalid_input",
            "workspace.glob root must be a directory",
        ));
    }
    let canonical_root = canonical_root(root)?;
    let max_matches = bounded_usize(input, "max_matches", 200, 1, MAX_SEARCH_MATCHES);
    let include_ignored = input
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut paths = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    for entry in WalkDir::new(&search_root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| traversable_entry(&canonical_root, entry, include_ignored))
    {
        let entry = entry.map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
        scanned += 1;
        if scanned > MAX_WALK_ENTRIES {
            truncated = true;
            break;
        }
        let relative_to_search = entry
            .path()
            .strip_prefix(&search_root)
            .unwrap_or(entry.path());
        if matcher.is_match(relative_to_search) {
            if paths.len() >= max_matches {
                truncated = true;
                break;
            }
            paths.push(relative_path_string(&canonical_root, entry.path()));
        }
    }
    paths.sort();
    Ok(json!({
        "pattern": pattern,
        "root": relative_path_string(&canonical_root, &search_root),
        "paths": paths,
        "truncated": truncated,
    }))
}

fn is_overly_broad_glob(pattern: &str, relative_root: &str) -> bool {
    let normalized_root = normalize_relative_pattern(relative_root);
    if normalized_root != "." && !normalized_root.is_empty() {
        return false;
    }

    let normalized_pattern = normalize_relative_pattern(pattern);
    !normalized_pattern.split('/').any(segment_has_literal)
}

fn normalize_relative_pattern(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix("./").unwrap_or(value)
}

fn segment_has_literal(segment: &str) -> bool {
    let mut chars = segment.chars();
    while let Some(character) = chars.next() {
        match character {
            '*' | '?' => {},
            '[' => {
                for class_character in chars.by_ref() {
                    if class_character == ']' {
                        break;
                    }
                }
            },
            _ if character.is_alphanumeric() => return true,
            _ => {},
        }
    }
    false
}

fn write(root: &str, input: &Value) -> Result<Value, ErrorPayload> {
    let relative_path = required_string(input, "path")?;
    let content = required_string_allow_empty(input, "content")?;
    enforce_content_limit(content)?;
    reject_sensitive_path(relative_path)?;
    let (parent, file_name, parent_created) = resolve_write_target(root, relative_path)?;
    let path = parent.join(file_name);
    reject_symlink_target(&path, "workspace.write")?;
    write_file_no_follow(&path, content.as_bytes())
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    Ok(json!({
        "path": relative_path,
        "bytes_written": content.len(),
        "parent_created": parent_created,
    }))
}

fn edit(root: &str, input: &Value) -> Result<Value, ErrorPayload> {
    let relative_path = required_string(input, "path")?;
    let old_text = required_string(input, "old_text")?;
    let new_text = required_string_allow_empty(input, "new_text")?;
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    reject_sensitive_path(relative_path)?;
    let path = resolve_existing_path(root, relative_path, "workspace.edit")?;
    let metadata = std::fs::metadata(&path)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(ErrorPayload::new(
            "file_too_large",
            format!("workspace.edit supports regular files up to {MAX_FILE_BYTES} bytes"),
        ));
    }
    let content = read_bounded_file(&path, MAX_FILE_BYTES)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?
        .ok_or_else(|| {
            ErrorPayload::new(
                "file_too_large",
                format!("workspace.edit supports regular files up to {MAX_FILE_BYTES} bytes"),
            )
        })?;
    let content = String::from_utf8(content)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    let replacements = content.matches(old_text).count();
    if replacements == 0 {
        return Err(ErrorPayload::new(
            "invalid_input",
            format!("old_text not found in {relative_path}"),
        ));
    }
    if !replace_all && replacements > 1 {
        return Err(ErrorPayload::new(
            "invalid_input",
            format!(
                "old_text matched {replacements} times in {relative_path}; set replace_all=true \
                 or provide more context"
            ),
        ));
    }
    let edited = if replace_all {
        content.replace(old_text, new_text)
    } else {
        content.replacen(old_text, new_text, 1)
    };
    enforce_content_limit(&edited)?;
    write_file_no_follow(&path, edited.as_bytes())
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    Ok(json!({
        "path": relative_path,
        "replacements": if replace_all { replacements } else { 1 },
        "bytes_written": edited.len(),
    }))
}

fn resolve_existing_path(
    root: &str,
    relative_path: &str,
    capability: &str,
) -> Result<PathBuf, ErrorPayload> {
    let canonical_root = canonical_root(root)?;
    reject_symlink_components(&canonical_root, Path::new(relative_path), capability)?;
    let path = resolve_under_workspace_root(root, relative_path)
        .map_err(|error| ErrorPayload::new(error.code, error.message))?;
    Ok(path)
}

fn resolve_write_target(
    root: &str,
    relative_path: &str,
) -> Result<(PathBuf, OsString, bool), ErrorPayload> {
    let relative = Path::new(relative_path);
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(ErrorPayload::new(
            "permission_denied",
            "path must be relative to the workspace",
        ));
    }
    let file_name = relative
        .file_name()
        .filter(|name| *name != OsStr::new(".."))
        .ok_or_else(|| ErrorPayload::new("invalid_input", "path must reference a file"))?
        .to_owned();
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
    reject_symlink_components(&canonical_root, relative_parent, "workspace.write")?;
    let parent = canonical_root.join(relative_parent);
    let parent_created = !parent.exists();
    std::fs::create_dir_all(&parent)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(ErrorPayload::new(
            "permission_denied",
            "path escapes the workspace root",
        ));
    }
    Ok((canonical_parent, file_name, parent_created))
}

fn reject_symlink_components(
    canonical_root: &Path,
    relative_path: &Path,
    capability: &str,
) -> Result<(), ErrorPayload> {
    let mut current = canonical_root.to_path_buf();
    for component in relative_path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(name) => current.push(name),
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(ErrorPayload::new(
                    "permission_denied",
                    "path must be relative to the workspace",
                ));
            },
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ErrorPayload::new(
                    "permission_denied",
                    format!("symlink paths are not accessible via {capability}"),
                ));
            },
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(ErrorPayload::new("io_error", error.to_string())),
        }
    }
    Ok(())
}

fn reject_symlink_target(path: &Path, capability: &str) -> Result<(), ErrorPayload> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ErrorPayload::new(
            "permission_denied",
            format!("symlink paths are not writable via {capability}"),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ErrorPayload::new("io_error", error.to_string())),
    }
}

fn reject_sensitive_path(relative_path: &str) -> Result<(), ErrorPayload> {
    let sensitive = Path::new(relative_path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .any(is_sensitive_component);
    if sensitive {
        return Err(ErrorPayload::new(
            "permission_denied",
            "workspace access to sensitive files is not allowed",
        ));
    }
    Ok(())
}

fn is_sensitive_component(component: &str) -> bool {
    let name = component.to_ascii_lowercase();
    name == ".git"
        || name == ".ssh"
        || name == ".aws"
        || name == ".azure"
        || name == ".gcloud"
        || name == ".gitconfig"
        || name == ".npmrc"
        || name == ".env"
        || name.starts_with(".env.")
        || name.starts_with("credentials")
        || name.starts_with("secret")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
}

fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str, ErrorPayload> {
    required_string_allow_empty(input, key).and_then(|value| {
        if value.is_empty() {
            Err(ErrorPayload::new(
                "invalid_input",
                format!("{key} must not be empty"),
            ))
        } else {
            Ok(value)
        }
    })
}

fn required_string_allow_empty<'a>(input: &'a Value, key: &str) -> Result<&'a str, ErrorPayload> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ErrorPayload::new("invalid_input", format!("{key} must be a string")))
}

fn enforce_content_limit(content: &str) -> Result<(), ErrorPayload> {
    if content.len() > MAX_FILE_BYTES {
        return Err(ErrorPayload::new(
            "file_too_large",
            format!("workspace writes are limited to {MAX_FILE_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn bounded_usize(input: &Value, key: &str, default: usize, min: usize, max: usize) -> usize {
    input
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn canonical_root(root: &str) -> Result<PathBuf, ErrorPayload> {
    std::fs::canonicalize(root).map_err(|error| ErrorPayload::new("io_error", error.to_string()))
}

fn relative_path_string(root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if relative.is_empty() {
        ".".into()
    } else {
        relative
    }
}

fn traversable_entry(root: &Path, entry: &DirEntry, include_ignored: bool) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        if name.to_str().is_some_and(is_sensitive_component)
            || (!include_ignored && IGNORED_DIRECTORIES.iter().any(|ignored| name == *ignored))
        {
            return false;
        }
    }
    true
}

struct SearchableFiles {
    files: Vec<PathBuf>,
    truncated: bool,
}

fn searchable_files_with_limit(
    root: &Path,
    search_root: &Path,
    max_entries: usize,
) -> Result<SearchableFiles, ErrorPayload> {
    if search_root.is_file() {
        return Ok(SearchableFiles {
            files: vec![search_root.to_path_buf()],
            truncated: false,
        });
    }
    let mut files = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    for entry in WalkDir::new(search_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| traversable_entry(root, entry, false))
    {
        let entry = entry.map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
        scanned += 1;
        if scanned > max_entries {
            truncated = true;
            break;
        }
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(SearchableFiles { files, truncated })
}

fn read_bounded_file(path: &Path, max_bytes: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut options = no_follow_options();
    options.read(true);
    let file = options.open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() <= max_bytes).then_some(bytes))
}

fn write_file_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut options = no_follow_options();
    let mut file = options.create(true).write(true).truncate(true).open(path)?;
    file.write_all(content)
}

fn no_follow_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    let was_truncated = chars.next().is_some();
    (truncated, was_truncated)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn write_and_edit_nested_workspace_file() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().to_str().expect("utf-8 workspace");

        let written = write(
            root,
            &json!({ "path": "src/example.txt", "content": "old value" }),
        )
        .expect("write nested file");
        assert_eq!(written["parent_created"], true);

        let edited = edit(
            root,
            &json!({
                "path": "src/example.txt",
                "old_text": "old",
                "new_text": "new"
            }),
        )
        .expect("edit file");
        assert_eq!(edited["replacements"], 1);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/example.txt"))
                .expect("read edited file"),
            "new value"
        );
    }

    #[test]
    fn write_rejects_escape_and_sensitive_files() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().to_str().expect("utf-8 workspace");

        let escape = write(root, &json!({ "path": "../escape", "content": "x" }))
            .expect_err("parent traversal must fail");
        assert_eq!(escape.code, "permission_denied");

        let sensitive = write(root, &json!({ "path": ".env", "content": "SECRET=x" }))
            .expect_err("sensitive file must fail");
        assert_eq!(sensitive.code, "permission_denied");

        std::fs::write(workspace.path().join("secret.pem"), "private")
            .expect("seed sensitive file");
        let sensitive_read =
            read(root, &json!({ "path": "secret.pem" })).expect_err("sensitive reads must fail");
        assert_eq!(sensitive_read.code, "permission_denied");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_ancestors_before_reading_or_creating_paths() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let root = workspace.path().to_str().expect("utf-8 workspace");

        std::fs::create_dir(workspace.path().join(".ssh")).expect("create sensitive directory");
        std::fs::write(workspace.path().join(".ssh/config"), "secret")
            .expect("seed sensitive file");
        symlink(
            workspace.path().join(".ssh"),
            workspace.path().join("alias"),
        )
        .expect("create internal symlink");
        symlink(outside.path(), workspace.path().join("outside")).expect("create external symlink");

        let read_error = read(root, &json!({ "path": "alias/config" }))
            .expect_err("intermediate symlink read must fail");
        assert_eq!(read_error.code, "permission_denied");

        let write_error = write(
            root,
            &json!({ "path": "outside/new/file.txt", "content": "x" }),
        )
        .expect_err("intermediate symlink write must fail");
        assert_eq!(write_error.code, "permission_denied");
        assert!(!outside.path().join("new").exists());
    }

    #[test]
    fn searchable_files_reports_walk_truncation() {
        let workspace = tempdir().expect("workspace");
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(workspace.path().join(name), name).expect("seed searchable file");
        }

        let result = searchable_files_with_limit(workspace.path(), workspace.path(), 2)
            .expect("collect searchable files");

        assert!(result.truncated);
        assert!(result.files.len() < 3);
    }

    #[test]
    fn list_grep_and_glob_are_bounded_and_hide_sensitive_paths() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().to_str().expect("utf-8 workspace");
        std::fs::create_dir_all(workspace.path().join("src")).expect("create src");
        std::fs::write(
            workspace.path().join("src/lib.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .expect("write source");
        std::fs::write(workspace.path().join(".env"), "TOKEN=secret").expect("write secret");

        let listed = list(root, &json!({ "path": ".", "depth": 2 })).expect("list workspace");
        assert!(
            listed["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .any(|entry| entry["path"] == "src/lib.rs")
        );
        assert!(
            listed["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .all(|entry| entry["path"] != ".env")
        );

        let matches = grep(
            root,
            &json!({ "pattern": "fn (alpha|beta)", "path": "src" }),
        )
        .expect("grep workspace");
        assert_eq!(matches["matches"].as_array().expect("matches").len(), 2);

        let paths = glob(root, &json!({ "pattern": "**/*.rs" })).expect("glob workspace");
        assert_eq!(paths["paths"], json!(["src/lib.rs"]));

        for pattern in [
            "*", "**/*", "**/**", "*/*", "./**/*", "**/?*", "[a-z]*", "**/*.*",
        ] {
            let error = glob(root, &json!({ "pattern": pattern }))
                .expect_err("root catch-all glob must be rejected");
            assert_eq!(error.code, "invalid_input", "pattern: {pattern}");
            assert!(error.message.contains("workspace.list"));
        }

        for pattern in ["*.rs", "**/*.rs", "**/*.toml", "src/**"] {
            glob(root, &json!({ "pattern": pattern }))
                .unwrap_or_else(|error| panic!("pattern {pattern} should pass: {error:?}"));
        }

        glob(root, &json!({ "pattern": "*", "root": "src" }))
            .expect("catch-all glob under an explicit subdirectory");
        let normalized_root_error = glob(root, &json!({ "pattern": "*", "root": "./" }))
            .expect_err("root ./ must not bypass the catch-all guard");
        assert_eq!(normalized_root_error.code, "invalid_input");

        let limited =
            list(root, &json!({ "path": ".", "depth": 2, "limit": 1 })).expect("limited list");
        assert_eq!(limited["returned_entries"], 1);
        assert_eq!(limited["truncated"], true);
    }
}
