//! Tool result budget management.

/// Budget manager for tool results in context.
///
/// Limits inline display size and provides content previews
/// to prevent tool results from consuming too much context window.
pub struct ToolResultBudget {
    inline_limit: usize,
    preview_limit: usize,
    aggregate_limit: usize,
}

impl ToolResultBudget {
    pub fn new(inline_limit: usize, preview_limit: usize, aggregate_limit: usize) -> Self {
        Self {
            inline_limit,
            preview_limit,
            aggregate_limit,
        }
    }

    /// Check if a tool result exceeds the inline limit.
    pub fn exceeds_inline(&self, content: &str) -> bool {
        content.len() > self.inline_limit
    }

    /// Total aggregate budget for all tool results in one turn.
    pub fn aggregate_limit(&self) -> usize {
        self.aggregate_limit
    }

    /// Check if total bytes exceed the aggregate limit.
    pub fn exceeds_aggregate(&self, total_bytes: usize) -> bool {
        total_bytes > self.aggregate_limit
    }

    /// Create a preview of large content, truncating at a char boundary.
    pub fn preview(&self, content: &str) -> String {
        if content.len() <= self.preview_limit {
            content.to_string()
        } else {
            let cutoff = crate::floor_char_boundary(content, self.preview_limit);
            format!("{}... (truncated)", &content[..cutoff])
        }
    }
}
