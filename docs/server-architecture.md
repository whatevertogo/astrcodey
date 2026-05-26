# astrcode-server 架构速览

## 三层状态

| 层 | 位置 | 含义 |
|---|---|---|
| Durable | `EventStore` / session `phase` | 持久化、可重放、进程重启后仍成立 |
| 进程 | `TurnRegistry` | 当前是否有活跃 turn 任务（优化索引，需 `repair_stale` 对齐） |
| 传输 | `CommandHandler.active_session_id` | stdio/ACP 的「当前会话」；HTTP 在 path 中带 `session_id` |

## 下一 turn 输入队列（唯一）

所有「当前 turn 运行中，稍后处理」的输入走 `TurnScheduler::notify_turn` → `pending_queues`。
`TurnCompleted` 后由 `on_turn_completed` **FIFO 每次弹出一条** 并 `submit`，与 HTTP 连发 prompt 行为一致。

## 启动顺序

```
bootstrap_with → TurnScheduler + TurnRegistry
              → ServerEventBus::new(fanout, scheduler)  // scheduler 构造时注入
              → SessionManager::bind_event_bus
              → CommandHandle::spawn
```

## 命令路径（摘要）

- **写**：`ClientCommand` / HTTP POST → `CommandHandle` → `CommandHandler`（Actor 串行）
- **Turn**：`start_turn` / `notify_turn` → `TurnScheduler::submit`
- **读（HTTP）**：`ServerRuntime::session_manager()` / `event_store()` → projection DTO
