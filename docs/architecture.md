# AstrCode 架构设计

Rust 实现的 AI coding agent，~74k 行（Rust ~67.6k + TypeScript ~6.3k），`crates/` 下 21 个 crate + Tauri 桌面壳，支持 TUI、Web 前端、Desktop GUI 和 ACP 四种前端。

核心判断：**EventLog 是事实，Session 是投影，Agent 是无状态运行时。**

---

## 1. 架构

### EventLog — append-only 事实层

- 事件不可变，只追加，`seq` 单调递增
- 工具结果超过阈值时持久化到 artifact 文件，日志只留引用，保持轻量
- 会话恢复 = 从 EventLog 重新投影，fork = 从某个 seq 开始重放

### Session — 投影层 + 进程内运行时

- `Session` 是 durable 事实的写入边界，同时持有进程内瞬态资源
- 持久层：`EventStore` 负责 JSONL 事件日志，`SessionReadModel` 是投影结果
- 瞬态层：`SessionRuntimeState` 持有工具表快照、后台任务管理器、file observation store、`ChildTurnManager` 和 session 级 broadcast channel
- 同一 sid 的所有 `Session` 实例共享同一份 `SessionRuntimeState`（由 `SessionManager` 的 `runtime_states` HashMap 保证），订阅者通过 `Session::subscribe()` 接收该 session 的所有事件
- **`Session::submit(input, turn_id)`** 返回 **`TurnHandle`**：发 `TurnStarted` + `UserMessage`（durable）与 `AgentRunStarted`（live），spawn `TurnRunner` 任务；调用方或 `TurnScheduler` 负责 registry 与 completion
- 不需要"保存 session"——事件已经写回了

### Agent — 无状态运行时

- `TurnRunner` 处理完一个回合即丢弃，不持有跨回合状态
- 从 session projection 读取历史，组装本轮工具和扩展，写回新事件，然后消失
- Agent 崩溃不影响会话——事件已持久化，重新投影即可继续
- Agent 循环核心位于 `astrcode-session` crate（`turn_runner.rs` / `turn_stages.rs`），而非 `astrcode-server`

### 事件流路径

```
Session::emit / Session::append_event
  → EventStore（持久化）
  → SessionRuntimeState::fanout（broadcast）
    → ServerEventBus forwarder（attach 后订阅）
      → ClientNotification broadcast
        → SSE / ACP 客户端
```

`ServerEventBus` 不写 EventStore，只做"session broadcast → 客户端通知"的桥接。
broadcast 发生 lag 时，forwarder 主动推送 `SessionResumed` 快照触发客户端 rehydrate。

### 事件日志格式

每行一个 JSON 对象（JSONL），通过 `EventPayload` 枚举序列化：

```jsonl
{"type":"SessionStarted","session_id":"abc123","timestamp":"...","working_dir":"/project","model_id":"deepseek-chat"}
{"type":"UserMessage","event_id":"evt1","turn_id":"turn1","timestamp":"...","text":"explain main.rs"}
{"type":"TurnStarted","turn_id":"turn1","timestamp":"..."}
{"type":"AssistantMessageStarted","event_id":"evt2","turn_id":"turn1","message_id":"msg1","timestamp":"..."}
{"type":"AssistantTextDelta","event_id":"evt3","delta":"..."}
{"type":"AssistantMessageCompleted","event_id":"evt4","text":"..."}
{"type":"TurnCompleted","turn_id":"turn1","timestamp":"...","finish_reason":"stop"}
```

### Session 树与快照恢复

```
session-A (root)
├── session-B (fork at cursor 42)
│   └── session-D (fork at cursor 15)
└── session-C (fork at cursor 58)
```

每个 fork 创建独立的事件日志。父 session 引用存储在 fork 事件中。

恢复时：加载最近快照 → 从快照偏移量 + 1 重放事件 → 到达当前状态。  
指定 cursor fork 时使用 `EventReader::replay_events_through(max_seq)`，在 seq 超过 fork 点后停止读取，避免大 session 全量 replay。

---

## 2. Server 架构

### 职责边界：session crate vs server crate

| 层 | Crate | 职责 |
|---|---|---|
| **执行** | `astrcode-session` | 单 session 的 turn 执行（`Session::submit` → `TurnRunner`）、工具管线、compact、子 agent `ChildTurnGuard`、事件 emit |
| **编排** | `astrcode-server` | Turn 调度（`TurnScheduler`）、进程内 turn 索引（`TurnRegistry`）、pending 队列、abort 级联、Handler/Actor、HTTP/扩展 API 边界 |
| **持久化** | `astrcode-storage` | EventStore、投影、`replay_events_through` 等读路径 |

**原则**：durable 状态只经 `Session::emit_durable` / `EventStore` 写入；server 不维护第二套 session 历史。进程内「是否有 turn 在跑」以 **`TurnRegistry` 为权威**，durable `phase` 仅用于 UI/投影，可能 stale（`repair_stale` 可对齐）。

### 三层状态

| 层 | 位置 | 含义 |
|---|---|---|
| Durable | `EventStore` / `SessionReadModel` | 持久化、可重放、进程重启后仍成立 |
| 进程 | `TurnRegistry` | 当前是否有 executing turn（优化索引；`has_active_turn` API 只看 registry） |
| 传输 | `CommandHandler.active_session_id` | 进程内/ACP 的「当前会话」；HTTP 在 path 中带 `session_id` |

### TurnScheduler — 统一 turn 生命周期

取代原先分散在 `CommandHandler.active_turns` 与 `SessionManager.ActiveExecutionIndex` 的双索引。主会话与子 session 共用同一条 submit/abort 路径。

**对外 API（调用方通常不接触 `TurnHandle`）：**

| 方法 | 用途 |
|---|---|
| `submit_tracked()` | 提交 turn，返回 `TurnId`，内部自动 spawn completion watcher |
| `submit_tracked_with_notify()` | 同上 + oneshot 完成通知（`TurnSummary`） |
| `submit_and_wait()` | 扩展 API 同步等待；**不** spawn watcher（与 watcher 互斥） |
| `submit_untracked()` | 子 agent：`ChildTurnGuard` 自行持有 `TurnHandle` |
| `accept_user_input()` | 连发 prompt：有 active turn → FIFO 入 `pending_queues`，否则 `submit_tracked` |
| `submit_or_inject()` | 有 active → inject 中途消息；否则 submit |
| `notify_step()` | 后台任务完成等：先 `process_child_completions`，再 inject/submit 标记 |
| `abort()` | 级联 abort 子 session + registry 清理 + 写终态 |
| `process_child_completions()` | drain 已完成子 agent（回收 / notify 父 session） |

**内部模块：**

- `turn_scheduler_completion.rs` — completion watcher 链（`run_chain`）、`TurnSummary`、session idle hook
- `turn_scheduler_queue.rs` — `pending_queues` 唯一 FIFO 队列

**Completion 链（`run_chain`）：**

```
wait turn → release_finished_turn → dequeue 下一条 pending?
  ├─ 有 → 继续循环（不在中间触发 session idle hook）
  └─ 无 → 发送 TurnSummary / 触发 SessionIdleHook → 结束
```

Shutdown（`CancellationToken`）时与 `submit_and_wait` 对称：先 `release_finished_turn` 再返回，避免 graceful restart 后 registry 残留。

### TurnRegistry

per-session 单 entry：`turn_id` + `AbortHandle` + `Arc<Session>`。`register` 失败时 abort 刚启动的 handle（TOCTOU 防护）。`remove_if_matches` 保证只清理匹配的 turn。

### 下一 turn 输入队列（唯一）

所有「当前 turn 运行中，稍后处理」的输入走 `TurnScheduler::accept_user_input` → `pending_queues`（HTTP / TUI / Actor 路径统一）。Turn 结束后由 **completion watcher** 链式 `dequeue_submit_raw`，FIFO 每次弹出一条；不可提交的输入会 skip 并打 warn。

TUI 在 turn 运行中连发 Enter → `SubmitPrompt` → `accept_user_input` → 入队。  
用户 **ESC 中止** 且 composer 仍有内容时 → `SubmitPromptStep`（`submit_or_inject` 语义）在 abort 完成后注入。

### 命令路径与入口

```
                    ┌─────────────────┐
  TUI / exec        │  CommandHandle  │
  HTTP / ACP   ───► │  (Actor 串行)   │──► TurnScheduler
                    └────────┬────────┘
                             │
  Extension API              ▼
  (SessionOperations)   ServerSessionOperations ──► TurnScheduler
                             │
                             ▼
                      SessionManager::open
                             │
                             ▼
                         Session::submit
```

- **写（进程内）**：`ClientCommand` → `CommandHandle::handle` → `CommandHandler`（Actor）
- **写（HTTP）**：REST → `CommandHandle` 显式方法（`create_session`、`submit_input_for_session` 等）
- **写（扩展 API）**：`ServerSessionOperations`（`create_session` / `submit_turn` / `inject_message` 等）
- **Turn**：Handler 层 `accept_user_input_for_session` → `TurnScheduler::accept_user_input`
- **读（HTTP）**：`session_manager().event_store().session_read_model()` → projection DTO → SSE deltas

Handler 层保留 `PromptSubmission`（`Accepted` / `Handled`）用于斜杠命令与 HTTP 映射；scheduler 层用 `UserInputOutcome` / `TurnSummary`。

### Actor 与 Session Idle

`CommandHandle::spawn` 时向 `TurnScheduler` 注册 **SessionIdleHook**：整条 turn 链（含 queued turns）结束后，异步 `send(SessionTurnIdle)` 到 Actor。Actor 仅在 **Completed** 且 registry 无 active turn 时启动 5 分钟 auto-recap 计时；用户提交新 prompt 会取消计时。

### SessionManager（server 侧）

- `sid → SessionRuntimeState` 映射（tool registry、background tasks、`ChildTurnManager`）
- `open` / `create` / `fork` / `delete` / `recycle`；**不**持有 turn 索引
- `register_child_session` — 子 session attach 到 `ServerEventBus`
- **Fork（cursor）**：`replay_events_through(max_seq)` 提前停止 I/O，避免全量 replay 再 filter

### 子 Agent 路径

1. 父 session `spawn_child` → `AgentSessionSpawned`（durable）
2. 扩展 / `ServerSessionOperations::submit_turn`（caller ≠ target）→ `submit_untracked` + `ChildTurnGuard::spawn`
3. Guard 后台等 `TurnHandle`，写 `AgentSessionCompleted/Failed` 到**父** session durable
4. `process_child_completions`（submit 返回前、turn 结束、`ServerEventBus` 收到完成事件）→ recycle / inject notify

`parent_chain` 带环检测与 `max_depth` 上限；`inject_message` 在 inject TOCTOU 时 fallback 到 durable `UserMessage`。

### ServerEventBus

不写 EventStore。`attach(session)` 后 forwarder 将 live 事件转为 `ClientNotification::Event`；收到 `AgentSessionCompleted/Failed` 时 spawn `process_child_completions`；`BackgroundTaskCompleted` 时 spawn `notify_step`。Queued prompt **不在** forwarder 出队（由 completion watcher 负责，避免 registry 竞态）。

### 启动顺序

```
bootstrap_with → TurnScheduler + TurnRegistry
              → ServerEventBus::new(fanout, scheduler)
              → SessionManager::bind_event_bus
              → CommandHandle::spawn  （注册 SessionIdleHook）
```

### Compact 与 Turn 调度

| 路径 | 阻塞条件 | 事件 |
|---|---|---|
| Manual compact（idle） | `registry.has_active` → 409 | `CompactionStarted/Completed` 为 **live-only** |
| In-turn auto/reactive | turn 在 registry 内执行 | 同上，不占 registry 外 slot |

Manual compact 期间 extension `query_session.has_active_turn` 可能为 false（compact 不占 registry）；live UI 仍可能显示 `Compacting`。

---

## 3. Compact 设计

### 结构化输出 contract

Compact 是一个**严格的 XML contract**：

- 模型必须返回 `<analysis>` scratchpad + `<summary>` 块，顺序固定
- Summary 必须包含固定段标题（Primary Request、Files、Errors 等），缺一则拒绝
- 输出有 token 上限（`COMPACT_OUTPUT_TOKEN_CAP`）
- 解析器容忍外层 markdown fence、大小写不敏感的 XML tag，但不容忍结构缺失

### 闭环式 LLM 调用

Compact 通过 `make_compact_request_fn` 从 `LlmProvider` 构造请求闭包：

- 闭包调用 `llm.generate(messages, vec![])`（不传工具），收集流式文本输出并返回
- 闭包传入 `compact_messages_with_fallback`，`LlmContextAssembler` 不持有 provider 引用，保持模型切换时的无状态设计
- compact prompt 禁止工具调用，如果模型尝试调用工具则解析失败，触发 contract repair 重试

### 双路径 + 熔断 + 安全持久化

- 自动压缩和手动压缩统一走 `compact_messages_with_fallback`：先尝试 LLM 生成结构化摘要，失败时降级到确定性模板
- LLM 调用通过闭包注入（`make_compact_request_fn`），`LlmContextAssembler` 不持有 provider 引用
- 确定性 fallback 仅在 LLM 完全不可用时触发，作为最后保障
- **压缩熔断器**（`CompactCircuitBreaker`）：LLM 连续失败达到阈值后，在冷却期内跳过自动压缩
- **可选预测性压缩**：根据 turn token 增长估算，在超出窗口前提前 compact
- **CAS 持久化**：`persist_compact_result` 使用 compare-and-swap；并发写入冲突时安全失败，不污染事件日志

### Post-compact 上下文恢复

Compact 会丢失操作上下文。`post_compact_context` 自动恢复：

- 从历史消息中提取 compact 前最近 read 过的文件路径
- 排除 retained tail 中仍然可见的路径，避免重复
- 按文件预算（数量 + token 数）截断大文件
- 渲染为 `<post_compact_context>` 块注入到新上下文

### Incremental compact

已有摘要时，compact 不是从零重写，而是 merge 模式：保留旧摘要，只合并新信息。

---

## 4. Prompt 工程

### 管道式组装

System prompt 由多个 `PromptContributor` 按固定顺序拼接，每个 contributor 产出 `PromptSection`，按 `PromptSectionOrder` 排序后输出：

```
Identity → System → Task Guidelines → Environment → Communication
→ Rules → Tool Summary → Extension → Extra Instructions
```

稳定部分（Identity、System、Task Guidelines）排在前面，易变部分（Environment、date）排在后面，配合 prompt cache 利用前缀匹配。

### 用户定制

- `~/.astrcode/IDENTITY.md` 覆盖默认身份
- 项目目录下 `AGENTS.md` 作为 project rules（从 working_dir 向上查找，深层覆盖浅层）
- 扩展通过 `PromptBuild` 事件注入 Skills、Agents、PlatformInstructions 等 section

### 设计取舍

借鉴了 Claude Code 的 prompt 结构，但刻意保持精简。对 MoE 稀疏模型（如 GLM），过长的 system prompt 会稀释专家路由效率，因此每个 section 都是简短的规则列表而非长篇叙述。

---

## 5. 工具设计

### 分层工具而非全 bash

9 个内置工具（read / write / edit / patch / find / grep / shell / terminal / task）：

- **为什么不全用 bash**：Codex 可以全 bash 是因为模型足够强。对能力较弱的模型，结构化工具（edit 的 oldStr/newStr 精确替换、patch 的 unified diff）比让模型写 shell 命令更可靠
- edit 支持 `edits` 数组做原子多编辑，先全部验证再一次性写回
- 每个工具声明 `ExecutionMode`：read-only 工具（find/grep/read）标记为 Parallel，写入工具（edit/write/shell）标记为 Sequential
- task 工具管理后台任务（list/cancel），shell 工具支持 `BackgroundPolicy::AutoAfter` 自动后台化

### 工具管线

- `ToolPipeline` 管理完整的 预处理 → 执行 → 提交 流程
- 并行执行用 `JoinSet` 做水位控制（MAX_PARALLEL_TOOL_CALLS = 5），一个任务完成立即补位
- LLM 输出的 JSON 参数解析失败时尝试修复（`parse_and_repair_json`），容错弱模型的格式问题
- 工具结果有**全局消息预算**：总字符数超限时按大小降序优先持久化最大的结果

---

## 6. 扩展 / Hook 系统

### 生命周期钩子

扩展订阅 agent 生命周期事件，可拦截、修改或阻止操作：

- `PreToolUse` / `PostToolUse` — 检查、修改或阻止工具执行
- `BeforeProviderRequest` / `AfterProviderResponse` — 修改消息或阻止 LLM 调用
- `PreCompact` / `PostCompact` — 注入 compact 指令
- `PromptBuild` — 贡献 system prompt 片段
- `TurnStart` / `TurnEnd` / `UserPromptSubmit` — 会话级生命周期

### 三种钩子模式

- **Blocking**：同步执行，可返回 Block / ModifiedInput / ModifiedResult
- **NonBlocking**：即发即弃，使用快照上下文，不阻塞主流程
- **Advisory**：结果仅记录日志，不强制执行

### 快捷键与状态栏注册

扩展通过 `Registrar` 注册交互能力：

- **Keybinding 注册** — `Registrar::keybinding()` 注册快捷键（如 `Shift+Tab`），绑定到扩展命令
- **StatusItem 注册** — `Registrar::status_item()` 贡献状态栏条目，运行时通过 `StatusItemUpdate` 通知动态更新

这些能力随 `ExtensionCommandList` 通知下发到客户端，TUI 和前端均可渲染。

### 延迟绑定

`ExtensionRuntime` 在 server 完全启动前允许扩展注册工具，等 session projection 就绪后通过 `bind()` 注入实际能力。子 agent 嵌套深度限制为 3 层，原子计数器保护。

### 插件化模式系统

Mode 扩展已从内置逻辑迁移为完整插件：通过 `Registrar` 注册 `/mode` 斜杠命令、`Shift+Tab` 快捷键和状态栏项。核心系统不硬编码任何业务逻辑，一切行为能力都通过扩展注册。

### 当前状态

内部插件实现（MCP client / Skill / Agent-Tool / Todo / Mode）统一依赖扩展 SDK；外置扩展通过 s5r 子进程加载，并在 `Initialize.metadata` 中声明宿主能力。

---

## 7. 前后端分离

借鉴 OpenCode 的架构，**两条传输路径**：

| 路径 | 使用者 | 机制 |
|------|--------|------|
| **进程内** | TUI、`exec` | `InProcessTransport` → `ClientCommand` → `CommandHandle` |
| **HTTP/SSE + ACP** | Desktop、Web、IDE、外部脚本 | Axum REST + conversation SSE + `/api/acp/ws` |

- **后端**：`astrcode-http-server` 二进制（crate `astrcode-server`）仅启动 HTTP/SSE；`astrcode server` 子命令等价
- **前端**：TUI 不经 HTTP；Desktop/Web 走 `/api/*`
- HTTP 模块：`routes/`（REST + ACP WebSocket）、`projection/`（事件投影）、`stream.rs`（SSE）
- SSE 携带 `cursor`（event seq），断连后可从 cursor 恢复
- broadcast 溢出时发送 `RehydrateRequired` delta

### 传输层（已移除 stdio JSON-RPC 服务端）

- ~~stdio JSON-RPC 外部服务端~~ 已删除；不再维护 `StdioTransport` / `StdioClientTransport`
- `astrcode-client`：`ClientTransport` trait + 进程内实现（在 `astrcode-cli`）
- `astrcode-protocol`：`ClientCommand` / `ClientNotification` 仍用于**进程内**线缆
- HTTP 线缆类型在 `protocol::http`（`ConversationDelta`、`PromptRequest` 等）
- **ACP** 经 HTTP WebSocket（`/api/acp/ws`）暴露，与 REST 共用 `CommandHandle` / `EventFanout`
- **`astrcode acp`** 仅为 stdio↔WebSocket 转发桥（读 `~/.astrcode/run.json`），不 bootstrap server

### Desktop GUI (Tauri)

- **Sidecar 模式**：嵌入 `astrcode-http-server` 作为 sidecar 进程
- **通信方式**：HTTP API + SSE（本地动态端口），通过 `tauri-plugin-http` 绕过 webkit2gtk 网络栈
- **技术栈**：Tauri v2 + React 19 + TypeScript + Tailwind CSS v4
- **状态管理**：Zustand

---

## 8. 运行模式

### Code 模式（默认）

- 完整工具访问权限
- 支持文件读写、编辑、shell 执行
- 适合实际编码任务

### Plan 模式

- 专用 Plan 管理工具
- 计划持久化到 `<session>/plan/plan.md`
- 适合复杂任务的前期规划

两种模式均通过 Mode 扩展插件实现

---

## 9. 关键设计决策

### Session-First 事件溯源

Session 是唯一的持久事实来源。所有状态变化都以不可变事件形式写入 JSONL。恢复时从事件重放重建状态，支持 fork/branch/replay。Agent 是临时的——从 Session 事件重建，处理后写回事件，可随时丢弃和重建。

### Extension-First 架构

核心只保留必须通用的能力（agent loop、hooks、context compaction、built-in tools）。其他能力（skills、MCP、自定义工具、模式切换）通过扩展接入。Mode 系统从内置逻辑迁移到插件即是这一架构的验证。

### 工具-First 而非 extension-First

工具是运行时基础能力，extension、SDK、MCP 都只是 tool source。所有工具走同一条执行路径，确保可观测性和统一调度。

### 前后端分离

前端不负责业务逻辑，只负责交互和渲染。后端通过 HTTP/SSE 提供标准化 API，支持多种前端形态（TUI/GUI/Headless）。
