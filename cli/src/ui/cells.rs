use unicode_width::UnicodeWidthStr;

use super::{
    markdown::{render_literal_text, render_markdown_lines, render_preformatted_block},
    theme::ThemePalette,
    truncate_to_width,
};
use crate::{
    capability::TerminalCapabilities,
    state::{
        ThinkingPresentationState, TranscriptCell, TranscriptCellKind, TranscriptCellStatus,
        WrappedLine, WrappedLineStyle, WrappedSpan,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptCellView {
    pub selected: bool,
    pub expanded: bool,
    pub thinking: Option<ThinkingPresentationState>,
}

pub trait RenderableCell {
    fn render_lines(
        &self,
        width: usize,
        capabilities: TerminalCapabilities,
        theme: &dyn ThemePalette,
        view: &TranscriptCellView,
    ) -> Vec<WrappedLine>;
}

impl RenderableCell for TranscriptCell {
    fn render_lines(
        &self,
        width: usize,
        capabilities: TerminalCapabilities,
        theme: &dyn ThemePalette,
        view: &TranscriptCellView,
    ) -> Vec<WrappedLine> {
        let width = width.max(28);
        match &self.kind {
            TranscriptCellKind::User { body } => {
                render_message(body, width, capabilities, theme, view, true)
            },
            TranscriptCellKind::Assistant { body, status } => {
                let content = if matches!(status, TranscriptCellStatus::Streaming) {
                    format!("{body}{}", status_suffix(*status))
                } else {
                    body.clone()
                };
                render_message(content.as_str(), width, capabilities, theme, view, false)
            },
            TranscriptCellKind::Thinking { .. } => {
                render_thinking_cell(width, capabilities, theme, view)
            },
            TranscriptCellKind::ToolCall {
                tool_name,
                summary,
                status,
                stdout,
                stderr,
                error,
                duration_ms,
                truncated,
                child_session_id,
            } => render_tool_call_cell(
                ToolCallView {
                    tool_name,
                    summary,
                    status: *status,
                    stdout,
                    stderr,
                    error: error.as_deref(),
                    duration_ms: *duration_ms,
                    truncated: *truncated,
                    child_session_id: child_session_id.as_deref(),
                },
                width,
                capabilities,
                theme,
                view,
            ),
            TranscriptCellKind::Error { code, message } => render_secondary_line(
                &format!("{code} {message}"),
                width,
                capabilities,
                theme,
                view,
                WrappedLineStyle::ErrorText,
                MarkdownRenderMode::Literal,
            ),
            TranscriptCellKind::SystemNote { markdown, .. } => render_secondary_line(
                markdown,
                width,
                capabilities,
                theme,
                view,
                WrappedLineStyle::Notice,
                MarkdownRenderMode::Display,
            ),
            TranscriptCellKind::ChildHandoff { title, message, .. } => render_secondary_line(
                &format!("{title} · {message}"),
                width,
                capabilities,
                theme,
                view,
                WrappedLineStyle::Notice,
                MarkdownRenderMode::Literal,
            ),
        }
    }
}

impl TranscriptCellView {
    fn resolve_style(&self, base: WrappedLineStyle) -> WrappedLineStyle {
        if self.selected {
            WrappedLineStyle::Selection
        } else {
            base
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkdownRenderMode {
    Literal,
    Display,
}

fn render_message(
    body: &str,
    width: usize,
    capabilities: TerminalCapabilities,
    theme: &dyn ThemePalette,
    view: &TranscriptCellView,
    is_user: bool,
) -> Vec<WrappedLine> {
    let first_prefix = if is_user {
        format!("{} ", prompt_marker(theme))
    } else {
        format!("{} ", assistant_marker(theme))
    };
    let subsequent_prefix = " ".repeat(display_width(first_prefix.as_str()));
    let wrapped = if is_user {
        render_literal_lines(
            body,
            width.saturating_sub(display_width(first_prefix.as_str())),
            capabilities,
            view.resolve_style(WrappedLineStyle::PromptEcho),
        )
    } else {
        render_markdown_lines(
            body,
            width.saturating_sub(display_width(first_prefix.as_str())),
            capabilities,
            view.resolve_style(WrappedLineStyle::Plain),
        )
    };

    let mut lines = Vec::new();
    for (index, line) in wrapped.into_iter().enumerate() {
        lines.push(prepend_prefix(
            line,
            if index == 0 {
                plain_prefix(first_prefix.as_str())
            } else {
                plain_prefix(subsequent_prefix.as_str())
            },
        ));
    }
    lines.push(blank_line());
    lines
}

fn render_thinking_cell(
    width: usize,
    capabilities: TerminalCapabilities,
    _theme: &dyn ThemePalette,
    view: &TranscriptCellView,
) -> Vec<WrappedLine> {
    let Some(thinking) = view.thinking.as_ref() else {
        return vec![blank_line()];
    };
    if !view.expanded {
        return vec![
            plain_line(
                view.resolve_style(WrappedLineStyle::ThinkingLabel),
                truncate_to_width(
                    format!("{} {}", thinking_marker(_theme), thinking.summary).as_str(),
                    width,
                ),
            ),
            plain_line(
                view.resolve_style(WrappedLineStyle::ThinkingPreview),
                truncate_to_width(
                    format!("  {} {}", thinking_preview_prefix(_theme), thinking.preview).as_str(),
                    width,
                ),
            ),
            blank_line(),
        ];
    }

    let mut lines = vec![plain_line(
        view.resolve_style(WrappedLineStyle::ThinkingLabel),
        format!("{} {}", thinking_marker(_theme), thinking.summary),
    )];
    lines.push(plain_line(
        view.resolve_style(WrappedLineStyle::ThinkingPreview),
        format!("  {}", thinking.hint),
    ));
    for line in render_markdown_lines(
        thinking.expanded_body.as_str(),
        width.saturating_sub(2),
        capabilities,
        view.resolve_style(WrappedLineStyle::ThinkingBody),
    ) {
        lines.push(prepend_prefix(line, plain_prefix("  ")));
    }
    lines.push(blank_line());
    lines
}

#[derive(Debug, Clone, Copy)]
struct ToolCallView<'a> {
    tool_name: &'a str,
    summary: &'a str,
    status: TranscriptCellStatus,
    stdout: &'a str,
    stderr: &'a str,
    error: Option<&'a str>,
    duration_ms: Option<u64>,
    truncated: bool,
    child_session_id: Option<&'a str>,
}

fn render_tool_call_cell(
    tool: ToolCallView<'_>,
    width: usize,
    capabilities: TerminalCapabilities,
    theme: &dyn ThemePalette,
    view: &TranscriptCellView,
) -> Vec<WrappedLine> {
    let mut lines = vec![plain_line(
        view.resolve_style(WrappedLineStyle::ToolLabel),
        truncate_to_width(
            format!(
                "{} tool {}{} · {}",
                tool_marker(theme),
                tool.tool_name,
                if tool.truncated { " · truncated" } else { "" },
                tool.summary
            )
            .as_str(),
            width,
        ),
    )];

    if view.expanded {
        let mut metadata = Vec::new();
        metadata.push(match tool.status {
            TranscriptCellStatus::Streaming => "streaming".to_string(),
            TranscriptCellStatus::Complete => "complete".to_string(),
            TranscriptCellStatus::Failed => "failed".to_string(),
            TranscriptCellStatus::Cancelled => "cancelled".to_string(),
        });
        if let Some(duration_ms) = tool.duration_ms {
            metadata.push(format!("{duration_ms}ms"));
        }
        if let Some(child_session_id) = tool.child_session_id {
            metadata.push(format!("child session {child_session_id}"));
        }
        if !metadata.is_empty() {
            lines.push(plain_line(
                view.resolve_style(WrappedLineStyle::ToolBody),
                format!("  meta {}", metadata.join(" · ")),
            ));
        }

        if !tool.stdout.trim().is_empty() {
            append_preformatted_tool_section(
                &mut lines,
                "stdout",
                tool.stdout,
                width,
                capabilities,
                theme,
                view,
            );
        }
        if !tool.stderr.trim().is_empty() {
            append_preformatted_tool_section(
                &mut lines,
                "stderr",
                tool.stderr,
                width,
                capabilities,
                theme,
                view,
            );
        }
        if let Some(error) = tool.error {
            append_preformatted_tool_section(
                &mut lines,
                "error",
                error,
                width,
                capabilities,
                theme,
                view,
            );
        }
    }

    lines.push(blank_line());
    lines
}

fn append_preformatted_tool_section(
    lines: &mut Vec<WrappedLine>,
    label: &str,
    body: &str,
    width: usize,
    capabilities: TerminalCapabilities,
    theme: &dyn ThemePalette,
    view: &TranscriptCellView,
) {
    let section_style = view.resolve_style(WrappedLineStyle::ToolBody);
    lines.push(plain_line(section_style, format!("  {label}")));
    for line in render_preformatted_block(body, width.saturating_sub(4), capabilities) {
        lines.push(plain_line(
            section_style,
            format!("  {} {line}", tool_block_marker(theme)),
        ));
    }
}

fn render_secondary_line(
    body: &str,
    width: usize,
    capabilities: TerminalCapabilities,
    theme: &dyn ThemePalette,
    view: &TranscriptCellView,
    style: WrappedLineStyle,
    render_mode: MarkdownRenderMode,
) -> Vec<WrappedLine> {
    let base_style = view.resolve_style(style);
    let rendered = match render_mode {
        MarkdownRenderMode::Literal => {
            render_literal_lines(body, width.saturating_sub(2), capabilities, base_style)
        },
        MarkdownRenderMode::Display => {
            render_markdown_lines(body, width.saturating_sub(2), capabilities, base_style)
        },
    };

    let mut lines = Vec::new();
    for line in rendered {
        lines.push(prepend_prefix(
            line,
            plain_prefix(format!("{} ", secondary_marker(theme)).as_str()),
        ));
    }
    lines.push(blank_line());
    lines
}

fn render_literal_lines(
    text: &str,
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    render_literal_text(text, width, capabilities)
        .into_iter()
        .map(|line| plain_line(style, line))
        .collect()
}

fn plain_line(style: WrappedLineStyle, content: impl Into<String>) -> WrappedLine {
    WrappedLine::plain(style, content)
}

fn plain_prefix(prefix: &str) -> Vec<WrappedSpan> {
    if prefix.is_empty() {
        Vec::new()
    } else {
        vec![WrappedSpan::plain(prefix.to_string())]
    }
}

fn prepend_prefix(mut line: WrappedLine, mut prefix: Vec<WrappedSpan>) -> WrappedLine {
    if prefix.is_empty() {
        return line;
    }
    prefix.extend(line.spans);
    line.spans = prefix;
    line
}

fn prompt_marker(theme: &dyn ThemePalette) -> &'static str {
    theme.glyph("›", ">")
}

fn assistant_marker(theme: &dyn ThemePalette) -> &'static str {
    theme.glyph("•", "*")
}

fn thinking_marker(theme: &dyn ThemePalette) -> &'static str {
    theme.glyph("◦", "o")
}

fn thinking_preview_prefix(theme: &dyn ThemePalette) -> &'static str {
    theme.glyph("↳", ">")
}

fn tool_marker(theme: &dyn ThemePalette) -> &'static str {
    theme.glyph("◆", "+")
}

fn secondary_marker(theme: &dyn ThemePalette) -> &'static str {
    theme.glyph("·", "-")
}

fn tool_block_marker(theme: &dyn ThemePalette) -> &'static str {
    theme.glyph("│", "|")
}

fn blank_line() -> WrappedLine {
    plain_line(WrappedLineStyle::Plain, String::new())
}

fn status_suffix(status: TranscriptCellStatus) -> &'static str {
    match status {
        TranscriptCellStatus::Streaming => " · streaming",
        TranscriptCellStatus::Complete => "",
        TranscriptCellStatus::Failed => " · failed",
        TranscriptCellStatus::Cancelled => " · cancelled",
    }
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::{
        RenderableCell, TranscriptCellView, assistant_marker, secondary_marker, thinking_marker,
        tool_marker,
    };
    use crate::{
        capability::{ColorLevel, GlyphMode, TerminalCapabilities},
        state::{TranscriptCell, TranscriptCellKind, TranscriptCellStatus},
        ui::CodexTheme,
    };

    fn unicode_capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            color: ColorLevel::TrueColor,
            glyphs: GlyphMode::Unicode,
            alt_screen: false,
            mouse: false,
            bracketed_paste: false,
        }
    }

    fn ascii_capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            color: ColorLevel::None,
            glyphs: GlyphMode::Ascii,
            alt_screen: false,
            mouse: false,
            bracketed_paste: false,
        }
    }

    #[test]
    fn ascii_markers_remain_distinct_by_cell_type() {
        let theme = CodexTheme::new(ascii_capabilities());
        assert_ne!(assistant_marker(&theme), thinking_marker(&theme));
        assert_ne!(tool_marker(&theme), secondary_marker(&theme));
    }

    #[test]
    fn assistant_wrapped_lines_use_hanging_indent() {
        let theme = CodexTheme::new(unicode_capabilities());
        let cell = TranscriptCell {
            id: "assistant-1".to_string(),
            expanded: false,
            kind: TranscriptCellKind::Assistant {
                body: "你好！我是 AstrCode，你的本地 AI \
                       编码助手。我可以帮你处理代码编写、文件编辑、终端命令、\
                       代码审查等各种开发任务。"
                    .to_string(),
                status: TranscriptCellStatus::Complete,
            },
        };

        let lines = cell.render_lines(
            36,
            unicode_capabilities(),
            &theme,
            &TranscriptCellView::default(),
        );

        assert!(lines.len() >= 3);
        assert!(lines[0].text().starts_with("• "));
        assert!(lines[1].text().starts_with("  "));
        assert!(!lines[1].text().starts_with("   "));
    }

    #[test]
    fn assistant_rendering_preserves_markdown_line_breaks() {
        let theme = CodexTheme::new(unicode_capabilities());
        let cell = TranscriptCell {
            id: "assistant-2".to_string(),
            expanded: false,
            kind: TranscriptCellKind::Assistant {
                body: "你好！\n\n- 第一项\n- 第二项".to_string(),
                status: TranscriptCellStatus::Complete,
            },
        };

        let lines = cell.render_lines(
            36,
            unicode_capabilities(),
            &theme,
            &TranscriptCellView::default(),
        );

        assert!(lines.iter().any(|line| line.text() == "  "));
        assert!(lines.iter().any(|line| line.text().contains("- 第一项")));
        assert!(lines.iter().any(|line| line.text().contains("- 第二项")));
    }
}
