# AstrCode 项目结构

## 目录概览

```
astrcode/
├── crates/              # Rust workspace (20 crates)
├── frontend/            # React + TypeScript + Tauri 桌面前端
├── src-tauri/           # Tauri 配置和 Rust 桥接代码 (1 crate)
├── docs/                # 项目文档
├── eval-tasks/          # 评测用例和 fixture
├── scripts/             # 开发工具脚本
├── npm/                 # NPM 分发包配置
├── .github/workflows/   # CI/CD 工作流
└── target/              # 构建输出
```

## Crates 结构 (21 crates, 含 src-tauri)

### Layer 0: Foundation (基础层)

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-core` | 4.9k | 共享领域类型与核心 trait，包括 tool、LLM provider、config 抽象、extension contract、prompt 组装 trait、StatusItem/Keybinding 类型 |
| `astrcode-support` | 682 | 宿主环境集成辅助能力，包括路径解析、shell 检测、文本处理 |
| `astrcode-log` | 353 | 文件旋转、stderr 输出、env-filter 日志 |

### Layer 1: Services (服务层)

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-ai` | 3.6k | 多 Provider LLM 层（Anthropic、OpenAI 兼容、Google GenAI）、SSE 流解析、重试、缓存追踪 |
| `astrcode-tools` | 4.6k | 内置工具（read/write/edit/patch/find/grep/shell/task）、工具注册表、执行包装、agent 协作工具 |
| `astrcode-storage` | 3.7k | JSONL event log、snapshot、config 持久化、锁 |
| `astrcode-context` | 3.5k | token 估算、tool result 预算、裁剪、压缩、文件恢复、prompt engine |
| `astrcode-session` | 5.2k | Agent 循环核心：turn runner、tool pipeline、LLM 流消费、compact 编排、事件 fanout、SessionRuntimeServices 共享 |
| `astrcode-extensions` | 2.9k | 扩展加载、生命周期分发、hook 执行策略、超时处理、能力注册、WASM 扩展运行时、keybinding/status item 收集 |

### Layer 2: Extensions (扩展层)

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-extension-mode` | 1.2k | Agent 运行模式切换（Code / Plan），包含 Exit Gate、计划 Artifact 持久化、快捷键与状态栏注册 |
| `astrcode-extension-skill` | 949 | 斜杠命令技能发现与分发 |
| `astrcode-extension-todo-tool` | 733 | 进度追踪 Todo 工具 |
| `astrcode-extension-agent-tools` | 704 | 子 Agent 委派（Agent 工具）、Agent 发现（兼容 Claude Code 格式） |
| `astrcode-extension-mcp` | 1.9k | MCP 协议客户端（stdio）、工具发现 |
| `astrcode-bundled-extensions` | 39 | 可选扩展 crate 的组合根，通过 feature flag 控制启用 |

### Layer 3: Server (服务层)

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-protocol` | 1.2k | 类型化 JSON-RPC 命令、事件、UI 子协议、HTTP DTO、Keybinding/StatusItem DTO |
| `astrcode-server` | 9.5k | session 生命周期、session 管理、config service、transport handling、HTTP 子模块（routes/projection/stream/auth）、ACP 适配器 |

### Layer 4: Client (客户端层)

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-client` | 521 | 面向 transport 的类型化 client 抽象 |
| `astrcode-cli` | 8.0k | Terminal UI (ratatui)、headless exec、server launcher、keybinding 运行时 |

### Eval (评测层)

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-eval` | 1.1k | 评测框架 — HTTP 服务器控制、事件日志指标提取、结构化报告 |

## HTTP 模块结构

`astrcode-server` 的 HTTP 层已拆分为 `http/` 子模块：

```
crates/astrcode-server/src/
├── acp/                # ACP (Agent Client Protocol) 适配器
│   ├── mod.rs          # stdio JSON-RPC server
│   └── events.rs      # 事件映射
├── http/               # HTTP/SSE 服务
│   ├── mod.rs          # HTTP 模块入口
│   ├── server.rs       # Axum 路由注册、CORS、静态文件
│   ├── auth.rs         # API Key 认证中间件
│   ├── stream.rs       # SSE 事件流（cursor 断连恢复）
│   ├── projection/     # 事件投影（EventStore → HTTP DTO）
│   │   ├── mod.rs
│   │   ├── args.rs     # 参数解析
│   │   ├── blocks.rs   # 消息块投影
│   │   ├── live.rs     # 实时 SSE delta 投影
│   │   └── replay.rs   # 历史回放投影
│   └── routes/         # REST 路由
│       ├── mod.rs
│       ├── config.rs   # /api/config*
│       ├── lifecycle.rs # /api/shutdown
│       ├── models.rs   # /api/models*
│       └── sessions.rs # /api/sessions*
├── handler/            # Session 命令处理
├── transport/          # Transport 层（stdio）
├── bootstrap.rs        # Server 启动引导
├── session_manager.rs  # 会话生命周期管理
└── ...
```

## 前端结构

### 技术栈

- **框架**: React 19 + TypeScript
- **构建**: Vite
- **样式**: Tailwind CSS v4
- **状态管理**: Zustand
- **桌面框架**: Tauri v2
- **Markdown 渲染**: react-markdown + remark-gfm

### 目录结构

```
frontend/
├── src/
│   ├── components/
│   │   ├── Chat/           # 聊天界面组件
│   │   ├── Sidebar/        # 侧边栏组件
│   │   ├── Settings/       # 设置面板
│   │   ├── ConnectingScreen.tsx
│   │   └── ErrorBoundary.tsx
│   ├── services/
│   │   ├── api.ts          # HTTP API 调用
│   │   ├── protocol.ts     # 协议类型定义与解码器
│   │   ├── sse-stream.ts   # SSE 流处理（Tauri 环境使用 plugin-http）
│   │   └── types.ts        # TypeScript 类型（含 KeybindingInfo、StatusItemInfo）
│   ├── store/
│   │   └── conversation.ts # 会话状态管理（Zustand）
│   ├── hooks/
│   ├── lib/
│   │   ├── hostBridge.ts   # 与 Tauri/Host 通信
│   │   ├── tauri.ts        # Tauri API 封装
│   │   └── utils.ts        # 通用工具
│   ├── main.tsx
│   ├── App.tsx
│   └── index.css
├── scripts/                # 构建脚本
├── package.json
├── vite.config.ts
└── tsconfig.json
```

## Tauri Desktop 结构

```
src-tauri/
├── src/                    # Tauri Rust 源码
│   ├── main.rs             # 入口、单实例 + Tauri Builder
│   ├── commands.rs         # Sidecar 管理、窗口控制、目录选择
│   ├── instance.rs         # 单实例协调（文件锁 + TCP 激活）
│   └── paths.rs            # 实例数据路径
├── capabilities/           # 权限配置（含 HTTP plugin 权限）
├── icons/                  # 应用图标
├── binaries/               # 嵌入式二进制 (astrcode-http-server)
├── Cargo.toml
└── tauri.conf.json         # Tauri 配置
```

### 架构

- **Sidecar 模式**: 嵌入 `astrcode-http-server` 作为 sidecar 进程
- **通信方式**: HTTP API + SSE（本地动态端口）
- **单实例协调**: 文件锁 + TCP 激活（后启动的实例通知已有实例聚焦窗口）
- **HTTP Plugin**: 通过 `tauri-plugin-http` 绕过 webkit2gtk 网络栈，解决 Linux SSE 缓冲问题
- **安全策略**: CSP 配置限制外部连接

## CI/CD 工作流

```
.github/workflows/
├── ci.yml               # 持续集成：Rust fmt/clippy/test + 前端检查，跨平台
├── release.yml          # 版本发布：标签触发，构建 CLI 二进制 + 桌面包 + NPM 发布
└── weekly-release.yml   # 每周自动发布：周一 08:00 UTC 自动计算版本号并推送标签
```

## 辅助目录

```
scripts/
├── check-deps.py            # 依赖方向检查
└── prepare-npm-packages.sh  # NPM 包准备脚本

npm/
└── astrcode/                # NPM 分发包
    ├── package.json
    ├── install.js
    └── bin/astrcode
```

## 构建命令

### Rust

```bash
# 格式化
cargo fmt

cargo fmt --check

# Lint
cargo clippy --workspace --all-targets --all-features -- -D warnings

# 测试
cargo test --workspace --all-features

# 构建
cargo build --release
```

### Frontend

```bash
cd frontend

# 开发
npm run dev

# 构建
npm run build

# 检查
npm run check
```

## 总计代码统计

- **Rust**: ~55k 行，20 crates + Tauri shell，203 源文件
- **TypeScript/TSX**: ~4.8k 行
- **Tauri (Rust)**: ~670 行

## 评测用例结构

```
eval-tasks/
└── cases/               # TOML 格式的评测用例
    ├── fix-buggy-lru-cache/
    ├── refactor-messy-python/
    ├── multi-file-split/
    ├── simple-file-create/
    ├── implement-from-spec/
    ├── fix-security-vulns/
    └── investigate-and-fix-bug/
```

每个用例目录包含 `case.toml`（定义 prompt、setup、judge 规则）和可选的 fixture 文件。
