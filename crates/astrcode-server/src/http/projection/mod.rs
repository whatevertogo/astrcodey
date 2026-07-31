//! Event → DTO 投影。
//!
//! 子模块按职责切分：
//! - `args`: 工具参数 → 折叠摘要文本。
//! - `blocks`: payload/message → ConversationBlockDto。
//! - `live`: 实时 event → ConversationDeltaDto。
//! - `replay`: 历史 event → ConversationDeltaDto。
//! - `snapshot`: session read model → ConversationSnapshotResponseDto。

use std::collections::BTreeMap;

pub(in crate::http) mod args;
pub(in crate::http) mod blocks;
pub(in crate::http) mod live;
pub(in crate::http) mod replay;
pub(in crate::http) mod snapshot;

pub(in crate::http) fn session_title_from_working_dir(working_dir: &str) -> String {
    std::path::Path::new(working_dir)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(working_dir)
        .to_string()
}

fn non_empty_metadata(metadata: &BTreeMap<String, serde_json::Value>) -> Option<serde_json::Value> {
    (!metadata.is_empty()).then(|| {
        serde_json::Value::Object(
            metadata
                .clone()
                .into_iter()
                .collect::<serde_json::Map<_, _>>(),
        )
    })
}
