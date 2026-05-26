# Session Runtime 优化计划

## 目标

提升 `astrcode-session` 的可读性、安全边界与模块边界，不改变对外 pub API（`Session`、`SessionError`、`TurnError` 等仍从 crate 根导出）。

## P0 — 已完成

| 项 | 做法 |
|---|---|
| CompactionCoordinator | 新增 `compaction_coordinator.rs`，`prepare_context_messages` 统一 auto / reactive compact |
| LifecycleContext 统一 | `SharedTurnContext::from_read_model` + `emit_lifecycle_for_read_model` 复用 |
| StepEnd 显式化 | `on_step_end_best_effort()` 替代 silent `let _ = ...` |

## P1 — 已完成

| 项 | 做法 |
|---|---|
| 拆分 session.rs | `src/session/{mod,events,prompt,turn_entry,children,compact}.rs` |
| TurnError 结构化 | `SessionReadFailed`、`StreamEndedUnexpectedly`、`ProviderBlocked` 等 |
| Turn 事件通道 | `TurnEventTx` 为 turn 内 live 事件发送端 |
| dispatch 改进 | `dispatch_turn_event` durable 失败时 `tracing::warn` |

## P2 — 已完成

| 项 | 做法 |
|---|---|
| SessionCreateParams | 新增 struct；`create_with_id` 委托 `create_with_params` |
| 命名/注释 | TurnRunner 模块注释统一；`drain_completed` signal 语义文档化 |
| ToolRuntimeCapabilities::for_turn | TurnRunner 构造时使用工厂 |

## 已拒绝 / 已移除

| 项 | 说明 |
|---|---|
| `max_steps` / step limit | 不在 `AgentSettings`（effective）中暴露；`TurnRunner` 不强制 step 上限；`TurnError::StepLimitExceeded` 已删除 |

## 测试

- `tool_pipeline`: parallel / sequential / blocked 调度顺序

## 验证

```bash
cargo fmt
cargo test -p astrcode-session -p astrcode-core -p astrcode-server
cargo clippy -p astrcode-session -p astrcode-core -p astrcode-server --all-targets -- -D warnings
```

## SSOT Route A（已完成）

| 项 | 做法 |
|---|---|
| Projection SSOT | `EventStore` + `SessionReadModel` 为 LLM 历史唯一真相源；删除 `TurnState.messages` 双写 |
| 同步 durable | `TurnPublisher::durable` 在 turn 任务内 `await emit_durable`，再进入下一轮 `prepare_stage` |
| Live 分离 | 流式 delta 等仅 `TurnPublisher::live` → `emit_live`；`drive_agent` 不再 channel 写 durable |
| 历史构建 | `llm_request_history`：`visible_messages_for_assembler` / `build_llm_request_messages` |
| 工具事件 | `ToolCallRequested` / `ToolCallCompleted` durable；`commit` 只 `push_tool_result` 到 turn 输出 |

决策：**direct_durable**（非 flush barrier）。

### SSOT 剩余风险（已缓解）

| 风险 | 处理 |
|------|------|
| `read_model()` 性能 | 已打开 session 的 `session_read_model` 为**内存投影 clone**，非 JSONL 全量 replay。Turn 内 [`TurnPublisher`](../crates/astrcode-session/src/turn_publish.rs) 用 `projection::reduce` 增量更新缓存；每 agent step 开始时 `invalidate_model_cache` 以吸收 mid-turn inject |
| `DurableEmitFailed` 半写入 | durable 失败时 `emit_live(ErrorOccurred)` + `run_and_finalize_turn` 再写 durable `ErrorOccurred`；`process_prompt` 对未走 `end_turn_with_error_typed` 的错误补发 `TurnEnd`。已写入 store 的事件为 SSOT，客户端应继续展示投影历史 |
| `BeforeProviderRequest` ReplaceMessages | **刻意非 durable**（单次 LLM 请求覆盖）；`tracing::debug` 标注。模式扩展等 ephemeral 注入依赖此语义 |
| 纯 tool_calls / blocked 工具 | tool 步前若无流式 assistant，补 `AssistantMessageStarted`（live）+ 空文本 `AssistantMessageCompleted`（durable），再 `ToolCallRequested`，与 projection 合并链一致 |

## 暂缓

- `TurnRunner::from_session` 别名：构造依赖过多，收益低
- 服务端 `TurnError` 细粒度客户端错误码：当前经 `HandlerError::Turn` 透传 `Display`，除 `TurnAlreadyRunning` 外未单独映射
- `visible_tools` 缓存
