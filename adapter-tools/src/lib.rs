//! # Astrcode 内置工具 + Agent 协作工具
//!
//! 本库实现 Astrcode 编码代理（agent）的本地工具集：
//! - **core builtin tools**（`builtin_tools`）：readFile、writeFile、editFile、apply_patch、
//!   findFiles、grep、shell、tool_search
//! - **agent tools**（`agent_tools`）：spawn、send、observe、close
//!
//! 所有工具均实现 `astrcode_runtime_contract::tool::Tool` trait。
//!
//! ## 架构约束
//!
//! - 本 crate 仅依赖 `astrcode-core`、`astrcode-runtime-contract` 与支撑库，不依赖 runtime 实现
//! - 所有工具通过 `Tool` trait 统一接口暴露，由 `runtime` 层统一调度
//! - 工具执行结果包含结构化 metadata，供前端渲染（如终端视图、diff 视图）

pub mod agent_tools;
pub mod builtin_tools;

pub use agent_tools::{
    CloseAgentTool, CollaborationExecutor, ObserveAgentTool, SendAgentTool, SpawnAgentTool,
};

#[cfg(test)]
pub(crate) mod test_support;
