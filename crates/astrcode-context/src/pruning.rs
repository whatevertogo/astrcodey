//! Pruning pass — removes stale/oversized content from context.

use astrcode_core::tool::ToolResult;

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
    pub fn prune_result(&self, result: &mut ToolResult) {
        if result.content.len() > self.max_tool_result_bytes {
            result.content = format!(
                "{}... [{} bytes truncated]",
                &result.content[..self.max_tool_result_bytes],
                result.content.len() - self.max_tool_result_bytes
            );
        }
    }
}
