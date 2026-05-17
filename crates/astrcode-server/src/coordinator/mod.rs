//! Coordinator 子系统：父子会话编排。
//!
//! `AgentSessionCoordinator` 处理扩展返回的 `RunSession` 声明式结果，把子
//! session 的创建、prompt、turn 启动委托给对应的 `SessionActor`。

mod agent;

pub(crate) use agent::AgentSessionCoordinator;
