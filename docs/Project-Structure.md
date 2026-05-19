# AstrCode 项目结构

## 目录概览

```
astrcode/
├── crates/              # Rust workspace (19 crates)
├── frontend/            # React + TypeScript + Tauri 桌面前端
├── src-tauri/           # Tauri 配置和 Rust 桥接代码
├── docs/                # 项目文档
├── scripts/             # 开发工具脚本
└── target/              # 构建输出
```

## Crates 结构 (19 crates)

### Layer 0: Foundation (基础层)

| Crate | 描述 |
|-------|------|
| `astrcode-core` | 共享领域类型与核心 trait，包括 tool、LLM provider、config 抽象、extension contract、prompt 组装 trait |
| `astrcode-support` | 宿主环境集成辅助能力，包括路径解析、shell 检测 |
| `astrcode-log` | 文件旋转、stderr 输出、env-filter 日志 |

### Layer 1: Services (服务层)

| Crate | 描述 |
|-------|------|
| `astrcode-ai` | OpenAI 兼容 provider、SSE 流解析、重试、缓存追踪 |
| `astrcode-tools` | 内置工具、工具注册表、执行包装、agent 协作工具 |
| `astrcode-storage` | JSONL event log、snapshot、config 持久化、锁 |
| `astrcode-context` | token 估算、tool result 预算、裁剪、压缩、文件恢复、prompt engine |
| `astrcode-session` | 会话运行时：session handle、turn 执行、工具管线、事件 fanout、SessionRuntimeServices 共享 |
| `astrcode-extensions` | 扩展加载、生命周期分发、hook 执行策略、超时处理、能力注册、WASM 扩展运行时 |

### Layer 2: Extensions (扩展层)

| Crate | 描述 |
|-------|------|
| `astrcode-extension-mode` | Agent 运行模式切换（Code / Plan），包含 Exit Gate、计划 Artifact 持久化 |
| `astrcode-extension-skill` | 斜杠命令技能发现与分发 |
| `astrcode-extension-todo-tool` | 进度追踪 Todo 工具 |
| `astrcode-extension-agent-tools` | 子 Agent 委派（Agent 工具） |
| `astrcode-extension-mcp` | MCP 协议客户端（stdio）、工具发现 |
| `astrcode-bundled-extensions` | 可选扩展 crate 的组合根，通过 feature flag 控制启用 |

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
│   │   ├── Sidebar/        # 侧边栏组件
│   │   ├── Settings/       # 设置面板
│   │   ├── ConnectingScreen.tsx
│   │   └── ErrorBoundary.tsx
│   ├── services/
│   │   ├── api.ts          # HTTP API 调用
│   │   ├── protocol.ts     # 协议类型定义
│   │   └── sse-stream.ts   # SSE 流处理（Tauri 环境使用 plugin-http）
│   ├── store/
│   │   └── conversation.ts # 会话状态管理
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
├── capabilities/           # 权限配置（含 HTTP plugin 权限）
├── icons/                  # 应用图标
├── binaries/               # 嵌入式二进制 (astrcode-http-server)
├── Cargo.toml
└── tauri.conf.json         # Tauri 配置
```

### 架构

- **Sidecar 模式**: 嵌入 `astrcode-http-server` 作为 sidecar 进程
- **通信方式**: HTTP API + SSE（本地动态端口）
- **HTTP Plugin**: 通过 `tauri-plugin-http` 绕过 webkit2gtk 网络栈，解决 Linux SSE 缓冲问题
- **安全策略**: CSP 配置限制外部连接

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

- **Rust**: ~50k 行，19 crates，158 源文件
- **TypeScript/TSX**: ~4.7k 行，34 源文件
- **Tauri (Rust)**: ~670 行
