//! astrcode-context：LLM 上下文窗口管理 crate。
//!
//! 提供 token 估算和 LLM 驱动的摘要压缩，生成 provider-ready 的可见对话上下文。
//!
//! 本 crate 只描述“对话窗口应该长什么样”：可见对话、compact 触发条件、
//! 摘要 contract 与压缩后的消息形态。system prompt 组装属于 `astrcode-prompt`；
//! 真正的 provider 调用、工具快照、session/eventlog 编排仍由 server/runtime 层负责。

pub mod compaction;
pub mod manager;
pub mod settings;
pub mod token_usage;
