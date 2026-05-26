# AstrCode 项目结构

## 目录概览

```
astrcode/
├── crates/              # Rust workspace（21 个 crate）
├── frontend/            # React + TypeScript Web 前端
├── src-tauri/           # Tauri 桌面壳（workspace 第 22 个成员）
├── docs/                # 项目文档
├── eval-tasks/          # 评测用例和 fixture
├── scripts/             # 开发工具脚本
├── npm/                 # NPM 分发包配置
├── .github/workflows/   # CI/CD 工作流
└── target/              # 构建输出
```

## Crates 结构（`crates/` 21 个 + `src-tauri/` 桌面壳）

> 行数为 `crates/**/*.rs` 与 `src-tauri/**/*.rs` 的当前快照（含测试），会随开发变化。

### Layer 0: Foundation（基础层）

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-core` | 5.3k | 共享领域类型与核心 trait：tool、LLM provider、config、extension contract、prompt 组装、StatusItem/Keybinding |
| `astrcode-support` | 1.0k | 宿主环境工具：路径解析、shell 检测、工具结果持久化 |
| `astrcode-log` | 308 | 文件轮转、stderr 输出、env-filter 日志 |

### Layer 1: Domain Services（领域服务层）

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-ai` | 3.5k | 多 Provider LLM（Anthropic、OpenAI 兼容、Google GenAI）、SSE 流解析、重试 |
| `astrcode-tools` | 5.1k | 内置工具（read/write/edit/patch/find/grep/shell/terminal/task）、注册表与执行包装 |
| `astrcode-storage` | 3.8k | JSONL event log、snapshot、config 持久化、文件锁 |
| `astrcode-context` | 3.6k | token 估算、tool result 预算、压缩、post-compact 恢复、prompt engine |
| `astrcode-session` | 8.0k | Agent 循环：turn runner、tool pipeline、LLM 流、compact 编排、SessionRuntimeServices、压缩熔断器 |
| `astrcode-extensions` | 5.1k | 扩展加载、hook 分发、能力门控、WASM 运行时（wasmtime + s5r）、keybinding/status item |

### Layer 2: Extensions（扩展层）

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-extension-sdk` | 642 | 扩展作者稳定 API、s5r 线协议类型、manifest/Registrar 辅助 |
| `astrcode-bundled-extensions` | 88 | 第一方扩展 crate 的组合根 |
| `astrcode-extension-mode` | 978 | Code / Plan 模式、Exit Gate、计划 Artifact、快捷键与状态栏 |
| `astrcode-extension-skill` | 852 | 斜杠命令技能发现与 Skill 工具 |
| `astrcode-extension-todo-tool` | 786 | 进度追踪 Todo 工具 |
| `astrcode-extension-agent-tools` | 658 | 子 Agent 委派、Agent 发现（兼容 Claude Code 格式） |
| `astrcode-extension-mcp` | 2.7k | MCP 客户端：stdio/HTTP、常驻进程池、后台预热、健康检查 |
| `astrcode-extension-memory` | 1.6k | 项目作用域 Markdown 记忆（默认关闭） |

### Layer 3: Server & Protocol（服务与协议层）

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-protocol` | 1.3k | JSON-RPC 命令/事件、UI 子协议、HTTP DTO、Keybinding/StatusItem DTO |
| `astrcode-server` | 12.2k | session 管理、config service、transport、HTTP（routes/projection/stream/auth）、ACP |

### Layer 4: Clients（客户端层）

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-client` | 617 | 类型化 JSON-RPC client、传输抽象、流订阅 |
| `astrcode-cli` | 7.7k | TUI (ratatui)、headless exec、server launcher、keybinding 运行时 |

### Eval（评测层）

| Crate | 行数 | 描述 |
|-------|------|------|
| `astrcode-eval` | 1.0k | 评测框架：HTTP 服务器控制、事件日志指标、结构化报告 |

### Desktop Shell（桌面壳）

| 组件 | 行数 | 描述 |
|------|------|------|
| `src-tauri` | ~690 | Tauri v2：sidecar、单实例、原生对话框、`tauri-plugin-http` |

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
│   │   ├── sse-stream.ts   # SSE 流处理（Tauri 环境使用 extension-http）
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
├── capabilities/           # 权限配置（含 HTTP extension 权限）
├── icons/                  # 应用图标
├── binaries/               # 嵌入式二进制 (astrcode-http-server)
├── Cargo.toml
└── tauri.conf.json         # Tauri 配置
```

### 架构

- **Sidecar 模式**: 嵌入 `astrcode-http-server` 作为 sidecar 进程
- **通信方式**: HTTP API + SSE（本地动态端口）
- **单实例协调**: 文件锁 + TCP 激活（后启动的实例通知已有实例聚焦窗口）
- **HTTP extension**: 通过 `tauri-plugin-http` 绕过 webkit2gtk 网络栈，解决 Linux SSE 缓冲问题
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

- **Rust**（`crates/`）: ~66.9k 行，21 个 crate，256 个 `.rs` 文件
- **Rust**（`src-tauri/`）: ~690 行，5 个 `.rs` 文件
- **Rust 合计**: ~67.6k 行，261 个 `.rs` 文件
- **TypeScript/TSX**（`frontend/`）: ~6.3k 行
- **整体**: ~74k 行（Rust + 前端）

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
