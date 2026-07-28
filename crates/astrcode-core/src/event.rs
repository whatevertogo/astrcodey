//! 统一的运行时事件与持久化事件类型。
//!
//! 本模块定义了 astrcode 平台中所有事件的核心数据结构，包括：
//! - [`Phase`]：会话执行阶段的枚举
//! - [`EventPayload`]：事件载荷的统一枚举类型
//! - [`Event`]：携带会话/轮次标识和存储序号的事件信封
//!
//! 本模块只做类型与 wire 格式定义：不追加、不派发、不投影事件，那些属于
//! `astrcode-storage` / `astrcode-session`。
//!
//! 实现拆分为 [`envelope`](self::envelope)(信封 + 自定义序列化)与
//! [`payload`](self::payload)(载荷枚举本体)两个子模块;本根模块仅 re-export,
//! 保证 `astrcode_core::event::*` 路径与 wire 格式不变。

mod envelope;
mod payload;

pub use envelope::{Event, Phase, ToolOutputStream};
pub use payload::EventPayload;

#[cfg(test)]
mod tests;
