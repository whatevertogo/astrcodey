//! Pruning pass — removes stale/oversized content from context.

use astrcode_core::tool::ToolResult;

/// State for pruning oversized tool results from context.
pub struct PruneState {
    max_tool_result_bytes: usize,
}

impl PruneState {
    pub fn new(max_tool_result_bytes: usize) -> Self {
        Self {
            max_tool_result_bytes,
        }
    }

    /// Prune a tool result that exceeds the size limit.
    /// Truncates at a valid UTF-8 character boundary.
    pub fn prune_result(&self, result: &mut ToolResult) {
        if result.content.len() > self.max_tool_result_bytes {
            let cutoff = floor_char_boundary(&result.content, self.max_tool_result_bytes);
            result.content = format!(
                "{}... [{} bytes truncated]",
                &result.content[..cutoff],
                result.content.len() - cutoff
            );
        }
    }
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut bound = max;
    while bound > 0 && !s.is_char_boundary(bound) {
        bound -= 1;
    }
    bound
}
