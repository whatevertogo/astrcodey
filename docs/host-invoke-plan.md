# host_invoke 实现 Plan

> 目标读者：实现此功能的 agent。
> 本文档是可直接执行的规范，所有类型、函数签名、文件路径均可直接作为实现依据。

---

## 一、范围与前提

### 不做什么

- 内置扩展（`astrcode-bundled-extensions`）**不走 WASM**，继续用 `Extension` trait in-process。
  理由：它们是可信代码，`Arc<dyn LlmProvider>` 无法跨 WASM 边界，
  且 `astrcode-core` 依赖 tokio rt-multi-thread/net/fs/process，wasm32 下编不过。

### 做什么

为外部 WASM 插件增加一个 host import：

```
host_invoke(cap_ptr: i32, cap_len: i32, input_ptr: i32, input_len: i32) -> i64
```

让第三方 WASM 插件能同步地调用宿主能力（当前仅暴露 `small_llm.chat`），
无需实现完整的双向 IPC 协议。

---

## 二、ABI 规范

### host_invoke 语义

```
输入：
  cap_ptr / cap_len   — guest 内存中的能力名称 UTF-8 字符串
  input_ptr / input_len — guest 内存中的输入 JSON UTF-8 字符串

输出：
  packed i64 = (resp_ptr as i64) << 32 | (resp_len as i64)
  指向 guest 内存中的 ResultMsg JSON。
  返回 0 表示能力不存在或内部错误。
```

**ResultMsg 格式**（宿主写入 guest 内存，guest 读取后必须调用 `dealloc(resp_ptr, resp_len)`）：

```json
{ "ok": true,  "output": { ... } }
{ "ok": false, "error": "capability not found" }
```

### 初始能力集（v1 只实现这一个）

**`small_llm.chat`**

输入：
```json
{
  "messages": [
    { "role": "user", "content": "hello" }
  ],
  "max_tokens": 512
}
```

输出（`output` 字段内）：
```json
{
  "content": "Hi there!",
  "model": "gpt-4o-mini"
}
```

仅当插件在 manifest 中声明了 `"small_model"` capability 时，此能力才可用；
否则返回 `{ "ok": false, "error": "permission denied: small_model not declared" }`。

---

## 三、改动文件一览

| 文件 | 改动类型 | 说明 |
|------|---------|------|
| `astrcode-extensions/src/wasm_api.rs` | 修改 | 添加 `HostInvoker` 类型别名、`HostState.invoker` 字段、`host_invoke` 函数、更新 `create_linker` |
| `astrcode-extensions/src/wasm_ext.rs` | 修改 | `WasmExtension::load` 接受 `Option<HostInvoker>` 参数，传入 `HostState` |
| `astrcode-extensions/src/loader.rs` | 修改 | `load_extension` 传递 `invoker`（当前传 `None`，宿主层后续注入） |

不改动：`s6r.rs`、`wasm_abi.rs`、`extension.rs`、bundled extensions。

---

## 四、wasm_api.rs 改动

### 4.1 新增类型别名

```rust
/// 宿主能力的同步调用接口。
///
/// 签名：`(capability_name, input_json) -> response_json`
///
/// 实现者负责将异步能力包装为同步（通过 `Handle::block_on`）。
/// 调用发生在 `spawn_blocking` 线程上，可以安全地 block。
pub type HostInvoker = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;
```

### 4.2 HostState：`declared_capabilities` + `finish_manifest`

```rust
pub struct HostState {
    pub fuel_budget: u64,
    pub memory_limit: usize,
    pub invoker: Option<HostInvoker>,
    pub declared_capabilities: Vec<ExtensionCapability>,
}

impl HostState {
    /// `extension_manifest` 成功后绑定；此前 `invoker` 为 None。
    pub fn finish_manifest(
        &mut self,
        declared: Vec<ExtensionCapability>,
        invoker: Option<HostInvoker>,
    ) { /* ... */ }
}
```

权限在 `host_invoke` import 内通过 `host_invoke::authorize` 校验（与 `ExtensionRunner::allows` 同源）。

### 4.3 host_invoke 回调实现

```rust
fn host_invoke(
    mut caller: Caller<'_, HostState>,
    cap_ptr: i32,
    cap_len: i32,
    input_ptr: i32,
    input_len: i32,
) -> i64 {
    // 1. 读取 capability 名称和输入 JSON
    let cap   = read_caller_string(&mut caller, cap_ptr   as u32, cap_len   as u32);
    let input = read_caller_string(&mut caller, input_ptr as u32, input_len as u32);

    // 2. 调用宿主能力（同步，我们在 spawn_blocking 线程上）
    let invoker = caller.data().invoker.clone();
    let Some(invoker) = invoker else {
        tracing::debug!(target: "wasm_ext", cap, "host_invoke: no invoker configured");
        return 0;
    };
    let resp_json   = invoker(&cap, &input);
    let resp_bytes  = resp_json.as_bytes();
    let resp_len    = resp_bytes.len();

    // 3. 在 guest 内存中分配响应缓冲区
    //    注意：typed() 只做类型验证（共享借用），call() 才是可变借用——NLL 无重叠。
    let Some(alloc_export) = caller.get_export("alloc").and_then(|e| e.into_func()) else {
        tracing::warn!(target: "wasm_ext", "host_invoke: guest missing alloc export");
        return 0;
    };
    let Ok(typed_alloc) = alloc_export.typed::<i32, i32>(&caller) else {
        tracing::warn!(target: "wasm_ext", "host_invoke: alloc has wrong type");
        return 0;
    };
    let ptr = match typed_alloc.call(&mut caller, resp_len as i32) {
        Ok(p) => p as u32,
        Err(e) => {
            tracing::warn!(target: "wasm_ext", "host_invoke: guest alloc failed: {e}");
            return 0;
        },
    };

    // 4. 写入响应数据
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        tracing::warn!(target: "wasm_ext", "host_invoke: guest missing memory export");
        return 0;
    };
    let start = ptr as usize;
    let end   = start + resp_len;
    if end > mem.data(&caller).len() {
        tracing::warn!(target: "wasm_ext", "host_invoke: response out of bounds");
        return 0;
    }
    mem.data_mut(&mut caller)[start..end].copy_from_slice(resp_bytes);

    // 5. 返回 packed (ptr << 32 | len)
    ((ptr as i64) << 32) | (resp_len as i64)
}
```

**关键：为什么可以在 import 回调里调用 `alloc`？**

Wasmtime 允许在 `Caller` 上调用 guest 导出函数，这是"re-entrant" host call 的标准模式。
`typed()` 不持有借用（只做类型验证），`call()` 获取可变借用，NLL 保证两者不重叠。

### 4.4 更新 create_linker

在现有 `host_log` 和 `host_emit` 注册之后添加：

```rust
linker
    .func_wrap("env", "host_invoke", host_invoke)
    .map_err(|e| format!("register host_invoke: {e}"))?;
```

---

## 五、wasm_ext.rs 改动

### 5.1 WasmExtension::load 签名变更

```rust
pub fn load(
    path: &std::path::Path,
    id: String,
    capabilities: Vec<ExtensionCapability>,
    fuel: u64,
    memory_bytes: usize,
    invoker: Option<HostInvoker>,   // ← 新增参数
) -> Result<Arc<Self>, String> {
    // ...
    let host_state = HostState::new().with_limits(fuel, memory_bytes);
    // extension_manifest() ...
    store.data_mut().finish_manifest(capabilities, invoker);
    // ...
}
```

### 5.2 调用方更新（loader.rs）

```rust
// 当前：传 None（无能力），后续由宿主层注入
crate::wasm_ext::WasmExtension::load(
    &lib_path,
    manifest.id.clone(),
    manifest.capabilities.clone(),
    limits.fuel,
    limits.memory_bytes,
    None,   // ← 新增
)
```

宿主层（server/core 层）需要构建真正的 `HostInvoker` 再传入。

---

## 六、宿主层构建 HostInvoker（示例）

这段代码在 `astrcode-extensions` 的**调用方**（如 `astrcode-server` 或 `astrcode-session`）中编写，
不在 `astrcode-extensions` 内部。

```rust
use astrcode_extensions::wasm_api::HostInvoker;
use std::sync::Arc;

fn build_invoker(
    small_llm: Option<Arc<dyn LlmProvider>>,
    declared_caps: &[ExtensionCapability],
) -> Option<HostInvoker> {
    let has_small_model = declared_caps.contains(&ExtensionCapability::SmallModel);
    let provider = small_llm?;
    let handle   = tokio::runtime::Handle::current();

    Some(Arc::new(move |cap: &str, input: &str| -> String {
        match cap {
            "small_llm.chat" => {
                if !has_small_model {
                    return r#"{"ok":false,"error":"permission denied: small_model not declared"}"#
                        .to_string();
                }
                // 在 spawn_blocking 线程上同步运行异步代码
                let result = handle.block_on(async {
                    let req: serde_json::Value = serde_json::from_str(input)
                        .unwrap_or(serde_json::json!({"messages":[]}));
                    invoke_small_llm(&*provider, req).await
                });
                match result {
                    Ok(output) => {
                        serde_json::json!({ "ok": true, "output": output }).to_string()
                    },
                    Err(e) => {
                        serde_json::json!({ "ok": false, "error": e.to_string() }).to_string()
                    },
                }
            },
            _ => {
                serde_json::json!({
                    "ok": false,
                    "error": format!("unknown capability: {cap}")
                })
                .to_string()
            },
        }
    }))
}
```

**为什么 `handle.block_on()` 在这里安全？**

`WasmExtension::call_guest_blocking` 通过 `tokio::task::spawn_blocking` 执行，
运行在 blocking 线程池。Tokio 的 blocking 线程与 async worker 线程分开，
可以安全地调用 `Handle::block_on()`。
在 async worker 线程上调用 `block_on` 会 panic；在 blocking 线程上是合法的。

---

## 七、guest 端使用示例

外部 WASM 插件在 `extension_call` 里调用 `host_invoke`：

```rust
// 声明 host import（no_std 兼容）
extern "C" {
    fn host_invoke(
        cap_ptr: i32, cap_len: i32,
        input_ptr: i32, input_len: i32,
    ) -> i64;
}

fn call_small_llm(prompt: &str) -> String {
    let input = serde_json::json!({
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": 256
    })
    .to_string();

    let cap   = "small_llm.chat";
    let packed = unsafe {
        host_invoke(
            cap.as_ptr()   as i32, cap.len()   as i32,
            input.as_ptr() as i32, input.len() as i32,
        )
    };

    if packed == 0 {
        return String::from("error: host_invoke failed");
    }

    let resp_ptr = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let resp_len = (packed & 0xFFFF_FFFF) as u32;

    // 读取响应
    let resp_json = unsafe {
        let slice = std::slice::from_raw_parts(resp_ptr as *const u8, resp_len as usize);
        String::from_utf8_lossy(slice).into_owned()
    };

    // 释放响应内存
    unsafe { dealloc(resp_ptr as i32, resp_len as i32) };

    // 解析
    let resp: serde_json::Value = serde_json::from_str(&resp_json).unwrap_or_default();
    resp["output"]["content"]
        .as_str()
        .unwrap_or("(no content)")
        .to_string()
}
```

---

## 八、边界情况处理

| 情况 | 宿主行为 |
|------|---------|
| `invoker = None` | `host_invoke` 返回 0，guest 视为失败 |
| 能力名未知 | 返回 `{"ok":false,"error":"unknown capability: xxx"}` |
| `small_model` 未声明 | 返回 `{"ok":false,"error":"permission denied: ..."}` |
| guest `alloc` 失败（内存不足） | `host_invoke` 返回 0 |
| `invoker` panic | `invoke_fn` 应在内部 catch_unwind；否则 spawn_blocking 任务 panic，上层得到 JoinError |
| `block_on` 超时 | invoker 自行设置 timeout（如 `tokio::time::timeout(Duration::from_secs(30), ...)` 包装） |

---

## 九、扩展能力的原则（未来）

新增 `host_invoke` 能力时遵守：

1. **声明才可用**：能力必须在 manifest 的 `capabilities` 中声明，宿主校验后才注入。
2. **无状态输入/输出**：input/output 均为 JSON，不传指针，不共享内存。
3. **有限集**：不要把整个 capability router 暴露给 WASM。小模型、事件发射、已足够。
4. **超时保护**：每个能力调用都包一个合理的 timeout（默认 30s）。
5. **日志隔离**：target 固定为 `"wasm_ext"` 便于过滤。
