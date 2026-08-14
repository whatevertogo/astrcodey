//! 工作区文件读写边界。

use std::{
    ffi::{OsStr, OsString},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use astrcode_core::tool::{FileObservation, FileObservationStore};
use astrcode_extension_sdk::{
    extension::ExtensionTasks,
    host::{
        HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES, HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES,
        HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS, HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES,
        HOST_WORKSPACE_LIST_DEFAULT_LIMIT, HOST_WORKSPACE_MAX_DIFF_BYTES,
        HOST_WORKSPACE_MAX_FILE_BYTES, HOST_WORKSPACE_MAX_IMAGE_BYTES,
        HOST_WORKSPACE_MAX_TEXT_OUTPUT_BYTES, HostOperation, HostWorkspaceEditOutput,
        HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
        HostWorkspaceGrepContextLine, HostWorkspaceGrepEntry, HostWorkspaceGrepMode,
        HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListEntry,
        HostWorkspaceListOutput, HostWorkspaceListRequest, HostWorkspaceReadOutput,
        HostWorkspaceReadRequest, HostWorkspaceTextChange, HostWorkspaceTextEdit,
        HostWorkspaceWriteOutput, HostWorkspaceWriteRequest, internal::HostOperationGroup,
    },
    s5r::ErrorPayload,
    wire::WireErrorCode,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use globset::{Glob, GlobMatcher};
use ignore::{DirEntry as IgnoreDirEntry, WalkBuilder};
use regex::{Regex, RegexBuilder};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use walkdir::{DirEntry, WalkDir};

use super::{
    InvokeContext, backend_unavailable, invalid_group_operation, io_error, parse_wire_request,
    path::{canonicalize_workspace_path, validate_relative_path_components},
    run_blocking_io, run_blocking_io_to_completion, serialize_wire_response,
};

const MAX_WALK_ENTRIES: usize = 5_000;
const MAX_SEARCH_SCAN_BYTES: usize = 64 * 1024 * 1024;
const IGNORED_DIRECTORIES: &[&str] = &[".git", "node_modules"];
const SEARCH_VCS_DIRECTORIES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];
const SEARCH_BUILD_DIRECTORIES: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    ".gradle",
    ".dart_tool",
    "Pods",
    ".swiftbuild",
];

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
        context: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let root = self.root(context.working_dir.as_deref())?.to_owned();
        match operation {
            HostOperation::WorkspaceApplyPatch => {
                let observations = context.file_observation_store.clone();
                run_persistent_workspace_io(
                    context.tasks.as_ref(),
                    "workspace-apply-patch",
                    operation,
                    input,
                    root,
                    move |root, request, name| {
                        super::workspace_patch::apply_patch(
                            root,
                            request,
                            name,
                            observations.as_ref(),
                        )
                    },
                )
                .await
            },
            HostOperation::WorkspaceRead => {
                let observations = context.file_observation_store.clone();
                run_workspace_io(operation, input, root, move |root, request, name| {
                    read(root, request, name, observations.as_ref())
                })
                .await
            },
            HostOperation::WorkspaceList => run_workspace_io(operation, input, root, list).await,
            HostOperation::WorkspaceGrep => run_workspace_io(operation, input, root, grep).await,
            HostOperation::WorkspaceGlob => run_workspace_io(operation, input, root, glob).await,
            HostOperation::WorkspaceWrite => {
                let observations = context.file_observation_store.clone();
                run_persistent_workspace_io(
                    context.tasks.as_ref(),
                    "workspace-write",
                    operation,
                    input,
                    root,
                    move |root, request, name| write(root, request, name, observations.as_ref()),
                )
                .await
            },
            HostOperation::WorkspaceEdit => {
                let observations = context.file_observation_store.clone();
                run_persistent_workspace_io(
                    context.tasks.as_ref(),
                    "workspace-edit",
                    operation,
                    input,
                    root,
                    move |root, request, name| edit(root, request, name, observations.as_ref()),
                )
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
    handler: impl FnOnce(&str, Request, &'static str) -> Result<Output, ErrorPayload> + Send + 'static,
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
    handler: impl FnOnce(&str, Request, &'static str) -> Result<Output, ErrorPayload> + Send + 'static,
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
    observations: Option<&std::sync::Arc<dyn FileObservationStore>>,
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
    if let Some(media_type) = image_media_type(&path) {
        if metadata.len() > HOST_WORKSPACE_MAX_IMAGE_BYTES as u64 {
            return Err(ErrorPayload::new(
                WireErrorCode::FileTooLarge,
                format!(
                    "image size {} exceeds {HOST_WORKSPACE_MAX_IMAGE_BYTES} bytes",
                    metadata.len()
                ),
            ));
        }
        let data = read_bounded_file(&path, HOST_WORKSPACE_MAX_IMAGE_BYTES)
            .map_err(io_error)?
            .ok_or_else(|| {
                ErrorPayload::new(WireErrorCode::FileTooLarge, "image grew while being read")
            })?;
        remember_observation(observations, &path)?;
        return Ok(HostWorkspaceReadOutput::Image {
            media_type: media_type.into(),
            data_base64: BASE64.encode(&data),
            bytes: data.len(),
        });
    }
    let bytes = read_bounded_file(&path, max_bytes as usize)
        .map_err(io_error)?
        .ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::FileTooLarge,
                format!("file size exceeds max_bytes {max_bytes}"),
            )
        })?;
    remember_observation(observations, &path)?;
    let byte_count = bytes.len();
    Ok(match String::from_utf8(bytes) {
        Ok(content) => {
            let (content, total_lines, line_offset, returned_lines, has_more_lines) =
                bounded_text_lines(
                    &content,
                    request.line_offset,
                    request.line_limit,
                    HOST_WORKSPACE_MAX_TEXT_OUTPUT_BYTES,
                )?;
            HostWorkspaceReadOutput::Text {
                content,
                bytes: byte_count,
                total_lines,
                line_offset,
                returned_lines,
                has_more_lines,
            }
        },
        Err(_) => HostWorkspaceReadOutput::Binary { bytes: byte_count },
    })
}

fn bounded_text_lines(
    content: &str,
    requested_offset: usize,
    line_limit: Option<usize>,
    max_output_bytes: usize,
) -> Result<(String, usize, usize, usize, bool), ErrorPayload> {
    let total_lines = content.lines().count();
    let line_offset = requested_offset.min(total_lines);
    let line_limit = line_limit.unwrap_or(usize::MAX);
    let mut output = String::new();
    let mut returned_lines = 0usize;
    for line in content.lines().skip(line_offset).take(line_limit) {
        let separator_bytes = usize::from(!output.is_empty());
        if output
            .len()
            .saturating_add(separator_bytes)
            .saturating_add(line.len())
            > max_output_bytes
        {
            if output.is_empty() {
                return Err(ErrorPayload::new(
                    WireErrorCode::FileTooLarge,
                    format!(
                        "line {} exceeds the {max_output_bytes}-byte workspace read output limit",
                        line_offset + 1
                    ),
                )
                .with_hint("use grep or a process tool to inspect an exceptionally long line"));
            }
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(line);
        returned_lines += 1;
    }
    let has_more_lines = line_offset.saturating_add(returned_lines) < total_lines;
    Ok((
        output,
        total_lines,
        line_offset,
        returned_lines,
        has_more_lines,
    ))
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
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
        .filter_entry(|entry| traversable_entry(&canonical_root, entry, false, true))
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
    let regex = RegexBuilder::new(pattern)
        .multi_line(request.multiline)
        .dot_matches_new_line(request.multiline)
        .build()
        .map_err(|error| {
            ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("invalid regex: {error}"),
            )
        })?;
    let path_filters = compile_path_filters(&request.path_filters)?;
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
    let offset = request.offset;
    let searchable = searchable_files_with_limit(
        &canonical_root,
        &search_root,
        MAX_WALK_ENTRIES,
        request.recursive,
        &path_filters,
    )?;
    let mut entries = Vec::new();
    let mut output_bytes = 0usize;
    let mut output_truncated = false;
    let mut scanned_bytes = 0usize;
    let mut scan_truncated = false;
    let mut skipped_files = 0usize;
    let mut logical_entries = 0usize;
    'files: for path in searchable.files {
        let content = match read_bounded_file(&path, HOST_WORKSPACE_MAX_FILE_BYTES) {
            Ok(Some(content)) => content,
            Ok(None) => {
                skipped_files += 1;
                scan_truncated = true;
                continue;
            },
            Err(_) => {
                skipped_files += 1;
                continue;
            },
        };
        if scanned_bytes.saturating_add(content.len()) > MAX_SEARCH_SCAN_BYTES {
            scan_truncated = true;
            break;
        }
        scanned_bytes += content.len();
        let Ok(content) = String::from_utf8(content) else {
            skipped_files += 1;
            continue;
        };
        let relative = relative_path_string(&canonical_root, &path);
        let file_entries = grep_file_entries(
            &relative,
            &content,
            &regex,
            request.multiline,
            request.mode,
            request.before_context,
            request.after_context,
            max_line_chars,
        );
        for entry in file_entries {
            if logical_entries < offset {
                logical_entries += 1;
                continue;
            }
            let entry_bytes = grep_entry_bytes(&entry);
            if entries.len() >= max_matches || output_bytes.saturating_add(entry_bytes) > max_bytes
            {
                if entries.is_empty() && entry_bytes > max_bytes {
                    return Err(ErrorPayload::new(
                        WireErrorCode::InvalidInput,
                        format!(
                            "one search result requires {entry_bytes} bytes, exceeding max_bytes \
                             {max_bytes}"
                        ),
                    )
                    .with_hint("increase max_bytes or reduce context and max_line_chars"));
                }
                output_truncated = true;
                break 'files;
            }
            logical_entries += 1;
            output_bytes = output_bytes.saturating_add(entry_bytes);
            entries.push(entry);
        }
    }
    Ok(HostWorkspaceGrepOutput {
        pattern: pattern.to_owned(),
        root: relative_path_string(&canonical_root, &search_root),
        entries,
        has_more: output_truncated,
        scan_truncated: searchable.truncated || scan_truncated,
        skipped_files,
    })
}

fn compile_path_filters(filters: &[String]) -> Result<Vec<GlobMatcher>, ErrorPayload> {
    filters
        .iter()
        .map(|filter| {
            let filter = required_non_empty(filter, "path filter")?;
            Glob::new(filter)
                .map(|pattern| pattern.compile_matcher())
                .map_err(|error| {
                    ErrorPayload::new(
                        WireErrorCode::InvalidInput,
                        format!("invalid path filter {filter:?}: {error}"),
                    )
                })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn grep_file_entries(
    path: &str,
    content: &str,
    regex: &Regex,
    multiline: bool,
    mode: HostWorkspaceGrepMode,
    before_context: usize,
    after_context: usize,
    max_line_chars: usize,
) -> Vec<HostWorkspaceGrepEntry> {
    let lines = content.lines().collect::<Vec<_>>();
    let matching_lines = if multiline {
        multiline_matches(content, regex)
    } else {
        lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| regex.is_match(line).then_some((index, index)))
            .collect()
    };
    if matching_lines.is_empty() {
        return Vec::new();
    }

    match mode {
        HostWorkspaceGrepMode::FilesWithMatches => {
            vec![HostWorkspaceGrepEntry::File { path: path.into() }]
        },
        HostWorkspaceGrepMode::Count => vec![HostWorkspaceGrepEntry::Count {
            path: path.into(),
            count: matching_lines.len(),
        }],
        HostWorkspaceGrepMode::Content => matching_lines
            .into_iter()
            .map(|(start, end)| {
                let matched = lines
                    .get(start..=end.min(lines.len().saturating_sub(1)))
                    .unwrap_or_default()
                    .join("\n");
                let (line, line_truncated) = truncate_chars(&matched, max_line_chars);
                HostWorkspaceGrepEntry::Content {
                    path: path.into(),
                    line_number: start + 1,
                    line,
                    line_truncated,
                    before_context: context_lines(
                        &lines,
                        start.saturating_sub(before_context),
                        start,
                        max_line_chars,
                    ),
                    after_context: context_lines(
                        &lines,
                        (end + 1).min(lines.len()),
                        (end + 1).saturating_add(after_context).min(lines.len()),
                        max_line_chars,
                    ),
                }
            })
            .collect(),
    }
}

fn multiline_matches(content: &str, regex: &Regex) -> Vec<(usize, usize)> {
    let mut line_starts = vec![0usize];
    line_starts.extend(
        content
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    regex
        .find_iter(content)
        .map(|matched| {
            let start = line_starts.partition_point(|offset| *offset <= matched.start()) - 1;
            let end_byte = matched.end().saturating_sub(1).max(matched.start());
            let end = line_starts.partition_point(|offset| *offset <= end_byte) - 1;
            (start, end)
        })
        .collect()
}

fn context_lines(
    lines: &[&str],
    start: usize,
    end: usize,
    max_line_chars: usize,
) -> Vec<HostWorkspaceGrepContextLine> {
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let (line, line_truncated) = truncate_chars(line, max_line_chars);
            HostWorkspaceGrepContextLine {
                line_number: start + index + 1,
                line,
                line_truncated,
            }
        })
        .collect()
}

fn grep_entry_bytes(entry: &HostWorkspaceGrepEntry) -> usize {
    match entry {
        HostWorkspaceGrepEntry::Content {
            path,
            line,
            before_context,
            after_context,
            ..
        } => path.len().saturating_add(line.len()).saturating_add(
            before_context
                .iter()
                .chain(after_context)
                .map(|context| context.line.len())
                .sum::<usize>(),
        ),
        HostWorkspaceGrepEntry::File { path } => path.len(),
        HostWorkspaceGrepEntry::Count { path, .. } => path.len().saturating_add(20),
    }
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
    let offset = request.offset;
    let include_hidden = request.include_hidden;
    let include_directories = request.include_directories;
    let mut matching_paths = Vec::new();
    let mut scanned = 0usize;
    let mut walk_truncated = false;
    let walker = workspace_search_walker(
        &canonical_root,
        &search_root,
        include_hidden,
        request.respect_gitignore,
    );
    for entry in walker.build().skip(1) {
        let entry = entry.map_err(|error| io_error(std::io::Error::other(error)))?;
        scanned += 1;
        if scanned > MAX_WALK_ENTRIES {
            walk_truncated = true;
            break;
        }
        let relative_to_search = entry
            .path()
            .strip_prefix(&search_root)
            .unwrap_or(entry.path());
        let file_type = entry.file_type();
        if (file_type.is_some_and(|kind| kind.is_file())
            || (include_directories && file_type.is_some_and(|kind| kind.is_dir())))
            && matcher.is_match(relative_to_search)
        {
            let mut path = relative_path_string(&canonical_root, entry.path());
            if file_type.is_some_and(|kind| kind.is_dir()) {
                path.push('/');
            }
            matching_paths.push(path);
        }
    }
    matching_paths.sort();
    let total_matches = (!walk_truncated).then_some(matching_paths.len());
    let has_more = offset.saturating_add(max_matches) < matching_paths.len();
    let paths = matching_paths
        .into_iter()
        .skip(offset)
        .take(max_matches)
        .collect();
    Ok(HostWorkspaceGlobOutput {
        pattern: pattern.to_owned(),
        root: relative_path_string(&canonical_root, &search_root),
        paths,
        total_matches,
        has_more,
        scan_truncated: walk_truncated,
    })
}

fn workspace_search_walker(
    workspace_root: &Path,
    search_root: &Path,
    include_hidden: bool,
    respect_gitignore: bool,
) -> WalkBuilder {
    let skip_build_directories = !path_contains_component(search_root, SEARCH_BUILD_DIRECTORIES);
    let workspace_root = workspace_root.to_owned();
    let mut builder = WalkBuilder::new(search_root);
    builder
        .hidden(!include_hidden)
        .git_ignore(respect_gitignore)
        .git_exclude(respect_gitignore)
        .git_global(respect_gitignore)
        .ignore(respect_gitignore)
        .parents(respect_gitignore)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            workspace_search_entry_allowed(&workspace_root, entry, skip_build_directories)
        });
    builder
}

fn workspace_search_entry_allowed(
    workspace_root: &Path,
    entry: &IgnoreDirEntry,
    skip_build_directories: bool,
) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let relative = entry
        .path()
        .strip_prefix(workspace_root)
        .unwrap_or(entry.path());
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        if name.to_str().is_some_and(is_sensitive_component)
            || SEARCH_VCS_DIRECTORIES
                .iter()
                .any(|ignored| name == *ignored)
            || (skip_build_directories
                && SEARCH_BUILD_DIRECTORIES
                    .iter()
                    .any(|ignored| name == *ignored))
        {
            return false;
        }
    }
    true
}

fn path_contains_component(path: &Path, names: &[&str]) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| names.contains(&name))
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
    observations: Option<&std::sync::Arc<dyn FileObservationStore>>,
) -> Result<HostWorkspaceWriteOutput, ErrorPayload> {
    let relative_path = required_non_empty(&request.path, "path")?;
    let content = request.content.as_str();
    enforce_content_limit(content)?;
    reject_sensitive_path(relative_path)?;
    let (parent, file_name, _) =
        resolve_write_target(root, relative_path, capability, request.create_dirs)?;
    let path = parent.join(file_name);
    reject_symlink_target(&path, capability)?;
    let previous_bytes = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .map(Some)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            }
        })
        .map_err(io_error)?;
    let previous_content = previous_bytes
        .filter(|bytes| *bytes <= HOST_WORKSPACE_MAX_FILE_BYTES as u64)
        .and_then(|_| std::fs::read_to_string(&path).ok());
    write_file_atomic(&path, content.as_bytes()).map_err(io_error)?;
    remember_observation(observations, &path)?;
    Ok(HostWorkspaceWriteOutput {
        path: relative_path.to_owned(),
        created: previous_bytes.is_none(),
        change: summarize_text_change(
            relative_path,
            previous_bytes,
            previous_content.as_deref(),
            content,
        ),
    })
}

fn edit(
    root: &str,
    request: HostWorkspaceEditRequest,
    capability: &'static str,
    observations: Option<&std::sync::Arc<dyn FileObservationStore>>,
) -> Result<HostWorkspaceEditOutput, ErrorPayload> {
    let relative_path = required_non_empty(&request.path, "path")?;
    let operations = normalize_edits(&request)?;
    reject_sensitive_path(relative_path)?;
    let path = resolve_existing_path(root, relative_path, capability)?;
    ensure_observation_current(observations, &path)?;
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
    let mut edited = content.clone();
    let mut replacements = 0usize;
    for operation in &operations {
        let matches = edited.matches(&operation.old_text).count();
        if matches == 0 {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("old_text not found in {relative_path}"),
            ));
        }
        if !operation.replace_all && matches > 1 {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!(
                    "old_text matched {matches} times in {relative_path}; set replace_all=true or \
                     provide more context"
                ),
            ));
        }
        edited = if operation.replace_all {
            edited.replace(&operation.old_text, &operation.new_text)
        } else {
            edited.replacen(&operation.old_text, &operation.new_text, 1)
        };
        replacements += if operation.replace_all { matches } else { 1 };
    }
    enforce_content_limit(&edited)?;
    write_file_atomic(&path, edited.as_bytes()).map_err(io_error)?;
    remember_observation(observations, &path)?;
    Ok(HostWorkspaceEditOutput {
        path: relative_path.to_owned(),
        operation_count: operations.len(),
        replacements,
        change: summarize_text_change(
            relative_path,
            Some(content.len() as u64),
            Some(&content),
            &edited,
        ),
    })
}

fn summarize_text_change(
    path: &str,
    old_bytes: Option<u64>,
    old_content: Option<&str>,
    new_content: &str,
) -> HostWorkspaceTextChange {
    let Some(old_content) = old_content else {
        return HostWorkspaceTextChange {
            old_bytes,
            new_bytes: new_content.len() as u64,
            unified_diff: None,
            insertions: 0,
            deletions: 0,
            diff_truncated: false,
        };
    };

    let diff = TextDiff::from_lines(old_content, new_content);
    let mut insertions = 0;
    let mut deletions = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => insertions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {},
        }
    }
    let unified_diff = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    let (unified_diff, diff_truncated) = bounded_diff(unified_diff);

    HostWorkspaceTextChange {
        old_bytes,
        new_bytes: new_content.len() as u64,
        unified_diff: (!unified_diff.is_empty()).then_some(unified_diff),
        insertions,
        deletions,
        diff_truncated,
    }
}

fn bounded_diff(mut diff: String) -> (String, bool) {
    if diff.len() <= HOST_WORKSPACE_MAX_DIFF_BYTES {
        return (diff, false);
    }

    const SUFFIX: &str = "\n... (diff truncated)\n";
    let mut prefix_bytes = HOST_WORKSPACE_MAX_DIFF_BYTES - SUFFIX.len();
    while !diff.is_char_boundary(prefix_bytes) {
        prefix_bytes -= 1;
    }
    diff.truncate(prefix_bytes);
    diff.push_str(SUFFIX);
    (diff, true)
}

fn normalize_edits(
    request: &HostWorkspaceEditRequest,
) -> Result<Vec<HostWorkspaceTextEdit>, ErrorPayload> {
    let has_top_level = request.old_text.is_some() || request.new_text.is_some();
    if has_top_level && !request.edits.is_empty() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "use either old_text/new_text or edits, not both",
        ));
    }
    let edits = if request.edits.is_empty() {
        vec![HostWorkspaceTextEdit {
            old_text: request.old_text.clone().ok_or_else(|| {
                ErrorPayload::new(WireErrorCode::InvalidInput, "old_text is required")
            })?,
            new_text: request.new_text.clone().ok_or_else(|| {
                ErrorPayload::new(WireErrorCode::InvalidInput, "new_text is required")
            })?,
            replace_all: request.replace_all,
        }]
    } else {
        request.edits.clone()
    };
    if edits.iter().any(|edit| edit.old_text.is_empty()) {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "old_text must not be empty",
        ));
    }
    Ok(edits)
}

pub(super) fn resolve_existing_path(
    root: &str,
    relative_path: &str,
    capability: &str,
) -> Result<PathBuf, ErrorPayload> {
    let canonical_root = canonical_root(root)?;
    reject_symlink_components(&canonical_root, Path::new(relative_path), capability)?;
    canonicalize_workspace_path(root, relative_path)
}

pub(super) fn resolve_write_target(
    root: &str,
    relative_path: &str,
    capability: &str,
    create_dirs: bool,
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
    if parent_created {
        if !create_dirs {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                "parent directory does not exist; set create_dirs=true to create it",
            ));
        }
        std::fs::create_dir_all(&parent).map_err(io_error)?;
    }
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

pub(super) fn reject_sensitive_path(relative_path: &str) -> Result<(), ErrorPayload> {
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

fn traversable_entry(
    root: &Path,
    entry: &DirEntry,
    include_ignored: bool,
    include_hidden: bool,
) -> bool {
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
            || (!include_hidden
                && name
                    .to_str()
                    .is_some_and(|name| name.starts_with('.') && name != "." && name != ".."))
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
    recursive: bool,
    path_filters: &[GlobMatcher],
) -> Result<SearchableFiles, ErrorPayload> {
    if search_root.is_file() {
        let relative = search_root
            .file_name()
            .map(Path::new)
            .unwrap_or(search_root);
        return Ok(SearchableFiles {
            files: path_filters
                .iter()
                .all(|filter| filter.is_match(relative))
                .then(|| search_root.to_path_buf())
                .into_iter()
                .collect(),
            truncated: false,
        });
    }
    let mut files = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    let mut walk = workspace_search_walker(root, search_root, true, true);
    if !recursive {
        walk.max_depth(Some(1));
    }
    for entry in walk.build().skip(1) {
        let entry = entry.map_err(|error| io_error(std::io::Error::other(error)))?;
        scanned += 1;
        if scanned > max_entries {
            truncated = true;
            break;
        }
        let relative = entry
            .path()
            .strip_prefix(search_root)
            .unwrap_or(entry.path());
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && path_filters.iter().all(|filter| filter.is_match(relative))
        {
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

pub(super) fn write_file_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary_path = parent.join(format!(".astrcode-write-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = no_follow_options();
    let mut temporary = options.write(true).create_new(true).open(&temporary_path)?;
    let result = temporary
        .write_all(content)
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.sync_all());
    drop(temporary);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary_path, path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    Ok(())
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

pub(super) fn remember_observation(
    store: Option<&std::sync::Arc<dyn FileObservationStore>>,
    path: &Path,
) -> Result<(), ErrorPayload> {
    if let Some(store) = store {
        store.remember(capture_observation(path)?);
    }
    Ok(())
}

pub(super) fn ensure_observation_current(
    store: Option<&std::sync::Arc<dyn FileObservationStore>>,
    path: &Path,
) -> Result<(), ErrorPayload> {
    let Some(store) = store else {
        return Ok(());
    };
    let key = path.to_string_lossy();
    let Some(previous) = store.load(&key) else {
        return Err(ErrorPayload::new(
            WireErrorCode::StaleFile,
            format!("read {} before editing it", path.display()),
        )
        .with_hint("read the current file content, then retry the edit")
        .with_details(serde_json::json!({
            "path": path.display().to_string(),
            "reason": "not_observed"
        })));
    };
    let current = capture_observation(path)?;
    if observations_match(&previous, &current) {
        Ok(())
    } else {
        Err(ErrorPayload::new(
            WireErrorCode::StaleFile,
            format!(
                "{} changed after the last read; read it again before editing",
                path.display()
            ),
        )
        .with_hint("read the current file content, then retry the edit")
        .with_details(serde_json::json!({
            "path": path.display().to_string(),
            "reason": "changed"
        })))
    }
}

fn capture_observation(path: &Path) -> Result<FileObservation, ErrorPayload> {
    let metadata = std::fs::metadata(path).map_err(io_error)?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64);
    let mut file = std::fs::File::open(path).map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(FileObservation {
        path: path.to_string_lossy().into_owned(),
        bytes: metadata.len(),
        modified_unix_nanos,
        content_fingerprint: format!("{:x}", hasher.finalize()),
    })
}

fn observations_match(left: &FileObservation, right: &FileObservation) -> bool {
    left.path == right.path
        && left.bytes == right.bytes
        && left.modified_unix_nanos == right.modified_unix_nanos
        && left.content_fingerprint == right.content_fingerprint
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn glob_request(pattern: &str) -> HostWorkspaceGlobRequest {
        HostWorkspaceGlobRequest {
            pattern: pattern.into(),
            root: None,
            offset: 0,
            max_matches: None,
            respect_gitignore: true,
            include_hidden: true,
            include_directories: true,
        }
    }

    fn grep_request(pattern: &str, path: Option<&str>) -> HostWorkspaceGrepRequest {
        HostWorkspaceGrepRequest {
            pattern: pattern.into(),
            path: path.map(str::to_owned),
            offset: 0,
            max_matches: None,
            max_bytes: None,
            max_line_chars: None,
            recursive: true,
            multiline: false,
            path_filters: Vec::new(),
            before_context: 0,
            after_context: 0,
            mode: HostWorkspaceGrepMode::Content,
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
                content: "old value\n".into(),
                create_dirs: true,
            },
            HostOperation::WorkspaceWrite.wire_name(),
            None,
        )
        .expect("write nested file");
        assert!(written.created);
        assert_eq!(written.change.old_bytes, None);
        assert_eq!(written.change.new_bytes, 10);
        assert_eq!(written.change.unified_diff, None);

        let overwritten = write(
            root,
            HostWorkspaceWriteRequest {
                path: "src/example.txt".into(),
                content: "new value\n".into(),
                create_dirs: false,
            },
            HostOperation::WorkspaceWrite.wire_name(),
            None,
        )
        .expect("overwrite file");
        assert!(!overwritten.created);
        assert_eq!(overwritten.change.old_bytes, Some(10));
        assert_eq!(overwritten.change.insertions, 1);
        assert_eq!(overwritten.change.deletions, 1);
        assert!(!overwritten.change.diff_truncated);
        assert!(
            overwritten
                .change
                .unified_diff
                .as_deref()
                .is_some_and(|diff| diff.contains("-old value") && diff.contains("+new value"))
        );

        let edited = edit(
            root,
            HostWorkspaceEditRequest {
                path: "src/example.txt".into(),
                old_text: Some("new".into()),
                new_text: Some("final".into()),
                replace_all: false,
                edits: Vec::new(),
            },
            HostOperation::WorkspaceEdit.wire_name(),
            None,
        )
        .expect("edit file");
        assert_eq!(edited.replacements, 1);
        assert_eq!(edited.change.old_bytes, Some(10));
        assert_eq!(edited.change.new_bytes, 12);
        assert_eq!(edited.change.insertions, 1);
        assert_eq!(edited.change.deletions, 1);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/example.txt"))
                .expect("read edited file"),
            "final value\n"
        );

        let old = "old line\n".repeat(20_000);
        let new = "new line\n".repeat(20_000);
        let bounded = summarize_text_change("large.txt", Some(old.len() as u64), Some(&old), &new);
        assert!(bounded.diff_truncated);
        assert_eq!(
            bounded.unified_diff.as_deref().map(str::len),
            Some(HOST_WORKSPACE_MAX_DIFF_BYTES)
        );
        assert_eq!(bounded.insertions, 20_000);
        assert_eq!(bounded.deletions, 20_000);
    }

    #[test]
    fn workspace_read_preserves_text_image_and_binary_semantics() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().to_str().expect("utf-8 workspace");
        std::fs::write(workspace.path().join("note.txt"), "hello").expect("write text");
        std::fs::write(workspace.path().join("pixel.png"), [0x89, b'P', b'N', b'G'])
            .expect("write image");
        std::fs::write(workspace.path().join("data.bin"), [0xff, 0x00]).expect("write binary");

        let read_file = |path: &str| {
            read(
                root,
                HostWorkspaceReadRequest {
                    path: path.into(),
                    max_bytes: None,
                    line_offset: 0,
                    line_limit: None,
                },
                HostOperation::WorkspaceRead.wire_name(),
                None,
            )
            .expect("read workspace file")
        };

        assert_eq!(
            read_file("note.txt"),
            HostWorkspaceReadOutput::Text {
                content: "hello".into(),
                bytes: 5,
                total_lines: 1,
                line_offset: 0,
                returned_lines: 1,
                has_more_lines: false,
            }
        );
        assert!(matches!(
            read_file("pixel.png"),
            HostWorkspaceReadOutput::Image {
                media_type,
                data_base64,
                bytes: 4,
            } if media_type == "image/png" && data_base64 == "iVBORw=="
        ));
        assert_eq!(
            read_file("data.bin"),
            HostWorkspaceReadOutput::Binary { bytes: 2 }
        );

        assert_eq!(
            bounded_text_lines("aa\nbb\ncc", 1, None, 2).expect("bounded text page"),
            ("bb".into(), 3, 1, 1, true)
        );
        assert_eq!(
            bounded_text_lines("aa\nbb\ncc", 1, Some(2), 5).expect("explicit line page"),
            ("bb\ncc".into(), 3, 1, 2, false)
        );
        assert!(bounded_text_lines("oversized", 0, None, 4).is_err());
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
                create_dirs: false,
            },
            HostOperation::WorkspaceWrite.wire_name(),
            None,
        )
        .expect_err("parent traversal must fail");
        assert_eq!(escape.code_enum(), Some(WireErrorCode::PermissionDenied));

        let sensitive = write(
            root,
            HostWorkspaceWriteRequest {
                path: ".env".into(),
                content: "SECRET=x".into(),
                create_dirs: false,
            },
            HostOperation::WorkspaceWrite.wire_name(),
            None,
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
                line_offset: 0,
                line_limit: None,
            },
            HostOperation::WorkspaceRead.wire_name(),
            None,
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
                line_offset: 0,
                line_limit: None,
            },
            HostOperation::WorkspaceRead.wire_name(),
            None,
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
                create_dirs: true,
            },
            HostOperation::WorkspaceWrite.wire_name(),
            None,
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

        let result = searchable_files_with_limit(workspace.path(), workspace.path(), 2, true, &[])
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
        std::fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n")
            .expect("write second source");
        std::fs::write(
            workspace.path().join("src/multiline.txt"),
            "before\nalpha()\nbeta()\nafter\n",
        )
        .expect("write multiline source");
        std::fs::create_dir(workspace.path().join(".hidden")).expect("create hidden directory");
        std::fs::write(workspace.path().join(".hidden/note.secret"), "alpha")
            .expect("write hidden file");
        std::fs::write(workspace.path().join(".env"), "TOKEN=secret").expect("write secret");
        std::fs::write(workspace.path().join(".gitignore"), "ignored.log\n")
            .expect("write gitignore");
        std::fs::write(workspace.path().join("ignored.log"), "alpha").expect("write ignored file");

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
            grep_request("fn (alpha|beta)", Some("src")),
            HostOperation::WorkspaceGrep.wire_name(),
        )
        .expect("grep workspace");
        assert_eq!(matches.entries.len(), 2);

        let grep_page = grep(
            root,
            HostWorkspaceGrepRequest {
                offset: 1,
                max_matches: Some(1),
                ..grep_request("fn", Some("src"))
            },
            HostOperation::WorkspaceGrep.wire_name(),
        )
        .expect("page grep results");
        assert!(matches!(
            &grep_page.entries[0],
            HostWorkspaceGrepEntry::Content { line, .. } if line == "fn beta() {}"
        ));
        assert!(grep_page.has_more);
        assert!(!grep_page.scan_truncated);

        let filtered_files = grep(
            root,
            HostWorkspaceGrepRequest {
                pattern: "fn".into(),
                path_filters: vec!["**/main.rs".into()],
                mode: HostWorkspaceGrepMode::FilesWithMatches,
                ..grep_request("fn", Some("src"))
            },
            HostOperation::WorkspaceGrep.wire_name(),
        )
        .expect("grep files with matches");
        assert_eq!(
            filtered_files.entries,
            [HostWorkspaceGrepEntry::File {
                path: "src/main.rs".into()
            }]
        );

        let multiline = grep(
            root,
            HostWorkspaceGrepRequest {
                pattern: "alpha\\(\\).*beta".into(),
                path_filters: vec!["**/multiline.txt".into()],
                multiline: true,
                before_context: 1,
                after_context: 1,
                ..grep_request("unused", Some("src"))
            },
            HostOperation::WorkspaceGrep.wire_name(),
        )
        .expect("multiline grep with context");
        assert!(matches!(
            &multiline.entries[0],
            HostWorkspaceGrepEntry::Content {
                line_number: 2,
                line,
                before_context,
                after_context,
                ..
            } if line == "alpha()\nbeta()"
                && before_context[0].line == "before"
                && after_context[0].line == "after"
        ));

        let first_path = glob(
            root,
            HostWorkspaceGlobRequest {
                max_matches: Some(1),
                ..glob_request("**/*.rs")
            },
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("glob workspace");
        assert_eq!(first_path.paths, ["src/lib.rs"]);
        assert_eq!(first_path.total_matches, Some(2));
        assert!(first_path.has_more);
        assert!(!first_path.scan_truncated);
        let second_path = glob(
            root,
            HostWorkspaceGlobRequest {
                offset: 1,
                max_matches: Some(1),
                ..glob_request("**/*.rs")
            },
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("page glob results");
        assert_eq!(second_path.paths, ["src/main.rs"]);
        assert!(!second_path.has_more);
        assert!(!second_path.scan_truncated);
        assert_eq!(second_path.total_matches, Some(2));

        let ignored = glob(
            root,
            glob_request("*.log"),
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("glob respects gitignore");
        assert!(ignored.paths.is_empty());
        let included_ignored = glob(
            root,
            HostWorkspaceGlobRequest {
                respect_gitignore: false,
                ..glob_request("*.log")
            },
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("glob can opt out of gitignore");
        assert_eq!(included_ignored.paths, ["ignored.log"]);

        let hidden = glob(
            root,
            HostWorkspaceGlobRequest {
                include_hidden: false,
                ..glob_request("**/*.secret")
            },
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("glob without hidden paths");
        assert!(hidden.paths.is_empty());
        let directory = glob(
            root,
            HostWorkspaceGlobRequest {
                include_directories: true,
                ..glob_request("src")
            },
            HostOperation::WorkspaceGlob.wire_name(),
        )
        .expect("glob directories");
        assert_eq!(directory.paths, ["src/"]);

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
