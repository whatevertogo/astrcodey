# AstrCode 扩展系统

> **本文档是 AstrCode 扩展机制的唯一规范说明。**  
> 内容以当前代码为准（`astrcode-core`、`astrcode-extension-sdk`、`astrcode-extensions`、`astrcode-server`）。  
> 历史文档 `plugin-system.md`（AstrBot s5r / IPC 起源）**不是**实现目标，仅作背景参考。

---

## 目录

1. [概览](#1-概览)
2. [代码地图](#2-代码地图)
3. [内置扩展（进程内）](#3-内置扩展进程内)
4. [磁盘 WASM 扩展](#4-磁盘-wasm-扩展)
5. [s5r 对称 peer 协议](#5-s5r-对称-peer-协议)
6. [宿主能力：`HostRouter` 与 `astrcode.*`](#6-宿主能力hostrouter-与-astrcode)
7. [运行时模型](#7-运行时模型)
8. [Guest 作者指南](#8-guest-作者指南)
9. [新增宿主能力 checklist](#9-新增宿主能力-checklist)
10. [边界与测试](#10-边界与测试)

---

## 1. 概览

AstrCode 的 **Extension（扩展）** 是主要可扩展机制：Agent 工具、斜杠命令、生命周期钩子、Prompt 片段、自定义 extension event 均通过 `Extension` trait 注册，由 `ExtensionRunner` 分发。

| 层级 | 实现 | 信任模型 |
|------|------|----------|
| **内置** | `astrcode-bundled-extensions` + 各 `astrcode-extension-*` | 进程内可信代码；通过 `ExtensionCtx::host_services()` 访问 `ExtensionHostServices` |
| **磁盘 WASM** | `~/.astrcode/extensions/`、`<project>/.astrcode/extensions/` | wasmtime 沙箱 + **s5r** 对称 peer + 能力白名单 |
| **外部工具** | `astrcode-extension-mcp` | MCP 子进程/HTTP；**不**实现 `Extension` trait |

**不做的事：**

- 磁盘路径**仅支持 `.wasm`**，无 native dlopen / FFI 加载。
- **不支持 s6r**：旧 guest（`extension_manifest` / `extension_call` / `host_invoke` / `host_emit`）无法加载。
- 不把 AstrBot 式 STDIO / `platform.*` IPC 当作 coding agent 的实现目标（MCP 已覆盖「任意语言外部工具」）。
- bundled 扩展**不走 WASM**（`Arc<dyn LlmProvider>` 无法跨 WASM 边界）。

---

## 2. 代码地图

| Crate / 模块 | 职责 |
|--------------|------|
| `astrcode-core::extension` | `Extension` trait、`ExtensionCapability`、`ExtensionEventSink`、`Registrar`、各 Hook 上下文 |
| `astrcode-extension-sdk::s5r` | 线缆类型：`WireMessage`、`HandlerResult`、`InitializeMsg`、能力名映射 |
| `astrcode-extensions::loader` | 磁盘发现、`extension.json`（`protocol.s5r`）解析、WASM 加载 |
| `astrcode-extensions::runner` | `ExtensionRunner`：注册、hook 分发、能力门控、event sink 绑定 |
| `astrcode-extensions::wasm_ext` | `WasmExtension`：s5r 桥接、专用 guest 线程 |
| `astrcode-extensions::extension_peer` | 单条宿主 ↔ guest 对等会话（握手、pending、stream、cancel） |
| `astrcode-extensions::wasm_peer_transport` | `peer_exchange` 阻塞交换 |
| `astrcode-extensions::host_router` | 唯一 `astrcode.*` 实现 + `emit_for_sink` |
| `astrcode-extensions::wasm_api` | wasmtime `HostState`、linker、`host_log`、嵌套 `peer_exchange` |
| `astrcode-server::bootstrap` | 启动时注入 `build_host_router` 到 `ExtensionLoadContext` |

**参考实现：**

- 进程内扩展：`crates/astrcode-extension-memory`
- WASM guest 示例：`crates/astrcode-extensions/tests/s5r-guest/`

---

## 3. 内置扩展（进程内）

### 3.1 Extension trait

每个扩展实现 `Extension`：

- `id()` — 唯一标识
- `capabilities()` — 声明需要的 `ExtensionCapability`（默认 `[]`）
- `register(&mut Registrar)` — 注册 tools、commands、hooks、extension events
- `start(ExtensionCtx)` / `stop(StopReason)` / `health()` / `on_config_changed()`

`ExtensionCtx` 在 `start()` 时提供：

- `startup_working_dir`
- `event_sink`（启动阶段 emit 自定义 event）
- `host_services`（`ExtensionHostServices`：可选 `session_read`、`small_llm`）

### 3.2 ExtensionCapability

定义于 `astrcode-core::extension::ExtensionCapability`（serde `snake_case`）：

| 枚举 | manifest 请求名 | 线缆 `astrcode.*` |
|------|----------------|-------------------|
| `SessionState` | `session_state` | `astrcode.session.state.read` / `.write` |
| `SessionControl` | `session_control` | `astrcode.session.control.*` |
| `SmallModel` | `small_model` | `astrcode.llm.small_chat` |
| `SessionHistory` | `session_history` | `astrcode.session.read_events` |
| `EmitEvents` | `emit_events` | `astrcode.event.emit` |
| `WorkspaceRead` | `workspace_read` | `astrcode.workspace.read` |
| `ProcessSpawn` | `process_spawn` | （预留；宿主返回 `not_implemented`） |
| `NetworkClient` | `network_client` | （预留；宿主返回 `not_implemented`） |

`ExtensionRunner` 按声明裁剪注入到工具/钩子上下文的敏感字段。

### 3.3 Hook 模式

| 模式 | 行为 |
|------|------|
| `Blocking` | 同步；可 block / modify |
| `NonBlocking` | 异步 fire-and-forget |
| `Advisory` | 执行但结果仅供参考 |

### 3.4 Extension event

内置扩展通过 `Registrar::extension_event("type.name")` 声明可 emit 的类型；运行时 `BoundExtensionEventSink` 校验后写入会话事件流。WASM 扩展通过 `astrcode.event.emit` 在 guest 线程内发射（须声明 `emit_events` 且在 manifest 中注册 event 类型）。

---

## 4. 磁盘 WASM 扩展

### 4.1 目录布局

```
~/.astrcode/extensions/<name>/
  extension.json      # 发现入口（须声明 s5r）
  my_ext.wasm           # library 指向的文件

<project>/.astrcode/extensions/<name>/
  extension.json
  ...
```

项目级扩展在加载顺序中优先于全局扩展。

### 4.2 extension.json

**Loader 读取：**

| 字段 | 必填 | 说明 |
|------|------|------|
| `protocol.s5r` | 是 | 必须为 `"1.0"`，否则拒绝加载（**BREAKING**：s6r 已移除） |
| `library` | 是 | 相对路径指向 `.wasm` |

```json
{
  "protocol": { "s5r": "1.0" },
  "library": "my_extension.wasm"
}
```

`id`、能力、工具、hook 均由 guest 在 **Initialize 握手** 中声明，不由 `extension.json` 提供。

### 4.3 加载流程

```
extension.json (protocol.s5r == "1.0", library)
  → WasmExtension::load(path, fuel, memory_bytes, Arc<HostRouter>)
  → instantiate + extension_init()     # 同步完成 s5r Initialize 握手
  → ExtensionPeer 保存 manifest 注册表
  → 注册 tools / commands / hooks 到 WasmExtension
  → ExtensionRunner::register
```

Server bootstrap 构建 `ExtensionLoadContext`：

- `wasm_limits` — 配置 `wasmFuel` / `wasmMemoryMb`
- `host_router` — `Some(build_host_router(host_services))`（WASM **必需**）

---

## 5. s5r 对称 peer 协议

协议版本：`astrcode-extension-sdk::s5r::S5R_VERSION = "1.0"`，`S5R_STACK = "astrcode"`。

设计原则：**单一传输原语 `peer_exchange`**，五类 JSON 消息，握手后双向 invoke；宿主能力统一为 `astrcode.*`，guest 能力统一为 `handler.invoke`。

```mermaid
flowchart LR
  HR[HostRouter] --> EP[ExtensionPeer]
  EP <-->|peer_exchange| GW[WASM Guest]
```

### 5.1 Guest 导出

| 函数 | 签名 | 说明 |
|------|------|------|
| `memory` | export | 线性内存 |
| `alloc` | `(len: i32) -> i32` | 分配；失败返回 0 |
| `dealloc` | `(ptr: i32, len: i32)` | 释放 |
| `extension_init` | `() -> i32` | 启动时完成 Initialize 握手（返回 0=成功） |
| `peer_exchange` | `(req_ptr, req_len) -> i64` | 阻塞交换一条 `WireMessage` JSON |

**packed i64** = `(ptr as u64) << 32 | (len as u64)`。读取响应后须对 guest 分配的指针 `dealloc`。

### 5.2 Host import

| import | 说明 |
|--------|------|
| `host_log` | `(level, msg_ptr, msg_len)` |
| `peer_exchange` | 与 guest 对称；guest 在 handler 内 invoke `astrcode.*` 时由宿主路由到 `HostRouter` |
| WASI preview1 | `wasm32-wasip1` guest |

**已删除**：`host_emit`、`host_invoke`、`extension_manifest`、`extension_call`。

### 5.3 WireMessage（五类）

| type | 方向 | 用途 |
|------|------|------|
| `initialize` | Guest→Host（握手） | 声明 peer、handlers、requested_capabilities |
| `result` | 双向 | 对 invoke/initialize 的应答 |
| `invoke` | 双向 | `capability` + `input`；`stream: true` 启用流式 |
| `event` | 双向 | 流式阶段：`started` / `delta` / `completed` / `failed` |
| `cancel` | 双向 | 取消进行中的 invoke（`CancellationToken`） |

**Host → Guest**：仅 `handler.invoke`（`capability: "handler.invoke"`，`input.handler_id` + `input.event`）。

**Guest → Host**：`invoke` 到 `astrcode.*`（须握手时 `requested_capabilities` 已声明且宿主授权）。

### 5.4 Initialize（握手）

Guest 在 `extension_init` 中发送 `Initialize`，宿主回复 `Result`（`kind: initialize_result`）：

```json
{
  "type": "initialize",
  "id": "init-1",
  "peer": { "name": "my-ext", "role": "extension", "version": "0.1.0" },
  "handlers": [
    { "handler_id": "my-ext:tool:greet", "description": "Say hello" }
  ],
  "requested_capabilities": ["small_model", "emit_events"],
  "metadata": {
    "tools": [...],
    "commands": [...],
    "hooks": [{ "on": "pre_tool_use", "mode": "blocking" }],
    "extension_events": [...]
  }
}
```

`metadata` 承载原 s6r manifest 中的 tools/commands/hooks/events 列表（由 `extension_peer::parse_initialize` 解析）。

`handler_id` 约定：`{extension_id}:{kind}:{name}`，其中 `kind` 为 `tool` | `command` | `hook`。

### 5.5 handler.invoke（宿主 → guest）

```json
{
  "type": "invoke",
  "id": "req-1",
  "capability": "handler.invoke",
  "input": {
    "handler_id": "my-ext:hook:pre_tool_use",
    "event": { "on": "pre_tool_use", "input": { ... } }
  }
}
```

成功时 `result.output` 为 **HandlerResult**：

```json
{
  "ok": true,
  "effect": "ok",
  "data": { "content": "..." },
  "continuations": [
    { "call": "hook", "on": "turn_end", "input": { ... } }
  ]
}
```

**Effect 字符串**（与旧 s6r 对齐）：

| effect | 用途 |
|--------|------|
| `ok` | 默认成功 |
| `block` | blocking hook 阻止 |
| `modified_input` | 修改工具入参 |
| `tool_outcome` | 自定义工具结果 |
| `prompt_contributions` / `compact_contributions` | Prompt / Compact |
| `replace_messages` / `append_messages` | Provider hook |

Continuations 在宿主 `extension_call` 返回后**顺序**调度，深度上限 **16**（`MAX_CONTINUATION_DEPTH`，见 `extension_peer.rs`）。

超过上限时，宿主停止继续调度并返回 `ExtensionError::Internal`，消息为 `continuation depth exceeded (max 16)`；guest 不应假设未完成链路上的后续 continuation 仍会执行。若需更深流水线，应拆成多次显式 invoke 或合并 handler 逻辑。

### 5.6 流式 invoke

`invoke` 设 `stream: true` 时，应答序列为：

1. `event` phase `started`
2. 零或多个 `event` phase `delta`（`payload` 字段承载片段）
3. `event` phase `completed`（含最终 output）或 `failed`

宿主对 `astrcode.llm.small_chat` 在流式路径上收集全部 delta 后，经单次 `peer_exchange` 返回终态 `event`（`completed` 或 `failed`）；`completed.data` 含完整 `chunks` 数组。guest 可同样对流式 handler 响应。

### 5.7 hooks 事件名

`hooks[].on` 映射（`s5r::event_from_name`）：

`session_start` · `session_resume` · `session_shutdown` · `turn_start` · `turn_end` · `turn_aborted` · `step_start` · `step_end` · `pre_tool_use` · `post_tool_use` · `post_tool_use_failure` · `before_provider_request` · `after_provider_response` · `user_prompt_submit` · `prompt_build` · `pre_compact` · `post_compact` · `post_recap`

---

## 6. 宿主能力：`HostRouter` 与 `astrcode.*`

`HostRouter` 是 WASM guest 访问宿主的**唯一**入口（经 `ExtensionPeer` → `peer_exchange` 嵌套 import）。

### 6.1 鉴权

`HostRouter::authorize_astrcode` 对照握手时批准的 `declared_capabilities`（与 `ExtensionRunner` per-extension allows 同源）。未声明的能力 invoke 返回 `permission_denied`。

### 6.2 已实现能力

| capability | 需要声明 | 说明 |
|------------|----------|------|
| `astrcode.llm.small_chat` | `small_model` | 小模型聊天；支持 `stream` |
| `astrcode.session.read_events` | `session_history` | `replay_events`，input: `session_id`, `limit` |
| `astrcode.session.control.create` | `session_control` | 创建子 session |
| `astrcode.session.control.submit_turn` | `session_control` | 提交 turn |
| `astrcode.session.control.dispose` | `session_control` | `recycle_session` |
| `astrcode.session.state.read` | `session_state` | 读扩展状态目录 |
| `astrcode.session.state.write` | `session_state` | 写扩展状态目录 |
| `astrcode.event.emit` | `emit_events` | 发射已声明 extension event |
| `astrcode.workspace.read` | `workspace_read` | 读工作区文件（路径校验；拒绝 `..`、绝对路径、符号链接；默认最大 1 MiB，可选 `max_bytes`） |
| `astrcode.process.spawn` | `process_spawn` | **未实现** — 返回 `not_implemented` |
| `astrcode.network.client` | `network_client` | **未实现** — 返回 `not_implemented` |

同步 invoke 在专用 guest OS 线程上通过内部 Tokio runtime `block_on` 执行（guest 线程无 tokio 上下文）。

### 6.3 错误载荷

```json
{ "code": "permission_denied", "message": "..." }
```

---

## 7. 运行时模型

- **专用 guest 线程**：`WasmPeerTransport::spawn` 运行 wasmtime；主 async 任务通过 channel 提交 `peer_exchange` 作业。
- **Turn 事件桥**：`process_prompt` 启动时 `ExtensionEventBridge` 将 hook 上下文的 `event_tx` 转发到 `TurnPublisher`（与工具路径的 per-call 桥一致），内置与 WASM 的 `astrcode.event.emit` / `ExtensionEventSink` 均写入会话事件流。
- **重入**：guest handler 内可再次 `peer_exchange` 调用宿主能力；深度上限 **8**。
- **取消**：`Cancel` 消息关联 `CancellationToken`，中断进行中的 LLM / 流式读取。
- **失败关闭**：guest 线程退出时 fail-all-pending，工具/hook 调用方收到明确错误。

---

## 8. Guest 作者指南

### 8.1 构建

```bash
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release
```

### 8.2 最小 extension.json

见 [§4.2](#42-extensionjson)。

### 8.3 SDK 类型

Rust guest 可直接序列化 `astrcode_extension_sdk::s5r` 类型；参考 guest 使用手写 JSON 以减小依赖：

- `crates/astrcode-extensions/tests/s5r-guest/`
- E2E：`cargo test -p astrcode-extensions --test s5r_e2e_test`（需先编译 guest WASM）

### 8.4 从 s6r 迁移（BREAKING）

| s6r | s5r |
|-----|-----|
| `extension_manifest()` | `extension_init` + Initialize |
| `extension_call` | 宿主 `handler.invoke` |
| `host_invoke("small_llm.chat")` | `invoke` → `astrcode.llm.small_chat` |
| `host_emit` | `invoke` → `astrcode.event.emit` |
| `extension.json` 仅 `library` | 必须 `protocol.s5r: "1.0"` |

---

## 9. 新增宿主能力 checklist

1. 在 `ExtensionCapability` 增加枚举（若需要 manifest 声明）。
2. 在 `s5r::capabilities` 增加 `astrcode.*` 名与 `capability_from_wire` 映射。
3. 在 `HostRouter::invoke_sync` / `invoke_stream` 实现路由与鉴权。
4. 在 `HostRouter::host_capability_descriptors` 暴露 descriptor（握手时返回给 guest）。
5. 更新本文档 §6.2 表格。
6. 在 `s5r-guest` + `s5r_e2e_test` 增加覆盖。

---

## 10. 边界与测试

**本 PR / 当前实现不做：**

- bundled 扩展走 s5r 线缆
- cdylib / STDIO worker / 扩展互 invoke
- Batch invoke
- `process_spawn` / `network_client` 宿主实现

**测试：**

```bash
cargo test -p astrcode-extension-sdk
cargo test -p astrcode-extensions
# guest WASM（在 tests/s5r-guest 目录）：
cargo build --target wasm32-wasip1 --release --offline
cargo test -p astrcode-extensions --test s5r_e2e_test
```

**Clippy：**

```bash
cargo clippy -p astrcode-extensions -p astrcode-extension-sdk --all-targets -- -D warnings
```
