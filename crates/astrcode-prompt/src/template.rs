//! Template engine with `{{variable}}` syntax and 4-tier resolution.

use astrcode_core::prompt::PromptContext;

/// A simple `{{variable}}` template engine.
pub struct PromptTemplate;

impl PromptTemplate {
    /// Render a template string with variables from the context.
    ///
    /// Variables are replaced in descending key-length order so that
    /// `{{os_type}}` is matched before `{{os}}`, preventing prefix corruption.
    pub fn render(template: &str, context: &PromptContext) -> String {
        let mut result = template.to_string();

        // Collect all replacements: (template_key, value)
        let mut replacements: Vec<(String, &str)> = vec![
            ("{{os}}".into(), &context.os),
            ("{{date}}".into(), &context.date),
            ("{{shell}}".into(), &context.shell),
            ("{{working_dir}}".into(), &context.working_dir),
            ("{{available_tools}}".into(), &context.available_tools),
        ];

        for (key, value) in &context.custom {
            replacements.push((format!("{{{{{}}}}}", key), value));
        }

        // Sort by key length descending to prevent prefix collisions
        replacements.sort_by(|(a, _), (b, _)| b.len().cmp(&a.len()));

        for (key, value) in replacements {
            result = result.replace(&key, value);
        }

        result
    }
}
