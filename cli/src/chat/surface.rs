use std::collections::HashSet;

use crate::{
    state::{
        CliState, TranscriptCell, TranscriptCellKind, TranscriptCellStatus, WrappedLine,
        WrappedLineStyle,
    },
    ui::{
        CodexTheme,
        cells::{RenderableCell, TranscriptCellView},
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatSurfaceFrame {
    pub history_lines: Vec<WrappedLine>,
    pub status_line: Option<WrappedLine>,
    pub detail_lines: Vec<WrappedLine>,
    pub preview_lines: Vec<WrappedLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatSurfaceState {
    committed_cells: HashSet<String>,
}

impl ChatSurfaceState {
    pub fn reset(&mut self) {
        self.committed_cells.clear();
    }

    pub fn build_frame(
        &mut self,
        state: &CliState,
        theme: &CodexTheme,
        width: u16,
    ) -> ChatSurfaceFrame {
        let mut frame = ChatSurfaceFrame::default();
        let content_width = usize::from(width.max(28));

        for cell in state.transcript_cells() {
            if cell_is_streaming(&cell) {
                self.apply_active_cell(&cell, state, theme, content_width, &mut frame);
                continue;
            }
            self.commit_completed_cell(&cell, state, theme, content_width, &mut frame);
        }

        if let Some(banner) = &state.conversation.banner {
            frame.status_line = Some(status_line(format!("• {}", banner.error.message)));
            frame.detail_lines.insert(
                0,
                plain_line("  当前流需要重新同步，建议等待自动恢复或重新加载快照。"),
            );
        }

        frame
    }

    fn apply_active_cell(
        &mut self,
        cell: &TranscriptCell,
        state: &CliState,
        theme: &CodexTheme,
        width: usize,
        frame: &mut ChatSurfaceFrame,
    ) {
        let rendered = trim_trailing_blank_lines(render_cell_lines(cell, state, theme, width));

        match &cell.kind {
            TranscriptCellKind::Assistant { .. } => {
                frame.status_line = Some(status_line("• 正在生成回复"));
                frame.preview_lines = rendered;
            },
            TranscriptCellKind::Thinking { .. } => {
                frame.status_line = Some(status_line("• 正在思考"));
                frame.detail_lines = rendered;
            },
            TranscriptCellKind::ToolCall { tool_name, .. } => {
                frame.status_line = Some(status_line(format!("• 正在运行 {tool_name}")));
                frame.detail_lines = rendered;
            },
            _ => {},
        }
    }

    fn commit_completed_cell(
        &mut self,
        cell: &TranscriptCell,
        state: &CliState,
        theme: &CodexTheme,
        width: usize,
        frame: &mut ChatSurfaceFrame,
    ) {
        if !self.committed_cells.insert(cell.id.clone()) {
            return;
        }
        let rendered = render_cell_lines(cell, state, theme, width);
        frame.history_lines.extend(rendered);
    }
}

fn plain_line(content: impl Into<String>) -> WrappedLine {
    WrappedLine::plain(WrappedLineStyle::Plain, content)
}

fn status_line(content: impl Into<String>) -> WrappedLine {
    WrappedLine::plain(WrappedLineStyle::Plain, content)
}

fn cell_is_streaming(cell: &TranscriptCell) -> bool {
    match &cell.kind {
        TranscriptCellKind::Assistant { status, .. }
        | TranscriptCellKind::Thinking { status, .. }
        | TranscriptCellKind::ToolCall { status, .. } => {
            matches!(status, TranscriptCellStatus::Streaming)
        },
        _ => false,
    }
}

fn render_cell_lines(
    cell: &TranscriptCell,
    state: &CliState,
    theme: &CodexTheme,
    width: usize,
) -> Vec<WrappedLine> {
    let view = TranscriptCellView {
        selected: false,
        expanded: state.is_cell_expanded(cell.id.as_str()) || cell.expanded,
        thinking: thinking_state_for_cell(cell, state),
    };
    cell.render_lines(width, state.shell.capabilities, theme, &view)
}

fn trim_trailing_blank_lines(mut lines: Vec<WrappedLine>) -> Vec<WrappedLine> {
    while lines.last().is_some_and(WrappedLine::is_blank) {
        lines.pop();
    }
    lines
}

fn thinking_state_for_cell(
    cell: &TranscriptCell,
    state: &CliState,
) -> Option<crate::state::ThinkingPresentationState> {
    let TranscriptCellKind::Thinking { body, status } = &cell.kind else {
        return None;
    };
    Some(state.thinking_playback.present(
        &state.thinking_pool,
        cell.id.as_str(),
        body.as_str(),
        *status,
        state.is_cell_expanded(cell.id.as_str()) || cell.expanded,
    ))
}

#[cfg(test)]
mod tests {
    use astrcode_client::{
        ConversationAssistantBlockDto, ConversationBlockDto, ConversationBlockStatusDto,
    };

    use super::ChatSurfaceState;
    use crate::{
        capability::{ColorLevel, GlyphMode, TerminalCapabilities},
        state::{CliState, WrappedLine},
        ui::CodexTheme,
    };

    fn capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            color: ColorLevel::Ansi16,
            glyphs: GlyphMode::Unicode,
            alt_screen: false,
            mouse: false,
            bracketed_paste: false,
        }
    }

    fn assistant_block(
        id: &str,
        status: ConversationBlockStatusDto,
        markdown: &str,
    ) -> ConversationBlockDto {
        ConversationBlockDto::Assistant(ConversationAssistantBlockDto {
            id: id.to_string(),
            turn_id: Some("turn-1".to_string()),
            status,
            markdown: markdown.to_string(),
            step_index: None,
        })
    }

    fn line_texts(lines: &[WrappedLine]) -> Vec<String> {
        lines.iter().map(WrappedLine::text).collect()
    }

    #[test]
    fn streaming_assistant_progressively_commits_history_and_keeps_tail_in_preview() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.conversation.transcript = vec![assistant_block(
            "assistant-1",
            ConversationBlockStatusDto::Streaming,
            "- 第1项：这是一个足够长的列表项，用来制造稳定折行。\n- \
             第2项：这是一个足够长的列表项，用来制造稳定折行。\n- \
             第3项：这是一个足够长的列表项，用来制造稳定折行。\n- \
             第4项：这是一个足够长的列表项，用来制造稳定折行。\n- \
             第5项：这是一个足够长的列表项，用来制造稳定折行。\n- \
             第6项：这是一个足够长的列表项，用来制造稳定折行。",
        )];
        let theme = CodexTheme::new(state.shell.capabilities);
        let mut surface = ChatSurfaceState::default();

        let frame = surface.build_frame(&state, &theme, 28);
        let history = line_texts(&frame.history_lines);
        let preview = line_texts(&frame.preview_lines);

        assert!(history.is_empty());
        assert!(preview.iter().any(|line| line.contains("第1项")));
        assert!(preview.iter().any(|line| line.contains("第6项")));

        let second = surface.build_frame(&state, &theme, 28);
        assert!(second.history_lines.is_empty());
        assert_eq!(line_texts(&second.preview_lines), preview);
    }

    #[test]
    fn completing_streaming_assistant_only_commits_remaining_tail_once() {
        let mut state = CliState::new("http://127.0.0.1:5529".to_string(), None, capabilities());
        state.conversation.transcript = vec![assistant_block(
            "assistant-1",
            ConversationBlockStatusDto::Streaming,
            "前言\n\n- 第一项\n- 第二项\n第5行\n第6行\n第7行\n第8行\n第9行\n第10行",
        )];
        let theme = CodexTheme::new(state.shell.capabilities);
        let mut surface = ChatSurfaceState::default();

        let _ = surface.build_frame(&state, &theme, 80);

        state.conversation.transcript = vec![assistant_block(
            "assistant-1",
            ConversationBlockStatusDto::Complete,
            "前言\n\n- 第一项\n- 第二项\n第5行\n第6行\n第7行\n第8行\n第9行\n第10行",
        )];

        let completed = surface.build_frame(&state, &theme, 80);
        let history = line_texts(&completed.history_lines);

        assert!(history.iter().any(|line| line.contains("前言")));
        assert!(history.iter().any(|line| line.contains("- 第一项")));
        assert!(history.iter().any(|line| line.contains("- 第二项")));
        assert!(history.iter().any(|line| line.contains("第10行")));
        assert!(completed.preview_lines.is_empty());

        let repeated = surface.build_frame(&state, &theme, 80);
        assert!(repeated.history_lines.is_empty());
    }
}
