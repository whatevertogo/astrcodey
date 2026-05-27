//! 集成测试辅助：轻量 runtime，避免完整 bootstrap（MCP / 真实 LLM）。

mod in_process;

pub use in_process::in_process_test_runtime;
