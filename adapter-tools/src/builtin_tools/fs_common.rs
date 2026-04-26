//! # 文件系统公共工具
//!
//! 提供所有文件工具共享的基础设施：
//!
//! - **路径解析**: `resolve_path` 将相对路径锚定到工作目录，并允许访问宿主机绝对路径
//! - **取消检查**: `check_cancel` 在长操作的关键节点检查用户取消
//! - **文件 I/O**: `read_utf8_file` / `write_text_file` 统一 UTF-8 读写
//! - **Diff 生成**: `build_text_change_report` 手工实现 unified diff
//! - **JSON 序列化**: `json_output` 统一工具输出的 JSON 编码
//!
//! ## Metadata 约定
//!
//! - 路径字段统一返回绝对路径字符串
//! - `count`/`bytes`/`truncated`/`skipped_files` 在适用时提供
//! - `metadata` 是机器可读的契约；`output` 仅供展示
//! - 结构化机器数据不应嵌入到 `output` 字符串中

use std::{
    collections::BTreeMap,
    fs,
    hash::Hasher,
    io::Read as _,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use astrcode_core::{AstrError, CancelToken, PersistedToolOutput, PersistedToolResult, Result};
use astrcode_runtime_contract::tool::ToolContext;
use astrcode_support::{hostpaths::project_dir, tool_results::maybe_persist_tool_result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 检查取消标记，如果已取消则返回 `AstrError::Cancelled`。
///
/// 在长操作（遍历目录、逐行搜索、大文件读取）的关键节点调用，
/// 确保用户取消能快速响应。
pub fn check_cancel(cancel: &CancelToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(AstrError::Cancelled);
    }
    Ok(())
}

/// 检查路径是否为 UNC 路径（Windows 网络路径）。
///
/// UNC 路径（如 `\\server\share\file.txt`）会触发 SMB 认证，
/// 可能导致 NTLM 凭据泄露到恶意服务器。
///
/// ## 安全风险
///
/// 在 Windows 上，访问 UNC 路径会自动触发 SMB 认证，
/// 如果路径指向恶意服务器（如 `\\evil.com\share\file.txt`），
/// 可能导致 NTLM 凭据泄露。
pub fn is_unc_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    path_str.starts_with("\\\\") || path_str.starts_with("//")
}

/// 检查路径是否为符号链接。
///
/// ## 安全考虑
///
/// 符号链接可能指向工作目录外的敏感文件（如 `/etc/passwd`），
/// 绕过路径沙箱检查。在写入操作前检测符号链接可以防止此类攻击。
///
/// ## 返回值
///
/// - `Ok(true)`: 路径是符号链接
/// - `Ok(false)`: 路径不是符号链接（普通文件/目录/不存在）
/// - `Err(_)`: 无法读取元数据（权限问题等）
pub fn is_symlink(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AstrError::io(
            format!("failed to check if path is symlink: '{}'", path.display()),
            e,
        )),
    }
}

/// 将路径解析为宿主机上的绝对路径。
///
/// 相对路径始终锚定到当前工具上下文的工作目录；绝对路径直接保留并解析。
///
/// **为什么使用 `resolve_for_host_access` 而非 `fs::canonicalize`**:
/// canonicalize 要求路径在磁盘上存在，但 writeFile/editFile 经常操作
/// 尚不存在的文件。resolve_for_host_access 从路径尾部向上找到第一个
/// 存在的祖先进行 canonicalize，再拼回缺失部分。
pub fn resolve_path(ctx: &ToolContext, path: &Path) -> Result<PathBuf> {
    let canonical_working_dir = canonicalize_path(
        ctx.working_dir(),
        &format!(
            "failed to canonicalize working directory '{}'",
            ctx.working_dir().display()
        ),
    )?;
    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        canonical_working_dir.join(path)
    };
    resolve_for_host_access(&normalize_lexically(&base))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReadTarget {
    pub path: PathBuf,
    pub persisted_relative_path: Option<String>,
}

/// 读取工具额外允许访问当前会话下的持久化结果目录。
///
/// `grep` 等工具可能将超大输出写入 `~/.astrcode/projects/<project>/sessions/<id>/tool-results`
/// 供后续 `readFile` 读取。这里仅对 `tool-results/**` 相对路径开放额外根目录，
/// 避免把工作区外的任意文件都暴露给只读工具。
pub fn resolve_read_path(ctx: &ToolContext, path: &Path) -> Result<PathBuf> {
    Ok(resolve_read_target(ctx, path)?.path)
}

pub fn resolve_read_target(ctx: &ToolContext, path: &Path) -> Result<ResolvedReadTarget> {
    if let Some(target) = resolve_session_tool_results_target(ctx, path)? {
        return Ok(target);
    }

    Ok(ResolvedReadTarget {
        path: resolve_path(ctx, path)?,
        persisted_relative_path: None,
    })
}

fn resolve_session_tool_results_target(
    ctx: &ToolContext,
    path: &Path,
) -> Result<Option<ResolvedReadTarget>> {
    let session_root = session_dir_for_tool_results(ctx)?;
    let tool_results_root = session_root.join(TOOL_RESULTS_DIR);

    if path.is_absolute() {
        if !tool_results_root.exists() {
            return Ok(None);
        }

        let canonical_tool_results_root = canonicalize_path(
            &tool_results_root,
            &format!(
                "failed to canonicalize session tool-results directory '{}'",
                tool_results_root.display()
            ),
        )?;
        let canonical_session_root = canonicalize_path(
            &session_root,
            &format!(
                "failed to canonicalize session directory '{}'",
                session_root.display()
            ),
        )?;
        let resolved = resolve_for_host_access(&normalize_lexically(path))?;
        if !is_path_within_root(&resolved, &canonical_tool_results_root) {
            return Ok(None);
        }

        let relative_path = resolved
            .strip_prefix(&canonical_session_root)
            .unwrap_or(&resolved)
            .to_string_lossy()
            .replace('\\', "/");
        return Ok(Some(ResolvedReadTarget {
            path: resolved,
            persisted_relative_path: Some(relative_path),
        }));
    }

    if should_use_session_tool_results_root(path) {
        let session_candidate = session_root.join(path);
        if session_candidate.exists() {
            let resolved = resolve_path_with_root(
                &session_root,
                path,
                "session tool-results directory",
                "failed to canonicalize session tool-results directory",
            )?;
            return Ok(Some(ResolvedReadTarget {
                path: resolved,
                persisted_relative_path: Some(path.to_string_lossy().replace('\\', "/")),
            }));
        }
    }

    Ok(None)
}

/// 读取文件内容为 UTF-8 字符串。
///
/// 文件包含无效 UTF-8 时返回错误。
pub async fn read_utf8_file(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map_err(|e| AstrError::io(format!("failed reading file '{}'", path.display()), e))
}

/// 将文本内容写入文件。
///
/// `create_dirs` 为 true 时自动创建缺失的父目录。
pub async fn write_text_file(path: &Path, content: &str, create_dirs: bool) -> Result<usize> {
    if create_dirs {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                AstrError::io(
                    format!("failed creating parent directory '{}'", parent.display()),
                    e,
                )
            })?;
        }
    }

    fs::write(path, content.as_bytes())
        .map_err(|e| AstrError::io(format!("failed writing file '{}'", path.display()), e))?;

    Ok(content.len())
}

/// 将值序列化为 JSON 字符串，用于工具的结构化输出。
pub fn json_output<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| AstrError::parse("failed to serialize output", e))
}

/// 返回当前会话持久化目录，供大型工具结果落盘使用。
pub fn session_dir_for_tool_results(ctx: &ToolContext) -> Result<PathBuf> {
    if let Some(session_storage_root) = ctx.session_storage_root() {
        return Ok(session_storage_root.join("sessions").join(ctx.session_id()));
    }

    let project_dir = project_dir(ctx.working_dir()).map_err(|e| {
        AstrError::Internal(format!(
            "failed to resolve project directory for '{}': {e}",
            ctx.working_dir().display()
        ))
    })?;
    Ok(project_dir.join("sessions").join(ctx.session_id()))
}

/// 拒绝通用文件写工具直接修改 canonical session plan。
///
/// session plan 的正式写入口必须统一走 `upsertSessionPlan`，否则会让 `state.json`
/// 与 markdown artifact 脱节，并污染 conversation 对 canonical plan 的投影语义。
pub fn ensure_not_canonical_session_plan_write_target(
    ctx: &ToolContext,
    path: &Path,
    tool_name: &str,
) -> Result<()> {
    let plan_dir = resolve_for_host_access(&normalize_lexically(
        &session_dir_for_tool_results(ctx)?.join("plan"),
    ))?;
    if !is_path_within_root(path, &plan_dir) {
        return Ok(());
    }

    let is_canonical_plan_file = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("md"))
        || path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("state.json"));
    if !is_canonical_plan_file {
        return Ok(());
    }

    Err(AstrError::Validation(format!(
        "`{tool_name}` cannot modify canonical session plan artifacts under '{}'; use \
         upsertSessionPlan instead",
        plan_dir.display()
    )))
}

/// 文件观察快照。
///
/// `readFile` 成功后记录当前版本，`editFile` 写入前用它检测文件是否已被外部修改。
/// 这是比“仅靠 oldStr 唯一匹配”更稳的一层乐观并发保护。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileObservation {
    pub path: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_unix_nanos: Option<u64>,
    pub content_fingerprint: String,
}

const TOOL_STATE_DIR: &str = "tool-state";
const FILE_OBSERVATIONS_FILE: &str = "file-observations.json";
const FILE_OBSERVATION_HASH_BUFFER_BYTES: usize = 16 * 1024;

/// 读取并计算文件观察快照。
///
/// 为什么除了 `mtime + size` 还要做内容指纹：
/// 某些编辑器/脚本可能保留时间戳或在极短时间内多次写入，单靠 metadata 容易漏检。
/// 这里追加流式内容哈希，确保“文件被外部改过”能可靠触发 reread 提示。
pub fn capture_file_observation(path: &Path) -> Result<FileObservation> {
    let metadata = fs::metadata(path).map_err(|e| {
        AstrError::io(
            format!("failed reading metadata for '{}'", path.display()),
            e,
        )
    })?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64);

    let mut file = fs::File::open(path)
        .map_err(|e| AstrError::io(format!("failed opening file '{}'", path.display()), e))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0u8; FILE_OBSERVATION_HASH_BUFFER_BYTES];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|e| AstrError::io(format!("failed hashing file '{}'", path.display()), e))?;
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

/// 比较观察快照是否仍代表同一个文件版本。
pub fn file_observation_matches(previous: &FileObservation, current: &FileObservation) -> bool {
    previous.path == current.path
        && previous.bytes == current.bytes
        && previous.modified_unix_nanos == current.modified_unix_nanos
        && previous.content_fingerprint == current.content_fingerprint
}

/// 将文件观察快照持久化到当前会话目录。
pub fn remember_file_observation(ctx: &ToolContext, path: &Path) -> Result<FileObservation> {
    let observation = capture_file_observation(path)?;
    let observations_path = file_observations_path(ctx)?;
    let mut observations = load_file_observation_map(&observations_path)?;
    observations.insert(observation.path.clone(), observation.clone());

    if let Some(parent) = observations_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            AstrError::io(
                format!(
                    "failed creating file observation directory '{}'",
                    parent.display()
                ),
                e,
            )
        })?;
    }

    let encoded = serde_json::to_vec(&observations)
        .map_err(|e| AstrError::parse("failed to serialize file observations", e))?;
    fs::write(&observations_path, encoded).map_err(|e| {
        AstrError::io(
            format!(
                "failed writing file observations '{}'",
                observations_path.display()
            ),
            e,
        )
    })?;

    Ok(observation)
}

/// 读取当前会话中某个文件最后一次被 `readFile`/`editFile` 观察到的版本。
pub fn load_file_observation(ctx: &ToolContext, path: &Path) -> Result<Option<FileObservation>> {
    let observations_path = file_observations_path(ctx)?;
    let observations = load_file_observation_map(&observations_path)?;
    Ok(observations
        .get(&path.to_string_lossy().to_string())
        .cloned())
}

fn file_observations_path(ctx: &ToolContext) -> Result<PathBuf> {
    Ok(session_dir_for_tool_results(ctx)?
        .join(TOOL_STATE_DIR)
        .join(FILE_OBSERVATIONS_FILE))
}

fn load_file_observation_map(path: &Path) -> Result<BTreeMap<String, FileObservation>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }

    let raw = fs::read(path).map_err(|e| {
        AstrError::io(
            format!("failed reading file observations '{}'", path.display()),
            e,
        )
    })?;
    serde_json::from_slice(&raw)
        .map_err(|e| AstrError::parse("failed to parse file observations", e))
}

/// 文本变更报告，由 `build_text_change_report` 生成。
pub struct TextChangeReport {
    pub summary: String,
    pub metadata: Value,
}

/// 构建文本变更报告，包含 unified diff 和变更统计。
pub fn build_text_change_report(
    path: &Path,
    change_type: &'static str,
    before: Option<&str>,
    after: &str,
) -> TextChangeReport {
    let diff = build_unified_diff(path, before.unwrap_or(""), after, before.is_none());
    let summary = if diff.has_changes {
        format!(
            "{change_type} {} (+{} -{})",
            path.display(),
            diff.added_lines,
            diff.removed_lines
        )
    } else {
        format!("{change_type} {} (no content changes)", path.display())
    };

    TextChangeReport {
        summary,
        metadata: json!({
            "path": path.to_string_lossy(),
            "changeType": change_type,
            "diff": {
                "patch": diff.patch,
                "addedLines": diff.added_lines,
                "removedLines": diff.removed_lines,
                "hasChanges": diff.has_changes,
                "truncated": diff.truncated,
            }
        }),
    }
}

struct UnifiedDiffReport {
    patch: String,
    added_lines: usize,
    removed_lines: usize,
    has_changes: bool,
    truncated: bool,
}

/// 简化的 unified diff 生成器（单 hunk）。
///
/// **为什么不使用外部 diff 库**：前端渲染只需要单 hunk 集中展示变更，
/// 不需要标准 diff 的多 hunk 分割。此算法通过前后缀匹配找到变更区域，
/// 加上最多 3 行上下文保证可读性。
fn build_unified_diff(path: &Path, before: &str, after: &str, created: bool) -> UnifiedDiffReport {
    let before_lines = text_lines(before);
    let after_lines = text_lines(after);

    let mut prefix_len = 0usize;
    while prefix_len < before_lines.len()
        && prefix_len < after_lines.len()
        && before_lines[prefix_len] == after_lines[prefix_len]
    {
        prefix_len += 1;
    }

    let mut suffix_len = 0usize;
    while suffix_len < before_lines.len().saturating_sub(prefix_len)
        && suffix_len < after_lines.len().saturating_sub(prefix_len)
        && before_lines[before_lines.len() - 1 - suffix_len]
            == after_lines[after_lines.len() - 1 - suffix_len]
    {
        suffix_len += 1;
    }

    let before_change_end = before_lines.len().saturating_sub(suffix_len);
    let after_change_end = after_lines.len().saturating_sub(suffix_len);
    let has_changes = prefix_len != before_lines.len() || prefix_len != after_lines.len();
    let removed_lines = before_change_end.saturating_sub(prefix_len);
    let added_lines = after_change_end.saturating_sub(prefix_len);

    let display_path = path.display().to_string().replace('\\', "/");
    let before_label = if created {
        "/dev/null".to_string()
    } else {
        format!("a/{display_path}")
    };
    let after_label = format!("b/{display_path}");
    let mut lines = vec![format!("--- {before_label}"), format!("+++ {after_label}")];

    if !has_changes {
        lines.push(format!("@@ -1,0 +1,0 @@ {}", "no changes"));
        return UnifiedDiffReport {
            patch: lines.join("\n"),
            added_lines,
            removed_lines,
            has_changes,
            truncated: false,
        };
    }

    // 变更区域前后各取最多3行上下文，保证diff可读性
    let context_start = prefix_len.saturating_sub(3);
    let before_hunk_end = (before_change_end + 3).min(before_lines.len());
    let after_hunk_end = (after_change_end + 3).min(after_lines.len());
    // hunk header起始行使用1-based：有修改行时加1使上下文区域对齐
    let before_hunk_start = if removed_lines == 0 {
        context_start
    } else {
        context_start + 1
    };
    let after_hunk_start = if added_lines == 0 {
        context_start
    } else {
        context_start + 1
    };

    lines.push(format!(
        "@@ -{},{} +{},{} @@",
        before_hunk_start,
        before_hunk_end.saturating_sub(context_start),
        after_hunk_start,
        after_hunk_end.saturating_sub(context_start)
    ));

    for line in &before_lines[context_start..prefix_len] {
        lines.push(format!(" {}", line));
    }

    for line in &before_lines[prefix_len..before_change_end] {
        lines.push(format!("-{}", line));
    }

    for line in &after_lines[prefix_len..after_change_end] {
        lines.push(format!("+{}", line));
    }

    for line in &before_lines[before_change_end..before_hunk_end] {
        lines.push(format!(" {}", line));
    }

    const MAX_PATCH_LINES: usize = 240;
    let truncated = lines.len() > MAX_PATCH_LINES;
    if truncated {
        lines.truncate(MAX_PATCH_LINES);
        lines.push("... diff truncated ...".to_string());
    }

    UnifiedDiffReport {
        patch: lines.join("\n"),
        added_lines,
        removed_lines,
        has_changes,
        truncated,
    }
}

fn text_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<&str> = text.lines().collect();
    if text.ends_with('\n') {
        lines.push("");
    }
    lines
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => {
                let popped = normalized.pop();
                if !popped {
                    normalized.push(component.as_os_str());
                }
            },
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            },
        }
    }

    normalized
}

fn resolve_path_with_root(
    root: &Path,
    path: &Path,
    root_label: &str,
    canonicalize_context: &str,
) -> Result<PathBuf> {
    let canonical_root = canonicalize_path(
        root,
        &format!("{canonicalize_context} '{}'", root.display()),
    )?;
    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        canonical_root.join(path)
    };

    let resolved = resolve_for_host_access(&normalize_lexically(&base))?;
    if is_path_within_root(&resolved, &canonical_root) {
        return Ok(resolved);
    }

    Err(AstrError::Validation(format!(
        "path '{}' escapes {} '{}'",
        path.display(),
        root_label,
        canonical_root.display()
    )))
}

/// 解析路径到绝对形式，用于沙箱边界检查。
///
/// 当路径尾部组件尚不存在时（如 writeFile 创建新文件），
/// 向上找到第一个存在的祖先 canonicalize 后拼回缺失部分。
fn resolve_for_host_access(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return canonicalize_path(
            path,
            &format!("failed to canonicalize path '{}'", path.display()),
        );
    }

    let mut missing_components = Vec::new();
    let mut current = path;
    while !current.exists() {
        let Some(name) = current.file_name() else {
            return Err(AstrError::Validation(format!(
                "path '{}' cannot be resolved on the host filesystem",
                path.display()
            )));
        };
        let Some(parent) = current.parent() else {
            return Err(AstrError::Validation(format!(
                "path '{}' cannot be resolved on the host filesystem",
                path.display()
            )));
        };
        missing_components.push(name.to_os_string());
        current = parent;
    }

    let mut resolved_parent = canonicalize_path(
        current,
        &format!("failed to canonicalize path '{}'", current.display()),
    )?;
    for component in missing_components.iter().rev() {
        resolved_parent.push(component);
    }

    Ok(normalize_lexically(&resolved_parent))
}

fn canonicalize_path(path: &Path, context: &str) -> Result<PathBuf> {
    fs::canonicalize(path)
        .map(normalize_absolute_path)
        .map_err(|e| AstrError::io(context.to_string(), e))
}

/// 移除 Windows `fs::canonicalize` 返回的 `\\?\` 前缀。
///
/// Windows canonicalize 返回 `\\?\` 开头的路径，移除后更友好。
/// 注意不要在此函数后使用 `starts_with` 做沙箱检查（已改用词法归一化）。
fn normalize_absolute_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rendered) = path.to_str() {
            if let Some(stripped) = rendered.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{}", stripped));
            }
            if let Some(stripped) = rendered.strip_prefix(r"\\?\") {
                return PathBuf::from(stripped);
            }
        }
    }

    path
}

fn is_path_within_root(path: &Path, root: &Path) -> bool {
    let normalized_path = normalize_lexically(path);
    let normalized_root = normalize_lexically(root);
    normalized_path == normalized_root || normalized_path.starts_with(&normalized_root)
}

fn should_use_session_tool_results_root(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }

    path.components()
        .next()
        .is_some_and(|component| match component {
            Component::Normal(part) => part == std::ffi::OsStr::new(TOOL_RESULTS_DIR),
            _ => false,
        })
}

/// 工具输出 inline 阈值：序列化结果超过此字节数时触发存盘。
pub use astrcode_core::tool_result_persist::DEFAULT_TOOL_RESULT_INLINE_LIMIT as TOOL_RESULT_INLINE_LIMIT;
/// 工具结果预览截断大小。
pub use astrcode_core::tool_result_persist::TOOL_RESULT_PREVIEW_LIMIT;
/// 工具结果存盘目录名（相对于 session 目录）。
pub use astrcode_core::tool_result_persist::TOOL_RESULTS_DIR;

/// 将大型工具结果存到磁盘并返回截断预览。
///
/// 委托给 `astrcode_support::tool_results::maybe_persist_tool_result`。
/// `force_inline` 用于调试/测试模式跳过存盘。
pub fn maybe_persist_large_tool_result(
    session_dir: &std::path::Path,
    tool_call_id: &str,
    content: &str,
    force_inline: bool,
) -> PersistedToolResult {
    if force_inline {
        return PersistedToolResult {
            output: content.to_string(),
            persisted: None,
        };
    }
    maybe_persist_tool_result(session_dir, tool_call_id, content, TOOL_RESULT_INLINE_LIMIT)
}

pub fn merge_persisted_tool_output_metadata(
    metadata: &mut serde_json::Map<String, Value>,
    persisted_output: Option<&PersistedToolOutput>,
) {
    let Some(persisted_output) = persisted_output else {
        return;
    };

    metadata.insert("persistedOutput".to_string(), json!(persisted_output));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{canonical_tool_path, test_tool_context_for};

    #[test]
    fn check_cancel_returns_error_for_cancelled_token() {
        let ctx = test_tool_context_for(std::env::temp_dir());
        ctx.cancel().cancel();

        let err = check_cancel(ctx.cancel()).expect_err("cancelled token should fail");
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn resolve_path_rejects_relative_escape_from_working_dir() {
        let parent = tempfile::tempdir().expect("tempdir should be created");
        let working_dir = parent.path().join("workspace");
        fs::create_dir_all(&working_dir).expect("workspace should be created");
        let ctx = test_tool_context_for(&working_dir);

        let resolved =
            resolve_path(&ctx, Path::new("../outside.txt")).expect("outside path should resolve");
        let expected = resolve_path(&ctx, Path::new("../outside.txt"))
            .expect("outside path should resolve consistently");

        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_path_allows_absolute_path_inside_working_dir() {
        let working_dir = tempfile::tempdir().expect("tempdir should be created");
        let file = working_dir.path().join("notes.txt");
        fs::write(&file, "hello").expect("file should be created");
        let ctx = test_tool_context_for(working_dir.path());

        let resolved = resolve_path(&ctx, &file).expect("path should resolve");

        assert_eq!(resolved, canonical_tool_path(&file));
    }

    #[test]
    fn resolve_path_allows_absolute_path_outside_working_dir() {
        let parent = tempfile::tempdir().expect("tempdir should be created");
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside.txt");
        fs::create_dir_all(&workspace).expect("workspace should be created");
        fs::write(&outside, "hello").expect("outside file should be created");
        let ctx = test_tool_context_for(&workspace);

        let resolved = resolve_path(&ctx, &outside).expect("absolute host path should resolve");

        assert_eq!(resolved, canonical_tool_path(&outside));
    }

    #[test]
    fn is_path_within_root_ignores_trailing_separators() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let root = temp.path().join("workspace");
        fs::create_dir_all(root.join("nested")).expect("workspace should be created");
        let root_with_separator =
            PathBuf::from(format!("{}{}", root.display(), std::path::MAIN_SEPARATOR));

        assert!(is_path_within_root(
            &root.join("nested"),
            &root_with_separator
        ));
    }

    #[test]
    fn resolve_read_path_allows_session_tool_results_relative_path() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let ctx = test_tool_context_for(temp.path());
        let session_dir =
            session_dir_for_tool_results(&ctx).expect("session tool-results dir should resolve");
        let persisted = session_dir.join(TOOL_RESULTS_DIR).join("sample.txt");
        fs::create_dir_all(
            persisted
                .parent()
                .expect("tool-results file should have a parent"),
        )
        .expect("tool-results dir should be created");
        fs::write(&persisted, "persisted").expect("persisted output should be written");

        let resolved = resolve_read_path(&ctx, Path::new("tool-results/sample.txt"))
            .expect("read path should resolve to session tool-results");

        assert_eq!(resolved, canonical_tool_path(&persisted));
    }

    #[test]
    fn resolve_read_path_falls_back_to_workspace_tool_results_when_session_copy_missing() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let ctx = test_tool_context_for(temp.path());
        let workspace_file = temp.path().join(TOOL_RESULTS_DIR).join("sample.txt");
        fs::create_dir_all(
            workspace_file
                .parent()
                .expect("workspace tool-results file should have a parent"),
        )
        .expect("workspace tool-results dir should be created");
        fs::write(&workspace_file, "workspace").expect("workspace output should be written");

        let resolved = resolve_read_path(&ctx, Path::new("tool-results/sample.txt"))
            .expect("workspace tool-results path should still resolve");

        assert_eq!(resolved, canonical_tool_path(&workspace_file));
    }

    #[test]
    fn resolve_read_target_allows_absolute_session_tool_results_path() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let ctx = test_tool_context_for(temp.path());
        let session_dir =
            session_dir_for_tool_results(&ctx).expect("session tool-results dir should resolve");
        let persisted = session_dir.join(TOOL_RESULTS_DIR).join("absolute.txt");
        fs::create_dir_all(
            persisted
                .parent()
                .expect("persisted file should have a parent"),
        )
        .expect("tool-results dir should be created");
        fs::write(&persisted, "persisted").expect("persisted output should be written");

        let resolved = resolve_read_target(&ctx, &canonical_tool_path(&persisted))
            .expect("absolute persisted path should resolve");

        assert_eq!(resolved.path, canonical_tool_path(&persisted));
        assert_eq!(
            resolved.persisted_relative_path.as_deref(),
            Some("tool-results/absolute.txt")
        );
    }

    #[test]
    fn session_dir_for_tool_results_prefers_context_override_root() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let ctx = test_tool_context_for(temp.path());

        let session_dir =
            session_dir_for_tool_results(&ctx).expect("session tool-results dir should resolve");

        assert_eq!(
            session_dir,
            temp.path()
                .join(".astrcode-test-state")
                .join("sessions")
                .join("session-test")
        );
    }
}
