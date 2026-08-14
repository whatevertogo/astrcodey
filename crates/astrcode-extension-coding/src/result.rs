use std::{collections::BTreeMap, time::Instant};

use astrcode_extension_sdk::tool::ToolResult;

pub(crate) fn success(
    started_at: Instant,
    content: impl Into<String>,
    metadata: BTreeMap<String, serde_json::Value>,
) -> ToolResult {
    ToolResult {
        content: content.into(),
        is_error: false,
        error: None,
        metadata,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

pub(crate) fn completed_error(
    started_at: Instant,
    content: impl Into<String>,
    metadata: BTreeMap<String, serde_json::Value>,
) -> ToolResult {
    let content = content.into();
    ToolResult {
        error: Some(content.clone()),
        content,
        is_error: true,
        metadata,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}
