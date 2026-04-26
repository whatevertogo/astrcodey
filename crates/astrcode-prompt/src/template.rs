//! Template engine with `{{variable}}` syntax and 4-tier resolution.

use astrcode_core::prompt::PromptContext;

/// A simple `{{variable}}` template engine.
pub struct PromptTemplate;

impl PromptTemplate {
    /// Render a template string with variables from the context.
    pub fn render(template: &str, context: &PromptContext) -> String {
        let mut result = template.to_string();

        // Replace {{os}}, {{date}}, {{shell}}, {{working_dir}}, {{available_tools}}
        let replacements: Vec<(&str, &str)> = vec![
            ("{{os}}", &context.os),
            ("{{date}}", &context.date),
            ("{{shell}}", &context.shell),
            ("{{working_dir}}", &context.working_dir),
            ("{{available_tools}}", &context.available_tools),
        ];

        for (key, value) in replacements {
            result = result.replace(key, value);
        }

        // Replace custom variables
        for (key, value) in &context.custom {
            result = result.replace(&format!("{{{{{}}}}}", key), value);
        }

        result
    }
}
