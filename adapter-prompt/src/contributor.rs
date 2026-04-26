//! Prompt 贡献者 trait 定义。
//!
//! [`PromptContributor`] 是 prompt 组装管线的核心扩展点。
//! 每个 contributor 负责生成特定领域的 prompt 内容（如身份、环境、规则等），
//! 通过 [`PromptComposer::with_contributor`](crate::composer::PromptComposer::with_contributor)
//! 注册到管线中。
//!
//! # 缓存机制
//!
//! Contributor 支持基于指纹的缓存：当 `cache_fingerprint()` 返回值不变时，
//! composer 会复用上次收集的贡献，避免重复的文件读取和字符串拼接。

use async_trait::async_trait;

use super::{PromptContext, PromptContribution};

/// Prompt 内容贡献者。
///
/// 实现此 trait 的类型可以向 prompt 组装管线注入 block 和工具定义。
/// 所有 contributor 按注册顺序依次执行，产出合并后由 composer 统一编排。
///
/// # 生命周期
///
/// 需要 `Send + Sync` 因为 composer 可能在异步上下文中并发调用。
/// `async_trait` 允许 `contribute()` 方法执行异步操作（如文件读取）。
#[async_trait]
pub trait PromptContributor: Send + Sync {
    /// 贡献者的唯一标识。
    ///
    /// 用于缓存键、诊断信息和去重。必须是 `'static str`，
    /// 因为 contributor 类型在编译期确定。
    fn contributor_id(&self) -> &'static str;

    /// 缓存版本号。
    ///
    /// 当 contributor 的内部逻辑发生变更（如修改了 prompt 模板）时，
    /// 应递增此值以使现有缓存失效。
    fn cache_version(&self) -> u64 {
        1
    }

    /// 计算当前上下文下的缓存指纹。
    ///
    /// 默认实现使用 [`PromptContext::contributor_cache_fingerprint`]，
    /// 但 contributor 可以覆盖此方法以缩小指纹范围（如仅关注特定文件的变化）。
    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        ctx.contributor_cache_fingerprint()
    }

    /// 收集此 contributor 对 prompt 的贡献。
    ///
    /// 返回的 [`PromptContribution`] 包含 block 规格、变量和额外工具定义。
    /// 此方法在每次 `build()` 时调用（缓存命中时除外）。
    async fn contribute(&self, ctx: &PromptContext) -> PromptContribution;
}
