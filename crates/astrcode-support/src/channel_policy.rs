//! Tokio mpsc 容量策略与 channel 分类说明。
//!
//! # 分类（当前策略）
//!
//! | 类别 | 策略 | 代表路径 |
//! |------|------|----------|
//! | **Fan-out + live UI** | **unbounded** | [`EventFanout`](crate::event_fanout::EventFanout) |
//! | **Turn 事件桥** | **unbounded** + 有序 durable worker | `TurnEventTx` / `spawn_event_bridge` |
//! | **控制面 / 低频信号** | bounded(小) | CLI 命令、scheduler finish、child 完成、stdio |
//! | **外部 I/O 单消费者** | unbounded | `LlmEvent` provider → turn |
//!
//! 事件路径用 unbounded 是为了：**不丢 live 事件、不踢 SSE 订阅、durable 写入保序**。
//! 控制面用 bounded 是为了：**有界内存、对慢 handler 施加背压**。
//!
//! 未单独设计：MCP pool 响应 multiplex、extension-sdk peer 事件。

/// 客户端通知 fan-out 容量参数（保留兼容；[`EventFanout`](crate::event_fanout::EventFanout) 内部为
/// unbounded）。
pub const EVENT_FANOUT_CAPACITY: usize = 1024;

/// 进程内 CLI → server 命令队列。
pub const CLIENT_COMMAND_CAPACITY: usize = 128;

/// 工具调度器 finish 信号。
pub const TOOL_SCHEDULER_FINISH_CAPACITY: usize = 256;

/// 子 session 完成通知队列。
pub const CHILD_SESSION_COMPLETE_CAPACITY: usize = 256;

/// JSON-RPC stdio 读线程 → handler 队列。
pub const STDIO_MESSAGE_CAPACITY: usize = 128;
