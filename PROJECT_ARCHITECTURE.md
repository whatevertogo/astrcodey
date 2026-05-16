# PROJECT_ARCHITECTURE

## 状态与范围

本文描述 astrcode 的实际架构。
它同时涵盖已落地的实现和设计方向上的取舍。

## 架构方向

astrcode v2 的目标是一套 session-first、extension-first、核心极简的 coding system。
它的整体形态是：

- 一个后端 server
- 多种 frontend
- 基于追加事件的会话持久化
- 由 session 历史重建的临时 agent
- 少量内置核心能力
- 其余能力全部通过扩展接入

这套设计明确避免把系统做成一个硬编码功能不断堆积的单体。
核心只保留那些必须对所有场景都成立的能力：

- agent loop
- hooks 与 extension runtime
- context compaction
- built-in tools

Session owns facts.
TurnRunner executes.
Extensions contribute.
Context derives.
Handler adapts protocol.


skills、agent profiles、自定义工具、prompt context providers 都不作为核心内建能力看待，而是统一通过扩展系统加载。

## 核心原则

### 1. Session-first

Session 是系统唯一的持久事实来源。
所有关键状态变化都以不可变事件的形式写入持久层。
任何时候都可以通过事件日志和快照重建 session 状态。

这意味着两件事：

- 持久化单元是 session，不是 agent
- 恢复、fork、branch、replay 首先是存储与事件模型问题

### 2. Agent 是临时处理器

Agent 不是持久对象，而是从 session 历史中重建出来的临时处理器。
它负责处理一个 turn，发射事件，落盘，然后可以被丢弃。

这样做能把长期状态从内存对象里剥离出来，让重启恢复、并发会话和故障恢复都更清晰。

### 3. Extension-first

扩展系统是整个架构的主要定制边界。
核心只暴露生命周期事件和注册点，工作流相关、领域相关、团队相关的能力都应该放在扩展里，而不是继续侵入核心。

### 4. 前后端分离

Frontend 不负责业务核心逻辑。
它们通过 stdio 上的类型化协议或 HTTP/SSE 与 server 通信，主要承担交互、渲染和连接职责。

Server 对外暴露三种接入方式：

- **stdio transport**：TUI 和 exec 前端通过 JSON-RPC 2.0 over stdio 直连 server 进程内嵌的 transport handler
- **HTTP/SSE**：桌面应用（Tauri）和第三方客户端通过 HTTP POST 发命令、SSE 流接收事件
- **ACP adapter**：标准 Agent Client Protocol 客户端通过 `astrcode acp` 子命令启动的 stdio JSON-RPC 服务接入

三种方式最终都汇入同一个 `CommandHandler` actor，保证命令处理逻辑一致。

### 5. 基础设施保持克制

v2 首期刻意选择保守、明确、容易落地的基础设施组合：

- 纯 Rust workspace
- JSON-RPC 2.0 over stdio（内部协议）
- Agent Client Protocol over stdio（标准 ACP 接入）
- HTTP/SSE（桌面端接入）
- JSONL 事件日志加快照
- v2 范围内不做执行沙箱

## 系统拓扑

```text
┌──────────────────────────────────────────────────────────────────────┐
│                              Frontends                               │
│                                                                      │
│  astrcode-tui      astrcode-exec      src-tauri      ACP 客户端     │
└────────┬───────────────┬───────────────┬────────────────┬───────────┘
         │               │               │                │
         │ JSON-RPC      │ JSON-RPC      │ HTTP/SSE       │ ACP JSON-RPC
         │ over stdio    │ over stdio    │                │ over stdio
         ▼               ▼               ▼                ▼
┌──────────────────────────────────────────────────────────────────────┐
│                          astrcode-server                              │
│                                                                      │
│  ┌─────────────┐  ┌──────────┐  ┌──────────┐                        │
│  │ Transport   │  │ HTTP     │  │ ACP      │   ← 三种接入适配器     │
│  │ (stdio)     │  │ (axum)   │  │ (stdio)  │                        │
│  └──────┬──────┘  └────┬─────┘  └────┬─────┘                        │
│         └──────────────┼─────────────┘                              │
│                        ▼                                             │
│                 ┌──────────────┐                                     │
│                 │CommandHandler│   ← actor (mpsc) 统一命令入口       │
│                 └──────┬───────┘                                     │
│                        ▼                                             │
│  SessionManager ─ Agent Loop ─ Config Service ─ Broadcast Events    │
└──────────────────────┬───────────────────────────┬───────────────────┘
                       │                           │
                       ▼                           ▼
┌──────────────────────────────┐  ┌──────────────────────────────────┐
│ Runtime Services             │  │ Extension Runtime                │
│                              │  │                                  │
│ astrcode-session             │  │ astrcode-extensions              │
│ astrcode-tools               │  │ ├─ astrcode-extension-mode       │
│ astrcode-ai                  │  │ ├─ astrcode-extension-skill      │
│ astrcode-context             │  │ ├─ astrcode-extension-todo-tool  │
│ astrcode-storage             │  │ ├─ astrcode-extension-agent-tools│
│                              │  │ ├─ astrcode-extension-mcp        │
│                              │  │ └─ disk-loaded native extensions │
└───────────────┬──────────────┘  └──────────────────────────────────┘
                │
                ▼
┌──────────────────────────────────────────────────────────────────────┐
│                          Session Storage                              │
│                                                                      │
│         append-only JSONL event log + snapshots + file locks         │
└──────────────────────────────────────────────────────────────────────┘
```

整体运行模型：一个后端，多种前端（TUI / exec / HTTP 客户端 / ACP 客户端），session 作为持久真相。

## Workspace 与分层

v2 设计把系统拆成 `crates/` 下的 18 个 crate，并分为五层。
依赖方向严格单向：上层可以依赖下层，下层不能反向依赖上层。

### Layer 0：Foundation

- `astrcode-core`：共享领域类型与核心 trait，例如 tool、LLM provider、config 抽象、extension contract、prompt 组装 trait
- `astrcode-support`：宿主环境集成辅助能力，例如路径解析、shell 检测、tool result 持久化工具
- `astrcode-log`：结构化日志初始化与格式化工具，无内部依赖，被上层 crate 直接引用

### Layer 1：Services

- `astrcode-ai`：OpenAI 兼容 provider、SSE 流解析、重试、缓存追踪
- `astrcode-tools`：内置工具、工具注册表、执行包装、agent 协作工具
- `astrcode-storage`：JSONL event log、snapshot、config 持久化、锁
- `astrcode-context`：token 估算、tool result 预算、裁剪、压缩、文件恢复、prompt engine
- `astrcode-session`：会话运行时，包含 session handle、turn 执行、事件总线、工具管线

### Layer 2：Extensions

- `astrcode-extensions`：扩展加载、生命周期分发、hook 执行策略、超时处理、能力注册、WASM 扩展运行时
- `astrcode-extension-mode`：Agent 运行模式切换（Code / Plan），包含 Exit Gate、计划 Artifact 持久化、heading 校验
- `astrcode-extension-skill`：斜杠命令技能发现与分发
- `astrcode-extension-todo-tool`：进度追踪 Todo 工具
- `astrcode-extension-agent-tools`：子 Agent 委派（Agent 工具）
- `astrcode-extension-mcp`：MCP 协议客户端（stdio）、工具发现

这一层是 v2 的主要定制边界。

### Layer 3：Server

- `astrcode-protocol`：类型化 JSON-RPC 命令、事件、UI 子协议、版本协商、HTTP DTO
- `astrcode-server`：session 生命周期、agent 编排、config service、transport handling、并发控制
  - `transport/`：stdio JSON-RPC 适配器，供 TUI 和 exec 使用
  - `http/`：Axum HTTP/SSE 入口，供桌面端和第三方客户端使用
  - `acp/`：ACP (Agent Client Protocol) stdio 适配器，桥接标准 ACP 客户端

### Layer 4：Frontend

- `astrcode-client`：面向 transport 的类型化 client 抽象
- `astrcode-tui`：交互式终端前端
- `astrcode-exec`：无头单次执行前端
- `astrcode-cli`：用户入口，子命令分发：
  - `tui`（默认）：启动 TUI 交互模式
  - `exec`：无头单次执行
  - `server`：启动 HTTP/SSE 后端服务
  - `acp`：启动 ACP stdio 服务器
  - `version`：版本信息
- `src-tauri`：Tauri 桌面应用前端，通过 HTTP 与 `astrcode-server` sidecar 通信

## 核心运行模型

### Session 模型

Session 是基于 JSONL 和定期 snapshot 的追加事件流。
它记录：

- 用户输入
- assistant 输出增量与最终消息
- tool call 与 tool result
- compaction 相关事件
- 恢复与协作用到的生命周期事件

每个 session 都以 `SessionStart` 事件开头。
fork 会基于某个 cursor 复制历史并创建新的 session，同时带上父 session 引用。

### Agent 模型

Agent 自身不持有持久状态。
每次 turn 开始时，server 从 session 的持久事件中重建 agent 状态。
turn 结束后，新事实再写回 session。

这个模型直接支持：

- 重启后的确定性恢复
- 多个独立 session 并存
- session tree 上的 fork / branch 操作
- 处理逻辑与持久化边界清晰分离

### 并发模型

Server 支持多个 session 同时活跃。
并发规则是：

- 单个 session 内一次只处理一个 turn
- 不同 session 之间可以并行处理

这样既能保证每个 session 内部事件历史线性一致，又能保留整体吞吐。

## Turn 处理流水线

Agent loop 是整个运行时的中心。
一个 turn 的高层流程如下：

```text
submit_prompt
  -> load or rebuild agent from session
  -> UserPromptSubmit hooks
  -> assemble prompt from contributors
  -> call LLM provider and stream deltas
  -> parse requested tool calls
  -> BeforeToolCall hooks
  -> execute tools
  -> AfterToolCall hooks
  -> append and broadcast events
  -> loop back into LLM if tool results require continuation
  -> TurnEnd hooks
```

这条流水线有几个关键特征：

- 全流程事件化，frontend 可以增量渲染
- tool execution 属于 turn 主流程，不是旁路能力
- tool error 会以结构化结果回送给模型
- abort 会保留终止点之前已经完成的结果

## Prompt 组装

Prompt 组装被视为独立子系统，而不是简单的字符串拼接。
设计采用 contributor 模式，由各 contributor 产出带元数据的 block，例如优先级、条件、依赖和缓存层级。

Prompt composer 负责：

- 收集内置 contributor 和扩展 contributor 产出的 block
- 去重与排序
- 解析模板变量
- 校验依赖关系
- 生成最终用于 LLM 调用的 prompt plan

Prompt cache 被拆成四层：

- Stable
- SemiStable
- Inherited
- Dynamic

这样可以在减少重复组装成本的同时，保留每个 turn 所需的动态上下文。

## 扩展系统

扩展系统不是附属功能，而是架构核心设计的一部分。
它定义了非通用能力如何进入系统。

### Extension trait

每个扩展实现 `astrcode-core::extension::Extension` trait，提供：

- `id()` — 唯一字符串标识
- `hook_subscriptions()` — 声明订阅的生命周期事件、hook mode 和优先级
- `on_event()` — 异步事件处理，返回 `HookEffect`
- `tools()` / `tools_for(working_dir)` — 贡献的工具定义（后者用于 MCP 等需要按工作目录定制的场景）
- `execute_tool()` — 按名执行工具
- `slash_commands()` / `slash_commands_for()` — 贡献的斜杠命令
- `execute_command()` — 执行斜杠命令
- `tool_prompt_metadata()` — 工具的 prompt 提示

### 生命周期事件 (`ExtensionEvent`)

`SessionStart`、`SessionShutdown`、`TurnAborted`、`PromptBuild`、`PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`BeforeProviderRequest`、`AfterProviderResponse`、`PreCompact`、`PostCompact`

### Hook Mode

- `Blocking`：同步执行（有超时），可以返回 `Block`、`ModifiedInput`、`ModifiedResult`、`ModifiedMessages`、`AppendMessages`、`PromptContributions`、`CompactContributions` 等效果
- `NonBlocking`：fire-and-forget，在 snapshot 上下文中派发，不阻塞主路径
- `Advisory`：执行但仅记录结果

### 内置扩展注册顺序（在 `bootstrap` 中确定）

1. `astrcode-extension-agent-tools` — 子 Agent 委派
2. `astrcode-extension-mcp` — MCP 协议客户端
3. `astrcode-extension-skill` — 斜杠命令技能发现
4. `astrcode-extension-todo-tool` — 进度追踪
5. `astrcode-extension-mode` — Code/Plan 模式切换

先注册的扩展在工具名冲突时优先。之后从磁盘加载 native 扩展。

### ExtensionRuntime 核心组件

- **`ExtensionRunner`**：管理扩展注册、按优先级分派 hook、超时执行、工具/命令收集
- **`ExtensionLoader`**：从磁盘发现并加载 native 扩展（`.dll`/`.so`），通过 `libloading` + FFI 适配
- **WASM 扩展运行时**：基于 wasmtime 的沙箱化扩展执行，提供 host-guest 协议用于工具注册和事件处理
- **`ExtensionRuntime`**：提供晚期绑定的 session 派生能力（`SessionSpawner`）、工具注册队列、派生深度限制（最大 3 层）
- **`ServerExtensionContext`**：实现 `ExtensionContext`，提供只读 session 视图，支持 snapshot

### 加载策略分层

- 全局扩展：`~/.astrcode/extensions/`
- 项目扩展：`.astrcode/extensions/`
- WASM 扩展：`.wasm` 文件通过 wasmtime 运行时加载，提供沙箱化的扩展执行环境

项目级行为在当前 session 中优先。

## Tool 模型

工具系统只内置少量核心工具，覆盖文件访问、编辑、搜索、shell 执行、计划管理和 agent 协作。
自定义工具与内置工具走同一条执行路径。

这里的边界是 tool-first，而不是 plugin-first：tool 是运行时基础能力，
extension、SDK、MCP 或内置模块都只是 tool source。所有来源最终都应进入
session 级工具注册表，并经过同一套 hook、事件、结果裁剪与持久化流程。

也就是说，每个 tool call 都会经过同样的架构链路：

- 类型化 tool definition
- registry lookup
- lifecycle hooks
- 必要时的流式执行事件
- 持久化结果
- 在返回模型前进行可选后处理

这点很重要，因为系统不希望出现绕过可观测性、策略检查和统一调度的“特殊工具”。

## Mode / Plan Mode

Mode 是 `astrcode-extension-mode` 提供的内置扩展（ID: `astrcode-mode`），允许 Agent 在两种运行模式之间切换。

### 两种模式

| 模式 | 说明 | 受限工具 | Exit Review |
|------|------|---------|-------------|
| **Code**（默认） | 完整执行模式 | 无 | 无 |
| **Plan** | 规划模式，保留全部工具访问权限 | 无 | 需通过 Exit Gate |

### 提供的工具

- **`switchMode`**：切换 Code ↔ Plan。从 Plan 退出时触发 Exit Gate：
  1. 第一次调用：检查 plan artifact 是否存在、必填 heading 是否齐全，返回 review checklist
  2. 第二次调用：Gate 通过，plan 内容附加到转换消息中，模式切回 Code
- **`upsertSessionPlan`**：创建/更新结构化计划 markdown（仅在 Plan 模式可用）

### Plan Artifact 结构

计划必须包含以下 heading：Context、Goal、Scope、Non-Goals、Existing Code to Reuse、Implementation Steps、Verification、Dependencies and Risks、Assumptions。

### Hook 订阅

- **`PreToolUse`**（priority 100, Blocking）：检查当前模式是否限制请求的工具
- **`BeforeProviderRequest`**（priority 50, Blocking）：模式切换时注入转换上下文消息（保持 system prompt 不变以利用 KV cache）

### 状态持久化

- 模式状态：`<session-dir>/mode/mode-state.json`（当前模式、前一个模式、pending 转换上下文、exit review 轮次）
- 计划文件：`<session-dir>/plan/plan.md`

## ACP 适配器

`astrcode-server::acp` 模块桥接标准 Agent Client Protocol 到 astrcode 内部的 `CommandHandle` / broadcast event 架构。

它是一个纯 DTO 映射边界——不包含业务逻辑，不泄露 session-runtime 类型。

### 请求流程

1. 客户端通过 stdio 发送 ACP JSON-RPC 请求（`InitializeRequest` / `NewSessionRequest` / `PromptRequest` / `CancelNotification`）
2. `run_acp_server()` 构建 `Agent` builder，注册四个 JSON-RPC handler
3. `PromptRequest` → `handle_prompt()` 提取文本 → `CommandHandle::submit_prompt_with_completion()` 提交 turn，获得 turn_id 和 completion oneshot
4. 通过 `tokio::select!` 同时从 broadcast channel 转发实时事件为 ACP `SessionNotification`，并等待 completion oneshot
5. completion oneshot ready 后，用 `try_recv()` 确定性 flush 已排队事件，返回 `PromptResponse`（含 `StopReason`）

### 事件映射

| astrcode EventPayload | ACP SessionUpdate |
|---|---|
| `AssistantTextDelta` | `AgentMessageChunk` |
| `ThinkingDelta` | `AgentThoughtChunk` |
| `ToolCallStarted` | `ToolCall` |
| `ToolCallRequested` | `ToolCallUpdate` (InProgress) |
| `ToolCallCompleted` | `ToolCallUpdate` (Completed/Failed) |
| `ErrorOccurred` | `AgentMessageChunk`（带 `[Error]` 前缀） |

### Turn 完成机制

- `ActiveTurn.completion_tx`：oneshot sender 存储在 `CommandHandler` actor 的 `active_turns` 中
- 所有 turn 移除路径（`finish_agent_turn` / `fail_agent_turn` / `abort_session` / `DeleteSession`）在广播最终事件后 resolve completion
- compact continuation 时 `completion_tx` 随 `ActiveTurn` 迁移到 child session
- `event_belongs_to_prompt` 维护 accepted session 集合，`CompactBoundaryCreated` 自动扩展，确保 auto-compact 后的事件不被过滤
- ACP notification 始终使用原始 ACP session id，不泄露内部 continuation session

### 核心组件

- **`TurnCompletion`**：枚举 `Completed { finish_reason }` / `Failed { error }` / `Aborted`
- **`events::to_session_notification()`**：`EventPayload` → `Option<SessionNotification>` 映射函数
- **`flush_queued_events()`**：确定性 `try_recv()` 循环，completion 后清空排队事件

## 存储与恢复

存储设计追求的是运行时清晰性，而不是引入复杂数据库。

核心持久化选择包括：

- 每个 session 一个 JSONL append-only event log
- 定期 snapshot，加速重建
- 基于 snapshot 之后 tail 的增量回放
- 基于文件的 turn lock
- 原子 config 写入

因此 session 恢复流程很直接：

1. 如果存在 snapshot，先加载最新 snapshot
2. 回放 snapshot 之后的剩余事件
3. 重建内存中的 session state
4. 在下一次 turn 启动时重建 agent

## 协议边界

Server 对外暴露三种协议边界，最终都汇入同一个 `CommandHandler` actor：

### stdio transport（TUI / exec）

类型化 JSON-RPC 2.0 over stdio。
每条协议消息都是单行 JSON 对象，以换行分隔。
适用于 TUI 和 exec 前端直接内嵌 transport 的场景。

### HTTP/SSE（桌面端 / 第三方客户端）

Axum HTTP server，命令通过 HTTP POST 提交，事件通过 SSE 流推送。
适用于需要独立进程、远程访问或非 Rust 客户端集成的场景。

### ACP adapter（标准 ACP 客户端）

基于 `agent-client-protocol` crate 的 stdio JSON-RPC 服务器。
提供标准化的 Agent Client Protocol 接口（Initialize / NewSession / Prompt / Cancel），
将 ACP schema 映射到 astrcode 内部事件。
适用于 IDE 插件、编辑器集成等支持 ACP 的场景。

### 通用通信模式

三类边界覆盖相同的通信模式：

- frontend 发给 server 的 commands
- server 发给 frontend 的流式 events
- confirm / select / input / notify 等交互型 UI requests

这让 server 可以复用于多种 frontend，也让 headless 执行成为自然能力，而不是嵌在 UI 里的特殊流程。

## 配置模型

配置采用多层叠加，并参与运行时行为控制。
优先级为：

`Defaults -> User -> Project -> Env`

配置模型包括：

- 基于 profile 的 model/provider 选择
- compaction、budget、retry、concurrency 等运行时参数
- sub-agent 限制
- 基于环境变量的 secret 解析
- 面向长生命周期 server 的热重载

因此 config 不只是启动参数，而是运行时编排的一部分。

## 取舍

这套架构做了几项明确取舍：

- 事件溯源会增加 replay 成本，但换来恢复能力、可审计性和分支能力
- extension-first 会提高灵活性，但要求更清晰的优先级规则与故障隔离
- 三种 transport（stdio / HTTP / ACP）增加适配层代码，但让 server 真正成为多前端共享的后端
- 极简核心降低耦合，但也把更多责任转移到扩展契约和 crate 边界设计上
- Mode 扩展把 plan/code 切换交给 LLM 自主决定，比硬编码流程更灵活，但依赖 prompt 质量
