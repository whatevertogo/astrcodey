# AstrCode 插件系统设计规范 v1.0

> **⚠️ AstrCode 产品说明**  
> 本文档描述的是 **AstrBot 起源的 s5r 双向插件平台**（`platform.*`、IPC STDIO、`plugin_init` 等），  
> **不是** 当前 AstrCode coding agent 的实现规范。  
> **请以 [`extension-system.md`](./extension-system.md) + [`plugin-system-wasm-s6r.md`](./plugin-system-wasm-s6r.md) + [`host-invoke-plan.md`](./host-invoke-plan.md) 为准。**

> **适用读者**：实现本系统的 agent 或开发者。本文档是可执行规范，所有类型、帧格式、ABI
> 均可直接作为实现依据。

---

## 目录

1. [设计决策](#一设计决策)
2. [架构总览](#二架构总览)
3. [协议规范（s5r）](#三协议规范s5r)
4. [能力系统](#四能力系统)
5. [处理器系统](#五处理器系统)
6. [传输层](#六传输层)
7. [Crate 布局](#七crate-布局)
8. [宿主侧实现要点](#八宿主侧实现要点)
9. [插件 SDK](#九插件-sdk)
10. [端到端示例](#十端到端示例)
11. [实现优先级](#十一实现优先级)
12. [附录：错误规范](#附录错误规范)

---

## 一、设计决策

### 1.1 为什么保留通信协议？

现有 WASM 系统有一个根本缺陷：**单向性**。插件只能被动响应事件，无法主动调用宿主能力（LLM、数据库、平台发消息）。任何有意义的插件都需要回调宿主。

s5r 协议（借鉴自 AstrBot）的核心价值是**双向能力交换模型**：

- 宿主暴露能力（`llm.chat`、`db.set`、`platform.send`……）给插件调用
- 插件暴露能力（自定义功能）给宿主和其他插件调用
- 两个方向都是同一套 `invoke → result` 消息

这与传输载体（WASM 线性内存 vs STDIO 字节流）正交。因此：

> **s5r 是协议模型，不是传输实现。WASM 和 IPC 都跑 s5r。**

### 1.2 双路径插件模型

| 路径 | 载体 | 语言 | 隔离 | 能力回调 | 流式 |
|------|------|------|------|----------|------|
| **WASM**（主路径） | 线性内存 + host imports | Rust（推荐）/ C / ... | wasmtime 沙箱 | 同步 `host_invoke` | v1 不支持 |
| **IPC**（兼容路径） | STDIO 帧 / WebSocket | 任意语言 | OS 进程 | 全异步 | 支持 |

两路径共享：
- 相同的 `HandlerDescriptor` / `CapabilityDescriptor`
- 相同的事件 payload 结构
- 相同的错误规范

### 1.3 与 AstrBot s5r 的差异

| 方面 | AstrBot (Python) | AstrCode (Rust) |
|------|-----------------|-----------------|
| 主路径 | 子进程 STDIO | WASM（沙箱内） |
| 异步模型 | asyncio | Tokio |
| 握手方向 | 插件先发 Initialize | **宿主先发**（宿主知道自己的能力） |
| 取消令牌 | 自研 CancelToken | `tokio_util::sync::CancellationToken` |
| 环境隔离 | venv per plugin | WASM 天然隔离；IPC 进程自管 |
| SDK 注册方式 | Python 装饰器 | Rust proc macros |
| WASM 异步 | 不适用 | `spawn_blocking` + `Handle::block_on` |

---

## 二、架构总览

```
┌─────────────────────────────────────────────────────────────────┐
│                         Plugin Host                              │
│                                                                  │
│  ┌────────────────────┐        ┌─────────────────────────────┐  │
│  │  CapabilityRouter  │        │     HandlerDispatcher       │  │
│  │  ─────────────     │        │  ─────────────────────────  │  │
│  │  llm.*             │        │  command_handlers           │  │
│  │  db.*              │        │  message_handlers           │  │
│  │  memory.*          │        │  event_handlers             │  │
│  │  platform.*        │        │  schedule_handlers          │  │
│  │  + plugin caps     │        │  (按 priority 降序排列)      │  │
│  └────────┬───────────┘        └──────────────┬──────────────┘  │
│           │                                   │                  │
│  ┌────────┴───────────────────────────────────┴──────────────┐  │
│  │                       PluginSupervisor                     │  │
│  │              (管理所有 PluginSession 的生命周期)             │  │
│  └──────────────────────────┬─────────────────────────────────┘  │
└─────────────────────────────┼───────────────────────────────────┘
                              │ 每个插件一个 PluginSession
            ┌─────────────────┼─────────────────────┐
            │                 │                      │
     ┌──────┴──────┐  ┌───────┴──────┐  ┌───────────┴────────┐
     │ WasmSession │  │  IpcSession  │  │  WsSession（可选）  │
     │  ─────────  │  │  ──────────  │  │  ────────────────   │
     │  wasmtime   │  │  stdin/out   │  │  WebSocket          │
     │  Store+Inst │  │  帧格式读写  │  │  TLS 可选           │
     └──────┬──────┘  └──────┬───────┘  └──────────┬──────────┘
            │                │                      │
     ┌──────┴──┐      ┌──────┴──┐           ┌──────┴──┐
     │ .wasm   │      │ 子进程  │           │ WS 端点 │
     │ module  │      │ 任意语言│           │ 任意语言│
     └─────────┘      └─────────┘           └─────────┘
```

---

## 三、协议规范（s5r）

协议名：**s5r**，当前版本：`"1.0"`。  
同主版本号内向前兼容（1.x 插件可与 1.y 宿主通信，x ≠ y 时取 min）。

### 3.1 消息类型枚举

所有消息均为 JSON 对象，含 `"type"` 判别字段：

```
initialize          宿主 → 插件    握手请求（宿主先发）
initialize_result   插件 → 宿主    握手响应
invoke              双向           能力或处理器调用请求
result              双向           invoke 的响应
event               双向           流式 invoke 的增量响应
cancel              双向           取消正在进行的调用
```

### 3.2 InitializeMsg（宿主 → 插件）

```json
{
  "type": "initialize",
  "id": "init-<uuid>",
  "protocol_version": "1.0",
  "host_id": "astrcode-host",
  "host_version": "0.2.2",
  "host_capabilities": [ ...CapabilityDescriptor ],
  "metadata": {}
}
```

### 3.3 InitializeResultMsg（插件 → 宿主）

成功：
```json
{
  "type": "initialize_result",
  "id": "init-<uuid>",
  "success": true,
  "protocol_version": "1.0",
  "plugin_id": "my-plugin",
  "plugin_version": "0.1.0",
  "handlers": [ ...HandlerDescriptor ],
  "provided_capabilities": [ ...CapabilityDescriptor ],
  "metadata": {}
}
```

失败：
```json
{
  "type": "initialize_result",
  "id": "init-<uuid>",
  "success": false,
  "error": {
    "code": "version_mismatch",
    "message": "Protocol 2.0 not supported by this plugin",
    "retryable": false
  }
}
```

### 3.4 InvokeMsg（双向）

```json
{
  "type": "invoke",
  "id": "<uuid>",
  "capability": "llm.chat",
  "input": { ... },
  "stream": false,
  "caller_plugin_id": "my-plugin"
}
```

- **宿主 → 插件**：`capability` 为 `"handler.<handler_id>"`，`input` 为事件 payload
- **插件 → 宿主**：`capability` 为内置或其他插件提供的能力名
- `caller_plugin_id`：由宿主透传，插件发起时可省略

### 3.5 ResultMsg（双向）

成功：
```json
{
  "type": "result",
  "id": "<invoke id>",
  "success": true,
  "output": { ... }
}
```

失败：
```json
{
  "type": "result",
  "id": "<invoke id>",
  "success": false,
  "error": {
    "code": "capability_not_found",
    "message": "llm.chat not available",
    "hint": "Ensure LLM provider is configured",
    "retryable": false,
    "details": null
  }
}
```

**约束**（编码时验证）：`success=true` 时 `error` 必须为 null；`success=false` 时 `output` 必须为空对象 `{}`。

### 3.6 EventMsg（流式，双向）

流式调用生命周期：`started → delta* → completed | failed`

```json
{ "type": "event", "id": "<invoke id>", "phase": "started" }
{ "type": "event", "id": "<invoke id>", "phase": "delta",     "data": { "text": "Hello " } }
{ "type": "event", "id": "<invoke id>", "phase": "delta",     "data": { "text": "world" } }
{ "type": "event", "id": "<invoke id>", "phase": "completed", "output": { "full_text": "Hello world" } }
```

阶段字段约束：

| phase | 必须有 | 必须为空 |
|-------|--------|---------|
| started | — | data, output, error |
| delta | data | output, error |
| completed | output | data, error |
| failed | error | data, output |

### 3.7 CancelMsg（双向）

```json
{
  "type": "cancel",
  "id": "<invoke id>",
  "reason": "user_cancelled"
}
```

标准原因值：`"user_cancelled"` / `"timeout"` / `"plugin_unloaded"`

### 3.8 ErrorPayload 结构

```json
{
  "code": "snake_case_error_code",
  "message": "人类可读描述",
  "hint": "可选的解决建议",
  "retryable": false,
  "details": { "extra": "context" }
}
```

标准错误码：

| code | 含义 |
|------|------|
| `version_mismatch` | 协议版本不兼容 |
| `capability_not_found` | 能力不存在 |
| `handler_not_found` | 处理器不存在 |
| `permission_denied` | 权限不足 |
| `invalid_input` | 输入验证失败 |
| `internal_error` | 宿主内部错误 |
| `cancelled` | 调用被取消 |
| `timeout` | 调用超时 |
| `plugin_error` | 插件内部错误（来自 WASM trap 或 IPC 插件主动上报） |

### 3.9 帧格式（IPC 路径）

STDIO / TCP / WebSocket text frame 均使用**长度前缀帧**：

```
<decimal_length_utf8_bytes>\n<json_payload_bytes>
```

示例：
```
83\n
{"type":"invoke","id":"r1","capability":"db.get","input":{"key":"x"}}
```

- 长度：payload 的 UTF-8 字节数，不含换行符
- 最大帧：16 MiB（超出拒绝连接）
- 每帧恰好一条消息，不支持 batch

**可选 msgpack**：若双方握手 `metadata.wire_codec = "msgpack"`，后续帧改为 msgpack 编码（帧结构不变）。默认 JSON。

---

## 四、能力系统

### 4.1 CapabilityDescriptor

```rust
// astrcode-plugin-proto/src/descriptor.rs
pub struct CapabilityDescriptor {
    /// 格式："namespace.action"，如 "llm.chat"
    pub name: String,
    pub description: String,
    pub input_schema: Option<serde_json::Value>,
    pub output_schema: Option<serde_json::Value>,
    pub supports_stream: bool,
    pub cancelable: bool,
}
```

命名规则：
- 格式：`namespace.action`，支持多级：`llm_tool.manager.activate`
- **保留命名空间**（插件不得注册）：`handler.*`、`system.*`、`internal.*`
- 插件自定义能力推荐：`<plugin_id>.<action>`

### 4.2 内置宿主能力注册表

#### LLM 能力

**`llm.chat`**（非流式）

输入：
```json
{
  "messages": [
    { "role": "system", "content": "You are helpful." },
    { "role": "user",   "content": "Hello" }
  ],
  "model": "gpt-4o",
  "temperature": 0.7,
  "max_tokens": 2048,
  "session_id": "sess-xxx"
}
```
`model`、`temperature`、`max_tokens`、`session_id` 均可选。

输出：
```json
{
  "content": "Hi there!",
  "model": "gpt-4o",
  "usage": { "prompt_tokens": 12, "completion_tokens": 5 }
}
```

---

**`llm.stream_chat`**（流式，仅 IPC 路径 v1）

输入：同 `llm.chat`

EventMsg delta：
```json
{ "data": { "text": "Hi " } }
{ "data": { "text": "there!" } }
```
EventMsg completed：
```json
{ "output": { "full_content": "Hi there!", "model": "gpt-4o", "usage": {...} } }
```

#### 数据库能力（KV）

**`db.get`**

输入：`{ "key": "my-key" }`  
输出：`{ "value": <any>, "found": true }`（未找到时 `found: false, value: null`）

---

**`db.set`**

输入：`{ "key": "my-key", "value": <any>, "ttl_secs": 3600 }`  
`ttl_secs` 可选，省略表示永久。  
输出：`{}`

---

**`db.delete`**

输入：`{ "key": "my-key" }`  
输出：`{ "deleted": true }`

---

**`db.list_keys`**

输入：`{ "prefix": "my-plugin:", "limit": 100 }`  
输出：`{ "keys": ["my-plugin:a", "my-plugin:b"] }`

#### 语义记忆能力

**`memory.save`**

输入：`{ "content": "text to remember", "namespace": "my-plugin", "ttl_secs": null }`  
输出：`{ "id": "mem-uuid" }`

---

**`memory.search`**

输入：`{ "query": "user preferences", "namespace": "my-plugin", "limit": 5 }`  
输出：
```json
{
  "results": [
    { "id": "mem-xxx", "content": "...", "score": 0.92 }
  ]
}
```

---

**`memory.delete`**

输入：`{ "id": "mem-xxx" }`  
输出：`{}`

#### 平台能力

**`platform.send`**

输入：
```json
{
  "session_id": "telegram:group:123456",
  "text": "Hello!"
}
```
输出：`{}`

---

**`platform.send_image`**

输入：`{ "session_id": "...", "url": "https://..." }`  
输出：`{}`

---

**`platform.send_chain`**

输入：
```json
{
  "session_id": "...",
  "components": [
    { "type": "plain", "text": "Look: " },
    { "type": "image", "url": "https://..." }
  ]
}
```
输出：`{}`

消息组件类型：`plain`、`image`、`at`、`reply`、`file`。

#### 会话能力

**`session.get_data`**

输入：`{ "session_id": "...", "key": "my-state" }`  
输出：`{ "value": <any>, "found": true }`

---

**`session.set_data`**

输入：`{ "session_id": "...", "key": "my-state", "value": <any> }`  
输出：`{}`

#### 系统能力

**`system.data_dir`**

输入：`{ "plugin_id": "my-plugin" }`  
输出：`{ "path": "/home/user/.astrcode/plugins/my-plugin/data" }`

---

**`system.text_to_image`**

输入：`{ "text": "Hello World", "width": 800 }`  
输出：`{ "url": "data:image/png;base64,..." }`

---

**`system.html_render`**

输入：`{ "html": "<h1>Hello</h1>", "width": 800, "height": 600 }`  
输出：`{ "url": "data:image/png;base64,..." }`

---

## 五、处理器系统

### 5.1 HandlerDescriptor

```rust
// astrcode-plugin-proto/src/descriptor.rs
pub struct HandlerDescriptor {
    /// 格式建议："<plugin_id>.<fn_name>"
    pub id: String,
    pub trigger: Trigger,
    /// 越大越先执行，默认 0
    pub priority: i32,
    pub permissions: Permissions,
    pub filters: Vec<FilterSpec>,
    pub description: Option<String>,
}
```

### 5.2 触发器

```rust
pub enum Trigger {
    Command(CommandTrigger),
    Message(MessageTrigger),
    Event(EventTrigger),
    Schedule(ScheduleTrigger),
}

pub struct CommandTrigger {
    /// 命令名，不含 "/" 前缀
    pub command: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    /// 空 = 所有平台
    pub platforms: Vec<String>,
}

pub struct MessageTrigger {
    /// Rust regex，省略 = 匹配所有消息
    pub regex: Option<String>,
    /// 任意命中即触发（与 regex 的关系：OR）
    pub keywords: Vec<String>,
    pub platforms: Vec<String>,
}

pub struct EventTrigger {
    /// 如 "session_start"、"turn_end"、"provider_changed"
    pub event_type: String,
}

pub struct ScheduleTrigger {
    pub name: Option<String>,
    /// cron 和 interval_seconds 必须且只能有一个非 None
    pub cron: Option<String>,
    pub interval_seconds: Option<u64>,
    /// IANA 时区，默认 "UTC"
    pub timezone: Option<String>,
}
```

### 5.3 过滤器

```rust
pub enum FilterSpec {
    Platform  { platforms: Vec<String> },
    MessageType { message_types: Vec<String> }, // "group" | "private" | "channel"
    And { children: Vec<FilterSpec> },
    Or  { children: Vec<FilterSpec> },
}
```

过滤器与 `Trigger` 的平台字段是独立的：Trigger.platforms 做第一道筛，FilterSpec 做第二道。

### 5.4 权限

```rust
pub struct Permissions {
    pub require_admin: bool,
    pub required_role: Option<Role>,
    pub level: u32,
}

pub enum Role { Member, Admin }
```

### 5.5 处理器调用（InvokeMsg input）

宿主调用处理器时，`capability = "handler.<handler_id>"`，`input` 为以下结构之一。

**MessageEvent**（CommandTrigger / MessageTrigger）：

```json
{
  "text": "hello world",
  "user_id": "u123",
  "group_id": "g456",
  "platform": "telegram",
  "platform_id": "tg-inst-1",
  "session_id": "telegram:group:g456",
  "self_id": "bot789",
  "message_type": "group",
  "sender_name": "Alice",
  "is_admin": false,
  "messages": [
    { "type": "plain", "text": "hello world" },
    { "type": "image", "url": "https://..." }
  ],
  "timestamp": 1700000000
}
```

**ScheduleEvent**（ScheduleTrigger）：

```json
{
  "handler_id": "my-plugin.daily_report",
  "scheduled_at": "2024-01-01T09:00:00Z",
  "timezone": "Asia/Shanghai"
}
```

**SystemEvent**（EventTrigger）：

```json
{
  "event_type": "session_start",
  "session_id": "sess-xxx",
  "data": { ... }
}
```

### 5.6 处理器响应（ResultMsg output）

```json
{
  "stop_propagation": false,
  "reply": {
    "type": "chain",
    "components": [
      { "type": "plain", "text": "Hello!" },
      { "type": "image", "url": "https://..." }
    ]
  }
}
```

- `stop_propagation = true`：阻止同一事件继续分发给优先级更低的处理器
- `reply`：可选，宿主负责调用 `platform.send_chain` 发送
- 也可在处理器内直接调用 `platform.send`（更灵活）

---

## 六、传输层

### 6.1 WASM 路径

#### 6.1.1 Guest（插件）必须导出的函数

```
alloc(len: i32) -> i32
  分配 len 字节，返回起始指针。失败返回 0。
  宿主用于在 guest 内存中写入请求数据。

dealloc(ptr: i32, len: i32)
  释放之前 alloc 分配的内存块。

plugin_init() -> i64
  返回 (ptr as i64) << 32 | (len as i64)，指向 InitializeResultMsg JSON。
  此时宿主尚未发送 InitializeMsg；插件只需返回自己的 handlers 和
  provided_capabilities，不需要 host_capabilities（此时还没有）。

plugin_post_init(result_ptr: i32, result_len: i32)
  宿主将 InitializeMsg JSON 写入 guest 内存 [result_ptr, result_ptr+result_len)
  后调用此函数。插件可在此缓存宿主能力列表。

plugin_handle(request_ptr: i32, request_len: i32) -> i64
  宿主传入 InvokeMsg JSON，插件返回 (ptr << 32 | len) 指向 ResultMsg JSON。
  此函数在整个会话中可被多次调用（每次 invoke 调用一次）。
```

**内存所有权规则**：
- 宿主 `alloc` 的内存，宿主负责 `dealloc`
- 插件从 `plugin_handle` / `plugin_init` 返回的指针，宿主读取完毕后调用 `dealloc`

#### 6.1.2 Host Import 函数（宿主提供给插件）

模块名：`"env"`

```
host_invoke(
  cap_ptr:   i32,  // 能力名称 UTF-8 字符串起始地址（in guest memory）
  cap_len:   i32,  // 能力名称字节数
  input_ptr: i32,  // 输入 JSON UTF-8 起始地址（in guest memory）
  input_len: i32   // 输入 JSON 字节数
) -> i64
  同步调用宿主能力。
  返回 (result_ptr << 32 | result_len)，指向 ResultMsg JSON（in guest memory）。
  失败或能力不存在时返回 0，插件视为 { success: false, error: { code: "internal_error" } }。
  宿主内部使用 tokio 的 block_in_place + block_on 将异步能力转为同步。

host_log(level: i32, msg_ptr: i32, msg_len: i32)
  level: 0=trace 1=debug 2=info 3=warn 4=error
  宿主将日志转发到 tracing，target = "plugin::<plugin_id>"。
```

注意：WASM 路径 v1 **不支持流式能力**（`stream = true` 的 invoke 宿主直接返回 `invalid_input` 错误）。

#### 6.1.3 宿主 WASM 执行模型

```
WASM 调用必须运行在 spawn_blocking 中（wasmtime 是同步的）：

  tokio::task::spawn_blocking(move || {
      let rt = Handle::current();

      // host_invoke 实现：
      // let result = rt.block_on(capability_router.invoke(name, input));
      // write result to guest memory, return packed ptr

      wasm_instance.call_plugin_handle(&mut store, &request_bytes)
  }).await?
```

每个 WASM 插件实例持有独立的 `Store<HostState>`，不跨请求共享（无并发安全问题）。  
并发请求：克隆 `Instance`（`Instance::clone_without_env` 或每个请求新建 Store）。  
推荐：**每个 PluginSession 维护一个连接池**（`crossbeam_channel` 或 `Arc<Mutex<Store>>`），
控制并发度在 wasmtime 线程池大小以内。

#### 6.1.4 WASM 资源限制

```rust
pub struct WasmLimits {
    /// 单次 plugin_handle 调用的 fuel 预算（默认 10_000_000）
    pub fuel_per_call: u64,
    /// 线性内存上限（默认 64 MB）
    pub memory_bytes: usize,
    /// 单次调用超时（默认 30s，含 host_invoke 等待时间）
    pub call_timeout: Duration,
}
```

超出 fuel：wasmtime 返回 `Trap::OutOfFuel`，宿主记录为 `plugin_error`。  
超出内存：`ResourceLimiter::memory_growing` 返回 `Ok(false)`，guest 分配失败。

### 6.2 IPC 路径（STDIO）

#### 6.2.1 进程启动

```rust
Command::new(&plugin_config.command)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())  // stderr 透传到宿主 tracing（level=debug）
    .env("ASTRCODE_PROTOCOL_VERSION", "1.0")
    .env("ASTRCODE_PLUGIN_ID", &plugin_id)
    .env("ASTRCODE_WIRE_CODEC", "json")  // 或 "msgpack"
    .spawn()
```

#### 6.2.2 握手流程

```
宿主                                   插件进程
  |                                       |
  |------ InitializeMsg (frame) --------> |   宿主先发（携带 host_capabilities）
  |                                       |   插件解析，准备自己的 handlers
  | <----- InitializeResultMsg (frame) -- |   插件回复
  |                                       |
  |   [握手完成，双方进入全双工运行]         |
  |                                       |
  |------ InvokeMsg (handler.xxx) ------> |   宿主触发处理器
  |                                       |
  | <----- InvokeMsg (llm.chat) --------- |   插件回调宿主能力
  |------ ResultMsg (llm.chat) ---------> |
  |                                       |
  | <----- ResultMsg (handler result) --- |   处理器返回
```

#### 6.2.3 IPC Peer 宿主侧接口

```rust
// astrcode-plugin-host/src/transport/ipc.rs
pub struct IpcPeer {
    // 内部：tokio channel 写端 + pending map
}

impl IpcPeer {
    pub async fn initialize(&self, msg: InitializeMsg) -> Result<InitializeResultMsg, PluginError>;
    pub async fn invoke(&self, capability: &str, input: Value) -> Result<Value, PluginError>;
    pub async fn invoke_stream(
        &self,
        capability: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> Result<impl Stream<Item = Result<Value, PluginError>> + Send, PluginError>;
    pub async fn cancel(&self, request_id: &str, reason: &str);
    pub fn is_alive(&self) -> bool;
}
```

内部实现：

```rust
// 发送方向：mpsc::Sender<PluginMessage>（序列化后写 stdout）
// 接收方向：tokio::spawn 的读循环
//
// 读循环收到消息后：
//   ResultMsg  → oneshot::Sender::send（唤醒等待的 invoke）
//   EventMsg   → mpsc::Sender::send（发到流的 channel）
//   InvokeMsg  → capability_router.invoke(...)（处理插件的回调请求）
```

**并发请求 ID**：使用 `uuid::Uuid::new_v4().to_string()`，存入 `DashMap<String, PendingRequest>`。

#### 6.2.4 WebSocket 路径（可选，Phase 3）

帧格式与 STDIO 完全相同。区别：
- 服务端监听 `wss://127.0.0.1:<port>`（宿主启动，插件连接）
- 支持 mutual-TLS（双方各持证书）
- 启用方式：`plugin.toml` 中 `transport = "websocket"` + `url = "wss://..."`

---

## 七、Crate 布局

完全重构后的 crate 拓扑：

```
workspace/
  ├── astrcode-plugin-proto/          新建，替代 protocol 中的扩展消息部分
  │   Cargo.toml                      dependencies: serde, serde_json, thiserror
  │   src/
  │     lib.rs
  │     message.rs      ← 所有协议消息类型（InitializeMsg 等）
  │     descriptor.rs   ← HandlerDescriptor, CapabilityDescriptor, Trigger 等
  │     codec.rs        ← ProtocolCodec trait + JsonCodec + (optional) MsgpackCodec
  │     event.rs        ← MessageEvent payload, ScheduleEvent payload, SystemEvent payload
  │     error.rs        ← PluginProtocolError
  │
  ├── astrcode-plugin-host/           新建，替代 astrcode-extensions
  │   Cargo.toml   deps: plugin-proto, tokio, wasmtime, dashmap,
  │                      tokio-util(CancellationToken), tracing, thiserror
  │   src/
  │     lib.rs
  │     capability/
  │       mod.rs          ← CapabilityRouter trait + 注册辅助
  │       router.rs       ← DefaultCapabilityRouter 实现
  │       builtin/
  │         mod.rs
  │         llm.rs
  │         db.rs
  │         memory.rs
  │         platform.rs
  │         session.rs
  │         system.rs
  │     handler/
  │       mod.rs          ← HandlerDispatcher
  │       matcher.rs      ← CommandMatcher, MessageMatcher（regex 缓存）
  │       filter.rs       ← FilterSpec 求值
  │       schedule.rs     ← tokio 定时器管理
  │     transport/
  │       mod.rs          ← PluginTransport trait
  │       wasm.rs         ← WasmSession（替代 wasm_ext.rs + 扩展 host_invoke）
  │       ipc.rs          ← IpcSession（STDIO 帧 + IpcPeer）
  │     runner/
  │       mod.rs          ← PluginSession（单插件生命周期）
  │       supervisor.rs   ← PluginSupervisor（多插件管理）
  │       manifest.rs     ← plugin.toml 解析
  │
  ├── astrcode-plugin-sdk/            新建，替代 astrcode-extension-sdk
  │   Cargo.toml   [lib] crate-type = ["cdylib", "rlib"]
  │                deps（wasm32）: 几乎无 async runtime
  │                deps（host）:  plugin-proto, serde, serde_json
  │   src/
  │     lib.rs
  │     plugin.rs      ← Plugin trait
  │     context.rs     ← PluginContext + 各 Client（LlmClient, DbClient...）
  │     event.rs       ← MessageEvent（插件侧视图）, ScheduleEvent
  │     result.rs      ← PluginResult<T>, MessageChain, MessageComponent
  │     wasm_abi.rs    ← #[cfg(target_arch = "wasm32")] WASM ABI 胶水
  │     ipc_runtime.rs ← #[cfg(not(target_arch = "wasm32"))] IPC 插件运行时
  │
  └── astrcode-plugin-macros/         新建，proc macro crate
      Cargo.toml   proc-macro = true; deps: syn, quote, proc-macro2
      src/
        lib.rs       ← #[plugin], #[on_command], #[on_message], #[on_event], #[on_schedule]
```

**与现有 crate 的处理方式**：

| 现有 crate | 处置 |
|-----------|------|
| `astrcode-extensions` | 逻辑迁移到 `astrcode-plugin-host`，原 crate 保留为 shim（re-export） |
| `astrcode-extension-sdk` | 逻辑迁移到 `astrcode-plugin-sdk`，原 crate 保留为 shim |
| `astrcode-core/extension.rs` | 保留 `Extension` trait 供 bundled extensions 使用，external plugins 改用新 SDK |
| `astrcode-protocol` | 保留（JSON-RPC 2.0 client↔server 协议），不合并 |

---

## 八、宿主侧实现要点

### 8.1 CapabilityRouter trait

```rust
// astrcode-plugin-host/src/capability/mod.rs

#[async_trait]
pub trait CapabilityRouter: Send + Sync {
    async fn invoke(
        &self,
        name: &str,
        input: serde_json::Value,
        caller: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError>;

    async fn invoke_stream(
        &self,
        name: &str,
        input: serde_json::Value,
        caller: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<serde_json::Value, CapabilityError>> + Send>>,
        CapabilityError,
    >;

    fn list_capabilities(&self) -> Vec<CapabilityDescriptor>;

    /// 注册插件提供的能力（握手后调用）
    fn register_plugin_capability(
        &self,
        descriptor: CapabilityDescriptor,
        peer: Arc<dyn PluginPeer>,
    ) -> Result<(), CapabilityError>;

    /// 插件下线时注销其能力
    fn unregister_plugin(&self, plugin_id: &str);
}
```

**命名空间冲突规则**：`register_plugin_capability` 收到 `handler.*`、`system.*`、`internal.*` 前缀时返回 `CapabilityError::ReservedNamespace`。

**路由优先级**：同名能力后注册覆盖先注册（用于测试 mock），内置能力优先于插件能力（建议内置能力注册在不同前缀下，所以实际上不会冲突）。

### 8.2 HandlerDispatcher

```rust
// astrcode-plugin-host/src/handler/mod.rs

pub struct HandlerDispatcher {
    /// key = 命令名（含别名展开后）
    command_handlers: RwLock<HashMap<String, Vec<RegisteredHandler>>>,
    message_handlers: RwLock<Vec<RegisteredHandler>>,
    /// key = event_type
    event_handlers: RwLock<HashMap<String, Vec<RegisteredHandler>>>,
    schedule_handles: Mutex<HashMap<String, JoinHandle<()>>>,
}

pub struct RegisteredHandler {
    pub descriptor: HandlerDescriptor,
    pub peer: Arc<dyn PluginPeer>,
}
```

分发算法（以命令为例）：

```
1. 从 MessageEvent.text 提取命令名 cmd（去掉 "/" 前缀）
2. handlers = command_handlers[cmd] ∪ command_handlers[alias for cmd]
3. 按 descriptor.priority DESC 稳定排序
4. for handler in handlers:
     a. 检查 handler.descriptor.filters（platform、message_type 过滤）
     b. 检查 handler.descriptor.permissions（admin、role）
     c. 若通过：result = peer.invoke("handler.<id>", event_payload, cancel).await
     d. 若 result.stop_propagation == true：break
5. 聚合 result.reply 并发送（调用 platform.send_chain）
```

**并发处理**：同一事件的多个处理器**串行执行**（保证 stop_propagation 语义）。  
**错误隔离**：单个处理器错误（`PluginError::InvocationFailed`）不影响其他处理器。

### 8.3 PluginSession 生命周期

```rust
// astrcode-plugin-host/src/runner/mod.rs

pub struct PluginSession {
    pub plugin_id: String,
    transport: Box<dyn PluginTransport>,
    state: Arc<RwLock<PluginState>>,
}

pub enum PluginState {
    Starting,
    Running {
        handlers: Vec<HandlerDescriptor>,
        capabilities: Vec<CapabilityDescriptor>,
    },
    Stopping,
    Failed { error: String },
}
```

**`start()` 流程**：

```
1. transport.connect()（WASM: 加载模块；IPC: spawn 进程）
2. 发送 InitializeMsg（携带 host_capabilities）
3. 等待 InitializeResultMsg（超时 30s）
4. 验证 plugin_id 匹配、protocol_version 兼容
5. 向 dispatcher 注册 handlers（按 trigger 类型分类存入）
6. 向 router 注册 provided_capabilities
7. state = Running
```

**`stop()` 流程**：

```
1. state = Stopping
2. 向 dispatcher 注销 handlers
3. 向 router 注销 capabilities
4. transport.close()
5. （IPC）等待进程退出，超时 5s 后 kill
```

**自动重启**：IPC 插件进程异常退出时，`PluginSession` 检测到（`is_alive() = false`），
由 `PluginSupervisor` 决策是否重启（配置项 `auto_restart_max_attempts`，默认 3）。

### 8.4 PluginSupervisor

```rust
// astrcode-plugin-host/src/runner/supervisor.rs

pub struct PluginSupervisor {
    sessions: DashMap<String, Arc<PluginSession>>,
    dispatcher: Arc<HandlerDispatcher>,
    router: Arc<dyn CapabilityRouter>,
}

impl PluginSupervisor {
    pub async fn load(&self, manifest_path: &Path) -> Result<(), PluginError>;
    pub async fn unload(&self, plugin_id: &str) -> Result<(), PluginError>;
    pub async fn reload(&self, plugin_id: &str) -> Result<(), PluginError>;

    pub async fn dispatch_message(
        &self,
        event: MessageEvent,
        cancel: CancellationToken,
    ) -> Result<DispatchResult, PluginError>;

    pub async fn dispatch_system_event(
        &self,
        event_type: &str,
        data: serde_json::Value,
    ) -> Result<(), PluginError>;

    pub fn list_plugins(&self) -> Vec<PluginInfo>;
}
```

**plugin.toml 解析**（`manifest.rs`）：

```toml
[plugin]
id          = "my-plugin"
version     = "0.1.0"
description = "A sample plugin"
author      = "Alice"

# 传输类型："wasm"（默认）或 "stdio" 或 "websocket"
transport = "wasm"

# transport = "wasm"
library = "my_plugin.wasm"

# transport = "stdio"
# command = ["./my-plugin-binary", "--arg"]
# env     = { LOG_LEVEL = "debug" }

# transport = "websocket"
# url = "wss://localhost:9000"
# tls = { ca = "ca.pem", cert = "cert.pem", key = "key.pem" }

required_capabilities = ["llm.chat", "db.get"]

[limits]                   # 仅 WASM 路径生效
fuel_per_call    = 10000000
memory_bytes     = 67108864
call_timeout_secs = 30
```

---

## 九、插件 SDK

### 9.1 Plugin trait

```rust
// astrcode-plugin-sdk/src/plugin.rs

#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn version(&self) -> &str { "0.0.0" }
    fn description(&self) -> &str { "" }

    /// 宿主握手完成、handlers 已注册后调用
    async fn on_start(&self, _ctx: &PluginContext) -> PluginResult<()> { Ok(()) }

    /// 插件即将停止时调用（cleanup 用）
    async fn on_stop(&self) -> PluginResult<()> { Ok(()) }
}
```

### 9.2 PluginContext（能力调用门面）

```rust
// astrcode-plugin-sdk/src/context.rs

pub struct PluginContext { /* 内部持有能力调用句柄 */ }

impl PluginContext {
    /// 底层 raw 调用（capability name + JSON input）
    pub async fn invoke_raw(
        &self,
        capability: &str,
        input: serde_json::Value,
    ) -> PluginResult<serde_json::Value>;

    // 类型化客户端（封装 invoke_raw）
    pub fn llm(&self)      -> LlmClient<'_>;
    pub fn db(&self)       -> DbClient<'_>;
    pub fn memory(&self)   -> MemoryClient<'_>;
    pub fn platform(&self) -> PlatformClient<'_>;
    pub fn session(&self)  -> SessionClient<'_>;
    pub fn system(&self)   -> SystemClient<'_>;
}
```

**LlmClient 示例**：

```rust
pub struct LlmClient<'a>(&'a PluginContext);

impl<'a> LlmClient<'a> {
    pub async fn chat(&self, req: ChatRequest) -> PluginResult<ChatResponse> {
        let output = self.0.invoke_raw("llm.chat", serde_json::to_value(req)?).await?;
        Ok(serde_json::from_value(output)?)
    }
}

pub struct ChatRequest {
    pub messages: Vec<LlmMessage>,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub session_id: Option<String>,
}

pub struct LlmMessage {
    pub role: String,    // "system" | "user" | "assistant"
    pub content: String,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self { ... }
    pub fn user(content: impl Into<String>) -> Self { ... }
}
```

### 9.3 MessageEvent（插件侧视图）

```rust
// astrcode-plugin-sdk/src/event.rs

pub struct MessageEvent {
    pub text: String,
    pub user_id: String,
    pub group_id: Option<String>,
    pub platform: String,
    pub platform_id: String,
    pub session_id: String,
    pub self_id: String,
    pub message_type: MessageType,
    pub sender_name: String,
    pub is_admin: bool,
    pub messages: Vec<MessageComponent>,
    pub timestamp: i64,
    ctx: PluginContext,  // 内部字段
}

impl MessageEvent {
    pub async fn reply(&self, text: &str) -> PluginResult<()> {
        self.ctx.platform().send(&self.session_id, text).await
    }
    pub async fn reply_chain(&self, chain: MessageChain) -> PluginResult<()> {
        self.ctx.platform().send_chain(&self.session_id, chain).await
    }
    pub async fn reply_image(&self, url: &str) -> PluginResult<()> {
        self.ctx.platform().send_image(&self.session_id, url).await
    }
    /// 中断事件继续分发
    pub fn stop_propagation(&mut self) { ... }
}

pub enum MessageType { Group, Private, Channel }

pub enum MessageComponent {
    Plain { text: String },
    Image { url: String },
    At    { user_id: String, name: Option<String> },
    Reply { /* ... */ },
    File  { name: String, url: String },
}
```

### 9.4 PluginResult 和 MessageChain

```rust
pub type PluginResult<T> = Result<T, PluginSdkError>;

#[derive(Debug, thiserror::Error)]
pub enum PluginSdkError {
    #[error("Capability error: {0}")]
    Capability(String),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub struct MessageChain(pub Vec<MessageComponent>);

impl MessageChain {
    pub fn new() -> Self { Self(Vec::new()) }
    pub fn text(mut self, t: impl Into<String>) -> Self {
        self.0.push(MessageComponent::Plain { text: t.into() }); self
    }
    pub fn image(mut self, url: impl Into<String>) -> Self {
        self.0.push(MessageComponent::Image { url: url.into() }); self
    }
}
```

### 9.5 Proc Macros

```rust
// astrcode-plugin-macros/src/lib.rs

/// 标注插件入口 struct，自动生成 WASM ABI 入口点
/// （alloc/dealloc/plugin_init/plugin_post_init/plugin_handle）
/// 以及 IPC 模式的 main 函数。
///
/// 用法：
/// ```rust
/// #[plugin]
/// struct MyPlugin;
/// impl Plugin for MyPlugin { fn id(&self) -> &str { "my-plugin" } ... }
/// ```
#[proc_macro_attribute]
pub fn plugin(args: TokenStream, input: TokenStream) -> TokenStream { ... }

/// 注册命令处理器。
///
/// 用法：
/// ```rust
/// #[on_command("hello", aliases = ["hi"], description = "Say hello", priority = 10)]
/// async fn hello(ctx: &PluginContext, event: &MessageEvent) -> PluginResult<()> { ... }
/// ```
#[proc_macro_attribute]
pub fn on_command(args: TokenStream, input: TokenStream) -> TokenStream { ... }

/// 注册消息处理器。
///
/// 用法：
/// ```rust
/// #[on_message(regex = r"^test\s+(.+)$")]
/// async fn test_handler(ctx: &PluginContext, event: &MessageEvent) -> PluginResult<()> { ... }
/// ```
#[proc_macro_attribute]
pub fn on_message(args: TokenStream, input: TokenStream) -> TokenStream { ... }

/// 注册系统事件处理器。
///
/// 用法：
/// ```rust
/// #[on_event("session_start")]
/// async fn on_start(ctx: &PluginContext, event: &SystemEvent) -> PluginResult<()> { ... }
/// ```
#[proc_macro_attribute]
pub fn on_event(args: TokenStream, input: TokenStream) -> TokenStream { ... }

/// 注册定时处理器。cron 和 interval_secs 必须且只能有一个。
///
/// 用法：
/// ```rust
/// #[on_schedule(cron = "0 9 * * *", timezone = "Asia/Shanghai")]
/// async fn daily(ctx: &PluginContext, _event: &ScheduleEvent) -> PluginResult<()> { ... }
///
/// #[on_schedule(interval_secs = 60)]
/// async fn every_minute(ctx: &PluginContext, _event: &ScheduleEvent) -> PluginResult<()> { ... }
/// ```
#[proc_macro_attribute]
pub fn on_schedule(args: TokenStream, input: TokenStream) -> TokenStream { ... }
```

**宏生成的 WASM ABI 骨架（伪代码）**：

```rust
// #[plugin] 在 wasm32 目标下生成：

static PLUGIN_INSTANCE: OnceCell<MyPlugin> = OnceCell::new();
static HANDLER_REGISTRY: OnceCell<Vec<HandlerEntry>> = OnceCell::new();

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    // Vec::with_capacity(len as usize) + Box::into_raw + as ptr
}

#[no_mangle]
pub extern "C" fn dealloc(ptr: i32, len: i32) {
    // Box::from_raw(...)
}

#[no_mangle]
pub extern "C" fn plugin_init() -> i64 {
    // 实例化 Plugin，收集所有 #[on_xxx] 注册的 HandlerDescriptor
    // 序列化为 InitializeResultMsg JSON
    // 返回 (ptr << 32 | len)
}

#[no_mangle]
pub extern "C" fn plugin_post_init(ptr: i32, len: i32) {
    // 读取 InitializeMsg JSON
    // 存储 host_capabilities 列表，供后续 context.llm() 等验证用
}

#[no_mangle]
pub extern "C" fn plugin_handle(req_ptr: i32, req_len: i32) -> i64 {
    // 读取 InvokeMsg JSON
    // 路由到对应 handler 函数（通过 handler_id 匹配）
    // 调用 handler，收集返回的 PluginResult<()> 和 stop_propagation
    // 序列化为 ResultMsg JSON
    // 返回 (ptr << 32 | len)
}
```

**WASM 路径同步执行约定**：`#[on_xxx]` 标注的函数签名中的 `async` 在 WASM 编译时
由宏展开为非 async（通过 `futures::executor::block_on` 或直接去掉 async），
因为 WASM 运行时不支持 Tokio。`ctx.invoke_raw` 在 WASM 路径下调用 `host_invoke`（同步）。

### 9.6 Cargo.toml 模板（WASM 插件）

```toml
[package]
name    = "my-astrcode-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]   # 编译为 .wasm

[dependencies]
astrcode-plugin-sdk    = "1.0"
astrcode-plugin-macros = "1.0"

# 不要依赖 tokio——WASM 路径是同步的
# 序列化用 serde_json（已由 sdk 重导出）

[profile.release]
opt-level = "s"   # 优化体积
lto       = true
strip     = true
```

---

## 十、端到端示例

### 10.1 Rust WASM 插件完整示例

```rust
// src/lib.rs
use astrcode_plugin_macros::*;
use astrcode_plugin_sdk::prelude::*;

// 声明插件入口，宏生成所有 WASM ABI 胶水代码
#[plugin]
struct WeatherPlugin;

impl Plugin for WeatherPlugin {
    fn id(&self) -> &str { "weather-plugin" }
    fn version(&self) -> &str { "0.1.0" }
    fn description(&self) -> &str { "查询天气的插件" }

    async fn on_start(&self, ctx: &PluginContext) -> PluginResult<()> {
        // 测试：向 DB 写入启动时间
        ctx.db().set("weather:last_start", chrono::Utc::now().to_rfc3339(), None).await?;
        Ok(())
    }
}

/// /weather <城市>
/// 调用 LLM 生成天气摘要并回复
#[on_command("weather", description = "查询城市天气", aliases = ["wthr"])]
async fn weather_cmd(ctx: &PluginContext, event: &MessageEvent) -> PluginResult<()> {
    let city = event.text
        .trim_start_matches("/weather")
        .trim_start_matches("/wthr")
        .trim();

    if city.is_empty() {
        event.reply("用法：/weather <城市名>").await?;
        return Ok(());
    }

    // 先查缓存
    let cache_key = format!("weather:cache:{city}");
    if let Ok(cached) = ctx.db().get::<String>(&cache_key).await {
        event.reply(&cached).await?;
        return Ok(());
    }

    // 调用宿主 LLM 能力
    let resp = ctx.llm().chat(ChatRequest {
        messages: vec![
            LlmMessage::system(
                "你是一个简洁的天气助手，用 2-3 句话描述天气。不要说你无法访问实时数据。"
            ),
            LlmMessage::user(format!("{}今天的天气情况如何？", city)),
        ],
        max_tokens: Some(200),
        ..Default::default()
    }).await?;

    // 写入缓存（10 分钟 TTL）
    ctx.db().set(&cache_key, &resp.content, Some(600)).await?;

    event.reply(&resp.content).await
}

/// 每天早上 9 点发送提醒（只发送到特定 session）
#[on_schedule(cron = "0 9 * * *", timezone = "Asia/Shanghai")]
async fn morning_weather(ctx: &PluginContext, _event: &ScheduleEvent) -> PluginResult<()> {
    // 定时任务无 event.reply()，直接调用 platform 能力
    ctx.platform().send("telegram:group:123456", "早安！记得查看今日天气 /weather").await?;
    Ok(())
}

/// 监听 session_start 事件，欢迎新用户
#[on_event("session_start")]
async fn on_session_start(ctx: &PluginContext, event: &SystemEvent) -> PluginResult<()> {
    if let Some(session_id) = event.data.get("session_id").and_then(|v| v.as_str()) {
        ctx.platform()
            .send(session_id, "欢迎！输入 /weather <城市> 查询天气。")
            .await?;
    }
    Ok(())
}
```

### 10.2 宿主加载插件

```rust
// 初始化能力路由器
let router = Arc::new(DefaultCapabilityRouter::new(
    llm_provider,
    db_backend,
    memory_backend,
    platform_backend,
));

// 初始化分发器和监管器
let dispatcher = Arc::new(HandlerDispatcher::new());
let supervisor = PluginSupervisor::new(router.clone(), dispatcher.clone());

// 加载插件（从 plugin.toml 自动判断传输类型）
supervisor.load(Path::new("plugins/weather-plugin/plugin.toml")).await?;

// 消息到达时分发
let result = supervisor
    .dispatch_message(message_event, CancellationToken::new())
    .await?;
```

### 10.3 IPC 插件示例（Python，只展示协议层）

```python
#!/usr/bin/env python3
"""最简 Python 插件，实现 s5r 协议的 STDIO IPC 路径。"""
import sys, json, uuid

def send(msg: dict):
    data = json.dumps(msg, ensure_ascii=False).encode()
    sys.stdout.buffer.write(f"{len(data)}\n".encode() + data)
    sys.stdout.buffer.flush()

def recv() -> dict:
    line = sys.stdin.readline()
    length = int(line.strip())
    return json.loads(sys.stdin.buffer.read(length))

# 1. 等待宿主发送 InitializeMsg
init = recv()
assert init["type"] == "initialize"

# 2. 回复 InitializeResultMsg
send({
    "type": "initialize_result",
    "id": init["id"],
    "success": True,
    "protocol_version": "1.0",
    "plugin_id": "python-demo",
    "plugin_version": "0.1.0",
    "handlers": [{
        "id": "python-demo.hello",
        "trigger": { "type": "command", "command": "hello_py" },
        "priority": 0,
        "permissions": { "require_admin": False, "level": 0 },
        "filters": []
    }],
    "provided_capabilities": []
})

# 3. 消息循环
while True:
    msg = recv()

    if msg["type"] == "invoke":
        cap = msg["capability"]

        if cap == "handler.python-demo.hello":
            # 调用宿主 platform.send 能力（双向 invoke）
            call_id = str(uuid.uuid4())
            send({
                "type": "invoke",
                "id": call_id,
                "capability": "platform.send",
                "input": {
                    "session_id": msg["input"]["session_id"],
                    "text": "Hello from Python!"
                }
            })
            # 等待宿主的 ResultMsg
            result = recv()
            assert result["id"] == call_id

            # 返回 handler 结果
            send({
                "type": "result",
                "id": msg["id"],
                "success": True,
                "output": { "stop_propagation": False }
            })

    elif msg["type"] == "cancel":
        pass  # 简单实现忽略取消
```

---

## 十一、实现优先级

### Phase 1（骨架，最小可用）

目标：一个 WASM 插件能响应命令并写入 DB。

1. **创建 `astrcode-plugin-proto`**：定义 `InitializeMsg`、`InitializeResultMsg`、
   `InvokeMsg`、`ResultMsg`、`HandlerDescriptor`、`CapabilityDescriptor`、所有 Trigger 类型。
2. **增强 `astrcode-plugin-host/transport/wasm.rs`**：
   - 实现 `host_invoke` import（`block_in_place` + `block_on` 调用 router）
   - 实现新 ABI（`plugin_init` / `plugin_post_init` / `plugin_handle`）
   - 替换现有 `wasm_ext.rs` 的 handler 注册逻辑
3. **内置能力**：只实现 `db.*`（4 个端点）和 `platform.send`
4. **`astrcode-plugin-sdk` 最小版本**：`Plugin` trait + `PluginContext`（只有 `invoke_raw`）+ `DbClient`
5. **端到端测试**：WASM 插件 `/ping` 命令 → 写 DB → 回复消息

### Phase 2（IPC + 完整能力）

目标：任意语言可写插件，所有内置能力可用。

1. **`astrcode-plugin-host/transport/ipc.rs`**：STDIO 帧读写 + IpcPeer 全实现
2. **所有内置能力**：`llm.*`、`memory.*`、`session.*`、`system.*`
3. **完整触发器**：MessageTrigger（regex）、EventTrigger、ScheduleTrigger
4. **`astrcode-plugin-sdk`**：`LlmClient`、`MemoryClient`、`PlatformClient`、`SessionClient`

### Phase 3（宏 + 流式 + 生态）

1. **proc macros**：`#[plugin]`、`#[on_command]`、`#[on_message]`、`#[on_event]`、`#[on_schedule]`
2. **流式能力**（IPC 路径）：`llm.stream_chat` + `EventMsg` 序列处理
3. **插件能力互调**：插件 A 调用插件 B 的 `provided_capabilities`
4. **热重载**：`supervisor.reload(plugin_id)` 不丢失状态
5. **WebSocket 传输**

---

## 附录：错误规范

### Rust 错误类型

```rust
// astrcode-plugin-proto/src/error.rs
#[derive(Debug, thiserror::Error)]
pub enum PluginProtocolError {
    #[error("Frame too large: {size} bytes (max {max})")]
    FrameTooLarge { size: usize, max: usize },
    #[error("Invalid frame: {0}")]
    InvalidFrame(String),
    #[error("Serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Phase constraint violated: {0}")]
    InvalidPhase(String),
}

// astrcode-plugin-host/src/...
#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("Capability '{name}' not found")]
    NotFound { name: String },
    #[error("Reserved namespace: '{name}'")]
    ReservedNamespace { name: String },
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Invocation failed: {0}")]
    InvocationFailed(String),
    #[error("Cancelled")]
    Cancelled,
    #[error("Timeout after {0:?}")]
    Timeout(std::time::Duration),
    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("Protocol error: {0}")]
    Protocol(#[from] PluginProtocolError),
    #[error("WASM trap: {0}")]
    WasmTrap(String),
    #[error("WASM out of fuel")]
    WasmOutOfFuel,
    #[error("Transport error: {0}")]
    Transport(String),
    #[error("Plugin '{id}' reported error: {message}")]
    PluginReported { id: String, code: String, message: String },
    #[error("Handshake failed: {0}")]
    HandshakeFailed(String),
    #[error("Plugin not found: {0}")]
    NotFound(String),
}
```

### 错误处理原则

1. **WASM trap**（panic、OOM、fuel 耗尽）由宿主捕获，记录为 `PluginError::WasmTrap`，
   不影响宿主进程。
2. **IPC 插件崩溃**（进程退出）由 `PluginSession` 检测，触发清理和可选自动重启。
3. **单个处理器失败**不影响同一事件的其他处理器（除非 `stop_propagation`）。
4. **单个插件失败**不影响其他插件的处理。
5. **所有 capability 调用都有超时**，超时返回 `CapabilityError::Timeout`。
6. **不在处理器内 panic**——WASM 路径 trap 可恢复，IPC 路径插件崩溃可恢复，但宿主 panic 不可接受。
