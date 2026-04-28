//! Token 估算与使用量追踪模块。
//!
//! 提供基于文本长度的粗略 token 估算，
//! 并支持锚定到 LLM 提供商返回的实际 token 计数。

/// Token 使用量追踪器。
///
/// 维护已报告的输入/输出 token 计数，
/// 并提供基于字符数的粗略 token 估算能力。
pub struct TokenUsageTracker {
    /// 提供商报告的实际输入 token 数。
    reported_input_tokens: usize,
    /// 提供商报告的实际输出 token 数。
    reported_output_tokens: usize,
}

impl TokenUsageTracker {
    /// 创建一个新的 token 使用量追踪器，初始计数为零。
    pub fn new() -> Self {
        Self {
            reported_input_tokens: 0,
            reported_output_tokens: 0,
        }
    }

    /// 基于文本字符数估算 token 数量。
    ///
    /// 使用 4/3 的乘数作为填充系数，即假设平均每 4 个字节约对应 3 个 token。
    /// 这是一个粗略估算，实际 token 数取决于分词器和文本内容。
    pub fn estimate_request_tokens(&self, text: &str) -> usize {
        (text.len() as f64 * 4.0 / 3.0) as usize
    }

    /// 用提供商返回的实际 token 计数更新追踪器。
    ///
    /// # 参数
    /// - `input`：提供商报告的输入 token 数
    /// - `output`：提供商报告的输出 token 数
    pub fn anchor_actuals(&mut self, input: usize, output: usize) {
        self.reported_input_tokens = input;
        self.reported_output_tokens = output;
    }
}

impl Default for TokenUsageTracker {
    fn default() -> Self {
        Self::new()
    }
}
