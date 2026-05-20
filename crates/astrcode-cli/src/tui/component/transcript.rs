//! Transcript component: manages active ToolRows and scrollback queue.

use std::collections::HashMap;

use ratatui::{buffer::Buffer, layout::Rect};

use super::{Component, KeyOutcome, tool_row::ToolRow};
use crate::tui::store::transcript::ScrollbackEntry;

pub struct Transcript {
    pub active_tool_rows: HashMap<String, ToolRow>,
    pub scrollback_queue: Vec<ScrollbackEntry>,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            active_tool_rows: HashMap::new(),
            scrollback_queue: Vec::new(),
        }
    }

    pub fn open_tool_row(&mut self, call_id: String, row: ToolRow) {
        self.active_tool_rows.insert(call_id, row);
    }

    pub fn get_tool_row_mut(&mut self, call_id: &str) -> Option<&mut ToolRow> {
        self.active_tool_rows.get_mut(call_id)
    }

    pub fn close_tool_row(&mut self, call_id: &str) -> Option<ToolRow> {
        self.active_tool_rows.remove(call_id)
    }

    pub fn push_scrollback(&mut self, entry: ScrollbackEntry) {
        self.scrollback_queue.push(entry);
    }

    pub fn drain_scrollback(&mut self) -> Vec<ScrollbackEntry> {
        std::mem::take(&mut self.scrollback_queue)
    }
}

impl Component for Transcript {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        // Active tool rows are rendered in the active area (above footer).
        // Scrollback is written to terminal history by TerminalSession, not here.
        for (idx, row) in self.active_tool_rows.values_mut().enumerate() {
            let y = area.y + idx as u16;
            if y >= area.y + area.height {
                break;
            }
            let row_area = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            row.render(row_area, buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        self.active_tool_rows.len() as u16
    }
}
