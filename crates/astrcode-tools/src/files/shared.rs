use std::{
    collections::BTreeMap,
    hash::Hasher,
    io::Read as IoRead,
    path::{Path, PathBuf},
    time::Instant,
};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::{is_path_within, resolve_path};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder};
use serde_json::Value;

pub(super) const DEFAULT_MAX_CHARS: usize = 20_000;
pub(super) const MAX_INLINE_IMAGE_BASE64_BYTES: u64 = 1024 * 1024;

const IMAGE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("bmp", "image/bmp"),
];

pub(super) struct TextSlice {
    pub(super) text: String,
    pub(super) returned_chars: usize,
    pub(super) next_char_offset: Option<usize>,
    pub(super) has_more: bool,
}
// ─── Shared ──────────────────────────────────────────────────────────────

pub(super) struct FileCollectOptions {
    pub(super) recursive: bool,
    pub(super) include_hidden: bool,
    pub(super) respect_gitignore: bool,
    pub(super) skip_vcs_dirs: bool,
    pub(super) skip_build_output: bool,
}

const VCS_DIR_NAMES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

const BUILD_OUTPUT_DIR_NAMES: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    ".gradle",
    ".dart_tool",
    "Pods",
    ".swiftbuild",
];

fn path_contains_component(path: &Path, names: &[&str]) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| names.contains(&name))
    })
}

pub(super) fn collect_candidate_files(
    working_dir: &Path,
    root: &Path,
    pattern: Option<&str>,
    options: FileCollectOptions,
) -> std::io::Result<Vec<(String, std::time::SystemTime)>> {
    let globset = pattern.map(build_globset).transpose()?;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!options.include_hidden)
        .git_ignore(options.respect_gitignore)
        .git_exclude(options.respect_gitignore)
        .git_global(options.respect_gitignore)
        .ignore(options.respect_gitignore)
        .parents(options.respect_gitignore)
        .require_git(false);
    if !options.recursive {
        builder.max_depth(Some(1));
    }
    let skip_vcs = options.skip_vcs_dirs;
    let skip_build =
        options.skip_build_output && !path_contains_component(root, BUILD_OUTPUT_DIR_NAMES);
    builder.filter_entry(move |entry| !should_skip_entry(entry, skip_vcs, skip_build));

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry.map_err(std::io::Error::other)?;
        let path = entry.path();
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        if let Some(globset) = &globset {
            let rel = path.strip_prefix(root).unwrap_or(path);
            if !globset.is_match(rel) {
                continue;
            }
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        let display_path = path
            .strip_prefix(working_dir)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        files.push((display_path, modified));
    }
    Ok(files)
}

pub(super) fn collect_grep_files(
    working_dir: &Path,
    root: &Path,
    glob: Option<&str>,
    file_type: Option<&str>,
    recursive: bool,
) -> std::io::Result<Vec<PathBuf>> {
    if root.is_file() {
        let glob_root = root.parent().unwrap_or(root);
        return Ok((matches_file_type_filter(root, file_type)
            && matches_glob_filter(root, glob_root, glob))
        .then(|| root.to_path_buf())
        .into_iter()
        .collect());
    }

    let candidates = collect_candidate_files(
        working_dir,
        root,
        glob,
        FileCollectOptions {
            recursive,
            include_hidden: true,
            respect_gitignore: true,
            skip_vcs_dirs: true,
            skip_build_output: true,
        },
    )?;
    Ok(candidates
        .into_iter()
        .map(|(path, _)| resolve_path(working_dir, Path::new(&path)))
        .filter(|path| matches_file_type_filter(path, file_type))
        .collect())
}

fn should_skip_entry(entry: &DirEntry, skip_vcs: bool, skip_build: bool) -> bool {
    let Some(file_type) = entry.file_type() else {
        return false;
    };
    if !file_type.is_dir() {
        return false;
    }
    let Some(name) = entry.file_name().to_str() else {
        return false;
    };
    if skip_vcs && VCS_DIR_NAMES.contains(&name) {
        return true;
    }
    if skip_build && BUILD_OUTPUT_DIR_NAMES.contains(&name) {
        return true;
    }
    false
}

fn build_globset(pattern: &str) -> std::io::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    builder.add(glob);
    builder
        .build()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

fn matches_glob_filter(path: &Path, root: &Path, glob: Option<&str>) -> bool {
    let Some(glob) = glob else {
        return true;
    };
    let Ok(globset) = build_globset(glob) else {
        return false;
    };
    let rel = path.strip_prefix(root).unwrap_or(path);
    globset.is_match(rel)
}

fn matches_file_type_filter(path: &Path, file_type: Option<&str>) -> bool {
    file_type.is_none_or(|file_type| matches_file_type(path, file_type))
}

fn matches_file_type(path: &Path, file_type: &str) -> bool {
    let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    match file_type {
        "rust" | "rs" => ext == "rs",
        "typescript" | "ts" => matches!(ext, "ts" | "tsx"),
        "javascript" | "js" => matches!(ext, "js" | "jsx" | "mjs" | "cjs"),
        "json" => ext == "json",
        "toml" => ext == "toml",
        "markdown" | "md" => ext == "md",
        other => ext == other.trim_start_matches('.'),
    }
}

pub(super) fn image_media_type(path: &Path) -> Option<&'static str> {
    // TODO: Use content sniffing or a MIME/magic-byte table so media detection
    // does not rely only on file extensions.
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_TYPES
        .iter()
        .find_map(|(candidate, media_type)| (*candidate == ext).then_some(*media_type))
}

pub(super) fn read_image_file(
    ctx: &ToolExecutionContext,
    started_at: Instant,
    path: &Path,
    media_type: &str,
) -> Result<ToolResult, ToolError> {
    let bytes =
        std::fs::read(path).map_err(|error| ToolError::Execution(format!("read: {error}")))?;
    let estimated_base64_bytes = (bytes.len() as u64).div_ceil(3) * 4;
    let mut metadata = BTreeMap::from([
        ("path".into(), serde_json::json!(path.display().to_string())),
        ("bytes".into(), serde_json::json!(bytes.len())),
        ("fileType".into(), serde_json::json!("image")),
        ("mediaType".into(), serde_json::json!(media_type)),
    ]);
    if estimated_base64_bytes > MAX_INLINE_IMAGE_BASE64_BYTES {
        metadata.insert(
            "estimatedBase64Bytes".into(),
            serde_json::json!(estimated_base64_bytes),
        );
        metadata.insert(
            "maxBase64Bytes".into(),
            serde_json::json!(MAX_INLINE_IMAGE_BASE64_BYTES),
        );
        return Ok(error_result(
            ctx,
            started_at,
            format!(
                "image payload would expand to about {estimated_base64_bytes} bytes after base64 \
                 encoding, exceeding the inline limit of {MAX_INLINE_IMAGE_BASE64_BYTES} bytes"
            ),
            metadata,
        ));
    }

    let content = serde_json::json!({
        "type": "image",
        "mediaType": media_type,
        "data": BASE64.encode(bytes),
    })
    .to_string();
    Ok(ToolResult {
        call_id: tool_call_id(ctx),
        content,
        is_error: false,
        error: None,
        metadata,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    })
}

/// Resolve `raw` relative to `working_dir` and verify it stays within the sandbox.
///
/// Returns the resolved absolute path on success, or a `ToolResult` error for
/// path-traversal violations. Callers typically match on the result:
///
/// ```ignore
/// let path = resolve_sandboxed_path(&self.working_dir, &args.path, ctx, started_at);
/// let Ok(path) = path else { return Ok(path.unwrap_err()) };
/// ```
pub(super) fn resolve_sandboxed_path(
    working_dir: &Path,
    raw: &Path,
    ctx: &ToolExecutionContext,
    started_at: Instant,
) -> Result<PathBuf, ToolResult> {
    let path = resolve_path(working_dir, raw);
    if !is_path_within(&path, working_dir) {
        return Err(error_result(
            ctx,
            started_at,
            format!("path escapes working directory: {}", path.display()),
            BTreeMap::from([
                ("path".into(), serde_json::json!(path.display().to_string())),
                ("pathEscapesWorkingDir".into(), serde_json::json!(true)),
            ]),
        ));
    }
    Ok(path)
}

pub(crate) fn tool_call_id(ctx: &ToolExecutionContext) -> String {
    ctx.tool_call_id.clone().unwrap_or_default()
}

pub(super) fn error_result(
    ctx: &ToolExecutionContext,
    started_at: Instant,
    error: String,
    metadata: BTreeMap<String, Value>,
) -> ToolResult {
    ToolResult {
        call_id: tool_call_id(ctx),
        content: error.clone(),
        is_error: true,
        error: Some(error),
        metadata,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

/// 检查路径是否为 UNC 路径（`\\server\share` 或 `//server/share`）。
pub(super) fn is_unc_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    path.starts_with("\\\\") || path.starts_with("//")
}

/// 通过检测前 8KB 中是否包含 NULL 字节来判断文件是否为二进制文件。
pub(super) fn is_binary(p: &Path) -> bool {
    // TODO: Replace the simple NUL-byte heuristic with shared MIME/magic-byte
    // detection that can classify non-text files without embedded NUL bytes.
    std::fs::read(p)
        .map(|d| d.iter().take(8192).any(|&b| b == 0))
        .unwrap_or(false)
}

/// 按字符偏移和最大字符数截取字符串，超出时追加截断标记。
pub(super) fn slice_chars(s: &str, char_offset: usize, max_chars: usize) -> TextSlice {
    let mut iter = s.chars().skip(char_offset);
    let mut out: String = iter.by_ref().take(max_chars).collect();
    let returned_chars = out.chars().count();
    let has_more = iter.next().is_some();
    if has_more {
        out.push_str("\n... [truncated]");
    }
    TextSlice {
        text: out,
        returned_chars,
        next_char_offset: has_more.then_some(char_offset.saturating_add(returned_chars)),
        has_more,
    }
}

/// 在 haystack 中查找 needle 的唯一出现位置。
///
/// 如果出现多次则返回错误（编辑不安全），未找到则返回 `Ok(None)`。
/// 逐 UTF-8 标量前进以正确处理重叠匹配。
pub(super) fn find_unique_occurrence(
    haystack: &str,
    needle: &str,
) -> Result<Option<usize>, ToolError> {
    let mut first_match = None;
    let mut offset = 0usize;

    while let Some(relative_pos) = haystack[offset..].find(needle) {
        let absolute_pos = offset + relative_pos;
        if first_match.replace(absolute_pos).is_some() {
            return Err(ToolError::Execution(
                "oldStr appears multiple times, must be unique to edit safely".into(),
            ));
        }

        // 逐 UTF-8 标量前进，避免漏掉重叠匹配，例如 "ababa" 中的 "aba"。
        let step = haystack[absolute_pos..]
            .chars()
            .next()
            .map_or(1, |c| c.len_utf8());
        offset = absolute_pos + step;
    }

    Ok(first_match)
}

/// 构造"文件未找到"的工具返回结果。
pub(super) fn not_found(ctx: &ToolExecutionContext, started_at: Instant, p: &Path) -> ToolResult {
    ToolResult {
        call_id: tool_call_id(ctx),
        content: format!("Not found: {}", p.display()),
        is_error: false,
        error: None,
        metadata: BTreeMap::from([
            ("path".into(), serde_json::json!(p.display().to_string())),
            ("notFound".into(), serde_json::json!(true)),
        ]),
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

/// 构造"路径是目录"的工具返回结果。
pub(super) fn directory(ctx: &ToolExecutionContext, started_at: Instant, p: &Path) -> ToolResult {
    ToolResult {
        call_id: tool_call_id(ctx),
        content: format!("Is a directory: {} — use find or shell ls", p.display()),
        is_error: false,
        error: None,
        metadata: BTreeMap::from([
            ("path".into(), serde_json::json!(p.display().to_string())),
            ("directory".into(), serde_json::json!(true)),
        ]),
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

/// 构造"二进制文件"的工具返回结果。
pub(super) fn binary(ctx: &ToolExecutionContext, started_at: Instant, p: &Path) -> ToolResult {
    ToolResult {
        call_id: tool_call_id(ctx),
        content: format!("Binary file: {}", p.display()),
        is_error: false,
        error: None,
        metadata: BTreeMap::from([
            ("path".into(), serde_json::json!(p.display().to_string())),
            ("binary".into(), serde_json::json!(true)),
        ]),
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

/// 截断字符串到最大长度，在 UTF-8 边界处安全截断并添加省略号。
pub(super) fn trunc(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.into();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// 将中文引号（""''）替换为 ASCII 引号，修正 LLM 可能产生的引号问题。
pub(super) fn clean_quotes(s: &str) -> String {
    s.replace(['\u{201C}', '\u{201D}'], "\"")
        .replace(['\u{2018}', '\u{2019}'], "'")
}

// ─── File observation helpers ──────────────────────────────────────────────

const FILE_OBSERVATION_HASH_BUFFER_BYTES: usize = 16 * 1024;

/// 读取文件并计算观察快照（大小 + 修改时间 + 内容指纹）。
pub(super) fn capture_file_observation(path: &Path) -> std::io::Result<FileObservation> {
    let metadata = std::fs::metadata(path)?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64);

    let mut file = std::fs::File::open(path)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0u8; FILE_OBSERVATION_HASH_BUFFER_BYTES];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.write(&buffer[..bytes_read]);
    }

    Ok(FileObservation {
        path: path.to_string_lossy().to_string(),
        bytes: metadata.len(),
        modified_unix_nanos,
        content_fingerprint: format!("{:016x}", hasher.finish()),
    })
}

/// 比较两个观察快照是否一致。
fn observation_matches(previous: &FileObservation, current: &FileObservation) -> bool {
    previous.path == current.path
        && previous.bytes == current.bytes
        && previous.modified_unix_nanos == current.modified_unix_nanos
        && previous.content_fingerprint == current.content_fingerprint
}

/// 记录文件观察快照到 store。
pub(super) fn remember_file_observation(
    ctx: &ToolExecutionContext,
    path: &Path,
) -> std::io::Result<FileObservation> {
    let observation = capture_file_observation(path)?;
    if let Some(store) = &ctx.capabilities.file_observation_store {
        store.remember(observation.clone());
    }
    Ok(observation)
}

/// 检查文件是否在上次观察后被外部修改。
///
/// 如果存在之前的观察记录且当前文件与记录不一致，返回错误提示。
/// 如果没有之前的观察记录，返回 `Ok(None)` 允许编辑继续。
pub(super) fn stale_file_guard_result(
    ctx: &ToolExecutionContext,
    path: &Path,
    started_at: Instant,
) -> Result<Option<ToolResult>, ToolError> {
    let Some(store) = &ctx.capabilities.file_observation_store else {
        return Ok(None);
    };
    let Some(previous) = store.load(&path.to_string_lossy()) else {
        return Ok(None);
    };
    let current =
        capture_file_observation(path).map_err(|e| ToolError::Execution(format!("read: {e}")))?;
    if observation_matches(&previous, &current) {
        return Ok(None);
    }

    Ok(Some(ToolResult {
        call_id: tool_call_id(ctx),
        content: format!(
            "File changed on disk after the last read in this session. Call read on '{}' first, \
             then retry edit.",
            path.display()
        ),
        is_error: true,
        error: Some(format!(
            "file changed on disk after the last read in this session: {}",
            path.display()
        )),
        metadata: BTreeMap::from([
            ("path".into(), serde_json::json!(path.display().to_string())),
            ("staleFile".into(), serde_json::json!(true)),
        ]),
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }))
}

// ─── Diff helpers ──────────────────────────────────────────────────────────

/// 计算两段文本之间的 unified diff 并返回统计信息。
///
/// 返回 `(diff_text, insertions, deletions)`。diff_text 使用 unified format，
/// 限制最大行数避免 metadata 膨胀。
pub(super) fn compute_unified_diff(
    _path: &str,
    old: &str,
    new: &str,
    max_diff_lines: usize,
) -> (String, usize, usize) {
    use similar::TextDiff;

    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();
    let mut insertions = 0usize;
    let mut deletions = 0usize;

    // 使用 unified_diff() 的 Display 输出完整 diff，然后按行截断统计。
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header("a", "b")
        .to_string();

    for (line_count, line) in unified.lines().enumerate() {
        if line_count >= max_diff_lines {
            output.push_str("... (truncated)\n");
            break;
        }
        match line.chars().next() {
            Some('+') if !line.starts_with("+++") => insertions += 1,
            Some('-') if !line.starts_with("---") => deletions += 1,
            _ => {},
        }
        output.push_str(line);
        output.push('\n');
    }

    (output, insertions, deletions)
}
