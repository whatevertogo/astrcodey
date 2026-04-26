//! Runtime-owned context window management.
//!
//! This module contains the local prompt-window work that must happen inside
//! the execution loop: token estimation, tool-result pruning, idle cleanup,
//! aggregate tool-result budgeting, file recovery, and LLM-backed compaction.

pub(crate) mod compaction;
pub(crate) mod file_access;
pub(crate) mod micro_compact;
pub(crate) mod prune_pass;
pub(crate) mod request;
pub(crate) mod settings;
pub(crate) mod token_usage;
pub(crate) mod tool_result_budget;
pub(crate) mod tool_results;

pub(crate) use settings::ContextWindowSettings;
