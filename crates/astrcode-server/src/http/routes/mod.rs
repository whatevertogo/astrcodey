//! 各 HTTP 路由按职责分组的子模块。

pub(in crate::http) mod acp;
pub(in crate::http) mod config;
pub(in crate::http) mod extensions;
pub(in crate::http) mod lifecycle;
pub(in crate::http) mod models;
pub(in crate::http) mod sessions;
