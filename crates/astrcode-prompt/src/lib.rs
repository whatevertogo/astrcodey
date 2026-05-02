//! System prompt composition pipeline.
//!
//! `composer` 是对外实现 `PromptProvider` 的薄门面；`pipeline` 保持为纯函数，
//! 负责按稳定顺序拼接 identity、rules、extension blocks 和环境信息。

pub mod composer;
pub mod pipeline;
