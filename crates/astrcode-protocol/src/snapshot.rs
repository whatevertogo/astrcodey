//! 会话快照类型模块——用于重连和状态传输。
//!
//! 从 [`events`](crate::events) 模块重新导出快照相关的类型，
//! 方便外部 crate 通过 `astrcode_protocol::snapshot::SessionSnapshot` 等路径引用。

pub use crate::events::{MessageDto, SessionListItem, SessionSnapshot};
