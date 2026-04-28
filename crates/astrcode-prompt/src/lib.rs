//! astrcode-prompt：提示词组装管线。
//!
//! 提供固定槽位的 system prompt 组装流程。
//! 核心组件包括：
//! - [`composer`] — 提示词组装器，按固定 section 顺序渲染最终 system prompt。
//! - [`contributors`] — 内置 section 填充函数（身份、环境、项目规则等）。
//! - [`template`] — 简单的 `{{variable}}` 模板引擎。

pub mod composer;
pub mod contributors;
pub mod template;
