//! s6r guest demo plugin — 用于 E2E 测试的真实 WASM 插件。
//!
//! 注册能力：
//! - tool `greet`：接收 `{ "name": "..." }`，返回 `"hello, {name}!"`
//! - tool `add`：接收 `{ "a": i64, "b": i64 }`，返回 `"a + b = {result}"`
//! - tool `ask_llm`：调用 `host_invoke("small_llm.chat", ...)`，返回 LLM 响应
//! - hook `pre_tool_use` (blocking)：阻止包含 `rm -rf` 的 bash 命令
//! - command `/demo`：返回 display 消息

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── s6r 协议类型（guest 侧自包含，不依赖 SDK） ─────────────────────────────

const S6R_VERSION: &str = "1";

#[derive(Serialize)]
struct Manifest {
    s6r: &'static str,
    id: &'static str,
    version: &'static str,
    capabilities: Vec<&'static str>,
    tools: Vec<ManifestTool>,
    commands: Vec<ManifestCommand>,
    hooks: Vec<ManifestHook>,
}

#[derive(Serialize)]
struct ManifestTool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

#[derive(Serialize)]
struct ManifestCommand {
    name: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
struct ManifestHook {
    on: &'static str,
    mode: &'static str,
}

#[derive(Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
enum CallRequest {
    Tool {
        id: String,
        name: String,
        input: Value,
    },
    Hook {
        id: String,
        on: String,
        input: Value,
    },
    Command {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Serialize)]
struct CallResponse {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    effect: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl CallResponse {
    fn ok(id: String) -> Self {
        Self {
            id,
            ok: true,
            effect: Some("ok"),
            data: None,
            error: None,
        }
    }

    fn with_effect(id: String, effect: &'static str, data: Value) -> Self {
        Self {
            id,
            ok: true,
            effect: Some(effect),
            data: Some(data),
            error: None,
        }
    }

    fn err(id: String, msg: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            effect: None,
            data: None,
            error: Some(msg.into()),
        }
    }
}

// ─── host import: host_invoke ────────────────────────────────────────────

#[link(wasm_import_module = "env")]
extern "C" {
    fn host_invoke(
        cap_ptr: i32,
        cap_len: i32,
        input_ptr: i32,
        input_len: i32,
    ) -> i64;
}

fn call_host_invoke(cap: &str, input: &str) -> Option<String> {
    let cap_bytes = cap.as_bytes();
    let input_bytes = input.as_bytes();
    let packed = unsafe {
        host_invoke(
            cap_bytes.as_ptr() as i32,
            cap_bytes.len() as i32,
            input_bytes.as_ptr() as i32,
            input_bytes.len() as i32,
        )
    };
    if packed == 0 {
        return None;
    }
    let resp_ptr = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let resp_len = (packed & 0xFFFF_FFFF) as u32;
    let resp = unsafe {
        let slice = std::slice::from_raw_parts(resp_ptr as *const u8, resp_len as usize);
        String::from_utf8_lossy(slice).into_owned()
    };
    dealloc(resp_ptr as i32, resp_len as i32);
    Some(resp)
}

// ─── 内存管理 ──────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let len = len as usize;
    let layout = std::alloc::Layout::from_size_align(len, 1).unwrap();
    unsafe { std::alloc::alloc(layout) as i32 }
}

#[no_mangle]
pub extern "C" fn dealloc(ptr: i32, len: i32) {
    let layout = std::alloc::Layout::from_size_align(len as usize, 1).unwrap();
    unsafe { std::alloc::dealloc(ptr as *mut u8, layout) };
}

fn pack(ptr: i32, len: i32) -> i64 {
    ((ptr as i64) << 32) | (len as i64 & 0xFFFF_FFFF)
}

fn write_packed(json: String) -> i64 {
    let bytes = json.into_bytes();
    let len = bytes.len() as i32;
    let ptr = alloc(len);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
    }
    pack(ptr, len)
}

// ─── extension_manifest ──────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_manifest() -> i64 {
    let manifest = Manifest {
        s6r: S6R_VERSION,
        id: "s6r-guest-demo",
        version: "0.1.0",
        capabilities: vec!["small_model"],
        tools: vec![
            ManifestTool {
                name: "greet",
                description: "Greet someone by name",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }),
            },
            ManifestTool {
                name: "add",
                description: "Add two numbers",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "a": { "type": "integer" },
                        "b": { "type": "integer" }
                    },
                    "required": ["a", "b"]
                }),
            },
            ManifestTool {
                name: "ask_llm",
                description: "Ask the host small LLM a question",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "prompt": { "type": "string" } },
                    "required": ["prompt"]
                }),
            },
        ],
        commands: vec![ManifestCommand {
            name: "demo",
            description: "Run a demo command",
        }],
        hooks: vec![ManifestHook {
            on: "pre_tool_use",
            mode: "blocking",
        }],
    };
    write_packed(serde_json::to_string(&manifest).unwrap())
}

// ─── extension_call ─────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_call(req_ptr: i32, req_len: i32) -> i64 {
    let bytes = unsafe { std::slice::from_raw_parts(req_ptr as *const u8, req_len as usize) };
    let req: CallRequest = match serde_json::from_slice(bytes) {
        Ok(r) => r,
        Err(e) => {
            let resp = CallResponse::err("?".into(), format!("parse: {e}"));
            return write_packed(serde_json::to_string(&resp).unwrap());
        }
    };
    let resp = dispatch(req);
    write_packed(serde_json::to_string(&resp).unwrap())
}

fn dispatch(req: CallRequest) -> CallResponse {
    match req {
        CallRequest::Tool { id, name, input } => handle_tool(id, name, input),
        CallRequest::Hook { id, on, input } => handle_hook(id, on, input),
        CallRequest::Command { id, name, input } => handle_command(id, name, input),
    }
}

fn handle_tool(id: String, name: String, input: Value) -> CallResponse {
    // host 将实际参数放在 input.arguments 中
    let args = input.get("arguments").cloned().unwrap_or(input);
    match name.as_str() {
        "greet" => {
            let n = args["name"].as_str().unwrap_or("world");
            CallResponse::with_effect(
                id,
                "ok",
                serde_json::json!({ "content": format!("hello, {n}!") }),
            )
        }
        "add" => {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            CallResponse::with_effect(
                id,
                "ok",
                serde_json::json!({ "content": format!("{} + {} = {}", a, b, a + b) }),
            )
        }
        "ask_llm" => {
            let prompt = args["prompt"].as_str().unwrap_or("");
            let input = serde_json::json!({
                "messages": [{ "role": "user", "content": prompt }],
                "max_tokens": 256
            })
            .to_string();
            match call_host_invoke("small_llm.chat", &input) {
                Some(resp_json) => {
                    let resp: Value = serde_json::from_str(&resp_json).unwrap_or_default();
                    if resp["ok"].as_bool().unwrap_or(false) {
                        let content = resp["output"]["content"]
                            .as_str()
                            .unwrap_or("(no content)");
                        CallResponse::with_effect(
                            id,
                            "ok",
                            serde_json::json!({ "content": content.to_string() }),
                        )
                    } else {
                        let err = resp["error"].as_str().unwrap_or("unknown error");
                        CallResponse::err(id, err.to_string())
                    }
                },
                None => CallResponse::err(id, "host_invoke returned null".to_string()),
            }
        }
        _ => CallResponse::err(id, format!("unknown tool: {name}")),
    }
}

fn handle_hook(id: String, on: String, input: Value) -> CallResponse {
    match on.as_str() {
        "pre_tool_use" => {
            let tool_name = input["tool_name"].as_str().unwrap_or("");
            if tool_name == "bash" {
                let cmd = input["tool_input"]["command"].as_str().unwrap_or("");
                if cmd.contains("rm -rf") {
                    return CallResponse::with_effect(
                        id,
                        "block",
                        serde_json::json!({ "reason": "dangerous rm -rf blocked by s6r-guest-demo" }),
                    );
                }
            }
            CallResponse::ok(id)
        }
        _ => CallResponse::ok(id),
    }
}

fn handle_command(id: String, name: String, _input: Value) -> CallResponse {
    match name.as_str() {
        "demo" => CallResponse::with_effect(
            id,
            "ok",
            serde_json::json!({ "kind": "display", "content": "s6r guest demo works!", "is_error": false }),
        ),
        _ => CallResponse::err(id, format!("unknown command: {name}")),
    }
}
