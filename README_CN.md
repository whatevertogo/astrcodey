# AstrCode

<img width="1401" height="995" alt="image" src="https://github.com/user-attachments/assets/26e9b719-8911-4fdf-a989-065ce9b9ea7e" />


用 Rust 从零构建的 AI 编程助手平台。

AstrCode 是一个全栈 AI 编程助手，用约 5.5 万行 Rust 代码、21 个 crate 构建，外加 React + TypeScript 前端（约 4800 行）。包含带工具执行的 Agent 循环、基于 SSE 流式传输的多 Provider LLM 层（Anthropic、OpenAI、Google GenAI）、插件/钩子扩展系统（支持通过 FFI 加载原生扩展和 WASM 扩展）、带自动压缩的上下文窗口管理、评测框架，以及多种交互方式：终端 TUI、Web 前端、Tauri 桌面应用、HTTP/SSE API 和 ACP（Agent Client Protocol）适配器。

> **为什么做？** 我想理解一个 AI 编程助手在每个层面的运作方式——从 SSE 流解析到上下文窗口压缩——所以自己造了一个。架构参考了多个编程助手的工程实践，但所有代码均为原创。

## 快速开始

```bash
# 构建
cargo build

# 交互式终端 UI
cargo run -- tui

# 无头单次执行
cargo run -- exec "解释一下 agent loop 的架构"

# HTTP/SSE 服务器
cargo run -- server

# Web 前端（开发服务器）
cd frontend && npm install && npm run dev

# Tauri 桌面应用（开发模式）
cd frontend && npm install && npm run tauri:dev

# 评测框架（需要 dev-mode feature）
cargo run --features dev-mode -- eval
```

## 架构

```
          ┌──────────┐  ┌───────────────────────┐  ┌───────────┐
          │   TUI    │  │  Web / Tauri 前端      │  │ ACP 客户端 │
          │ (ratatui)│  │  React 19 + TypeScript │  │  (stdio)  │
          └────┬─────┘  └────────┬──────────────┘  └─────┬─────┘
               │                  │ SSE / JSON-RPC        │ ACP JSON-RPC
               │    stdio         │                       │ over stdio
               └────────┬────────┘───────────────────────┘
                   ┌─────┴──────┐
                   │astrcode-cli│  TUI / exec / server 启动器
                   └─────┬──────┘
                         │
                   ┌─────┴──────┐
                   │astrcode-   │  会话管理、JSON-RPC + HTTP 处理器
                   │server      │  ACP 适配器、transport、并发控制
                   └─────┬──────┘
                         │
                   ┌─────┴───────┐
                   │astrcode-    │  Agent 循环核心：turn runner、工具管线
                   │session      │  LLM 流消费、上下文压缩编排
                   └─────┬───────┘
             ┌───────────┼───────────┐
             │           │           │
    ┌────────┴───┐ ┌─────┴─────┐ ┌───┴──────────┐
    │ astrcode-ai│ │astrcode-  │ │ astrcode-    │
    │            │ │extensions │ │ tools        │
    │ Anthropic  │ │钩子系统    │ │文件/Shell/   │
    │ OpenAI     │ │原生 FFI   │ │Task 工具     │
    │ Google     │ │WASM 扩展  │ │              │
    │ SSE + 重试  │ │           │ │              │
    └────────┬───┘ └─────┬─────┘ └──────────────┘
             │           │
   ┌─────────┴──┐  ┌────┴────────────┐
   │astrcode-   │  │   扩展 crate     │
   │ context    │  │ ├ mcp            │
   │ Token 预算  │  │ ├ skill         │
   │ 自动压缩    │  │ ├ todo-tool     │
   └────────────┘  │ ├ mode          │
                   │ └ agent-tools   │
                   └─────────────────┘
        ┌─────────────────────────────┐
        │         共享基础层           │
        │ core · protocol · storage   │
        │ support · log               │
        └─────────────────────────────┘
```

## Crate 一览

| Crate | 行数 | 说明 |
|---|---|---|
| `astrcode-server` | 9.5k | 会话管理、JSON-RPC/HTTP/ACP 处理器、transport、并发控制 |
| `astrcode-cli` | 8.0k | 终端 TUI（ratatui）、无头执行、服务器启动器 |
| `astrcode-session` | 5.2k | Agent 循环核心：turn runner、工具管线、LLM 流消费、压缩编排 |
| `astrcode-core` | 4.9k | 共享类型、trait、配置系统、错误类型、提示词组合、扩展契约 |
| `astrcode-tools` | 4.6k | 内置工具：read、write、edit、patch、find、grep、shell、task |
| `astrcode-storage` | 3.7k | JSONL 事件日志、会话快照、配置持久化、文件锁 |
| `astrcode-ai` | 3.6k | 多 Provider LLM 层（Anthropic、OpenAI、Google GenAI）、SSE 流式、重试 |
| `astrcode-context` | 3.5k | Token 估算、上下文窗口预算、自动压缩、提示词引擎 |
| `astrcode-extensions` | 2.8k | 扩展生命周期、钩子分发、原生扩展加载（FFI）、WASM 扩展运行时 |
| `astrcode-extension-mcp` | 1.9k | MCP 协议客户端（stdio）、工具发现 |
| `astrcode-protocol` | 1.2k | JSON-RPC 2.0 线协议类型、命令、事件、HTTP DTO |
| `astrcode-extension-mode` | 1.2k | Agent 运行模式切换（Code / Plan）、计划 Artifact、Exit Gate、快捷键与状态栏注册 |
| `astrcode-eval` | 1.1k | 评测框架 — HTTP 服务器控制、事件日志指标提取、结构化报告 |
| `astrcode-extension-skill` | 949 | 斜杠命令技能发现与分发 |
| `astrcode-extension-todo-tool` | 733 | 进度追踪 Todo 工具 |
| `astrcode-extension-agent-tools` | 704 | 子 Agent 委派、Agent 发现（兼容 Claude Code 格式） |
| `astrcode-support` | 682 | 路径解析、Shell 检测、文本处理 |
| `astrcode-client` | 521 | 类型化 JSON-RPC 客户端、传输层、流订阅 |
| `astrcode-log` | 353 | 文件轮转、stderr 输出、env-filter 日志 |
| `astrcode-bundled-extensions` | 39 | 可选扩展 crate 的组合根 |

**共计：20 个 Rust crate + Tauri shell、203 个源文件、约 5.5 万行代码。**

### 前端与桌面应用

| 组件 | 行数 | 说明 |
|---|---|---|
| `frontend/`（React + TS） | ~4.8k | Web 前端——聊天视图、侧边栏、会话管理、SSE 流式传输 |
| `src-tauri/`（Tauri v2） | ~670 | 桌面应用外壳——sidecar 管理、单实例协调、原生对话框 |

Web 前端（`frontend/`）是 React 19 + TypeScript + Tailwind CSS v4 + Vite 单页应用，通过 SSE 实时接收流式事件，通过 JSON-RPC 发送命令。支持浏览器独立运行（`npm run dev`）或打包为 Tauri 桌面应用（`npm run tauri:dev`）。

Tauri 桌面应用（`src-tauri/`）将 Web 前端包装在原生窗口中，自动管理 `astrcode-server` 作为 sidecar 进程——启动时自动拉起、发现空闲端口、桥接连接。还提供单实例协调（文件锁 + TCP 激活）和通过 `tauri-plugin-dialog` 的原生文件对话框。

## 核心设计

### Agent 循环

Agent 循环（`astrcode-session`）采用分阶段流水线模式：

1. **准备上下文** — 检查 token 预算，必要时触发自动压缩
2. **构建 Provider 请求** — 分发钩子、组装消息、MCP 工具发现
3. **流式接收 LLM 响应** — SSE 解析、UTF-8 安全解码、事件累积
4. **执行工具** — 并行批量执行，支持 pre/post 钩子，结果持久化
5. **循环或返回** — 有工具调用则回到步骤 1；纯文本回复则终止

Agent 支持运行模式切换（Code / Plan）。Plan 模式下只暴露只读工具和计划管理工具，通过 Exit Gate（自审清单 + 必填 heading 校验）控制退出条件，计划 Artifact 持久化到 `<session>/plan/plan.md`。模式指令通过 `BeforeProviderRequest` 注入，不影响 system prompt 的 KV 缓存。

`ToolPipeline` 结构体负责工具预处理、并行调度和结果持久化。`SharedTurnContext` 携带会话级标识。`consume_llm_stream` 返回 `StreamOutcome` 枚举（`Complete` | `ToolCalls`），让循环体读起来是一组线性排列的命名阶段。

### LLM Provider 层

`astrcode-ai` 支持多个 Provider — Anthropic（原生 Messages API）、OpenAI 兼容（Chat Completions + Responses API）、Google GenAI。核心组件：

- **`Utf8StreamDecoder`** — 跨 TCP chunk 处理多字节 UTF-8 边界和坏字节恢复
- **`SseLineReader`** — 通用 SSE 行缓冲（可供所有 Provider 复用）
- **`RetryPolicy`** — 针对 429/5xx 错误的指数退避重试（带抖动）

### 上下文窗口管理

当对话历史接近模型的上下文限制 83.5% 时，`astrcode-context` 触发自动压缩：

1. 默认使用 LLM 生成结构化 9 段摘要（自动压缩和手动压缩均走此路径）
2. LLM 调用失败（网络错误、解析错误、超时等）时自动降级为确定性规则摘要
3. 压缩记录持久化为快照，用于调试
4. 压缩后自动恢复最近读取的文件和 Agent/Skill/Tool 状态

### 工具执行

工具以并行批量方式执行（最多 5 个并发）。执行管线：

1. **预处理** — 解析 JSON 参数（支持修复格式不正确的 LLM 输出）、检查可见性、分发 `PreToolUse` 钩子
2. **执行** — 通过 `JoinSet` 并行批量执行，串行工具会先刷新当前批次
3. **提交** — 分发 `PostToolUse` 钩子、持久化大结果、执行消息字符预算、发射事件

大型工具结果自动持久化到磁盘，替换为预览摘要以保持在消息字符预算内。

### 扩展系统

扩展系统（`astrcode-extensions`）是架构核心支柱，而非附属功能：

- **Extension trait** — 每个扩展声明钩子订阅、贡献工具和斜杠命令、处理生命周期事件
- **Hook 模式** — `Blocking`（可修改输入/输出）、`NonBlocking`（fire-and-forget）、`Advisory`（仅观察）
- **快捷键注册** — 扩展通过 `Registrar::keybinding()` 注册键盘快捷键（如 `Shift+Tab` 切换模式）
- **状态栏项** — 扩展贡献状态栏条目（如当前模式指示器），通过 `StatusItemUpdate` 通知动态更新
- **原生扩展加载** — 通过 `libloading` + FFI 从磁盘加载 `.dll`/`.so` 扩展，支持全局（`~/.astrcode/extensions/`）和项目级（`.astrcode/extensions/`）目录
- **WASM 扩展运行时** — 基于 wasmtime 的沙箱化扩展执行，提供 host-guest 协议用于工具注册和事件处理
- **扩展运行时** — 带深度限制的会话派生、工具注册队列、优先级分派

### ACP 适配器

ACP 适配器（`astrcode-server::acp`）将标准 Agent Client Protocol 桥接到 astrcode 内部的命令/广播架构：

- 基于 stdio 的 JSON-RPC 服务器，实现 Initialize / NewSession / Prompt / Cancel
- 通过 broadcast channel 实时流式转发事件为 ACP `SessionNotification`
- 利用 completion oneshot 实现 turn 生命周期的确定性事件刷新
- 为 IDE 插件和编辑器集成设计

## 运行模式

| 模式 | 命令 | 说明 |
|---|---|---|
| **TUI** | `cargo run -- tui` | 交互式终端 UI，支持消息历史、工具展示、斜杠命令 |
| **Exec** | `cargo run -- exec "提示词"` | 无头单次执行，支持 `--jsonl` |
| **Server** | `cargo run -- server [--addr 0.0.0.0:3847]` | HTTP/SSE 服务器，支持 JSON-RPC、会话管理、实时事件流 |
| **ACP** | `cargo run -- acp` | ACP stdio 适配器，用于 IDE/编辑器集成 |
| **Eval** | `cargo run --features dev-mode -- eval` | 运行评测基准（需要 `dev-mode` feature） |
| **Web** | `cd frontend && npm run dev` | 浏览器聊天界面，通过 SSE 连接后端 |
| **Desktop** | `cd frontend && npm run tauri:dev` | Tauri 桌面应用（自动启动 server 作为 sidecar） |

### TUI 参考

**键盘快捷键：**

| 按键 | 功能 |
|---|---|
| `Enter` | 提交提示词 / 确认斜杠命令选择 |
| `Shift+Enter` / `Alt+Enter` | 插入换行 |
| `Esc` | 关闭斜杠面板 / 停止流式回复 |
| `Tab` | 补全斜杠命令 |
| `Shift+Tab` | 触发插件注册的快捷键 |
| `Ctrl+A` / `Ctrl+E` | 移动到行首 / 行尾 |
| `Ctrl+U` / `Ctrl+K` | 删除光标前 / 后的内容 |
| `Ctrl+W` | 删除前一个单词 |
| `Ctrl+C` | 退出（需二次确认） |

**斜杠命令：**

| 命令 | 说明 |
|---|---|
| `/new` | 创建新会话 |
| `/resume <id>` 或 `/r <id>` | 恢复之前的会话 |
| `/sessions` 或 `/ls` | 打开会话选择器 |
| `/compact` | 压缩当前会话上下文 |
| `/help` 或 `/?` | 显示帮助信息 |
| `/quit` 或 `/q` | 退出 astrcode |

插件扩展可在运行时注册额外的斜杠命令和快捷键。

## 发行

每个版本标签自动触发 GitHub Release，提供 Linux、macOS、Windows（x86_64 + aarch64）的预编译二进制文件。每周一自动发布 patch 版本。

## 致谢

本项目借鉴了以下开源项目的设计思想和工程实践：

- **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** — 工具执行管线、系统提示词设计
- **[OpenCode](https://github.com/anomalyco/opencode)** — 前后端分离（HTTP/SSE + JSON-RPC）参考了 OpenCode 的架构。
- **[Codex CLI](https://github.com/openai/codex)** — TUI 布局和终端 UI 设计借鉴了 Codex 在终端中渲染 Agent 交互的方式。

## License

AGPL-3.0
