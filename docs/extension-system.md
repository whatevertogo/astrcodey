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

---

## 3. 内置扩展（进程内）

实现 `Extension` trait，在 `start()` 时通过 `ExtensionCtx` 获取配置与 `host_services`。

`ExtensionCapability` 控制宿主注入的敏感能力（`session_control`、`workspace_read` 等）。

---

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

实现于 `HostRouter`；子进程 invoke 的 capability 须以 `astrcode.` 开头，且 manifest 中已声明对应 capability。

| manifest capability | wire 名 | 说明 |
|---------------------|---------|------|
| `main_model` | `astrcode.llm.main_chat` | 主模型 chat（可 stream） |
| `small_model` | `astrcode.llm.small_chat` | 小模型 chat（可 stream） |
| `session_history` | `astrcode.session.read_events` | 读 session event log（access 校验） |
| `session_control` | `astrcode.session.control.create` | 创建子 session |
| `session_control` | `astrcode.session.control.submit_turn` | 提交 turn（见下） |
| `session_control` | `astrcode.session.control.dispose` | 销毁 session |
| `session_state` | `astrcode.session.state.read` / `.write` | 扩展命名空间 KV |
| `emit_events` | `astrcode.event.emit` | 声明式自定义事件 |
| `workspace_read` | `astrcode.workspace.read` | 读 working_dir 下文件 |
| `process_spawn` / `network_client` | 预留 | 当前宿主返回 `not_implemented` |

完整 descriptor 见 `host_router.rs::descriptors_for_capability`。

### `submit_turn` 与 TurnScheduler

扩展 API 不直接操作 `TurnHandle`；`HostRouter` → `ServerSessionOperations::submit_turn` → **`TurnScheduler`**：

- **同 session 同步**（`wait_for_result: true`）→ `submit_and_wait`
- **同 session 后台** → `submit_tracked`（completion watcher 链 + `pending_queues`）
- **子 session**（caller ≠ target）→ `submit_untracked` + `ChildTurnGuard`

进程内「是否有 turn 在跑」以 **`TurnRegistry`** 为权威（`query_session.has_active_turn` 仅看 registry，durable `phase` 可能 stale）。架构细节见 [architecture.md §2](architecture.md#2-server-架构)。

---

## 6. 编写插件

使用 `astrcode-extension-sdk::worker::Worker` 注册 handler，参考 `tests/s5r-guest/src/main.rs` 与 `s5r_e2e_test.rs`。

**agent-tool 类外置插件**（子 Agent 委派）：见 [extension-author-guide.md — 外置 agent-tool](extension-author-guide.md#外置-agent-tool-类插件)。

协议细节见 [s5r-protocol.md](s5r-protocol.md)。
