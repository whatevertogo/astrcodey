# 从 AgentLoop 出发
---

## 第一讲：AgentLoop 的本质——一个 while 循环

### 核心问题

> AI Agent 到底在干什么？

Agent 就是一个 **"思考 → 行动 → 观察"** 的循环，直到 LLM 认为它做完了。

但是其实a/在这上面加了一步，思考行动总结观察的循环，通过提示词实现要求agent在每一步之后给出简短的总结，这也是为什么你们的claude code中会有很多每一步总结的输出的原因。这也符合论文react，self-reflection的思路

### 架构全景

```
用户输入
   ↓
┌──────────────────────────────────────────────┐
│  AgentLoop::process_prompt                   │
│                                              │
│  ┌─ loop ──────────────────────────────────┐ │
│  │  1. 准备上下文（可能触发 auto compact）   │ │
│  │  2. 调用 LLM（流式）                    │ │
│  │  3. 消费 LLM 事件流                     │ │
│  │     ├─ ContentDelta → 累积文本          │ │
│  │     ├─ ToolCallStart → 记录工具调用      │ │
│  │     └─ Done → 判断是否有工具调用         │ │
│  │  4. 如果有工具调用：                     │ │
│  │     ├─ 预处理（JSON 修复 + 扩展钩子）    │ │
│  │     ├─ 执行（并行/串行）                 │ │
│  │     ├─ 结果追加到消息历史                │ │
│  │     └─ continue → 回到步骤 1            │ │
│  │  5. 如果没有工具调用：                   │ │
│  │     └─ break → 返回最终输出              │ │
│  └──────────────────────────────────────────┘ │
└──────────────────────────────────────────────┘
   ↓
AgentTurnOutput { text, tool_results, ... }
```

### 核心代码

**入口函数** — [`process_prompt()`](../../crates/astrcode-server/src/agent/loop.rs:232)

```rust
// crates/astrcode-server/src/agent/loop.rs:232
pub(crate) async fn process_prompt(
    &self,
    user_text: &str,
    history: Vec<LlmMessage>,
    event_tx: Option<mpsc::UnboundedSender<AgentSignal>>,
) -> Result<AgentTurnOutput, AgentError> {
    // ... 初始化 ...

    // ── Agent 主循环 ──
    // 每轮迭代：调用 LLM → 处理响应 → 执行工具 → 将结果追加到消息历史 → 下一轮
    loop {
        // 步骤 1: 准备上下文（含 auto compact）
        let (system_messages, prepared_context, compacted) = self
            .prepare_provider_context(&mut messages, &tools, &ext_ctx, &event_tx)
            .await?;

        // 步骤 2: 调用 LLM（流式）
        let mut rx = self
            .start_provider_stream(send_messages, &tools, &event_tx, &ext_ctx)
            .await?;

        // 步骤 3: 消费 LLM 事件流
        while let Some(event) = rx.recv().await {
            match event {
                LlmEvent::ContentDelta { delta } => { /* 累积文本 */ },
                LlmEvent::ToolCallStart { call_id, name, arguments } => {
                    tool_calls.push(PendingToolCall { call_id, name, arguments });
                },
                LlmEvent::Done { finish_reason } => {
                    if tool_calls.is_empty() {
                        // 步骤 5: 无工具调用 → 返回
                        return Ok(AgentTurnOutput { text: final_text, ... });
                    }
                    break; // 步骤 4: 有工具调用 → 继续循环
                },
                // ...
            }
        }

        // 步骤 4: 执行工具调用
        let prepared_tool_calls = self.prepare_tool_calls(&tool_calls, &tools, &event_tx).await?;
        self.execute_and_commit_tool_calls(ExecuteToolCalls { ... }).await?;
        // → 回到 loop 顶部
    }
}
```

**驱动函数** — [`drive_agent()`](crates/astrcode-server/src/agent/loop.rs:80)

```rust
// crates/astrcode-server/src/agent/loop.rs:80
pub(crate) async fn drive_agent<F, Fut>(
    agent: &AgentLoop,
    user_text: &str,
    history: Vec<LlmMessage>,
    mut on_signal: F,
) -> (Result<AgentTurnOutput, AgentError>, bool)
where
    F: FnMut(AgentSignal) -> Fut,
    Fut: Future<Output = ()>,
{
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let agent_future = agent.process_prompt(user_text, history, Some(event_tx));
    tokio::pin!(agent_future);

    // select! 同时等待 agent 完成和事件到达
    let output = loop {
        tokio::select! {
            result = &mut agent_future => break result,
            payload = event_rx.recv(), if !events_closed => {
                match payload {
                    Some(signal) => on_signal(signal).await,
                    None => events_closed = true,
                }
            },
        }
    };
    // ...
}
```

### 关键设计决策

| 决策 | 原因 |
|------|------|
| 用 `loop` 而非递归 | 避免栈溢出，状态管理更清晰 |
| `event_tx` 为 `Option` | `None` 时静默执行（用于 compact 子调用） |
| `drive_agent` 用 `select!` | 实时转发事件到 handler 层，不阻塞 agent |

### 延伸讨论

- 为什么不用状态机（FSM）？→ LLM 输出非确定性，状态转移不固定
- 为什么 `drive_agent` 和 `process_prompt` 分开？→ 关注点分离：`process_prompt` 管逻辑，`drive_agent` 管事件流
