//! astrcode-context: Context window management.
//!
//! Token estimation, tool result budgeting, micro-compaction,
//! pruning, LLM-driven compaction, and file access tracking.

pub mod budget;
pub mod compaction;
pub mod file_access;
pub mod pruning;
pub mod settings;
pub mod token_usage;
