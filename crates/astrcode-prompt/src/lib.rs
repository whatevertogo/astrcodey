//! astrcode-prompt：提示词组装管线。
//!
//! - [`pipeline`] — `build_system_prompt()` 纯函数。
//! - [`composer`] — [`PromptProvider`] 的薄包装实现。
//! - [`template`] — `{{variable}}` 模板引擎。

pub mod composer;
pub mod pipeline;
pub mod template;
