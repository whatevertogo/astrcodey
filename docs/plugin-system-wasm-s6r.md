# s6r：AstrCode WASM 扩展协议规范 v1.0

> s6r 是对当前 WASM ABI 的最小结构化升级。目标：**两个入口、两条消息、无魔术整数、无 side-effect registration**。
> 不引入新的 host 能力（DB/LLM/platform 等留待将来），只让现有的 hooks + tools + commands 拥有干净的协议层。

---

## 一、当前问题（改造动机）

| 问题 | 表现 |
|------|------|
| 命令式注册 | `extension_init()` 通过 `host_register_tool/command/subscribe` 副作用写入 `HostState` |
| 分散的 manifest | `extension.json` 声明 `id/capabilities`，WASM init 声明 tools/commands/hooks，两处不一致 |
| 三个入口 | `handle_tool`, `handle_command`, `handle_event` 各自独立 |
| 魔术整数 | `GUEST_EFFECT_OK=0 .. GUEST_EFFECT_APPEND_MESSAGES=7`、事件判别符 `0..17` |
| 副作用响应 | 调用 `host_set_response(ptr, len)` 写结果，返回值只是 effect code |

---

## 二、s6r 核心设计

### 2.1 两个 guest 导出函数

```
extension_manifest() -> i64
  调用一次。返回 ManifestMsg JSON。
  packed: (ptr as i64) << 32 | (len as i64)
  宿主读取后调用 dealloc(ptr, len)。

extension_call(req_ptr: i32, req_len: i32) -> i64
  每次调用一次。接收 CallRequest JSON，返回 CallResponse JSON。
  packed: 同上。
```

仅保留两个 host import：

```
host_log(level: i32, msg_ptr: i32, msg_len: i32)
  level: 0=trace 1=debug 2=info 3=warn 4=error

host_emit(event_ptr: i32, event_len: i32) -> i64
  仅限声明了 EmitEvents capability 的扩展使用。
  输入：EmitEventMsg JSON。返回 packed (ptr << 32 | len) 的 ResultMsg JSON。
  失败时返回 0。
```

**完全移除**：`host_register_tool`、`host_register_command`、`host_subscribe`、`host_set_response`、`extension_init`、`handle_tool`、`handle_command`、`handle_event`。

内存接口不变：

```
alloc(len: i32) -> i32    宿主写入数据前调用，失败返回 0
dealloc(ptr: i32, len: i32)
```

内存所有权：宿主 alloc 的由宿主 dealloc；guest 从两个导出函数返回的指针，宿主读取完毕后调用 dealloc。

---

## 三、ManifestMsg

`extension_manifest()` 返回的 JSON。

```json
{
  "s6r": "1",
  "id": "my-extension",
  "version": "0.1.0",
  "description": "选填描述",
  "capabilities": ["workspace_read"],
  "tools": [
    {
      "name": "grep_files",
      "description": "在工作区文件中搜索文本",
      "parameters": {
        "type": "object",
        "properties": {
          "pattern": { "type": "string" },
          "path":    { "type": "string" }
        },
        "required": ["pattern"]
      },
      "mode": "parallel"
    }
  ],
  "commands": [
    { "name": "hello", "description": "打招呼" }
  ],
  "hooks": [
    { "on": "pre_tool_use",  "mode": "blocking"     },
    { "on": "session_start", "mode": "non_blocking"  }
  ]
}
```

字段说明：

| 字段 | 类型 | 说明 |
|------|------|------|
| `s6r` | `"1"` | 协议版本，固定值，宿主据此拒绝不兼容版本 |
| `id` | string | 扩展唯一 ID，替代 `extension.json` 中的 id |
| `version` | string | semver |
| `capabilities` | string[] | 同现有 `ExtensionCapability`，snake_case |
| `tools[].name` | string | 工具名 |
| `tools[].parameters` | JSON Schema | 参数 schema |
| `tools[].mode` | `"sequential"` \| `"parallel"` | 执行模式，默认 sequential |
| `commands[].name` | string | 斜杠命令名 |
| `hooks[].on` | string | 事件名（见下表） |
| `hooks[].mode` | `"blocking"` \| `"non_blocking"` \| `"advisory"` | Hook 模式 |

### 事件名映射（`hooks[].on`）

| s6r 字符串 | 现有 `ExtensionEvent` |
|------------|----------------------|
| `"session_start"` | `SessionStart` |
| `"session_resume"` | `SessionResume` |
| `"session_shutdown"` | `SessionShutdown` |
| `"turn_start"` | `TurnStart` |
| `"turn_end"` | `TurnEnd` |
| `"turn_aborted"` | `TurnAborted` |
| `"step_start"` | `StepStart` |
| `"step_end"` | `StepEnd` |
| `"pre_tool_use"` | `PreToolUse` |
| `"post_tool_use"` | `PostToolUse` |
| `"post_tool_use_failure"` | `PostToolUseFailure` |
| `"before_provider_request"` | `BeforeProviderRequest` |
| `"after_provider_response"` | `AfterProviderResponse` |
| `"user_prompt_submit"` | `UserPromptSubmit` |
| `"prompt_build"` | `PromptBuild` |
| `"pre_compact"` | `PreCompact` |
| `"post_compact"` | `PostCompact` |
| `"post_recap"` | `PostRecap` |

---

## 四、CallRequest

宿主传给 `extension_call()` 的 JSON。

### 4.1 Tool 调用

```json
{
  "id": "req-01JXYZ",
  "call": "tool",
  "name": "grep_files",
  "input": {
    "pattern": "TODO",
    "path": "src/"
  }
}
```

`input` 的结构与 `ToolExecutionContext` 对齐（由宿主序列化）。

### 4.2 Hook 调用

```json
{
  "id": "req-01JABC",
  "call": "hook",
  "on": "pre_tool_use",
  "input": {
    "tool_name": "bash",
    "tool_input": { "command": "rm -rf /" }
  }
}
```

`input` 结构按事件类型不同（由宿主序列化各 `*Context` 结构体）。

### 4.3 Command 调用

```json
{
  "id": "req-01JDEF",
  "call": "command",
  "name": "hello",
  "input": {
    "args": "world"
  }
}
```

---

## 五、CallResponse

`extension_call()` 返回的 JSON。

### 5.1 成功（无附加数据）

```json
{
  "id": "req-01JXYZ",
  "ok": true,
  "effect": "ok"
}
```

### 5.2 成功（有附加数据）

```json
{
  "id": "req-01JABC",
  "ok": true,
  "effect": "block",
  "data": { "reason": "rm -rf / is not permitted" }
}
```

### 5.3 失败

```json
{
  "id": "req-01JXYZ",
  "ok": false,
  "error": "pattern is required"
}
```

`ok: false` 时 `effect` 和 `data` 省略。

### 5.4 Effect 枚举

| effect | 替代的旧常量 | 含义 | data 字段 |
|--------|------------|------|----------|
| `"ok"` | `GUEST_EFFECT_OK` | 成功，无特殊操作 | 省略 |
| `"block"` | — | 阻止操作（blocking hook 专用）| `{ "reason": string }` |
| `"modified_input"` | `GUEST_EFFECT_MODIFIED_INPUT` | 修改工具入参（`pre_tool_use` blocking）| `{ "tool_input": object }` |
| `"tool_outcome"` | `GUEST_EFFECT_TOOL_OUTCOME` | 工具自定义执行结果 | `{ "outcome": object }` |
| `"prompt_contributions"` | `GUEST_EFFECT_PROMPT_CONTRIBUTIONS` | 向 prompt 注入内容 | `PromptContributions` |
| `"compact_contributions"` | `GUEST_EFFECT_COMPACT_CONTRIBUTIONS` | 向 compact 注入内容 | `CompactContributions` |
| `"replace_messages"` | `GUEST_EFFECT_REPLACE_MESSAGES` | 替换 provider 消息列表 | `{ "messages": array }` |
| `"append_messages"` | `GUEST_EFFECT_APPEND_MESSAGES` | 追加 provider 消息 | `{ "messages": array }` |

---

## 六、Rust 类型定义

以下类型放在 `astrcode-extension-sdk/src/s6r.rs`（新文件），在 `wasm_abi.rs` 中 `pub use s6r::*`。

```rust
// astrcode-extension-sdk/src/s6r.rs
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本常量
pub const S6R_VERSION: &str = "1";

// ─── Manifest ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub s6r: String,
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
    #[serde(default)]
    pub commands: Vec<ManifestCommand>,
    #[serde(default)]
    pub hooks: Vec<ManifestHook>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default = "default_mode")]
    pub mode: String, // "sequential" | "parallel"
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestHook {
    pub on: String,   // 事件名，如 "pre_tool_use"
    pub mode: String, // "blocking" | "non_blocking" | "advisory"
}

fn default_mode() -> String { "sequential".into() }

// ─── CallRequest ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum CallRequest {
    Tool    { id: String, name: String,         input: Value },
    Hook    { id: String, on: String,           input: Value },
    Command { id: String, name: String,         input: Value },
}

impl CallRequest {
    pub fn id(&self) -> &str {
        match self {
            Self::Tool    { id, .. } => id,
            Self::Hook    { id, .. } => id,
            Self::Command { id, .. } => id,
        }
    }
}

// ─── CallResponse ──────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct CallResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CallResponse {
    pub fn ok(id: impl Into<String>) -> Self {
        Self { id: id.into(), ok: true, effect: Some("ok".into()), data: None, error: None }
    }

    pub fn with_effect(id: impl Into<String>, effect: impl Into<String>, data: Value) -> Self {
        Self { id: id.into(), ok: true, effect: Some(effect.into()), data: Some(data), error: None }
    }

    pub fn err(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self { id: id.into(), ok: false, effect: None, data: None, error: Some(message.into()) }
    }
}
```

---

## 七、宿主侧改动要点

### 7.1 wasm_api.rs

移除：
- `HostState.tools`, `.commands`, `.subscriptions`, `.response_ptr`, `.response_len`
- `host_register_tool`, `host_register_command`, `host_subscribe`, `host_set_response`
- `take_response()`
- 所有 `GUEST_EFFECT_*` 常量（迁移到 `s6r.rs` 的 effect 字符串）
- 所有 `event_discriminant` / `mode_discriminant` 等判别函数（`wasm_abi.rs`）

保留：
- `host_log`
- `host_emit`（`EmitEvents` capability 需要）
- `write_to_guest()`
- `HostState`（只保留 `fuel_budget`, `memory_limit`）

新增：`create_linker` 只注册 `host_log` 和 `host_emit`。

### 7.2 wasm_ext.rs 加载流程

```rust
// 旧：调用 extension_init()，读 HostState.tools/commands/subscriptions
// 新：调用 extension_manifest()，解析 ManifestMsg JSON

let manifest_fn = instance
    .get_typed_func::<(), i64>(&mut store, "extension_manifest")
    .map_err(|e| format!("must export 'extension_manifest': {e}"))?;

store.set_fuel(fuel_budget)?;
let packed = manifest_fn.call(&mut store, ())?;
let manifest: Manifest = read_packed_json(&mut store, &memory, packed)?;
// 校验 manifest.s6r == S6R_VERSION
// 从 manifest 提取 tools/commands/hooks
```

### 7.3 wasm_ext.rs 调用流程

```rust
// 旧：分别调用 handle_tool / handle_command / handle_event
// 新：统一调用 extension_call

let call_fn = instance
    .get_typed_func::<(i32, i32), i64>(&mut store, "extension_call")
    .map_err(|e| format!("must export 'extension_call': {e}"))?;

// 构造 CallRequest，序列化为 JSON
let req = CallRequest::Tool { id: req_id, name: tool_name, input: ctx_json };
let req_bytes = serde_json::to_vec(&req)?;

store.set_fuel(fuel_budget)?;
let (ptr, len) = write_to_guest(&mut store, &memory, &alloc_fn, &req_bytes)?;
let packed = call_fn.call(&mut store, (ptr as i32, len as i32))?;
let resp: CallResponse = read_packed_json(&mut store, &memory, packed)?;
```

### 7.4 辅助函数

```rust
/// 从 packed i64 读取 guest 内存中的 JSON，然后 dealloc。
fn read_packed_json<T: serde::de::DeserializeOwned>(
    store: &mut Store<HostState>,
    memory: &Memory,
    packed: i64,
) -> Result<T, String> {
    if packed == 0 { return Err("guest returned null ptr".into()); }
    let ptr = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let len = (packed & 0xFFFF_FFFF) as u32;
    let json_str = read_memory_string(store, ptr, len)?;
    // 调用 dealloc
    store.data_mut(); // 借用结束
    let result = serde_json::from_str(&json_str).map_err(|e| format!("parse response: {e}"))?;
    Ok(result)
}
```

---

## 八、guest SDK 侧（extension_manifest / extension_call 实现参考）

扩展作者在 `wasm32` 目标下需要提供这两个入口。`astrcode-extension-sdk` 未来可提供宏来生成这段胶水，当前版本手动实现。

```rust
// src/lib.rs（WASM 插件）

use astrcode_extension_sdk::s6r::{
    CallRequest, CallResponse, Manifest, ManifestCommand, ManifestHook, ManifestTool,
};

// ─── 内存管理 ──────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut buf = Vec::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn dealloc(ptr: i32, len: i32) {
    unsafe { drop(Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize)) };
}

fn write_response(json: String) -> i64 {
    let bytes = json.into_bytes();
    let len = bytes.len() as i64;
    let ptr = alloc(len as i32) as i64;
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
    }
    (ptr << 32) | len
}

// ─── Manifest ──────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_manifest() -> i64 {
    let manifest = Manifest {
        s6r: "1".into(),
        id: "my-extension".into(),
        version: "0.1.0".into(),
        description: "示例扩展".into(),
        capabilities: vec![],
        tools: vec![
            ManifestTool {
                name: "grep_files".into(),
                description: "搜索文件".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "pattern": { "type": "string" } },
                    "required": ["pattern"]
                }),
                mode: "parallel".into(),
            },
        ],
        commands: vec![],
        hooks: vec![
            ManifestHook { on: "pre_tool_use".into(), mode: "blocking".into() },
        ],
    };
    write_response(serde_json::to_string(&manifest).unwrap())
}

// ─── 统一调用入口 ───────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_call(req_ptr: i32, req_len: i32) -> i64 {
    let bytes = unsafe {
        std::slice::from_raw_parts(req_ptr as *const u8, req_len as usize)
    };
    let req: CallRequest = match serde_json::from_slice(bytes) {
        Ok(r) => r,
        Err(e) => {
            let resp = CallResponse::err("?", format!("parse request: {e}"));
            return write_response(serde_json::to_string(&resp).unwrap());
        }
    };

    let resp = handle(&req);
    write_response(serde_json::to_string(&resp).unwrap())
}

fn handle(req: &CallRequest) -> CallResponse {
    match req {
        CallRequest::Tool { id, name, input } => handle_tool(id, name, input),
        CallRequest::Hook { id, on, input }   => handle_hook(id, on, input),
        CallRequest::Command { id, name, input } => handle_command(id, name, input),
    }
}

fn handle_tool(id: &str, name: &str, input: &serde_json::Value) -> CallResponse {
    match name {
        "grep_files" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            // ... 实际搜索逻辑 ...
            CallResponse::ok(id)
        }
        _ => CallResponse::err(id, format!("unknown tool: {name}")),
    }
}

fn handle_hook(id: &str, on: &str, input: &serde_json::Value) -> CallResponse {
    match on {
        "pre_tool_use" => {
            let tool_name = input["tool_name"].as_str().unwrap_or("");
            if tool_name == "bash" {
                let cmd = input["tool_input"]["command"].as_str().unwrap_or("");
                if cmd.contains("rm -rf /") {
                    return CallResponse::with_effect(
                        id,
                        "block",
                        serde_json::json!({ "reason": "dangerous command blocked" }),
                    );
                }
            }
            CallResponse::ok(id)
        }
        _ => CallResponse::ok(id),
    }
}

fn handle_command(id: &str, name: &str, _input: &serde_json::Value) -> CallResponse {
    CallResponse::err(id, format!("unknown command: {name}"))
}
```

---

## 九、extension.json 的去留

`extension.json` 现在只需要两个字段，其余由 manifest 内联：

```json
{
  "library": "my_extension.wasm"
}
```

`id` 和 `capabilities` 从 WASM manifest 读取，不再需要在 `extension.json` 中重复声明。`extension.json` 只作为目录发现的入口（宿主扫描目录时找到它，就知道这里有个扩展，然后读取 `library` 字段加载 WASM 文件）。

---

## 十、改动范围总结

| 文件 | 改动 |
|------|------|
| `astrcode-extension-sdk/src/s6r.rs` | **新建**：`Manifest`, `CallRequest`, `CallResponse`, `S6R_VERSION` |
| `astrcode-extension-sdk/src/wasm_abi.rs` | 删除判别符函数和 `GUEST_EFFECT_*` 常量；`pub use crate::s6r::*` |
| `astrcode-extensions/src/wasm_api.rs` | 简化 `HostState`；`create_linker` 只注册 `host_log` + `host_emit` |
| `astrcode-extensions/src/wasm_ext.rs` | 加载改用 `extension_manifest()`；调用改用 `extension_call()`；`WasmInner` 只保留 `call_fn` |
| `astrcode-extension-sdk/src/manifest.rs` | 去掉 `library` 以外字段的强制校验 |

新建文件 1 个，修改文件 4 个。`extension.rs`（`Extension` trait、`Registrar`、`HookMode` 等）**不动**。

---

## 十一、host_invoke（v1）

已实现（见 [`host-invoke-plan.md`](./host-invoke-plan.md)）：

- 第三个 host import：`env.host_invoke(cap, input) -> i64`
- 响应 JSON：`{ "ok": true, "output": {...} }` / `{ "ok": false, "error": "..." }`
- v1 能力：`small_llm.chat`（须 manifest 声明 `small_model`；`HostState::declared_capabilities` + `host_invoke::authorize` 校验）

## 十二、不做的事（边界）

- **不引入 s5r 全协议 / IPC/STDIO 传输**：WASM + MCP 已覆盖 AstrCode 需求
- **不改 `Registrar` 和各 Handler trait**：宿主侧保持不变，只改 wasm_ext.rs 对它们的桥接方式
- **不加 proc macro**：当前 guest 手写两个入口即可，macro 是后续优化