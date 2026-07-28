//! 会话存储 trait 定义。
//!
//! 本模块定义了会话事件持久化的核心抽象：
//! - [`EventReader`] trait：只读查询能力，满足接口隔离原则（ISP）
//! - [`EventStore`] trait：完整读写能力，继承 `EventReader` 的所有读取方法
//! - [`StorageError`]：存储操作错误类型
//!
//! 通过 trait upcasting（Rust 1.86+），`Arc<dyn EventStore>` 可直接转换为
//! `Arc<dyn EventReader>` 传递给只读消费者，不泄漏写入能力。
//!
//! 本模块不含任何具体存储实现（SQLite/文件等实现位于 `astrcode-storage`）。
//!
//! 实现拆分为 [`traits`](self::traits)(存储抽象)、
//! [`read_model`](self::read_model)(读模型与投影类型)与
//! [`error`](self::error)(错误类型)三个子模块;本根模块仅 re-export,
//! 保证 `astrcode_core::storage::*` 路径与 wire 格式不变。

mod error;
mod read_model;
mod traits;

pub use error::*;
pub use read_model::*;
pub use traits::*;

#[cfg(test)]
mod tests;
