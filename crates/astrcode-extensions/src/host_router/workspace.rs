//! 工作区文件读写边界。

use std::{
    ffi::{OsStr, OsString},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use astrcode_extension_contract::WireErrorCode;
use astrcode_extension_sdk::{
    extension::ExtensionTasks,
    host::{
        HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES, HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES,
        HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS, HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES,
        HOST_WORKSPACE_LIST_DEFAULT_LIMIT, HOST_WORKSPACE_MAX_FILE_BYTES, HostOperation,
        HostOperationGroup, HostWorkspaceEditOutput, HostWorkspaceEditRequest,
        HostWorkspaceGlobOutput, HostWorkspaceGlobRequest, HostWorkspaceGrepMatch,
        HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListEntry,
        HostWorkspaceListOutput, HostWorkspaceListRequest, HostWorkspaceReadOutput,
        HostWorkspaceReadRequest, HostWorkspaceWriteOutput, HostWorkspaceWriteRequest,
    },
    s5r::ErrorPayload,
};
use globset::Glob;
use regex::Regex;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use walkdir::{DirEntry, WalkDir};

use super::{
    backend_unavailable, invalid_group_operation, io_error, parse_wire_request,
    path::{canonicalize_workspace_path, validate_relative_path_components},
    run_blocking_io, run_blocking_io_to_completion, serialize_wire_response,
};

const MAX_WALK_ENTRIES: usize = 5_000;
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

    pub(super) async fn invoke(
        &self,
        operation: HostOperation,
        input: Value,
        working_dir: Option<&str>,
        tasks: Option<&ExtensionTasks>,
    ) -> Result<Value, ErrorPayload> {
        let root = self.root(working_dir)?.to_owned();
        match operation {
            HostOperation::WorkspaceRead => run_workspace_io(operation, input, root, read).await,
            HostOperation::WorkspaceList => run_workspace_io(operation, input, root, list).await,
            HostOperation::WorkspaceGrep => run_workspace_io(operation, input, root, grep).await,
            HostOperation::WorkspaceGlob => run_workspace_io(operation, input, root, glob).await,
            HostOperation::WorkspaceWrite => {
                run_persistent_workspace_io(tasks, "workspace-write", operation, input, root, write)
                    .await
            },
            HostOperation::WorkspaceEdit => {
                run_persistent_workspace_io(tasks, "workspace-edit", operation, input, root, edit)
                    .await
            },
            _ => Err(invalid_group_operation(
                operation,
                HostOperationGroup::Workspace,
            )),
        }
    }

    pub(super) fn has_root(&self, working_dir: Option<&str>) -> bool {
        working_dir.is_some() || self.default_working_dir.is_some()
    }

    fn root<'a>(&'a self, working_dir: Option<&'a str>) -> Result<&'a str, ErrorPayload> {
        working_dir
            .or(self.default_working_dir.as_deref())
            .ok_or_else(|| backend_unavailable("working_dir not set"))
    }
}

async fn run_workspace_io<Request, Output>(
    operation: HostOperation,
    input: Value,
    root: String,
    handler: fn(&str, Request, &'static str) -> Result<Output, ErrorPayload>,
) -> Result<Value, ErrorPayload>
where
    Request: DeserializeOwned + Send + 'static,
    Output: Serialize + Send + 'static,
{
    let name = operation.wire_name();
    let request = parse_wire_request(&input, name)?;
    let output = run_blocking_io(move || handler(&root, request, name)).await?;
    serialize_wire_response(output, name)
}

async fn run_persistent_workspace_io<Request, Output>(
    tasks: Option<&ExtensionTasks>,
    name: &'static str,
    operation: HostOperation,
    input: Value,
    root: String,
    handler: fn(&str, Request, &'static str) -> Result<Output, ErrorPayload>,
) -> Result<Value, ErrorPayload>
where
    Request: DeserializeOwned + Send + 'static,
    Output: Serialize + Send + 'static,
{
    let wire_name = operation.wire_name();
    let request = parse_wire_request(&input, wire_name)?;
    let output =
        run_blocking_io_to_completion(tasks, name, move || handler(&root, request, wire_name))
            .await?;
    serialize_wire_response(output, wire_name)
}

fn read(
    root: &str,
    request: HostWorkspaceReadRequest,
    capability: &'static str,
) -> Result<HostWorkspaceReadOutput, ErrorPayload> {
    let relative_path = required_non_empty(&request.path, "path")?;
    reject_sensitive_path(relative_path)?;
    let path = resolve_existing_path(root, relative_path, capability)?;
    let metadata = std::fs::metadata(&path).map_err(io_error)?;
    if !metadata.is_file() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("{capability} path must be a regular file"),
        ));
    }
    let max_bytes = request
        .max_bytes
        .unwrap_or(HOST_WORKSPACE_MAX_FILE_BYTES as u64);
    if metadata.len() > max_bytes {
        return Err(ErrorPayload::new(
            WireErrorCode::FileTooLarge,
            format!("file size {} exceeds max_bytes {max_bytes}", metadata.len()),
        ));
    }
    let content = read_bounded_file(&path, max_bytes as usize)
        .map_err(io_error)?
        .ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::FileTooLarge,
                format!("file size exceeds max_bytes {max_bytes}"),
            )
        })?;
    let content = String::from_utf8(content).map_err(io_error)?;
    Ok(HostWorkspaceReadOutput { content })
}

fn list(
    root: &str,
    request: HostWorkspaceListRequest,
    capability: &'static str,
) -> Result<HostWorkspaceListOutput, ErrorPayload> {
    let relative_path = request.path.as_str();
    reject_sensitive_path(relative_path)?;
    let path = resolve_existing_path(root, relative_path, capability)?;
    if !path.is_dir() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("{capability} path must be a directory"),
        ));
    }
    let depth = request.depth;
    let limit = request.limit.unwrap_or(HOST_WORKSPACE_LIST_DEFAULT_LIMIT);
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
        let entry = entry.map_err(io_error)?;
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
        entries.push(HostWorkspaceListEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: relative_path_string(&canonical_root, entry.path()),
            kind: kind.into(),
            bytes,
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let returned_entries = entries.len();
    Ok(HostWorkspaceListOutput {
        path: relative_path_string(&canonical_root, &path),
        entries,
        returned_entries,
        truncated,
    })
}

fn grep(
    root: &str,
    request: HostWorkspaceGrepRequest,
    capability: &'static str,
) -> Result<HostWorkspaceGrepOutput, ErrorPayload> {
    let pattern = required_non_empty(&request.pattern, "pattern")?;
    let regex = Regex::new(pattern).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("invalid regex: {error}"),
        )
    })?;
    let relative_path = request.path.as_deref().unwrap_or(".");
    reject_sensitive_path(relative_path)?;
    let search_root = resolve_existing_path(root, relative_path, capability)?;
    let canonical_root = canonical_root(root)?;
    let max_matches = request
        .max_matches
        .unwrap_or(HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES);
    let max_bytes = request
        .max_bytes
        .unwrap_or(HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES);
    let max_line_chars = request
        .max_line_chars
        .unwrap_or(HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS);
    let searchable = searchable_files_with_limit(&canonical_root, &search_root, MAX_WALK_ENTRIES)?;
    let mut matches = Vec::new();
    let mut output_bytes = 0usize;
    let mut output_truncated = false;
    let mut scanned_bytes = 0usize;
    let mut scan_truncated = false;
    for path in searchable.files {
        let content = match read_bounded_file(&path, HOST_WORKSPACE_MAX_FILE_BYTES) {
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
            matches.push(HostWorkspaceGrepMatch {
                path: relative_path_string(&canonical_root, &path),
                line_number: index + 1,
                line,
                line_truncated,
            });
        }
        if output_truncated {
            break;
        }
    }
    Ok(HostWorkspaceGrepOutput {
        pattern: pattern.to_owned(),
        root: relative_path_string(&canonical_root, &search_root),
        matches,
        truncated: searchable.truncated || scan_truncated || output_truncated,
    })
}

fn glob(
    root: &str,
    request: HostWorkspaceGlobRequest,
    capability: &'static str,
) -> Result<HostWorkspaceGlobOutput, ErrorPayload> {
    let pattern = required_non_empty(&request.pattern, "pattern")?;
    if Path::new(pattern).is_absolute() {
        return Err(ErrorPayload::new(
            WireErrorCode::PermissionDenied,
            "glob pattern must be relative to the workspace",
        ));
    }
    let matcher = Glob::new(pattern)
        .map_err(|error| ErrorPayload::new(WireErrorCode::InvalidInput, error.to_string()))?
        .compile_matcher();
    let relative_root = request.root.as_deref().unwrap_or(".");
    if is_overly_broad_glob(pattern, relative_root) {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!(
                "Use {} to inspect workspace structure; {capability} is for targeted file \
                 discovery (for example **/*.rs or crates/astrcode-core/**)",
                HostOperation::WorkspaceList.wire_name(),
            ),
        ));
    }
    reject_sensitive_path(relative_root)?;
    let search_root = resolve_existing_path(root, relative_root, capability)?;
    if !search_root.is_dir() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("{capability} root must be a directory"),
        ));
    }
    let canonical_root = canonical_root(root)?;
    let max_matches = request
        .max_matches
        .unwrap_or(HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES);
    let include_ignored = request.include_ignored;
    let mut paths = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    for entry in WalkDir::new(&search_root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| traversable_entry(&canonical_root, entry, include_ignored))
    {
        let entry = entry.map_err(io_error)?;
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
    Ok(HostWorkspaceGlobOutput {
        pattern: pattern.to_owned(),
        root: relative_path_string(&canonical_root, &search_root),
        paths,
        truncated,
    })
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

fn write(
    root: &str,
    request: HostWorkspaceWriteRequest,
    capability: &'static str,
) -> Result<HostWorkspaceWriteOutput, ErrorPayload> {
    let relative_path = required_non_empty(&request.path, "path")?;
    let content = request.content.as_str();
    enforce_content_limit(content)?;
    reject_sensitive_path(relative_path)?;
    let (parent, file_name, parent_created) =
        resolve_write_target(root, relative_path, capability)?;
    let path = parent.join(file_name);
    reject_symlink_target(&path, capability)?;
    write_file_no_follow(&path, content.as_bytes()).map_err(io_error)?;
    Ok(HostWorkspaceWriteOutput {
        path: relative_path.to_owned(),
        bytes_written: content.len(),
        parent_created,
    })
}

fn edit(
    root: &str,
    request: HostWorkspaceEditRequest,
    capability: &'static str,
) -> Result<HostWorkspaceEditOutput, ErrorPayload> {
    let relative_path = required_non_empty(&request.path, "path")?;
    let old_text = required_non_empty(&request.old_text, "old_text")?;
    let new_text = request.new_text.as_str();
    let replace_all = request.replace_all;
    reject_sensitive_path(relative_path)?;
    let path = resolve_existing_path(root, relative_path, capability)?;
    let metadata = std::fs::metadata(&path).map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > HOST_WORKSPACE_MAX_FILE_BYTES as u64 {
        return Err(ErrorPayload::new(
            WireErrorCode::FileTooLarge,
            format!(
                "{capability} supports regular files up to {HOST_WORKSPACE_MAX_FILE_BYTES} bytes"
            ),
        ));
    }
    let content = read_bounded_file(&path, HOST_WORKSPACE_MAX_FILE_BYTES)
        .map_err(io_error)?
        .ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::FileTooLarge,
                format!(
                    "{capability} supports regular files up to {HOST_WORKSPACE_MAX_FILE_BYTES} \
                     bytes"
                ),
            )
        })?;
    let content = String::from_utf8(content).map_err(io_error)?;
    let replacements = content.matches(old_text).count();
    if replacements == 0 {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("old_text not found in {relative_path}"),
        ));
    }
    if !replace_all && replacements > 1 {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
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
    write_file_no_follow(&path, edited.as_bytes()).map_err(io_error)?;
    Ok(HostWorkspaceEditOutput {
        path: relative_path.to_owned(),
        replacements: if replace_all { replacements } else { 1 },
        bytes_written: edited.len(),
    })
}

fn resolve_existing_path(
    root: &str,
    relative_path: &str,
    capability: &str,
) -> Result<PathBuf, ErrorPayload> {
    let canonical_root = canonical_root(root)?;
    reject_symlink_components(&canonical_root, Path::new(relative_path), capability)?;
    canonicalize_workspace_path(root, relative_path)
}

fn resolve_write_target(
    root: &str,
    relative_path: &str,
    capability: &str,
) -> Result<(PathBuf, OsString, bool), ErrorPayload> {
    let relative = Path::new(relative_path);
    validate_relative_path_components(relative)?;
    let file_name = relative
        .file_name()
        .filter(|name| *name != OsStr::new(".."))
        .ok_or_else(|| {
            ErrorPayload::new(WireErrorCode::InvalidInput, "path must reference a file")
        })?
        .to_owned();
    let canonical_root = std::fs::canonicalize(root).map_err(io_error)?;
    let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
    reject_symlink_components(&canonical_root, relative_parent, capability)?;
    let parent = canonical_root.join(relative_parent);
    let parent_created = !parent.exists();
    std::fs::create_dir_all(&parent).map_err(io_error)?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(io_error)?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(ErrorPayload::new(
            WireErrorCode::PermissionDenied,
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
                    WireErrorCode::PermissionDenied,
                    "path must be relative to the workspace",
                ));
            },
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ErrorPayload::new(
                    WireErrorCode::PermissionDenied,
                    format!("symlink paths are not accessible via {capability}"),
                ));
            },
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(io_error(error));
            },
        }
    }
    Ok(())
}

fn reject_symlink_target(path: &Path, capability: &str) -> Result<(), ErrorPayload> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ErrorPayload::new(
            WireErrorCode::PermissionDenied,
            format!("symlink paths are not writable via {capability}"),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
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
            WireErrorCode::PermissionDenied,
            "workspace access to sensitive files is not allowed",
        ));
    }
    Ok(())
}

// astrcode-session::permission::sensitive_file_ask::SENSITIVE_PATTERNS 有对应的 glob
// 定义,修改时需同步。
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

fn required_non_empty<'a>(value: &'a str, key: &str) -> Result<&'a str, ErrorPayload> {
    if value.is_empty() {
        Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("{key} must not be empty"),
        ))
    } else {
        Ok(value)
    }
}

fn enforce_content_limit(content: &str) -> Result<(), ErrorPayload> {
    if content.len() > HOST_WORKSPACE_MAX_FILE_BYTES {
        return Err(ErrorPayload::new(
            WireErrorCode::FileTooLarge,
            format!("workspace writes are limited to {HOST_WORKSPACE_MAX_FILE_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn canonical_root(root: &str) -> Result<PathBuf, ErrorPayload> {
    std::fs::canonicalize(root).map_err(io_error)
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
        let entry = entry.map_err(io_error)?;
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

    fn glob_request(pattern: &str) -> HostWorkspaceGlobRequest {
        HostWorkspaceGlobRequest {
            pattern: pattern.into(),
            root: None,
            max_matches: None,
            include_ignored: false,
        }
    }

    #[test]
    fn write_and_edit_nested_workspace_file() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().to_str().expect("utf-8 workspace");

        let written = write(
            root,
            HostWorkspaceWriteRequest {
                path: "src/example.txt".into(),
                content: "old value".into(),
            },
            HostOperation::WorkspaceWrite.wire_name(),
        )
        .expect("write nested file");
        assert!(written.parent_created);

        let edited = edit(
            root,
            HostWorkspaceEditRequest {
                path: "src/example.txt".into(),
                old_text: "old".into(),
                new_text: "new".into(),
                replace_all: false,
            },
            HostOperation::WorkspaceEdit.wire_name(),
        )
        .expect("edit file");
        assert_eq!(edited.replacements, 1);
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

        let escape = write(
            root,
            HostWorkspaceWriteRequest {
                path: "../escape".into(),
                content: "x".into(),
            },
            HostOperation::WorkspaceWrite.wire_name(),
        )
        .expect_err("parent traversal must fail");
        assert_eq!(escape.code_enum(), Some(WireErrorCode::PermissionDenied));

        let sensitive = write(
            root,
            HostWorkspaceWriteRequest {
                path: ".env".into(),
                content: "SECRET=x".into(),
            },
            HostOperation::WorkspaceWrite.wire_name(),
        )
        .expect_err("sensitive file must fail");
        assert_eq!(sensitive.code_enum(), Some(WireErrorCode::PermissionDenied));

        std::fs::write(workspace.path().join("secret.pem"), "private")
            .expect("seed sensitive file");
        let sensitive_read = read(
            root,
            HostWorkspaceReadRequest {
                path: "secret.pem".into(),
                max_bytes: None,
            },
            HostOperation::WorkspaceRead.wire_name(),
        )
        .expect_err("sensitive reads must fail");
        assert_eq!(
            sensitive_read.code_enum(),
            Some(WireErrorCode::PermissionDenied)
        );
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

        let read_error = read(
            root,
            HostWorkspaceReadRequest {
                path: "alias/config".into(),
                max_bytes: None,
            },
            HostOperation::WorkspaceRead.wire_name(),
        )
        .expect_err("intermediate symlink read must fail");
        assert_eq!(
            read_error.code_enum(),
            Some(WireErrorCode::PermissionDenied)
        );

        let write_error = write(
            root,
            HostWorkspaceWriteRequest {
                path: "outside/new/file.txt".into(),
                content: "x".into(),
            },
            HostOperation::WorkspaceWrite.wire_name(),
        )
        .expect_err("intermediate symlink write must fail");
        assert_eq!(
            write_error.code_enum(),
            Some(WireErrorCode::PermissionDenied)
        );
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

        let listed = list(
            root,
            HostWorkspaceListRequest {
                path: ".".into(),
                depth: 2,
                limit: None,
            },
            HostOperation::WorkspaceList.wire_name(),
        )
        .expect("list workspace");
        assert!(
            listed
                .entries
                .iter()
                .any(|entry| entry.path == "src/lib.rs")
        );
        assert!(listed.entries.iter().all(|entry| entry.path != ".env"));

        let matches = grep(
            root,
            HostWorkspaceGrepRequest {
                pattern: "fn (alpha|beta)".into(),
                path: Some("src".into()),
                max_matches: None,
                max_bytes: None,
                max_line_chars: None,
            },
            HostOperation::WorkspaceGrep.wire_name(),
        )
        .expect("grep workspace");
        assert_eq!(matches.matches.len(), 2);

        let paths = glob(
            root,
            glob_request("**/*.rs"),
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("glob workspace");
        assert_eq!(paths.paths, ["src/lib.rs"]);

        for pattern in [
            "*", "**/*", "**/**", "*/*", "./**/*", "**/?*", "[a-z]*", "**/*.*",
        ] {
            let error = glob(
                root,
                glob_request(pattern),
                HostOperation::WorkspaceGlob.wire_name(),
            )
            .expect_err("root catch-all glob must be rejected");
            assert_eq!(
                error.code_enum(),
                Some(WireErrorCode::InvalidInput),
                "pattern: {pattern}"
            );
            assert!(error.message.contains("workspace.list"));
        }

        for pattern in ["*.rs", "**/*.rs", "**/*.toml", "src/**"] {
            glob(
                root,
                glob_request(pattern),
                HostOperation::WorkspaceGlob.wire_name(),
            )
            .unwrap_or_else(|error| panic!("pattern {pattern} should pass: {error:?}"));
        }

        glob(
            root,
            HostWorkspaceGlobRequest {
                root: Some("src".into()),
                ..glob_request("*")
            },
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("catch-all glob under an explicit subdirectory");
        let normalized_root_error = glob(
            root,
            HostWorkspaceGlobRequest {
                root: Some("./".into()),
                ..glob_request("*")
            },
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect_err("root ./ must not bypass the catch-all guard");
        assert_eq!(
            normalized_root_error.code_enum(),
            Some(WireErrorCode::InvalidInput)
        );

        let limited = list(
            root,
            HostWorkspaceListRequest {
                path: ".".into(),
                depth: 2,
                limit: Some(1),
            },
            HostOperation::WorkspaceList.wire_name(),
        )
        .expect("limited list");
        assert_eq!(limited.returned_entries, 1);
        assert!(limited.truncated);
    }
}
