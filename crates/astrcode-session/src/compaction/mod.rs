//! 同步 Session Compact。
//!
//! [`pipeline`] 拥有一次 compact 的完整生命周期；manual 与 turn 调用方只准备各自的
//! runtime 输入，不维护第二套状态机。

mod circuit_breaker;
mod manual;
mod persistence;
mod pipeline;
mod turn;

pub(crate) use circuit_breaker::CompactCircuitBreaker;
pub use manual::{ManualCompactionOutcome, compact_manual_session};
pub(crate) use turn::{
    CompactionHost, PreparedProviderHistory, plan_auto_compaction, prepare_provider_history,
    run_reactive_compaction,
};
