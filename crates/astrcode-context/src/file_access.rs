//! 文件访问追踪模块。
//!
//! 记录最近访问的文件路径，用于压缩后恢复上下文时
//! 优先重新加载相关文件信息。

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use astrcode_core::{llm::LlmMessage, tool::ToolResult};
use astrcode_support::hostpaths::{is_path_within, resolve_path};

use crate::token_usage::estimate_text_tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRecoveryConfig {
    pub max_recovered_files: usize,
    pub recovery_token_budget: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFileAccess {
    pub path: PathBuf,
    pub line_offset: Option<usize>,
    pub line_limit: Option<usize>,
    pub char_offset: Option<usize>,
    pub char_limit: Option<usize>,
    pub next_line_offset: Option<usize>,
    pub next_char_offset: Option<usize>,
    pub truncated: bool,
}

/// 文件访问追踪器，使用 FIFO 策略在容量满时淘汰最旧的记录。
///
/// 当同一文件被重复访问时，会将其移到最新位置（类似 LRU 策略）。
pub struct FileAccessTracker {
    /// 按访问顺序排列的文件记录队列（最新在尾部）。
    accesses: VecDeque<ReadFileAccess>,
    /// 最大追踪文件数量。
    max_tracked: usize,
}

impl FileAccessTracker {
    /// 创建一个新的文件访问追踪器。
    ///
    /// # 参数
    /// - `max_tracked`：最多追踪的文件数量，超过时淘汰最旧的记录
    pub fn new(max_tracked: usize) -> Self {
        Self {
            accesses: VecDeque::with_capacity(max_tracked),
            max_tracked,
        }
    }

    /// 记录一次文件访问。
    ///
    /// 如果该文件已在追踪列表中，则将其移到最新位置；
    /// 如果追踪列表已满且是新文件，则淘汰最旧的记录。
    pub fn record(&mut self, path: &str) {
        self.record_access(ReadFileAccess {
            path: PathBuf::from(path),
            line_offset: None,
            line_limit: None,
            char_offset: None,
            char_limit: None,
            next_line_offset: None,
            next_char_offset: None,
            truncated: false,
        });
    }

    pub fn record_tool_result(&mut self, tool_name: &str, result: &ToolResult) {
        if tool_name != "readFile" || result.is_error {
            return;
        }
        let Some(access) = ReadFileAccess::from_metadata(&result.metadata) else {
            return;
        };
        self.record_access(access);
    }

    /// 获取所有已追踪的文件路径，按最近访问优先排序。
    pub fn get_tracked(&self) -> Vec<String> {
        self.accesses
            .iter()
            .rev()
            .map(|access| access.path.display().to_string())
            .collect()
    }

    pub fn build_recovery_messages(
        &self,
        working_dir: &Path,
        config: FileRecoveryConfig,
    ) -> Vec<LlmMessage> {
        let mut recovered = Vec::new();
        let mut remaining_tokens = config.recovery_token_budget.max(1);

        for access in self.accesses.iter().rev() {
            if recovered.len() >= config.max_recovered_files.max(1) {
                break;
            }

            let content = render_recovery_message(access, working_dir, remaining_tokens);
            let used_tokens = estimate_text_tokens(&content);
            if used_tokens > remaining_tokens {
                continue;
            }
            remaining_tokens = remaining_tokens.saturating_sub(used_tokens);
            recovered.push(LlmMessage::user(content));
        }

        recovered.reverse();
        recovered
    }

    fn record_access(&mut self, access: ReadFileAccess) {
        self.accesses.retain(|entry| !same_access(entry, &access));
        if self.accesses.len() >= self.max_tracked {
            self.accesses.pop_front();
        }
        self.accesses.push_back(access);
    }
}

impl ReadFileAccess {
    fn from_metadata(
        metadata: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Option<Self> {
        let path = metadata.get("path")?.as_str()?;
        Some(Self {
            path: PathBuf::from(path),
            line_offset: metadata
                .get("offset")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
            line_limit: metadata
                .get("limit")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
            char_offset: metadata
                .get("charOffset")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
            char_limit: metadata
                .get("maxChars")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
            next_line_offset: metadata
                .get("nextOffset")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
            next_char_offset: metadata
                .get("nextCharOffset")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
            truncated: metadata
                .get("truncated")
                .and_then(|value| value.as_bool())
                .or_else(|| metadata.get("hasMore").and_then(|value| value.as_bool()))
                .unwrap_or(false),
        })
    }
}

fn same_access(left: &ReadFileAccess, right: &ReadFileAccess) -> bool {
    left.path == right.path
        && left.line_offset == right.line_offset
        && left.line_limit == right.line_limit
        && left.char_offset == right.char_offset
        && left.char_limit == right.char_limit
}

fn render_recovery_message(
    access: &ReadFileAccess,
    working_dir: &Path,
    budget_tokens: usize,
) -> String {
    let path = if access.path.is_absolute() {
        access.path.clone()
    } else {
        resolve_path(working_dir, &access.path)
    };
    if !is_path_within(&path, working_dir) {
        return format!(
            "Recovered file context after compaction is unavailable.\nPath: {}\nReason: path \
             escapes working directory",
            path.display()
        );
    }

    let raw_text = match std::fs::read_to_string(&path) {
        Ok(text) => slice_text(text, access),
        Err(error) => {
            return format!(
                "Recovered file context after compaction is unavailable.\nPath: {}\nReason: {}",
                path.display(),
                error
            );
        },
    };

    let header = format!(
        "Recovered file context after compaction.\nPath: {}\n{}Content:\n",
        path.display(),
        format_range(access)
    );
    let available_body_tokens = budget_tokens
        .saturating_sub(estimate_text_tokens(&header))
        .max(32);
    let body = truncate_to_token_budget(&raw_text, available_body_tokens);
    format!("{header}```text\n{body}\n```")
}

fn slice_text(text: String, access: &ReadFileAccess) -> String {
    if access.line_offset.is_some() || access.line_limit.is_some() {
        let lines = text.lines().collect::<Vec<_>>();
        let start = access.line_offset.unwrap_or(0).min(lines.len());
        let end = access
            .line_limit
            .map(|limit| start.saturating_add(limit).min(lines.len()))
            .unwrap_or(lines.len());
        return lines[start..end].join("\n");
    }

    let start = access.char_offset.unwrap_or(0);
    let limit = access.char_limit.unwrap_or(usize::MAX);
    text.chars().skip(start).take(limit).collect()
}

fn format_range(access: &ReadFileAccess) -> String {
    match (access.line_offset, access.line_limit) {
        (Some(offset), Some(limit)) => format!("Line range: {}-{}\n", offset + 1, offset + limit),
        (Some(offset), None) => format!("Line start: {}\n", offset + 1),
        _ if access.char_offset.is_some() || access.char_limit.is_some() => format!(
            "Char range: {}+{}\n",
            access.char_offset.unwrap_or(0),
            access.char_limit.unwrap_or(usize::MAX)
        ),
        _ => String::new(),
    }
}

fn truncate_to_token_budget(text: &str, budget_tokens: usize) -> String {
    let target_chars = budget_tokens.saturating_mul(4).max(64);
    if text.chars().count() <= target_chars {
        return text.to_string();
    }
    let mut end = 0usize;
    for (index, _) in text.char_indices().take(target_chars) {
        end = index;
    }
    if end == 0 {
        return text.chars().take(target_chars).collect();
    }
    format!(
        "{}\n[truncated after compaction recovery budget]",
        &text[..end]
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::SystemTime};

    use astrcode_core::{llm::LlmContent, tool::ToolResult};

    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("astrcode-context-{name}-{stamp}"));
            std::fs::create_dir_all(&path).expect("temp dir should be created");
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn read_file_metadata_preserves_line_range_for_recovery() {
        let temp = TestDir::new("file-access-range");
        std::fs::write(temp.path.join("notes.txt"), "alpha\nbeta\ngamma\ndelta\n")
            .expect("seed file");
        let mut metadata = BTreeMap::new();
        metadata.insert("path".into(), serde_json::json!("notes.txt"));
        metadata.insert("offset".into(), serde_json::json!(1));
        metadata.insert("limit".into(), serde_json::json!(2));
        metadata.insert("nextOffset".into(), serde_json::json!(3));
        metadata.insert("truncated".into(), serde_json::json!(true));

        let mut tracker = FileAccessTracker::new(4);
        tracker.record_tool_result(
            "readFile",
            &ToolResult {
                call_id: String::new(),
                content: String::new(),
                is_error: false,
                error: None,
                metadata,
                duration_ms: None,
            },
        );

        let messages = tracker.build_recovery_messages(
            &temp.path,
            FileRecoveryConfig {
                max_recovered_files: 1,
                recovery_token_budget: 256,
            },
        );

        assert_eq!(messages.len(), 1);
        let LlmContent::Text { text } = &messages[0].content[0] else {
            panic!("recovery message should be text");
        };
        assert!(text.contains("Line range: 2-3"));
        assert!(text.contains("beta\ngamma"));
        assert!(!text.contains("alpha"));
    }
}
