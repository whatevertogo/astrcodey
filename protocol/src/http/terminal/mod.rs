//! Terminal HTTP DTO 命名空间。
//!
//! terminal surface 从第一天开始按路径版本化，因此这里显式暴露 `v1` 线缆类型，
//! 供 server route、client facade 与 conformance tests 共享。

pub mod v1;
