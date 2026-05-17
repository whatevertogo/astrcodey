//! astrcode-server: Backend server runtime.
//!
//! Per-session actor 化的 server crate。
//!
//! - `router`：协议适配 + 前台 session 路由（CommandRouter / CommandRouterHandle）
//! - `session`：per-session actor、directory、bootstrapper、supervisor、turn/slash/compact 行为
//! - `coordinator`：父子会话编排
//! - `events`：客户端通知发布
//! - `bootstrap`：装配 ServerRuntime
//! - `transport` / `http` / `acp`：三种协议接入

pub mod acp;
pub mod bootstrap;
pub mod events;
pub mod http;
pub mod router;
pub mod session;
pub mod transport;

pub(crate) mod config_manager;
pub(crate) mod coordinator;
