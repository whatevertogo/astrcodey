//! astrcode-context: Context window management.
//!
//! Token estimation, tool result budgeting, micro-compaction,
//! pruning, LLM-driven compaction, and file access tracking.

pub mod budget;
pub mod compaction;
pub mod file_access;
pub mod pruning;
pub mod settings;
pub mod token_usage;

/// 找到 <= max 的最近 UTF-8 字符边界，防止截断多字节字符。
/// budget 和 pruning 模块都需要此函数，所以提取到 crate 根级别共享。
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut bound = max;
    while bound > 0 && !s.is_char_boundary(bound) {
        bound -= 1;
    }
    bound
}
