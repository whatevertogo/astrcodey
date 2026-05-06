# AstrCode

用 Rust 从零构建的 AI 编程助手平台。

AstrCode 是一个全栈 AI 编程助手，用约 4 万行 Rust 代码、17 个 crate 构建。包含带工具执行的 Agent 循环、基于 SSE 流式传输的 LLM Provider 层、插件/钩子扩展系统、带自动压缩的上下文窗口管理，以及终端 TUI 和 HTTP/SSE API 两种交互方式。

> **为什么做？** 我想理解一个 AI 编程助手在每个层面的运作方式——从 SSE 流解析到上下文窗口压缩——所以自己造了一个。架构参考了多个编程助手的工程实践，但所有代码均为原创。

## 快速开始

```bash
# 需要 Nightly Rust
rustup toolchain install nightly

# 构建
cargo build

# 交互式终端 UI
cargo run -- tui

# 无头单次执行
cargo run -- exec "解释一下 agent loop 的架构"

# HTTP/SSE 服务器
cargo run --bin astrcode-server
```

## 架构

```
                    ┌─────────────┐
                    │ astrcode-cli │  TUI / exec / server 启动器
                    └──────┬──────┘
                           │
                    ┌──────┴──────┐
                    │astrcode-    │
                    │ server      │  Agent 循环、会话管理、JSON-RPC + HTTP 处理器
                    └──────┬──────┘
              ┌────────────┼────────────┐
              │            │            │
     ┌────────┴───┐ ┌─────┴─────┐ ┌───┴──────────┐
     │ astrcode-ai│ │astrcode-  │ │ astrcode-    │
     │            │ │extensions │ │ tools        │
     │ LLM 提供者  │ │钩子系统   │ │文件/Shell/   │
     │ SSE + 重试  │ │插件 SDK   │ │Agent 工具    │
     └────────┬───┘ └─────┬─────┘ └──────────────┘
              │            │
    ┌─────────┴──┐  ┌──────┴──────────┐
    │astrcode-   │  │   扩展 crate     │
    │ context    │  │ ├ mcp            │
    │ Token 预算  │  │ ├ skill         │
    │ 自动压缩    │  │ ├ todo-tool     │
    └────────────┘  │ └ agent-tools   │
                    └─────────────────┘
         ┌─────────────────────────────┐
         │         共享基础层           │
         │ core · protocol · storage   │
         │ support · log · prompt      │
         └─────────────────────────────┘
```

## Crate 一览

| Crate | 行数 | 说明 |
|---|---|---|
| `astrcode-server` | 10.7k | Agent 循环、会话管理、JSON-RPC/HTTP 处理器 |
| `astrcode-cli` | 5.9k | 终端 TUI（ratatui）、无头执行、服务器启动器 |
| `astrcode-tools` | 4.0k | 内置工具：read、write、edit、patch、find、grep、shell |
| `astrcode-core` | 3.2k | 共享类型、trait、配置系统、错误类型 |
| `astrcode-extensions` | 3.0k | 扩展生命周期、钩子分发、插件加载 |
| `astrcode-storage` | 2.1k | JSONL 事件日志、会话快照、文件锁 |
| `astrcode-context` | 2.1k | Token 估算、上下文窗口预算、自动压缩 |
| `astrcode-extension-mcp` | 1.8k | MCP 协议客户端（stdio）、工具发现 |
| `astrcode-ai` | 1.6k | OpenAI 兼容 Provider（Chat Completions + Responses API） |
| `astrcode-prompt` | 839 | 系统提示词组装（收集扩展贡献） |
| `astrcode-protocol` | 848 | JSON-RPC 2.0 线协议类型、命令、事件、HTTP DTO |
| `astrcode-support` | 831 | 路径解析、Shell 检测、工具结果持久化 |
| `astrcode-extension-skill` | 829 | 斜杠命令技能发现与分发 |
| `astrcode-extension-todo-tool` | 743 | 进度追踪 Todo 工具 |
| `astrcode-extension-agent-tools` | 586 | 子 Agent 委派（Agent 工具） |
| `astrcode-client` | 496 | 类型化 JSON-RPC 客户端、传输层、流订阅 |
| `astrcode-log` | 344 | 文件轮转、stderr 输出、env-filter 日志 |

**共计：17 个 crate、135 个源文件、约 4 万行代码。**

## 核心设计

### Agent 循环

Agent 循环（`astrcode-server/src/agent/`）采用分阶段流水线模式：

1. **准备上下文** — 检查 token 预算，必要时触发自动压缩
2. **构建 Provider 请求** — 分发钩子、组装消息、MCP 工具发现
3. **流式接收 LLM 响应** — SSE 解析、UTF-8 安全解码、事件累积
4. **执行工具** — 并行批量执行，支持 pre/post 钩子，结果持久化
5. **循环或返回** — 有工具调用则回到步骤 1；纯文本回复则终止

`ToolPipeline` 结构体负责工具预处理、并行调度和结果持久化。`SharedTurnContext` 携带会话级标识。`consume_llm_stream` 返回 `StreamOutcome` 枚举（`Complete` | `ToolCalls`），让循环体读起来是一组线性排列的命名阶段。


### LLM Provider 层

`astrcode-ai` 同时支持 OpenAI Chat Completions 和 Responses API 两种模式。核心组件：

- **`Utf8StreamDecoder`** — 跨 TCP chunk 处理多字节 UTF-8 边界和坏字节恢复
- **`SseLineReader`** — 通用 SSE 行缓冲（可供任意未来 Provider 复用）
- **`LlmAccumulator`** — OpenAI 特定的事件累积（工具调用追踪、内容增量合并）
- **`RetryPolicy`** — 针对 429/5xx 错误的指数退避重试（带抖动）

### 上下文窗口管理

当对话历史接近模型的上下文限制83.5%时，`astrcode-context` 触发自动压缩：

1. 默认运行确定性压缩（基于规则的摘要）
2. 可用时尝试 Provider 支持的压缩（LLM 生成摘要）
3. 压缩记录持久化为快照，用于调试
4. 连续 Provider 失败时自动降级为确定性模式

### 工具执行

工具以并行批量方式执行（最多 5 个并发）。执行管线：

1. **预处理** — 解析 JSON 参数（支持修复格式不正确的 LLM 输出）、检查可见性、分发 `PreToolUse` 钩子
2. **执行** — 通过 `JoinSet` 并行批量执行，串行工具会先刷新当前批次
3. **提交** — 分发 `PostToolUse` 钩子、持久化大结果、执行消息字符预算、发射事件

大型工具结果自动持久化到磁盘，替换为预览摘要以保持在消息字符预算内。

## 运行模式

| 模式 | 命令 | 说明 |
|---|---|---|
| **TUI** | `cargo run -- tui` | 交互式终端 UI，支持消息历史、工具展示、斜杠命令 |
| **Exec** | `cargo run -- exec "提示词"` | 无头单次执行，支持 `--jsonl` 流式输出 |
| **Server** | `cargo run --bin astrcode-server` | HTTP/SSE 服务器，支持 JSON-RPC、会话管理、实时事件流 |

## 致谢

本项目借鉴了以下开源项目的设计思想和工程实践：

- **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** — 工具执行管线、系统提示词设计
- **[OpenCode](https://github.com/anomalyco/opencode)** — 前后端分离（HTTP/SSE + JSON-RPC）参考了 OpenCode 的架构。
- **[Codex CLI](https://github.com/openai/codex)** — TUI 布局和终端 UI 设计借鉴了 Codex 在终端中渲染 Agent 交互的方式。
- **[pi-mono](https://github.com/anthropics/pi-mono)** — 插件扩展模型和生命周期钩子设计受到了 pi-mono 组合式、事件驱动扩展思路的影响。

## License

MIT
