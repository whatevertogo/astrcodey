//! 提供商兼容性自动探测。
//!
//! 从 `base_url` 和 `model_id` 推断提供商的响应格式特征，
//! 避免硬编码布尔标志。新增异构模型时只需在此添加一条规则。

/// 响应格式探测定性结果。
///
/// 目前只有 Kimi 一种异构模型使用内联令牌格式（thinking 和工具调用
/// 参数嵌入在 `delta.content` 中），因此只需一个布尔判定。当未来出现
/// 第二种异构格式时，再扩展为 enum。
#[derive(Debug, Clone)]
pub(crate) struct ProviderCompat {
    pub is_kimi: bool,
}

impl ProviderCompat {
    pub fn detect(base_url: &str, model_id: &str) -> Self {
        let url_lower = base_url.to_lowercase();
        let model_lower = model_id.to_lowercase();

        Self {
            is_kimi: url_lower.contains("moonshot") || model_lower.contains("kimi"),
        }
    }
}
