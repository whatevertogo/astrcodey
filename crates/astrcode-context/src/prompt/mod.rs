//! System prompt 组装。
//!
//! `composer` 是对外实现 `PromptProvider` 的薄门面；
//! `pipeline` 负责按稳定顺序拼接 prompt section。
//!
//! 静态内容（Identity、System、Task Guidelines、Communication）在前，
//! 动态内容（Environment、Rules、Tool Summary、Extension blocks、Extra Instructions）在后。

pub mod composer;
pub mod pipeline;
