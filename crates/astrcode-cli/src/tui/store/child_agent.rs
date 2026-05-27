//! Child agent output tracker.
//!
//! 每个父 agent 的工具调用对应一个 tracker，独立维护"最近 N 次工具完成摘要"。
//! 多个并行 agent 的 tracker 互不干扰。
//!
//! 显示策略：
//! - 子 agent **正在执行**的工具由 status bar 实时反映（`apply_child_session_event` 更新
//!   `app.status_text`），不写 scrollback。
//! - 子 agent **已完成**的工具调用，前 `MAX_VISIBLE_TOOLS` 个写一行 `✓ tool: summary`（错误用
//!   `✗`），更多的只累计 `omitted` 计数。
//! - agent 整体结束时 `flush_on_completion` 写 `… +N more tool use(s)` 折叠提示 （仅当 omitted >
//!   0）和一个分隔空行。

use crate::tui::store::transcript::{MessageRole, ScrollbackEntry};

/// 每个子 agent 在 transcript 里最多展开的工具完成行数。
///
/// 选 5 是经验值：足以表达 "Read → Read → Bash → ..." 的探索路径，
/// 又不至于让单个 agent 占据屏幕过多空间。
pub const MAX_VISIBLE_TOOLS: usize = 5;

#[derive(Debug, Clone, Default)]
pub struct ChildAgentTracker {
    /// 当前正在执行的工具名称（驱动 status bar）。
    pub current_tool: Option<String>,
    /// 已展开显示的工具完成次数。
    visible_completed: usize,
    /// 因超出窗口而被折叠的工具完成次数。
    omitted_completed: usize,
    /// 是否至少写入过一条可见工具输出。
    has_visible_output: bool,
}

impl ChildAgentTracker {
    /// 子 session 工具调用开始：仅更新 `current_tool` 供 status bar 使用。
    pub fn on_tool_started(&mut self, tool_name: &str) {
        self.current_tool = Some(tool_name.to_string());
    }

    /// 子 session 工具调用完成：窗口内写一行摘要，否则累加 omitted。
    pub fn on_tool_completed(
        &mut self,
        tool_name: &str,
        summary: &str,
        is_error: bool,
        scrollback_queue: &mut Vec<ScrollbackEntry>,
    ) {
        self.current_tool = None;
        if self.visible_completed < MAX_VISIBLE_TOOLS {
            self.visible_completed += 1;
            self.has_visible_output = true;
            let mark = if is_error { "✗" } else { "✓" };
            scrollback_queue.push(ScrollbackEntry::StreamText {
                role: MessageRole::Tool,
                text: format!("  {mark} {tool_name}: {summary}"),
            });
        } else {
            self.omitted_completed += 1;
        }
    }

    /// agent 整体结束时调用：写折叠提示和分隔空行。
    pub fn flush_on_completion(&mut self, scrollback_queue: &mut Vec<ScrollbackEntry>) {
        self.current_tool = None;
        if self.omitted_completed > 0 {
            self.has_visible_output = true;
            scrollback_queue.push(ScrollbackEntry::StreamText {
                role: MessageRole::Tool,
                text: format!("  … +{} more tool use(s)", self.omitted_completed),
            });
        }
        if self.has_visible_output {
            scrollback_queue.push(ScrollbackEntry::BlankLine);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_only_updates_current_tool() {
        let mut tracker = ChildAgentTracker::default();
        tracker.on_tool_started("grep");
        assert_eq!(tracker.current_tool.as_deref(), Some("grep"));
    }

    #[test]
    fn within_window_completions_show_in_scrollback() {
        let mut tracker = ChildAgentTracker::default();
        let mut queue = Vec::new();

        tracker.on_tool_completed("grep", "3 matches", false, &mut queue);
        tracker.on_tool_completed("read", "10 lines", false, &mut queue);

        assert_eq!(queue.len(), 2);
        assert!(matches!(
            &queue[0],
            ScrollbackEntry::StreamText { text, .. } if text.contains("✓ grep")
        ));
        assert!(matches!(
            &queue[1],
            ScrollbackEntry::StreamText { text, .. } if text.contains("✓ read")
        ));
    }

    #[test]
    fn beyond_window_completions_are_only_counted() {
        let mut tracker = ChildAgentTracker::default();
        let mut queue = Vec::new();

        for i in 0..(MAX_VISIBLE_TOOLS + 4) {
            tracker.on_tool_completed(&format!("tool{i}"), "done", false, &mut queue);
        }

        assert_eq!(queue.len(), MAX_VISIBLE_TOOLS);
        assert_eq!(tracker.omitted_completed, 4);
    }

    #[test]
    fn flush_emits_overflow_summary() {
        let mut tracker = ChildAgentTracker::default();
        let mut queue = Vec::new();

        for i in 0..(MAX_VISIBLE_TOOLS + 4) {
            tracker.on_tool_completed(&format!("tool{i}"), "done", false, &mut queue);
        }
        tracker.flush_on_completion(&mut queue);

        let summary = queue
            .iter()
            .find_map(|e| match e {
                ScrollbackEntry::StreamText { text, .. } if text.contains("more tool use") => {
                    Some(text.clone())
                },
                _ => None,
            })
            .expect("overflow summary");
        assert!(summary.contains("+4"));
    }

    #[test]
    fn flush_without_overflow_omits_summary() {
        let mut tracker = ChildAgentTracker::default();
        let mut queue = Vec::new();

        tracker.on_tool_completed("grep", "3 matches", false, &mut queue);
        tracker.flush_on_completion(&mut queue);

        assert!(!queue.iter().any(|e| matches!(
            e,
            ScrollbackEntry::StreamText { text, .. } if text.contains("more tool use")
        )));
    }

    #[test]
    fn error_completion_uses_cross_mark() {
        let mut tracker = ChildAgentTracker::default();
        let mut queue = Vec::new();

        tracker.on_tool_completed("shell", "permission denied", true, &mut queue);

        assert_eq!(queue.len(), 1);
        assert!(matches!(
            &queue[0],
            ScrollbackEntry::StreamText { text, .. } if text.contains("✗ shell")
        ));
    }

    #[test]
    fn each_tracker_independent_for_parallel_agents() {
        let mut a = ChildAgentTracker::default();
        let mut b = ChildAgentTracker::default();
        let mut queue = Vec::new();

        for i in 0..(MAX_VISIBLE_TOOLS + 2) {
            a.on_tool_completed(&format!("a{i}"), "done", false, &mut queue);
        }
        b.on_tool_completed("b0", "done", false, &mut queue);

        assert_eq!(a.omitted_completed, 2);
        assert_eq!(b.omitted_completed, 0);
    }
}
