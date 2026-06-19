# AstrCode 扩展系统

> 以当前代码为准（`astrcode-core`、`astrcode-extension-sdk`、`astrcode-extensions`、`astrcode-server`）。

---

## 1. 概览

| 层级 | 实现 | 说明 |
|------|------|------|
| **内置扩展** | `astrcode-bundled-extensions` + 各 `astrcode-extension-*` | 进程内 Rust，`ExtensionHostServices` 满能力 |
| **磁盘扩展** | s5r 子进程 | `~/.astrcode/extensions/`、`<project>/.astrcode/extensions/` |
| **外部工具** | `astrcode-extension-mcp` | MCP 子进程/HTTP，**不**实现 `Extension` trait |

磁盘扩展使用 **s5r** 协议：stdio 长度前缀帧 + JSON `WireMessage`（非 JSON-RPC）。详见 [s5r-protocol.md](s5r-protocol.md)。

**插件作者入门**：[extension-author-guide.md](extension-author-guide.md)

---

## 2. 代码地图

| 模块 | 职责 |
|------|------|
| `astrcode-core::extension` | `Extension` trait、能力、钩子、Registrar |
| `astrcode-extensions::loader` | 发现 `extension.json`、启动 s5r 子进程 |
| `astrcode-extensions::s5r_ext` | `S5rExtension`、Peer 会话、宿主 `invoke` 路由 |
| `astrcode-extensions::host_router` | 唯一 `astrcode.*` 宿主能力实现 |
| `astrcode-extensions::remote_manifest` | manifest 构建、HandlerResult 解析 |
| `astrcode-extension-sdk::s5r` | 线缆类型、`HandlerResult`、事件名、能力 wire 名 |
| `astrcode-extension-sdk::runtime` | `Peer`、帧传输、取消、流式 |
| `astrcode-extension-sdk::worker` | Worker 入口、`HandlerRegistry`、`HostClient` |

参考实现：`crates/astrcode-extensions/tests/s5r-guest/`  
E2E：`cargo test -p astrcode-extensions --test s5r_e2e_test`

Hook 语义矩阵见 [extension-hook-matrix.md](extension-hook-matrix.md)。

---

## 3. 内置扩展（进程内）

实现 `Extension` trait，在 `start()` 时通过 `ExtensionCtx` 获取配置与 `host_services`。

`ExtensionCapability` 控制宿主注入的敏感能力（`session_control`、`workspace_read` 等）。
当前 session 的 `session_store_dir` 和按 extension id 隔离的
`astrcode.session.state.read/write` 是默认 session 上下文/API，不需要 capability。

| 扩展 ID | Crate | 默认 | 说明 |
|---------|-------|------|------|
| `astrcode-agent-tools` | `astrcode-extension-agent-tools` | 启用 | 子 Agent 委派与发现 |
| `astrcode-mcp` | `astrcode-extension-mcp` | 启用 | MCP 客户端（stdio/HTTP） |
| `astrcode-skill` | `astrcode-extension-skill` | 启用 | 斜杠命令 Skill 发现与调度 |
| `astrcode-todo-tool` | `astrcode-extension-todo-tool` | 启用 | Todo 进度追踪工具 |
| `astrcode-mode` | `astrcode-extension-mode` | 启用 | Code / Plan 模式切换 |
| `astrcode-goal` | `astrcode-extension-goal` | 启用 | Codex-style session goal 与自动续跑 |
| `astrcode.memory` | `astrcode-extension-memory` | **关闭** | 项目级 Markdown 记忆 |
| `astrcode-channels` | `astrcode-extension-channels` | **关闭** | Telegram 通道桥接 |
| `astrcode-web-tools` | `astrcode-extension-web-tools` | 启用 | `web-search` / `fetch-url` 内置 Web 工具 |

通过 `config.json` 的 `extensionStates` 覆盖默认开关。配置示例见 [configuration.md](configuration.md#web-tools-extension)。

## 4. 磁盘 s5r 扩展

### 4.1 目录布局

```
~/.astrcode/extensions/my-ext/
  extension.json
  my-ext-binary

<project>/.astrcode/extensions/my-ext/
  extension.json
  ...
```

### 4.2 extension.json

| 字段 | 必填 | 说明 |
|------|------|------|
| `protocol.s5r` | 是 | `"1.0"` |
| `command` | 是 | 字符串数组：`[可执行文件, ...参数]` |
| `env` | 否 | 额外环境变量 |

```json
{
  "protocol": { "s5r": "1.0" },
  "command": ["./my-extension"]
}
```

### 4.3 握手与调用

1. 子进程启动后通过 `Worker::run_stdio()` 发送 `Initialize`（manifest 在 `metadata`）
2. 宿主回复 `initialize_result` 与授权的 `astrcode.*` 能力
3. 宿主经 `handler.invoke` 调用工具 / 命令 / 钩子
4. 子进程经 `astrcode.*` `invoke` 调用宿主能力（可 `stream: true`）

---

## 5. 宿主能力

见 `HostRouter`；除默认 session state API 外，子进程 invoke 的 capability 须以
`astrcode.` 开头，且 manifest 中已声明对应 capability。

默认可用、无需 manifest capability：

| API | 说明 |
|------|------|
| `astrcode.session.state.read` | 读取当前 session 下按 extension id 隔离的状态。 |
| `astrcode.session.state.write` | 写入当前 session 下按 extension id 隔离的状态。 |

`session_state` 不是有效 capability，插件不要在 manifest 中声明它。

---

## 6. 编写插件

使用 `astrcode-extension-sdk::worker::Worker` 注册 handler，参考 `tests/s5r-guest/src/main.rs` 与 `s5r_e2e_test.rs`。

**agent-tool 类外置插件**（子 Agent 委派）：见 [extension-author-guide.md — 外置 agent-tool](extension-author-guide.md#外置-agent-tool-类插件)。

### ContinueAfterStop 预算

`ContinueAfterStop` 是 blocking-only decision hook，注册时可声明
`ContinueAfterStopOptions`。默认不做 host 级次数限制，是否继续主要交给 handler
自己的状态机决定；需要 host 代为限制时声明 `ContinueAfterStopOptions::limited(n)`，
需要明确表达无限续跑时声明 `ContinueAfterStopOptions::unlimited()`。

磁盘 s5r 扩展的握手 manifest 可在 `continue_after_stop` hook 的 `options.max_per_turn` 上携带数字字段；缺省表示不限制，`-1` 也表示无限续跑，非负数表示每 turn 上限。宿主调用 hook 时会在 input 中传入 `continuations_this_turn`，表示当前 turn 已经发生的自动续跑次数。

### Typed decision hooks

进程内扩展还可以注册 typed decision hook：

| Hook | 用途 |
|------|------|
| `on_user_message_envelope(priority, handler)` | 用户消息写入 durable transcript 前的改写或阻断。 |
| `on_after_tool_results(priority, handler)` | 工具结果批次已提交后的继续/结束决策。 |

这两个 hook 不接收 `HookMode`，宿主总是按优先级同步等待。它们暂不暴露给磁盘
s5r manifest；s5r manifest 中声明 `user_message_envelope` 或
`after_tool_results` 会在握手校验阶段失败。

协议细节见 [s5r-protocol.md](s5r-protocol.md)。
