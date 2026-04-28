//! 工具结果预算管理模块。
//!
//! 控制工具调用结果在上下文窗口中的显示大小，
//! 防止单个或累计的工具输出占用过多 token 空间。

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
