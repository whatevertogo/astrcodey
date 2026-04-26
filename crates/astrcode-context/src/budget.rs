//! Tool result budget management.

use astrcode_core::tool::ToolResult;

pub struct ToolResultBudget {
    aggregate_limit: usize,
    inline_limit: usize,
    preview_limit: usize,
}

impl ToolResultBudget {
    pub fn new(aggregate_limit: usize, inline_limit: usize, preview_limit: usize) -> Self {
        Self {
            aggregate_limit,
            inline_limit,
            preview_limit,
        }
    }

    /// Check if a tool result exceeds the inline limit.
    pub fn exceeds_inline(&self, content: &str) -> bool {
        content.len() > self.inline_limit
    }

    /// Create a preview of large content.
    pub fn preview(&self, content: &str) -> String {
        if content.len() <= self.preview_limit {
            content.to_string()
        } else {
            format!("{}... (truncated)", &content[..self.preview_limit])
        }
    }
}
