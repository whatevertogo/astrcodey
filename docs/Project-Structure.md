# AstrCode 项目结构

## 目录概览

```
astrcode/
├── crates/              # Rust workspace (18 crates)
├── frontend/            # React + TypeScript + Tauri 桌面前端
├── src-tauri/           # Tauri 配置和 Rust 桥接代码
├── docs/                # 项目文档
└── target/              # 构建输出
```

## Crates 结构 (18 crates)

### Layer 0: Foundation (基础层)

| Crate | 描述 |
|-------|------|
| `astrcode-core` | 共享领域类型与核心 trait，包括 tool、LLM provider、config 抽象、extension contract、prompt 组装 trait |
| `astrcode-support` | 宿主环境集成辅助能力，包括路径解析、shell 检测、tool result 持久化工具 |
| `astrcode-log` | 文件旋转、stderr 输出、env-filter 日志 |

### Layer 1: Services (服务层)

| Crate | 描述 |
|-------|------|
| `astrcode-ai` | OpenAI 兼容 provider、SSE 流解析、重试、缓存追踪 |
| `astrcode-tools` | 内置工具、工具注册表、执行包装、agent 协作工具 |
| `astrcode-storage` | JSONL event log、snapshot、config 持久化、锁 |
| `astrcode-context` | token 估算、tool result 预算、裁剪、压缩、文件恢复、prompt engine |
| `astrcode-session` | 会话运行时：session handle、turn 执行、事件总线、工具管线 |

### Layer 2: Extensions (扩展层)

| Crate | 描述 |
|-------|------|
| `astrcode-extensions` | 扩展加载、生命周期分发、hook 执行策略、超时处理、能力注册、WASM 扩展运行时 |
| `astrcode-extension-mode` | Agent 运行模式切换（Code / Plan），包含 Exit Gate、计划 Artifact 持久化 |
| `astrcode-extension-skill` | 斜杠命令技能发现与分发 |
| `astrcode-extension-todo-tool` | 进度追踪 Todo 工具 |
| `astrcode-extension-agent-tools` | 子 Agent 委派（Agent 工具） |
| `astrcode-extension-mcp` | MCP 协议客户端（stdio）、工具发现 |

### Layer 3: Server (服务层)

| Crate | 描述 |
|-------|------|
| `astrcode-protocol` | 类型化 JSON-RPC 命令、事件、UI 子协议、版本协商 |
| `astrcode-server` | session 生命周期、agent 编排、config service、transport handling |

### Layer 4: Frontend (前端层)

| Crate | 描述 |
|-------|------|
| `astrcode-client` | 面向 transport 的类型化 client 抽象 |
| `astrcode-cli` | Terminal UI (ratatui)、headless exec、server launcher |

## Frontend 结构

### 技术栈

- **框架**: React 19 + TypeScript
- **构建**: Vite 8
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
│   │   │   ├── ChatView.tsx
│   │   │   ├── MessageList.tsx
│   │   │   ├── AssistantMessage.tsx
│   │   │   ├── UserMessage.tsx
│   │   │   ├── ToolCallBlock.tsx
│   │   │   ├── InputBar.tsx
│   │   │   ├── TopBar.tsx
│   │   │   └── ...
│   │   ├── Sidebar/        # 侧边栏组件
│   │   │   ├── Sidebar.tsx
│   │   │   ├── SessionItem.tsx
│   │   │   ├── ProjectGroup.tsx
│   │   │   └── NewProjectModal.tsx
│   │   ├── Settings/       # 设置面板
│   │   │   └── SettingsModal.tsx
│   │   ├── ConnectingScreen.tsx
│   │   └── ErrorBoundary.tsx
│   ├── services/
│   │   ├── api.ts          # HTTP API 调用
│   │   ├── protocol.ts     # 协议类型定义
│   │   └── sse-stream.ts   # SSE 流处理
│   ├── store/
│   │   └── conversation.ts # 会话状态管理
│   ├── hooks/
│   │   └── useSidebarResize.ts
│   ├── lib/
│   │   ├── hostBridge.ts   # 与 Tauri/Host 通信
│   │   ├── tauri.ts        # Tauri API 封装
│   │   ├── logger.ts       # 日志工具
│   │   └── utils.ts        # 通用工具
│   ├── main.tsx
│   ├── App.tsx
│   └── index.css
├── scripts/                # 构建脚本
│   ├── prepare-sidecar.mjs # 准备 sidecar 二进制
│   └── protocol-contract.test.mjs
├── package.json
├── vite.config.ts
├── tsconfig.json
└── eslint.config.js
```

## Tauri Desktop 结构

```
src-tauri/
├── src/                    # Tauri Rust 源码
├── capabilities/           # 权限配置
├── icons/                  # 应用图标
├── binaries/               # 嵌入式二进制 (astrcode-server)
├── Cargo.toml
└── tauri.conf.json         # Tauri 配置
```

### 架构

- **Sidecar 模式**: 嵌入 `astrcode-server` 作为 sidecar 进程
- **通信方式**: HTTP API + SSE (本地端口)
- **安全策略**: CSP 配置限制外部连接

## 关键文件

| 文件 | 描述 |
|------|------|
| `Cargo.toml` | Workspace 配置，定义 18 个 member crates |
| `rust-toolchain.toml` | Rust nightly 工具链指定 |
| `rustfmt.toml` / `.clippy.toml` | 代码风格配置 |
| `AGENTS.md` | 项目编码规范与架构原则 |
| `PROJECT_ARCHITECTURE.md` | v2 目标架构设计文档 |
| `CLAUDE.md` | 快速参考：构建命令、代码规范 |

## 构建命令

### Rust

```bash
# 格式化
cargo fmt
cargo fmt --check

# Lint
cargo clippy --workspace
cargo clippy --workspace -- -D warnings

# 测试
cargo test --workspace
cargo test -p <crate>

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
npm run lint
npm run typecheck
npm run format:check

# Tauri 桌面应用
cd ..
npx @tauri-apps/cli dev      # 开发模式
npx @tauri-apps/cli build    # 发布构建
```

## 总计代码统计

- **Rust**: ~49k 行，18 crates，153 源文件
- **TypeScript/TSX**: ~2.8k 行，34 源文件
- **Total**: ~52k 行
