//! ToolRow: single-line active tool display (Running → Args → Complete/Error).

use std::{any::Any, sync::Arc};

use astrcode_core::{render::RenderSpec, tool::ToolResult};
use ratatui::{buffer::Buffer, layout::Rect, prelude::Widget, text::Line, widgets::Paragraph};

use super::Component;
use crate::tui::{
    ext::tool::{ToolRenderCtx, ToolRenderer},
    render::render_spec_to_lines,
    theme::Theme,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRowState {
    Running,
    ArgsReceived,
    Complete,
    Error,
}

pub struct ToolRow {
    pub call_id: String,
    pub tool_name: String,
    pub state: ToolRowState,
    pub args: Option<serde_json::Value>,
    pub result: Option<ToolResult>,
    pub render_spec: Option<RenderSpec>,
    renderer: Arc<dyn ToolRenderer>,
    renderer_state: Box<dyn Any + Send>,
    theme: Theme,
}

impl ToolRow {
    pub fn new(
        call_id: String,
        tool_name: String,
        renderer: Arc<dyn ToolRenderer>,
        theme: Theme,
    ) -> Self {
        Self {
            call_id,
            tool_name,
            state: ToolRowState::Running,
            args: None,
            result: None,
            render_spec: None,
            renderer,
            renderer_state: Box::new(()),
            theme,
        }
    }

    pub fn set_args(&mut self, args: serde_json::Value) {
        self.args = Some(args);
        self.state = ToolRowState::ArgsReceived;
    }

    pub fn complete(&mut self, result: ToolResult, spec: Option<RenderSpec>) {
        self.state = if result.is_error {
            ToolRowState::Error
        } else {
            ToolRowState::Complete
        };
        self.render_spec = spec;
        self.result = Some(result);
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, ToolRowState::Complete | ToolRowState::Error)
    }

    fn current_spec(&mut self) -> RenderSpec {
        let mut ctx = ToolRenderCtx {
            call_id: &self.call_id,
            tool_name: &self.tool_name,
            args: self.args.as_ref(),
            args_complete: matches!(
                self.state,
                ToolRowState::ArgsReceived | ToolRowState::Complete | ToolRowState::Error
            ),
            execution_started: true,
            is_partial: !self.is_done(),
            is_error: self.state == ToolRowState::Error,
            expanded: false,
            state: &mut self.renderer_state,
        };
        if let Some(spec) = &self.render_spec {
            return spec.clone();
        }
        if let Some(result) = &self.result.clone() {
            if let Some(spec) = self.renderer.render_result(result, &mut ctx) {
                return spec;
            }
        }
        self.renderer.render_call(&mut ctx)
    }
}

impl Component for ToolRow {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let spec = self.current_spec();
        let lines = render_spec_to_lines(&spec, "  ", area.width as usize, &self.theme);
        let first_line = lines.into_iter().next().unwrap_or_else(|| Line::from(""));
        Paragraph::new(first_line).render(area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}
