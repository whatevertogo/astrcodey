# Astrcode v2 Architecture

## 概述

Astrcode 是一个 AI 编码代理平台，采用 **session-first 事件溯源架构**，前后端分离，扩展驱动一切。

**核心理念**：核心只保留 Agent Loop、Hooks、Compaction、Built-in Tools 四个能力，其余一切交给扩展实现。

## Crate 分层

```
Layer 4: Frontend    astrcode-cli ─── astrcode-tui ─── astrcode-exec ─── astrcode-client
Layer 3: Server      astrcode-server ─── astrcode-protocol
Layer 2: Extensions  astrcode-extensions
Layer 1: Services    astrcode-ai  astrcode-prompt  astrcode-tools  astrcode-storage  astrcode-context
Layer 0: Foundation  astrcode-core  astrcode-support
```

## 数据流

```
User (TUI/Exec) → ClientCommand (JSON-RPC/stdio)
                → Server receives command
                → SessionManager resolves session
                → Agent created from session events
                → Agent Loop: prompt assembly → LLM call → tool execution
                → Events appended to session event log
                → ServerEvent streamed back to client
                → Client renders response
```

## 核心概念

### Session（会话）
- 只追加的 JSONL 事件日志，是数据的唯一真相来源
- 支持 fork/branch/switch 形成 session 树
- 通过快照 + 尾部事件实现快速恢复

### Agent（代理）
- 临时处理器，从 session 事件日志重建状态
- 处理 turn 后写回新事件，可随时丢弃和重建
- 状态完全由 Session 事件定义

### Extension（扩展）
- 订阅 9 个当前生命周期事件，注册工具定义、工具执行 handler、命令和上下文提供者
- 3 种 HookMode：Blocking（可阻断）、NonBlocking（异步）、Advisory（参考）
- Skills、Agent Profiles、自定义行为全部由扩展实现

### Compaction（压缩）
- 上下文窗口管理流水线：token 估算 → 预算控制 → 微压缩 → 修剪 → LLM 驱动压缩
- 自动在上下文接近窗口限制时触发

## 通信协议

- JSON-RPC 2.0 over stdio（一期）
- ClientCommand（前端→后端）和 ServerEvent（后端→前端）
- UI 请求子协议：confirm/select/input/notify

## 配置系统

4 层加载（优先级从低到高）：
1. 内置默认值（deepseek + openai profiles）
2. 用户配置 `~/.astrcode/config.json`
3. 项目叠加 `.astrcode/config.json`
4. 环境变量
