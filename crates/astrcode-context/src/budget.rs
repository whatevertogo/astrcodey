//! 工具结果预算管理模块。
//!
//! 控制工具调用结果在上下文窗口中的显示大小，
//! 防止单个或累计的工具输出占用过多 token 空间。

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use astrcode_core::llm::{LlmContent, LlmMessage};
use astrcode_support::tool_results::persist_tool_result;

/// 工具结果预算管理器。
///
/// 通过三层限制来控制工具结果对上下文窗口的消耗：
/// - `inline_limit`：单条结果的内联显示上限（字节）
/// - `preview_limit`：截断预览的长度上限（字节）
/// - `aggregate_limit`：单轮所有工具结果的累计上限（字节）
pub struct ToolResultBudget {
    /// 单条工具结果的内联显示字节数上限。
    inline_limit: usize,
    /// 预览截断的字节数上限。
    preview_limit: usize,
    /// 单轮所有工具结果的累计字节数上限。
    aggregate_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultReplacementRecord {
    pub tool_call_id: String,
    pub persisted_path: String,
    pub replacement: String,
    pub original_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ToolResultReplacementState {
    replacements: HashMap<String, ToolResultReplacementRecord>,
    frozen: HashSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolResultBudgetStats {
    pub replacement_count: usize,
    pub reapply_count: usize,
    pub bytes_saved: usize,
    pub over_budget_batch_count: usize,
}

#[derive(Debug, Clone)]
pub struct ToolResultBudgetOutcome {
    pub messages: Vec<LlmMessage>,
    pub stats: ToolResultBudgetStats,
}

impl ToolResultBudget {
    /// 创建一个新的预算管理器。
    ///
    /// # 参数
    /// - `inline_limit`：单条结果内联显示的字节上限
    /// - `preview_limit`：预览截断的字节上限
    /// - `aggregate_limit`：单轮累计结果的字节上限
    pub fn new(inline_limit: usize, preview_limit: usize, aggregate_limit: usize) -> Self {
        Self {
            inline_limit,
            preview_limit,
            aggregate_limit,
        }
    }

    /// 检查工具结果内容是否超过内联显示上限。
    pub fn exceeds_inline(&self, content: &str) -> bool {
        content.len() > self.inline_limit
    }

    /// 返回单轮所有工具结果的累计字节数上限。
    pub fn aggregate_limit(&self) -> usize {
        self.aggregate_limit
    }

    /// 检查累计字节数是否超过总量上限。
    pub fn exceeds_aggregate(&self, total_bytes: usize) -> bool {
        total_bytes > self.aggregate_limit
    }

    /// 为超长内容生成截断预览。
    ///
    /// 如果内容未超过预览上限则原样返回；
    /// 否则在安全的 UTF-8 字符边界处截断并追加 `... (truncated)` 标记。
    pub fn preview(&self, content: &str) -> String {
        if content.len() <= self.preview_limit {
            content.to_string()
        } else {
            // 在字符边界处截断，避免拆分多字节 UTF-8 字符
            let cutoff = crate::floor_char_boundary(content, self.preview_limit);
            format!("{}... (truncated)", &content[..cutoff])
        }
    }
}

impl ToolResultReplacementState {
    pub fn seed(records: impl IntoIterator<Item = ToolResultReplacementRecord>) -> Self {
        let mut state = Self::default();
        for record in records {
            state
                .replacements
                .insert(record.tool_call_id.clone(), record);
        }
        state
    }

    pub fn records(&self) -> impl Iterator<Item = &ToolResultReplacementRecord> {
        self.replacements.values()
    }

    fn replacement_for(&self, tool_call_id: &str) -> Option<&ToolResultReplacementRecord> {
        self.replacements.get(tool_call_id)
    }

    fn is_frozen(&self, tool_call_id: &str) -> bool {
        self.frozen.contains(tool_call_id)
    }

    fn record_replacement(&mut self, record: ToolResultReplacementRecord) {
        self.frozen.remove(&record.tool_call_id);
        self.replacements
            .insert(record.tool_call_id.clone(), record);
    }

    fn freeze(&mut self, tool_call_id: String) {
        self.frozen.insert(tool_call_id);
    }
}

pub fn apply_tool_result_budget(
    messages: &[LlmMessage],
    state: &mut ToolResultReplacementState,
    budget: &ToolResultBudget,
    persist_dir: Option<&Path>,
) -> ToolResultBudgetOutcome {
    let mut messages = messages.to_vec();
    let mut stats = ToolResultBudgetStats::default();
    let Some(batch_start) = trailing_tool_batch_start(&messages) else {
        return ToolResultBudgetOutcome { messages, stats };
    };

    let mut total_bytes = tool_result_bytes(&messages[batch_start..]);
    for message in &mut messages[batch_start..] {
        for content in &mut message.content {
            let LlmContent::ToolResult {
                tool_call_id,
                content,
                ..
            } = content
            else {
                continue;
            };
            let Some(record) = state.replacement_for(tool_call_id) else {
                continue;
            };
            if content != &record.replacement {
                total_bytes = total_bytes
                    .saturating_sub(content.len())
                    .saturating_add(record.replacement.len());
                *content = record.replacement.clone();
                stats.reapply_count = stats.reapply_count.saturating_add(1);
            }
        }
    }

    if !budget.exceeds_aggregate(total_bytes) {
        return ToolResultBudgetOutcome { messages, stats };
    }
    stats.over_budget_batch_count = 1;

    let mut fresh_candidates = Vec::new();
    for (offset, message) in messages[batch_start..].iter().enumerate() {
        for content in &message.content {
            let candidate = (|| {
                let LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } = content
                else {
                    return None;
                };
                if state.replacement_for(tool_call_id).is_some()
                    || state.is_frozen(tool_call_id)
                    || is_persisted_output(content)
                {
                    return None;
                }
                Some((batch_start + offset, tool_call_id.clone(), content.len()))
            })();
            if let Some(candidate) = candidate {
                fresh_candidates.push(candidate);
            }
        }
    }
    fresh_candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.2));

    let mut replaced = HashSet::new();
    for (message_index, tool_call_id, original_len) in fresh_candidates {
        if !budget.exceeds_aggregate(total_bytes) {
            break;
        }

        let Some(content) = tool_result_content_mut(&mut messages[message_index], &tool_call_id)
        else {
            continue;
        };

        let replacement = if let Some(persist_dir) = persist_dir {
            match persist_tool_result(content, &tool_call_id, &persist_dir.to_path_buf()) {
                Ok(path) => format_persisted_output(path.display().to_string(), original_len),
                Err(error) => {
                    tracing::warn!(%tool_call_id, %error, "failed to persist large tool result");
                    budget.preview(content)
                },
            }
        } else {
            budget.preview(content)
        };

        let saved_bytes = original_len.saturating_sub(replacement.len());
        let record = ToolResultReplacementRecord {
            tool_call_id: tool_call_id.clone(),
            persisted_path: persisted_path_from_replacement(&replacement).unwrap_or_default(),
            replacement: replacement.clone(),
            original_bytes: original_len,
        };
        *content = replacement.clone();
        state.record_replacement(record);
        total_bytes = total_bytes
            .saturating_sub(original_len)
            .saturating_add(replacement.len());
        stats.replacement_count = stats.replacement_count.saturating_add(1);
        stats.bytes_saved = stats.bytes_saved.saturating_add(saved_bytes);
        replaced.insert(tool_call_id);
    }

    for message in &messages[batch_start..] {
        for content in &message.content {
            let LlmContent::ToolResult {
                tool_call_id,
                content,
                ..
            } = content
            else {
                continue;
            };
            if state.replacement_for(tool_call_id).is_none()
                && !replaced.contains(tool_call_id)
                && !is_persisted_output(content)
            {
                state.freeze(tool_call_id.clone());
            }
        }
    }

    ToolResultBudgetOutcome { messages, stats }
}

fn trailing_tool_batch_start(messages: &[LlmMessage]) -> Option<usize> {
    let trailing_tools = messages
        .iter()
        .rev()
        .take_while(|message| message.role == astrcode_core::llm::LlmRole::Tool)
        .count();
    (trailing_tools > 0).then(|| messages.len().saturating_sub(trailing_tools))
}

fn tool_result_bytes(messages: &[LlmMessage]) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .map(|content| match content {
            LlmContent::ToolResult { content, .. } => content.len(),
            _ => 0,
        })
        .sum()
}

fn tool_result_content_mut<'a>(
    message: &'a mut LlmMessage,
    wanted_call_id: &str,
) -> Option<&'a mut String> {
    message
        .content
        .iter_mut()
        .find_map(|content| match content {
            LlmContent::ToolResult {
                tool_call_id,
                content,
                ..
            } if tool_call_id == wanted_call_id => Some(content),
            _ => None,
        })
}

pub fn is_persisted_output(content: &str) -> bool {
    content.contains("<persisted-output>") || content.contains("[Tool result persisted to ")
}

fn format_persisted_output(path: String, original_len: usize) -> String {
    format!(
        "<persisted-output>\nLarge tool output was saved to a file instead of being \
         inlined.\nPath: {path}\nBytes: {original_len}\nRead the file with readFile when exact \
         output is needed.\n</persisted-output>"
    )
}

fn persisted_path_from_replacement(replacement: &str) -> Option<String> {
    replacement
        .lines()
        .find_map(|line| line.strip_prefix("Path: "))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_message(call_id: &str, content: &str) -> LlmMessage {
        LlmMessage::tool("shell", call_id, content, false)
    }

    #[test]
    fn budget_replaces_largest_trailing_tool_result_first() {
        let budget = ToolResultBudget::new(10, 12, 40);
        let mut state = ToolResultReplacementState::default();
        let messages = vec![
            LlmMessage::user("hi"),
            tool_message("small", "short"),
            tool_message("large", &"x".repeat(80)),
        ];

        let outcome = apply_tool_result_budget(&messages, &mut state, &budget, None);

        assert_eq!(outcome.stats.replacement_count, 1);
        assert!(matches!(
            &outcome.messages[2].content[0],
            LlmContent::ToolResult { content, .. } if content.contains("truncated")
        ));
    }
}
