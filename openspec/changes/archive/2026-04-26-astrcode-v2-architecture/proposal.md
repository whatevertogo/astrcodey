## Why

astrcode 当前版本耦合紧密，缺乏前后端分离和可扩展的 Hook 系统。v2 采用极简核心哲学：**核心只保留 hooks、agent loop、compaction、内置 tools 四个能力，其余一切交给扩展实现**。借鉴 pi-mono 的 extension 设计和 codex 的 app-server 分层，构建「一个后端、多种前端、扩展驱动一切」的 session-first 架构。

## What Changes

- **BREAKING** 完全重构为 ~14 个职责单一的 `astrcode-*` crate，放入 `crates/` 目录
- 新增 session-first 事件溯源架构：Session 是只追加事件日志（JSONL + 快照），Agent 是从 Session 事件重建的临时处理器
- **核心四能力**：
  - **Agent Loop**：agent 执行循环，接收 prompt → 组装上下文 → 调 LLM → 执行工具 → 发射事件 → 追加日志
  - **Hooks**：扩展系统，12 个生命周期事件 + Blocking/NonBlocking/Advisory 三种 mode，全局+session 级加载，扩展可注册工具/命令/上下文
  - **Compaction**：上下文窗口管理，token 估算、预算控制、修剪、LLM 驱动压缩、文件恢复
  - **Built-in Tools**：13 个文件/执行/计划工具 + 4 个 agent 协作工具
- **一切皆扩展**：skills、agent profiles、自定义工具、slash 命令、上下文注入 —— 全部通过扩展系统注册，核心不做硬编码
- 新增统一通信协议（`astrcode-protocol`）：JSON-RPC 2.0 over stdio
- 新增 TUI 前端（`astrcode-tui`）和 headless 前端（`astrcode-exec`）
- 支持多 Session 并发、Session fork/branch/switch
- **不做 MCP**：MCP 集成留到后续或由扩展实现

## Capabilities

### New Capabilities

- `session-first-architecture`: Session 事件溯源 — 事件日志追加、快照、重放、Agent 重建、Session 树
- `agent-loop`: Agent 执行循环 — prompt→context→LLM→tools→events→log 管线
- `extension-system`: **核心扩展机制** — 生命周期事件、Blocking/NonBlocking/Advisory mode、全局+session 加载、工具/命令/上下文注册。skills、agent profiles、自定义行为全部由此实现
- `compaction`: 上下文窗口管理 — token 估算、工具结果预算、微压缩、修剪、LLM 驱动压缩、文件追踪恢复
- `tool-system`: 内置工具 — 13 个文件/执行/计划工具 + 4 个 agent 协作工具，扩展可注册新工具
- `communication-protocol`: JSON-RPC 2.0 over stdio — ClientCommand/ServerEvent/UI 请求子协议、版本协商
- `server-runtime`: 后端 server — SessionManager、Agent 编排、JSON-RPC handler、配置管理、多 Session 并发
- `tui-frontend`: 交互式终端 — state/render 分离、主题系统、多会话切换
- `headless-exec`: 无头执行 — 单次 prompt、JSONL 输出、CI/CD 集成
- `llm-providers`: LLM 提供者 — OpenAI 兼容 API、SSE 流式解析、指数退避重试、缓存追踪
- `prompt-composition`: Prompt 组装 — Contributor 模式、4 层缓存、模板引擎，扩展可注册新 contributor
- `session-storage`: 会话持久化 — JSONL 追加日志、快照+尾部恢复、文件锁、多会话管理
- `configuration-system`: 多层配置 — Profile、RuntimeConfig(~30 参数)、AgentConfig、层叠加载、热重载

## Impact

- 现有 adapter crate 迁移重构到 `crates/astrcode-*` 下
- `Cargo.toml` workspace root 新建
- `docs/` 目录新建
- skills 和 agent profiles 从核心移除，改为扩展实现
- MCP 完全移除，不做集成
