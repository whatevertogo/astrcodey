//! Prompt 模板引擎。
//!
//! 提供简单的 `{{variable}}` 占位符替换功能。
//! 模板由 [`BlockContent::Template`](crate::BlockContent::Template) 使用，
//! 在 [`PromptComposer`](crate::composer::PromptComposer) 渲染阶段解析。
//!
//! # 设计选择
//!
//! 采用极简的 `{{key}}` 语法而非完整的模板语言（如 Handlebars），
//! 因为 prompt 模板只需要变量替换，不需要条件、循环等复杂逻辑。

use std::borrow::Cow;

/// Prompt 模板，包含 `{{variable}}` 占位符。
///
/// 通过 [`render`](Self::render) 方法传入变量解析器，生成最终字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    source: Cow<'static, str>,
}

impl PromptTemplate {
    pub fn new(source: impl Into<Cow<'static, str>>) -> Self {
        Self {
            source: source.into(),
        }
    }

    /// 渲染模板，将占位符替换为实际值。
    ///
    /// `resolver` 闭包接收变量名，返回 `Some(value)` 或 `None`（表示变量缺失）。
    /// 当遇到缺失的变量时，返回 [`TemplateRenderError::MissingVariable`]。
    pub fn render<F>(&self, mut resolver: F) -> Result<String, TemplateRenderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let mut rendered = String::new();
        let mut remaining = self.source.as_ref();

        while let Some(start) = remaining.find("{{") {
            let (prefix, after_start) = remaining.split_at(start);
            rendered.push_str(prefix);

            let after_start = &after_start[2..];
            let Some(end) = after_start.find("}}") else {
                return Err(TemplateRenderError::UnclosedPlaceholder);
            };

            let (placeholder, after_end) = after_start.split_at(end);
            let key = placeholder.trim();
            if key.is_empty() {
                return Err(TemplateRenderError::EmptyPlaceholder);
            }

            let value = resolver(key)
                .ok_or_else(|| TemplateRenderError::MissingVariable(key.to_string()))?;
            rendered.push_str(&value);
            remaining = &after_end[2..];
        }

        rendered.push_str(remaining);
        Ok(rendered)
    }
}

/// 模板渲染错误。
///
/// 当模板格式不正确或变量缺失时返回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRenderError {
    EmptyPlaceholder,
    MissingVariable(String),
    UnclosedPlaceholder,
}

impl std::fmt::Display for TemplateRenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPlaceholder => write!(f, "template contains an empty placeholder"),
            Self::MissingVariable(variable) => {
                write!(f, "template variable '{variable}' is missing")
            },
            Self::UnclosedPlaceholder => {
                write!(f, "template contains an unclosed placeholder")
            },
        }
    }
}

impl std::error::Error for TemplateRenderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_variables_in_template() {
        let template = PromptTemplate::new("hello {{ name }}");
        let rendered = template
            .render(|key| (key == "name").then(|| "world".to_string()))
            .expect("template should render");

        assert_eq!(rendered, "hello world");
    }

    #[test]
    fn errors_when_variable_is_missing() {
        let template = PromptTemplate::new("hello {{ name }}");
        let err = template
            .render(|_| None)
            .expect_err("missing variable should fail");

        assert_eq!(
            err,
            TemplateRenderError::MissingVariable("name".to_string())
        );
    }

    #[test]
    fn errors_when_placeholder_is_unclosed() {
        let template = PromptTemplate::new("hello {{ name");

        let err = template
            .render(|_| Some("world".to_string()))
            .expect_err("unclosed placeholder should fail");

        assert_eq!(err, TemplateRenderError::UnclosedPlaceholder);
    }

    #[test]
    fn errors_when_placeholder_is_empty() {
        let template = PromptTemplate::new("hello {{   }}");

        let err = template
            .render(|_| Some("world".to_string()))
            .expect_err("empty placeholder should fail");

        assert_eq!(err, TemplateRenderError::EmptyPlaceholder);
    }
}
