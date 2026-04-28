use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::resolve_path;
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
    pub(super) skip_git_dir: bool,
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
    let skip_git_dir = options.skip_git_dir;
    builder.filter_entry(move |entry| !should_skip_entry(entry, skip_git_dir));

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
            skip_git_dir: true,
        },
    )?;
    Ok(candidates
        .into_iter()
        .map(|(path, _)| resolve_path(working_dir, Path::new(&path)))
        .filter(|path| matches_file_type_filter(path, file_type))
        .collect())
}

fn should_skip_entry(entry: &DirEntry, skip_git_dir: bool) -> bool {
    skip_git_dir
        && entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
        && entry.file_name().to_str() == Some(".git")
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

pub(super) fn tool_call_id(ctx: &ToolExecutionContext) -> String {
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
        content: format!(
            "Is a directory: {} — use findFiles or shell ls",
            p.display()
        ),
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
