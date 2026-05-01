//! astrcode-context：上下文窗口管理 crate。
//!
//! 提供 token 估算、工具结果预算控制、微压缩（micro-compaction）、
//! 剪枝（pruning）、LLM 驱动的上下文压缩以及文件访问追踪等能力，
//! 确保 LLM 的上下文窗口在有限 token 预算内高效运作。

pub mod budget;
pub mod compaction;
pub mod file_access;
pub mod manager;
pub mod pruning;
pub mod settings;
pub mod token_usage;
pub mod tool_results;

/// 找到 `<= max` 的最近 UTF-8 字符边界，防止截断多字节字符。
///
/// budget 和 pruning 模块都需要此函数，因此提取到 crate 根级别共享。
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut bound = max;
    // 从 max 位置向前搜索，直到找到一个合法的字符边界
    while bound > 0 && !s.is_char_boundary(bound) {
        bound -= 1;
    }
    bound
}
