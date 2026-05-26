# AstrCode

**[English](README.md) | 中文**

**BE PI OR BETTER THAN PI**  
*受 Claude Code、Codex、OpenCode 和 Pi 启发 — 但用 Rust 原生构建*

| 界面 | 预览 |
|------|------|
| **CLI（TUI）** | <img width="1210" height="924" alt="astrcode TUI screenshot" src="https://github.com/user-attachments/assets/55259723-9bd7-4a1a-a74e-1e799ece2eed" /> |
| **Web / 桌面端** | <img width="1252" height="960" alt="astrcode web frontend screenshot" src="https://github.com/user-attachments/assets/af918c12-6fb7-4d72-b9ea-64133a2e2729" /> |

用 Rust 从零构建的 AI 编程助手平台。

AstrCode 是一个全栈 AI 编程助手，在 `crates/` 下包含 21 个 Rust crate（另加 Tauri 桌面壳），合计约 6.76 万行 Rust，外加 React + TypeScript 前端（约 6300 行）。包含带工具执行的 Agent 循环、基于 SSE 流式传输的多 Provider LLM 层（Anthropic、OpenAI、Google GenAI）、基于 SDK 与 IPC 子进程的扩展/钩子系统（后台预热、健康检查、启动阶段事件通道）、MCP 常驻进程池（跨 turn 复用长连接）、带自动压缩的上下文窗口管理、评测框架，以及多种交互方式：终端 TUI、Web 前端、Tauri 桌面应用、HTTP/SSE API 和 ACP（Agent Client Protocol）适配器。

## 目录

- [安装](#安装)
- [配置（首次运行前推荐）](#配置首次运行前推荐)
- [快速开始](#快速开始)
- [架构](#架构)
- [Crate 一览](#crate-一览)
- [核心设计](#核心设计)
- [运行模式](#运行模式)
- [延伸阅读](#延伸阅读)
- [发行](#发行)
- [致谢](#致谢)
- [License](#license)

## 安装

### NPM 包

```bash
npm i @whatevertogo/astrcode
```

`@whatevertogo/astrcode` npm 包提供了 Linux、macOS 和 Windows（x64 + arm64）的预编译二进制文件。安装后，`astrcode` 命令将全局可用。

**包地址**：[`@whatevertogo/astrcode`](https://www.npmjs.com/package/@whatevertogo/astrcode)

### 从源代码构建

参见下方的[快速开始](#快速开始)。

## 配置（首次运行前推荐）

AstrCode 需要配置 LLM Provider 和 API Key 才能正常运行。建议在首次运行前完成以下配置。

### 配置文件位置

| 文件 | 路径 | 用途 |
|---|---|---|
| 主配置 | `~/.astrcode/config.json` | LLM Provider、模型、运行时参数 |
| 项目配置 | `<workspace>/.astrcode/config.json` | 项目级覆盖（可选） |
| 全局 MCP | `~/.astrcode/mcp.json` | MCP 服务器配置 |
| 项目 MCP | `<workspace>/.astrcode/mcp.json` | 项目级 MCP 配置（可选） |

### LLM Provider 配置

`~/.astrcode/config.json` 示例：

```json
{
  "version": "1",
  "activeProfile": "anthropic",
  "activeModel": "claude-sonnet-4-6",
  "activeSmallProfile": "anthropic",
  "activeSmallModel": "claude-haiku-4-5-20251001",
  "profiles": [
    {
      "name": "anthropic",
      "providerKind": "anthropic",
      "apiKey": "env:ANTHROPIC_API_KEY",
      "models": [
        { "id": "claude-sonnet-4-6", "maxTokens": 16384, "contextLimit": 200000 }
      ]
    },
    {
      "name": "openai",
      "providerKind": "openai",
      "apiKey": "env:OPENAI_API_KEY",
      "apiMode": "chat_completions",
      "models": [
        { "id": "gpt-4.1", "maxTokens": 16384, "contextLimit": 128000 }
      ]
    },
    {
      "name": "deepseek",
      "providerKind": "openai",
      "baseUrl": "https://api.deepseek.com",
      "apiKey": "env:DEEPSEEK_API_KEY",
      "apiMode": "chat_completions",
      "models": [
        { "id": "deepseek-chat", "maxTokens": 16384, "contextLimit": 128000 }
      ]
    }
  ]
}
```

**API 密钥说明**：推荐使用 `"apiKey": "env:VARIABLE_NAME"` 引用环境变量，而非直接在配置文件中写入密钥。

请先设置对应的环境变量：

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export OPENAI_API_KEY="sk-..."
export DEEPSEEK_API_KEY="sk-..."
```

### MCP 服务器配置

`~/.astrcode/mcp.json` 用于注册外部 MCP 工具服务器，支持 stdio（子进程）和 HTTP 两种传输方式：

**Stdio 示例：**

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/dir"],
      "env": {}
    }
  }
}
```

**HTTP 示例：**

```json
{
  "mcpServers": {
    "web-reader": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

字段说明：

| 字段 | 必填 | 说明 |
|---|---|---|
| `command` | 是（stdio） | 启动 MCP 服务器的命令 |
| `args` | 否 | 命令行参数数组 |
| `env` | 否 | 传递给进程的环境变量 |
| `cwd` | 否 | 工作目录（项目级配置中会校验在工作区内） |
| `type` | 否 | 传输类型：`"stdio"`（默认）或 `"http"` |
| `url` | 是（http） | MCP 服务器的 HTTP 端点 |
| `headers` | 否 | MCP 端点的自定义 HTTP 头 |

MCP 服务器在扩展初始化时启动，通过长连接进程池跨 turn 复用。全局服务器（`~/.astrcode/mcp.json`）在启动时预热；项目级服务器（`<workspace>/.astrcode/mcp.json`）在会话创建或恢复时后台预热。仅当后台预热尚未完成时，首个 turn 才会同步等待一次。

### 扩展配置

可通过 `~/.astrcode/config.json` 的 `extensionStates` 字段启用或禁用扩展。默认情况下，除 `memory` 扩展外，所有扩展均处于启用状态。

```json
{
  "version": "1",
  "extensionStates": {
    "astrcode.memory": true
  }
}
```

要启用 memory 扩展，在 `extensionStates` 中添加 `"astrcode.memory": true`。

### 内置扩展一览

第一方扩展由 [`astrcode-bundled-extensions`](crates/astrcode-bundled-extensions) 统一注册；新扩展开发应依赖 [`astrcode-extension-sdk`](crates/astrcode-extension-sdk)，而非直接耦合宿主内部 crate。

| 扩展 | Crate | 说明 |
|---|---|---|
| **Mode** | `astrcode-extension-mode` | Agent 运行模式切换（Code / Plan），含 Exit Gate、计划 Artifact 持久化、快捷键与状态栏注册 |
| **Skill** | `astrcode-extension-skill` | 斜杠命令技能发现与调度 |
| **MCP** | `astrcode-extension-mcp` | MCP 协议客户端（常驻进程池、后台预热、并发合并） |
| **Todo Tool** | `astrcode-extension-todo-tool` | 进度追踪 Todo 工具 |
| **Agent Tools** | `astrcode-extension-agent-tools` | 子 Agent 委派、Agent 发现 |
| **Memory** | `astrcode-extension-memory` | 项目作用域的 Markdown 记忆存储（默认关闭） |

## 快速开始

```bash
# 1. 构建
cargo build

# 2. 创建配置目录和配置文件
mkdir -p ~/.astrcode
cat > ~/.astrcode/config.json << 'EOF'
{
  "version": "1",
  "activeProfile": "openai",
  "activeModel": "gpt-4o",
  "activeSmallProfile": "openai",
  "activeSmallModel": "gpt-4o-mini",
  "profiles": [
    {
      "name": "openai",
      "providerKind": "openai",
      "baseUrl": "https://api.openai.com/v1",
      "apiKey": "env:OPENAI_API_KEY",
      "models": [
        {
          "id": "gpt-4o",
          "maxTokens": 128000,
          "contextLimit": 128000,
          "reasoning": false
        },
        {
          "id": "gpt-4o-mini",
          "maxTokens": 128000,
          "contextLimit": 128000,
          "reasoning": false
        }
      ],
      "apiMode": "chat_completions"
    }
  ]
}
EOF

# 3. 设置 API 密钥环境变量
export OPENAI_API_KEY="your-api-key-here"

# 4. 运行交互式终端 UI
cargo run -- tui

# 无头单次执行
cargo run -- exec "解释一下 agent loop 的架构"

# HTTP/SSE 服务器
cargo run -- server

# Web 前端（开发服务器）
cd frontend && npm ci && npm run dev

# Tauri 桌面应用（开发模式）
cd frontend && npm ci && npm run tauri:dev

# 评测框架（需要 dev-mode feature）
cargo run --features dev-mode -- eval
```

## 配置

AstrCode 使用存储在 `~/.astrcode/config.json` 的基于 JSON 的配置系统。配置支持多个 LLM Provider、模型选择、运行时行为调优和项目级覆盖。

**主要配置特性：**
- 多 Provider 支持（Anthropic、OpenAI、Google GenAI）
- 独立的小模型配置供扩展使用（如记忆提取）
- 通过 `.astrcode/config.json` 进行项目级配置覆盖
- API 密钥的环境变量替换（`env:VAR_NAME`）
- 运行时行为调优（超时、重试、压缩、Agent 限制）
- 压缩熔断器与可选的预测性压缩

详细的配置文档请参阅[配置指南](docs/configuration.md)。

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
    │ OpenAI     │ │扩展 SDK   │ │Task 工具     │
    │ Google     │ │IPC 扩展   │ │              │
    │ SSE + 重试  │ │           │ │              │
    └────────┬───┘ └─────┬─────┘ └──────────────┘
             │           │
   ┌─────────┴──┐  ┌────┴─────────────────────────┐
   │astrcode-   │  │ 扩展层                        │
   │ context    │  │ bundled-extensions           │
   │ Token 预算  │  │ sdk · mode · skill · todo   │
   │ 自动压缩    │  │ agent-tools · mcp · memory  │
   └────────────┘  │ + 磁盘 IPC 扩展              │
                   └────────────────────────────┘
        ┌─────────────────────────────────────┐
        │              共享基础层               │
        │ core · protocol · storage · support │
        │ log · client                        │
        └─────────────────────────────────────┘
```

## Crate 一览

Cargo workspace 在 [`crates/`](crates/) 下包含 **21 个 crate**，另有 [`src-tauri/`](src-tauri/) 作为桌面壳（workspace 共 **22 个成员**）。按架构分层如下（详见[架构设计](docs/architecture.md)）。

### Layer 0：基础层

| Crate | 行数 | 说明 |
|---|---|---|
| [`astrcode-core`](crates/astrcode-core) | 5.3k | 共享领域类型、trait、配置系统、扩展契约、提示词组合 |
| [`astrcode-support`](crates/astrcode-support) | 1.0k | 宿主工具：路径解析、Shell 检测、工具结果持久化 |
| [`astrcode-log`](crates/astrcode-log) | 308 | 文件轮转、stderr 输出、env-filter 日志 |

### Layer 1：领域服务层

| Crate | 行数 | 说明 |
|---|---|---|
| [`astrcode-ai`](crates/astrcode-ai) | 3.5k | 多 Provider LLM 层（Anthropic、OpenAI 兼容、Google GenAI）、SSE 流式、重试 |
| [`astrcode-tools`](crates/astrcode-tools) | 5.1k | 内置工具：read、write、edit、patch、find、grep、shell、terminal、task |
| [`astrcode-storage`](crates/astrcode-storage) | 3.8k | JSONL 事件日志、快照、配置持久化、文件锁 |
| [`astrcode-context`](crates/astrcode-context) | 3.6k | Token 估算、上下文窗口预算、自动压缩、提示词引擎 |
| [`astrcode-session`](crates/astrcode-session) | 8.0k | Agent 循环：turn runner、工具管线、LLM 流、压缩编排、运行时服务 |
| [`astrcode-extensions`](crates/astrcode-extensions) | 5.1k | 扩展生命周期、钩子分发、能力门控、磁盘 IPC 扩展加载 |

### Layer 2：扩展层

| Crate | 行数 | 说明 |
|---|---|---|
| [`astrcode-extension-sdk`](crates/astrcode-extension-sdk) | 642 | 扩展作者稳定 API、能力声明、线缆协议类型、manifest 辅助 |
| [`astrcode-bundled-extensions`](crates/astrcode-bundled-extensions) | 88 | 组合根：注册全部第一方扩展 crate |
| [`astrcode-extension-mode`](crates/astrcode-extension-mode) | 978 | Code / Plan 模式切换、Exit Gate、计划 Artifact、快捷键与状态栏 |
| [`astrcode-extension-skill`](crates/astrcode-extension-skill) | 852 | 斜杠命令技能发现与 Skill 工具调度 |
| [`astrcode-extension-todo-tool`](crates/astrcode-extension-todo-tool) | 786 | 进度追踪 Todo 工具 |
| [`astrcode-extension-agent-tools`](crates/astrcode-extension-agent-tools) | 658 | 子 Agent 委派、Agent 发现（兼容 Claude Code 格式） |
| [`astrcode-extension-mcp`](crates/astrcode-extension-mcp) | 2.7k | MCP 客户端：stdio/HTTP 传输、常驻进程池、预热、健康检查 |
| [`astrcode-extension-memory`](crates/astrcode-extension-memory) | 1.6k | 项目作用域 Markdown 记忆（默认关闭） |

### Layer 3：服务与协议层

| Crate | 行数 | 说明 |
|---|---|---|
| [`astrcode-protocol`](crates/astrcode-protocol) | 1.3k | JSON-RPC 2.0 线协议类型、命令、事件、HTTP/UI DTO |
| [`astrcode-server`](crates/astrcode-server) | 12.2k | 会话管理、JSON-RPC/HTTP/ACP、transport、HTTP 投影与 SSE |

### Layer 4：客户端层

| Crate | 行数 | 说明 |
|---|---|---|
| [`astrcode-client`](crates/astrcode-client) | 617 | 类型化 JSON-RPC 客户端、传输抽象、流订阅 |
| [`astrcode-cli`](crates/astrcode-cli) | 7.7k | CLI 入口：TUI（ratatui）、无头 exec、server 启动器 |

### 评测层

| Crate | 行数 | 说明 |
|---|---|---|
| [`astrcode-eval`](crates/astrcode-eval) | 1.0k | 评测运行器：HTTP 服务器控制、事件日志指标、结构化报告 |

### 桌面壳

| 组件 | 行数 | 说明 |
|---|---|---|
| [`src-tauri/`](src-tauri) | ~690 | Tauri v2 壳：sidecar 管理、单实例协调、原生对话框 |

**合计：** Rust 约 6.76 万行（21 个 crate + Tauri），**261** 个 `.rs` 文件；`frontend/` 约 6300 行 TypeScript（整体约 **7.4 万行**）。

### 前端与桌面应用

| 组件 | 行数 | 说明 |
|---|---|---|
| `frontend/`（React + TS） | ~6.3k | Web 前端——聊天视图、侧边栏、会话管理、SSE 流式传输、状态栏 |
| `src-tauri/`（Tauri v2） | ~670 | 桌面应用外壳——sidecar 管理、单实例协调、原生对话框 |

Web 前端（`frontend/`）是 React 19 + TypeScript + Tailwind CSS v4 + Vite 单页应用，通过 SSE 实时接收流式事件，通过 JSON-RPC 发送命令。支持浏览器独立运行（`npm run dev`）或打包为 Tauri 桌面应用（`npm run tauri:dev`）。

Tauri 桌面应用（`src-tauri/`）将 Web 前端包装在原生窗口中，自动管理 `astrcode-server` 作为 sidecar 进程——启动时自动拉起、发现空闲端口、桥接连接。还提供单实例协调（文件锁 + TCP 激活）和通过 `tauri-plugin-dialog` 的原生文件对话框。

## 核心设计

### Agent 循环

Agent 循环（`astrcode-session`）采用分阶段流水线模式：

1. **准备上下文** — 检查 token 预算，必要时触发自动压缩
2. **构建 Provider 请求** — 分发钩子、组装消息、收集工具（MCP 工具从预热缓存读取，延迟工具通过 `tool_search_tool` 激活）
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

当对话历史接近模型上下文限制的 83.5% 时，`astrcode-context` 触发自动压缩：

1. 默认使用 LLM 生成结构化 9 段摘要（自动压缩和手动压缩均走此路径）
2. LLM 调用失败（网络错误、解析错误、超时等）时自动降级为确定性规则摘要
3. 压缩熔断器在 LLM 连续失败后临时跳过自动压缩，冷却时间可配置
4. 可选的预测性压缩根据 turn token 增长估算，在超出上下文窗口前提前压缩
5. 压缩结果通过 CAS 冲突检测持久化，并发写入会安全失败而非污染历史
6. 压缩记录持久化为快照，用于调试
7. 压缩后自动恢复最近读取的文件和 Agent/Skill/Tool 状态
8. **增量压缩** — 已有摘要时，新压缩会合并新信息而非从头重写

### 工具执行

工具以并行批量方式执行（最多 5 个并发）。执行管线：

1. **预处理** — 解析 JSON 参数（支持修复格式不正确的 LLM 输出）、检查可见性、分发 `PreToolUse` 钩子
2. **执行** — 通过 `JoinSet` 并行批量执行，串行工具会先刷新当前批次
3. **提交** — 分发 `PostToolUse` / `PostToolUseFailure` 钩子、持久化大结果、执行消息字符预算、发射事件

大型工具结果自动持久化到磁盘，替换为预览摘要以保持在消息字符预算内。每个工具声明执行模式：只读工具（find/grep/read）标记为 Parallel，写入工具（edit/write/shell）标记为 Sequential。

### 扩展系统

扩展系统（`astrcode-extensions`）是架构核心支柱，而非附属功能：

- **Extension trait** — 每个扩展声明钩子订阅、贡献工具和斜杠命令、处理生命周期事件
- **扩展 SDK** — 内置扩展和扩展作者统一依赖 `astrcode-extension-sdk`，不直接耦合宿主 `astrcode-core`
- **能力声明** — 内置扩展通过 `Extension::capabilities()`；磁盘 IPC 扩展在 `extension/initialize` 的 `capabilities` 中声明 `session_state`、`session_control`、`small_model` 等；运行时经 `HostRouter` 鉴权后仅允许已声明的 `astrcode.*` invoke
- **隔离状态目录** — session 级扩展状态存入 `<session>/extension_data/<extension-id>/`，避免扩展写入 session 根目录
- **Hook 模式** — `Blocking`（可修改输入/输出）、`NonBlocking`（fire-and-forget）、`Advisory`（仅观察）
- **快捷键注册** — 扩展通过 `Registrar::keybinding()` 注册键盘快捷键（如 `Shift+Tab` 切换模式）
- **状态栏项** — 扩展贡献状态栏条目（如当前模式指示器），通过 `StatusItemUpdate` 通知动态更新
- **磁盘 s5r 扩展** — stdio 长度前缀帧 + JSON `WireMessage`（`extension.json` 中 `protocol.s5r` + `command`）；Worker 发 `Initialize`、`handler.invoke` 与按能力裁剪的 `astrcode.*` invoke。规范见 [docs/extension-system.md](docs/extension-system.md)
- **扩展运行时** — 带深度限制的会话派生、工具注册队列、优先级分派
- **生命周期钩子** — `SessionStart` / `SessionResume` / `SessionShutdown`、`TurnStart` / `TurnEnd` / `TurnAborted`、`PreToolUse` / `PostToolUse` / `PostToolUseFailure`、`BeforeProviderRequest` / `AfterProviderResponse`、`PreCompact` / `PostCompact`、`PromptBuild`、`UserPromptSubmit`
- **扩展运行时 API** — `Extension::start()`（携带 `ExtensionCtx`，含 `startup_working_dir`、`event_sink` 和按能力裁剪的宿主服务）、`Extension::stop()`（携带 `StopReason`）、`Extension::health()`（健康探测）、`Extension::on_config_changed()`（热更新配置）
- **主动健康检查** — `ExtensionRunner::check_health()` 提供采样 API，宿主决定轮询策略
- **启动阶段事件** — `bind_startup_event_channel()` 绑定进程级事件通道，扩展在 `start()` 阶段即可 emit 自定义事件

### ACP 适配器

ACP 适配器（`astrcode-server::acp`）将标准 Agent Client Protocol 桥接到 astrcode 内部的命令/广播架构：

- 基于 stdio 的 JSON-RPC 服务器，实现 Initialize / NewSession / Prompt / Cancel
- 通过 broadcast channel 实时流式转发事件为 ACP `SessionNotification`
- 利用 completion oneshot 实现 turn 生命周期的确定性事件刷新
- 为 IDE 插件和编辑器集成设计

### 事件溯源架构

AstrCode 遵循 Session-First 事件溯源模式：

- **EventLog 是唯一事实来源** — 所有状态变化都以不可变事件形式追加写入
- **Session 是投影** — 从事件日志重放重建状态；fork 即从指定序列号重放
- **Agent 无状态** — `TurnRunner` 每回合后丢弃；状态存在于事件日志中
- **恢复即重放** — Agent 崩溃不影响会话完整性；重新投影即可继续

### 提示词工程

系统提示词使用管道式组装：

```
Identity → System → Task Guidelines → Communication → Environment
→ User Rules → Project Rules → Tool Summary → Extension → Additional
```

稳定部分（Identity、System、Task Guidelines）排在前面以利用提示词缓存前缀匹配。用户可通过 `~/.astrcode/IDENTITY.md` 覆盖身份，通过项目级 `AGENTS.md` 提供项目规则（从工作目录向上查找）。

## 运行模式

| 模式 | 命令 | 说明 |
|---|---|---|
| **TUI** | `cargo run -- tui` | 交互式终端 UI，支持消息历史、工具展示、斜杠命令、状态栏 |
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
| `Shift+Tab` | 触发扩展注册的快捷键 |
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

扩展可在运行时注册额外的斜杠命令和快捷键。

## 延伸阅读

| 文档 | 说明 |
|---|---|
| [架构设计](docs/architecture.md) | 事件溯源、Server 分层、压缩、Prompt 管线、工具、扩展 |
| [配置指南](docs/configuration.md) | 完整 `config.json` 参考 |
| [扩展系统](docs/extension-system.md) | 内置与磁盘 IPC 扩展、宿主能力 |
| [UI 渲染协议](docs/ui-render-spec.md) | 工具结果的结构化渲染协议 |
| [待办事项](docs/TODO.md) | 项目路线图与待办项 |

## 发行

每个版本标签自动触发 GitHub Release，提供 Linux、macOS、Windows（x86_64 + aarch64）的预编译二进制文件。每周一自动发布 patch 版本。

**NPM 包**：[`@whatevertogo/astrcode`](https://www.npmjs.com/package/@whatevertogo/astrcode)

## 致谢

本项目借鉴了以下开源项目的设计思想和工程实践：

- **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** — 工具执行管线、系统提示词设计
- **[OpenCode](https://github.com/anomalyco/opencode)** — 前后端分离（HTTP/SSE + JSON-RPC）参考了 OpenCode 的架构。
- **[Codex CLI](https://github.com/openai/codex)** — TUI 布局和终端 UI 设计借鉴了 Codex 在终端中渲染 Agent 交互的方式。

## License

AGPL-3.0
