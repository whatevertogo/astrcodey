//! 模板引擎，支持 `{{variable}}` 语法和多层级变量解析。
//!
//! 变量替换按 key 长度降序排列，确保 `{{os_type}}` 优先于 `{{os}}` 匹配，
//! 避免前缀冲突导致错误替换。

use std::cmp::Reverse;

use astrcode_core::prompt::PromptContext;

/// 简单的 `{{variable}}` 模板引擎。
///
/// 无状态的静态 API 设计，直接通过 `PromptTemplate::render()` 调用。
pub struct PromptTemplate;

impl PromptTemplate {
    /// 使用上下文中的变量渲染模板字符串。
    ///
    /// 内置变量包括 `os`、`date`、`shell`、`working_dir`、`available_tools`，
    /// 同时支持通过 `context.custom` 注入的自定义变量。
    ///
    /// 变量替换按 key 长度降序执行，防止 `{{os_type}}` 被 `{{os}}` 提前匹配。
    pub fn render(template: &str, context: &PromptContext) -> String {
        let mut result = template.to_string();

        // 收集所有需要替换的键值对：(模板占位符, 变量值)
        let mut replacements: Vec<(String, &str)> = vec![
            ("{{os}}".into(), &context.os),
            ("{{date}}".into(), &context.date),
            ("{{shell}}".into(), &context.shell),
            ("{{working_dir}}".into(), &context.working_dir),
            ("{{available_tools}}".into(), &context.available_tools),
        ];

        // 追加自定义变量，格式为 `{{key}}`。
        for (key, value) in &context.custom {
            replacements.push((format!("{{{{{}}}}}", key), value));
        }

        // 按 key 长度降序排列，防止短 key 截断长 key 的前缀。
        replacements.sort_by_key(|(key, _)| Reverse(key.len()));

        // 依次执行替换。
        for (key, value) in replacements {
            result = result.replace(&key, value);
        }

        result
    }
}
