# Session-First 事件溯源

## 设计哲学

Session 是架构的核心。它不是"聊天记录"，而是**事件日志**——所有状态变化的不可变记录。

Agent 是临时的——从 Session 事件重建，处理后写回事件，可随时丢弃和重建。

## Session 生命周期

```
CreateSession → SessionStart event
SubmitPrompt  → UserMessage event → TurnStart → Agent processes → TurnEnd
              → AssistantMessage events
              → ToolCall events
              → ToolResult events
ForkSession   → SessionFork event → new independent branch
Compact       → Compaction event
```

## 事件日志格式

每行一个 JSON 对象（JSONL）：

```jsonl
{"type":"session_start","session_id":"abc123","timestamp":"...","working_dir":"/project","model_id":"deepseek-chat"}
{"type":"user_message","event_id":"evt1","turn_id":"turn1","timestamp":"...","text":"explain main.rs"}
{"type":"turn_start","turn_id":"turn1","timestamp":"..."}
{"type":"assistant_message","event_id":"evt2","turn_id":"turn1","message_id":"msg1","timestamp":"...","text":"..."}
{"type":"turn_end","turn_id":"turn1","timestamp":"...","finish_reason":"stop"}
```

## Agent 重建

Agent 通过重放 session 事件日志重建：

1. 读取 SessionStart → 获取 working_dir、model_id
2. 读取 UserMessage → 构建用户消息历史
3. 读取 AssistantMessage → 构建 assistant 响应历史
4. 读取 ToolCall + ToolResult → 构建工具调用上下文
5. 读取 Compaction → 应用上下文压缩

重建后的 Agent 状态与崩溃前完全一致。

## Session 树

```
session-A (root)
├── session-B (fork at cursor 42)
│   └── session-D (fork at cursor 15)
└── session-C (fork at cursor 58)
```

每个 fork 创建独立的事件日志。父 session 引用存储在 SessionFork 事件中。

## 快照与恢复

定期创建快照（保存内存状态摘要到事件偏移量）。恢复时：

1. 加载最近快照
2. 从快照偏移量 + 1 开始重放事件
3. 到达当前状态
