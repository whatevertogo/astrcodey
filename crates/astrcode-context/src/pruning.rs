//! 剪枝模块——移除上下文中过时或超尺寸的内容。
//!
//! 当工具调用结果超过大小限制时，将其截断到安全边界，
//! 防止单条结果占用过多上下文空间。

use astrcode_core::tool::ToolResult;

/// 工具结果剪枝状态，负责裁剪超尺寸的工具输出。
pub struct PruneState {
    /// 单条工具结果的最大字节数限制。
    max_tool_result_bytes: usize,
}

impl PruneState {
    /// 创建一个新的剪枝状态。
    ///
    /// # 参数
    /// - `max_tool_result_bytes`：单条工具结果允许的最大字节数
    pub fn new(max_tool_result_bytes: usize) -> Self {
        Self {
            max_tool_result_bytes,
        }
    }

    /// 对超尺寸的工具结果进行截断剪枝。
    ///
    /// 如果结果内容超过大小限制，在安全的 UTF-8 字符边界处截断，
    /// 并追加被截断的字节数提示信息。
    pub fn prune_result(&self, result: &mut ToolResult) {
        if result.content.len() > self.max_tool_result_bytes {
            // 在字符边界处截断，避免拆分多字节字符
            let cutoff = crate::floor_char_boundary(&result.content, self.max_tool_result_bytes);
            result.content = format!(
                "{}... [{} bytes truncated]",
                &result.content[..cutoff],
                result.content.len() - cutoff
            );
        }
    }
}
