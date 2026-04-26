//! # 会话存储模块
//!
//! 提供基于 JSONL 文件系统的完整会话生命周期管理：
//!
//! - **事件持久化**：通过 [`EventLog`] 以 append-only 方式写入 `StoredEvent`
//! - **事件回放**：通过 [`EventLogIterator`] 流式读取历史事件
//! - **会话管理**：通过 [`FileSystemSessionRepository`] 直接实现 `EventStore`，
//!   提供创建、打开、列出、删除会话的统一接口
//! - **并发控制**：通过文件锁（`active-turn.lock`）防止多进程同时写入同一会话
//! - **路径解析**：自动将工作目录映射到 `~/.astrcode/projects/<project>/sessions/` 下的分桶路径
//!
//! ## 文件布局
//!
//! ```text
//! ~/.astrcode/projects/<project>/
//! └── sessions/
//!     └── <session-id>/
//!         ├── session-<session-id>.jsonl   # 事件日志（append-only）
//!         ├── active-turn.lock             # 文件锁（互斥写入）
//!         └── active-turn.json             # 锁持有者元数据
//! ```

mod batch_appender;
mod checkpoint;
mod event_log;
mod iterator;
mod paths;
mod query;
mod repository;
mod turn_lock;

/// JSONL 事件日志 writer，负责以 append-only 方式持久化会话事件。
pub use event_log::EventLog;
/// 逐行流式读取 JSONL 会话事件的迭代器。
pub use iterator::EventLogIterator;
/// 基于本地文件系统的会话仓储实现，直接服务 `EventStore`。
pub use repository::FileSystemSessionRepository;
