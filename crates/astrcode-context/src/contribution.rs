//! 扩展动态贡献类型。
//!
//! 扩展通过 `PromptBuildHandler` 返回 `PromptContributions`（定义在 `astrcode-core`）。
//! 此模块定义了额外可由扩展贡献的上下文内容。
//!
//! ## 动态贡献流程
//!
//! ```text
//! TurnRunner (每轮)
//!   │
//!   ├─ ExtensionRunner::collect_prompt_contributions_typed()
//!   │    → PromptContributions
//!   │
//!   ├─ PromptEngine::ensure(contribs, base, tools)
//!   │    → system prompt（指纹缓存，动态内容变化时自动重建）
//!   │
//!   └─ LlmContextAssembler::prepare_messages_with_llm()
//!        → provider-ready messages
//! ```
//!
//! 扩展不直接依赖 `astrcode-context`。它们只返回贡献数据，
//! 由 TurnRunner 收集后传给 ContextEngine 组装。
//!
//! MCP 断连/重连、skill 文件变化、工具增删等动态变化，
//! 都会在下一轮 TurnRunner 收集贡献时自动反映到 prompt 和 context。

use astrcode_core::llm::LlmMessage;

/// 扩展可向上下文注入的内容。
///
/// 与 `PromptContributions`（影响 system prompt）不同，
/// `ContextContribution` 影响可见对话消息。
#[derive(Debug, Clone, Default)]
pub struct ContextContribution {
    /// 注入到 provider 消息列表的固定消息（如 MCP 连接状态提示）
    pub pinned_messages: Vec<LlmMessage>,
    /// 注入的文本片段（如 skill 加载的上下文文件内容）
    pub snippets: Vec<ContextSnippet>,
}

/// 扩展注入的文本片段。
#[derive(Debug, Clone)]
pub struct ContextSnippet {
    pub title: String,
    pub content: String,
}
