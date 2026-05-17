//! Compact 子系统：hook 桥接、post-compact 上下文恢复、payload helpers。

pub mod hooks;
pub mod payloads;
pub mod post_context;

pub use hooks::{
    CompactHookContext, collect_compact_instructions, compact_trigger_name, dispatch_post_compact,
    make_compact_request_fn,
};
pub use payloads::{compact_boundary_payload, session_continued_from_compaction_payload};
pub use post_context::enrich_post_compact_context;
