//! s5r guest demo — E2E WASM 插件参考实现。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const S5R_VERSION: &str = "1.0";
const EXT_ID: &str = "s5r-guest-demo";

#[derive(Serialize)]
struct WireMessageInitialize {
    #[serde(rename = "type")]
    msg_type: &'static str,
    id: String,
    peer: PeerInfo,
    handlers: Vec<HandlerDescriptor>,
    provided_capabilities: Vec<CapabilityDescriptor>,
    requested_capabilities: Vec<&'static str>,
    metadata: Value,
}

#[derive(Serialize, Deserialize)]
struct PeerInfo {
    name: String,
    role: String,
    version: String,
}

#[derive(Serialize)]
struct HandlerDescriptor {
    handler_id: String,
    description: String,
}

#[derive(Serialize)]
struct CapabilityDescriptor {
    name: String,
    description: String,
}

#[derive(Deserialize)]
struct WireEnvelope {
    #[serde(rename = "type")]
    msg_type: String,
    id: Option<String>,
    kind: Option<String>,
    success: Option<bool>,
    output: Option<Value>,
    error: Option<Value>,
    capability: Option<String>,
    input: Option<Value>,
}

#[derive(Deserialize)]
struct HandlerInvokeInput {
    handler_id: String,
    event: Value,
}

#[derive(Serialize, Deserialize)]
struct HandlerResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    effect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    continuations: Vec<CallContinuationHook>,
}

#[derive(Serialize, Deserialize)]
struct CallContinuationHook {
    call: String,
    on: String,
    input: Value,
}

static PIPELINE_STEPS: AtomicU32 = AtomicU32::new(0);
static PIPELINE_LLM_OK: AtomicBool = AtomicBool::new(false);

#[link(wasm_import_module = "env")]
extern "C" {
    #[link_name = "peer_exchange"]
    fn host_peer_exchange(req_ptr: i32, req_len: i32) -> i64;
}

fn call_host_peer_exchange(json: &str) -> Option<String> {
    let bytes = json.as_bytes();
    let packed = unsafe {
        host_peer_exchange(
            bytes.as_ptr() as i32,
            bytes.len() as i32,
        )
    };
    if packed == 0 {
        return None;
    }
    let ptr = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let len = (packed & 0xFFFF_FFFF) as u32;
    let resp = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        String::from_utf8_lossy(slice).into_owned()
    };
    dealloc(ptr as i32, len as i32);
    Some(resp)
}

fn invoke_astrcode(cap: &str, input: &str) -> Option<Value> {
    let msg = json!({
        "type": "invoke",
        "id": format!("guest-{}", cap),
        "capability": cap,
        "input": serde_json::from_str::<Value>(input).unwrap_or(Value::Null),
        "stream": false
    });
    let resp = call_host_peer_exchange(&msg.to_string())?;
    let env: WireEnvelope = serde_json::from_str(&resp).ok()?;
    if env.success == Some(true) {
        env.output
    } else {
        None
    }
}

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let layout = std::alloc::Layout::from_size_align(len as usize, 1).unwrap();
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

#[no_mangle]
pub extern "C" fn extension_init() -> i32 {
    let init = WireMessageInitialize {
        msg_type: "initialize",
        id: "init-1".into(),
        peer: PeerInfo {
            name: EXT_ID.into(),
            role: "extension".into(),
            version: "0.1.0".into(),
        },
        handlers: vec![
            HandlerDescriptor {
                handler_id: format!("{EXT_ID}:hook:pre_tool_use"),
                description: "block dangerous commands".into(),
            },
            HandlerDescriptor {
                handler_id: format!("{EXT_ID}:hook:turn_end"),
                description: "continuation pipeline".into(),
            },
            HandlerDescriptor {
                handler_id: format!("{EXT_ID}:tool:greet"),
                description: "greet tool".into(),
            },
            HandlerDescriptor {
                handler_id: format!("{EXT_ID}:tool:add"),
                description: "add tool".into(),
            },
            HandlerDescriptor {
                handler_id: format!("{EXT_ID}:tool:ask_llm"),
                description: "small llm".into(),
            },
            HandlerDescriptor {
                handler_id: format!("{EXT_ID}:tool:pipeline_status"),
                description: "pipeline status".into(),
            },
            HandlerDescriptor {
                handler_id: format!("{EXT_ID}:command:demo"),
                description: "demo command".into(),
            },
        ],
        provided_capabilities: vec![],
        requested_capabilities: vec!["small_model", "emit_events"],
        metadata: json!({
            "stack": "astrcode",
            "protocol": { "s5r": S5R_VERSION },
            "version": "0.1.0",
            "tools": [
                { "name": "greet", "description": "Greet", "parameters": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] } },
                { "name": "add", "description": "Add", "parameters": { "type": "object", "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } }, "required": ["a", "b"] } },
                { "name": "ask_llm", "description": "Ask LLM", "parameters": { "type": "object", "properties": { "prompt": { "type": "string" } }, "required": ["prompt"] } },
                { "name": "pipeline_status", "description": "Pipeline", "parameters": { "type": "object" } }
            ],
            "commands": [{ "name": "demo", "description": "Demo" }],
            "hooks": [
                { "on": "pre_tool_use", "mode": "blocking" },
                { "on": "turn_end", "mode": "non_blocking" }
            ],
            "extension_events": [
                {
                    "event_type": "s5r_guest.probe",
                    "schema_version": 1,
                    "durable": true,
                    "max_payload_bytes": 4096
                }
            ]
        }),
    };
    match call_host_peer_exchange(&serde_json::to_string(&init).unwrap()) {
        Some(_) => 0,
        None => 1,
    }
}

#[no_mangle]
pub extern "C" fn peer_exchange(req_ptr: i32, req_len: i32) -> i64 {
    let bytes = unsafe { std::slice::from_raw_parts(req_ptr as *const u8, req_len as usize) };
    let env: WireEnvelope = match serde_json::from_slice(bytes) {
        Ok(e) => e,
        Err(e) => {
            let err = json!({ "type": "result", "id": "?", "kind": "invoke_result", "success": false, "error": { "code": "parse", "message": e.to_string() }});
            return write_packed(err.to_string());
        }
    };
    if env.msg_type == "invoke" && env.capability.as_deref() == Some("handler.invoke") {
        let input: HandlerInvokeInput = serde_json::from_value(env.input.clone().unwrap_or(Value::Null))
            .unwrap_or(HandlerInvokeInput {
                handler_id: String::new(),
                event: Value::Null,
            });
        let result = dispatch_handler(&input.handler_id, &input.event);
        let out = json!({
            "type": "result",
            "id": env.id.unwrap_or_else(|| "req".into()),
            "kind": "invoke_result",
            "success": result.ok,
            "output": result
        });
        return write_packed(out.to_string());
    }
    let out = json!({ "type": "result", "id": env.id.unwrap_or_default(), "kind": "invoke_result", "success": false, "error": { "code": "unsupported", "message": "guest export only handles handler.invoke" }});
    write_packed(out.to_string())
}

fn dispatch_handler(handler_id: &str, event: &Value) -> HandlerResult {
    if handler_id.ends_with(":tool:greet") {
        return handle_tool_greet(event);
    }
    if handler_id.ends_with(":tool:add") {
        return handle_tool_add(event);
    }
    if handler_id.ends_with(":tool:ask_llm") {
        return handle_tool_ask_llm(event);
    }
    if handler_id.ends_with(":tool:pipeline_status") {
        return handle_tool_pipeline_status();
    }
    if handler_id.ends_with(":command:demo") {
        return HandlerResult::effect("ok", json!({ "kind": "display", "content": "s5r guest demo works!", "is_error": false }));
    }
    if handler_id.ends_with(":hook:pre_tool_use") {
        return handle_pre_tool_use(event);
    }
    if handler_id.ends_with(":hook:turn_end") {
        return HandlerResult {
            ok: true,
            effect: Some("ok".into()),
            data: None,
            error: None,
            continuations: vec![CallContinuationHook {
                call: "hook".into(),
                on: "pipeline_step".into(),
                input: json!({ "step": 1 }),
            }],
        };
    }
    if handler_id.contains(":hook:pipeline_step") {
        return handle_pipeline_step(event);
    }
    HandlerResult::err(format!("unknown handler: {handler_id}"))
}

fn handle_tool_greet(event: &Value) -> HandlerResult {
    let args = event.get("input").and_then(|i| i.get("arguments")).unwrap_or(event);
    let n = args["name"].as_str().unwrap_or("world");
    HandlerResult::effect("ok", json!({ "content": format!("hello, {n}!") }))
}

fn handle_tool_add(event: &Value) -> HandlerResult {
    let args = event.get("input").and_then(|i| i.get("arguments")).unwrap_or(event);
    let a = args["a"].as_i64().unwrap_or(0);
    let b = args["b"].as_i64().unwrap_or(0);
    HandlerResult::effect("ok", json!({ "content": format!("{a} + {b} = {}", a + b) }))
}

fn handle_tool_ask_llm(event: &Value) -> HandlerResult {
    let args = event.get("input").and_then(|i| i.get("arguments")).unwrap_or(event);
    let prompt = args["prompt"].as_str().unwrap_or("");
    let input = json!({ "messages": [{ "role": "user", "content": prompt }] }).to_string();
    match invoke_astrcode("astrcode.llm.small_chat", &input) {
        Some(out) => {
            let content = out["content"].as_str().unwrap_or("(no content)");
            HandlerResult::effect("ok", json!({ "content": content }))
        },
        None => HandlerResult::err("astrcode.llm.small_chat failed"),
    }
}

fn handle_tool_pipeline_status() -> HandlerResult {
    let steps = PIPELINE_STEPS.load(Ordering::SeqCst);
    let llm_ok = PIPELINE_LLM_OK.load(Ordering::SeqCst);
    HandlerResult::effect(
        "ok",
        json!({ "content": format!("steps={steps} llm_ok={llm_ok}") }),
    )
}

fn handle_pre_tool_use(event: &Value) -> HandlerResult {
    let input = event.get("input").unwrap_or(event);
    let tool_name = input["tool_name"].as_str().unwrap_or("");
    if tool_name == "emit_hook_probe" {
        let _ = invoke_astrcode(
            "astrcode.event.emit",
            r#"{"event_type":"s5r_guest.probe","schema_version":1,"payload":{"from":"pre_tool_use"}}"#,
        );
        return HandlerResult::ok();
    }
    if tool_name == "bash" {
        let cmd = input["tool_input"]["command"].as_str().unwrap_or("");
        if cmd.contains("rm -rf") {
            return HandlerResult::effect(
                "block",
                json!({ "reason": "dangerous rm -rf blocked by s5r-guest-demo" }),
            );
        }
    }
    HandlerResult::ok()
}

fn handle_pipeline_step(event: &Value) -> HandlerResult {
    let input = event.get("input").unwrap_or(event);
    let step = input["step"].as_u64().unwrap_or(0);
    match step {
        1 => {
            PIPELINE_STEPS.store(1, Ordering::SeqCst);
            HandlerResult {
                ok: true,
                effect: Some("ok".into()),
                data: None,
                error: None,
                continuations: vec![CallContinuationHook {
                    call: "hook".into(),
                    on: "pipeline_step".into(),
                    input: json!({ "step": 2 }),
                }],
            }
        },
        2 => {
            PIPELINE_STEPS.store(2, Ordering::SeqCst);
            let input = json!({ "messages": [{ "role": "user", "content": "continuation pipeline" }] }).to_string();
            match invoke_astrcode("astrcode.llm.small_chat", &input) {
                Some(_) => {
                    PIPELINE_LLM_OK.store(true, Ordering::SeqCst);
                    HandlerResult::ok()
                },
                None => HandlerResult::err("astrcode.llm.small_chat failed in pipeline"),
            }
        },
        _ => HandlerResult::err(format!("unknown pipeline step: {step}")),
    }
}

impl HandlerResult {
    fn ok() -> Self {
        Self {
            ok: true,
            effect: Some("ok".into()),
            data: None,
            error: None,
            continuations: Vec::new(),
        }
    }
    fn effect(effect: &str, data: Value) -> Self {
        Self {
            ok: true,
            effect: Some(effect.into()),
            data: Some(data),
            error: None,
            continuations: Vec::new(),
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            effect: None,
            data: None,
            error: Some(msg.into()),
            continuations: Vec::new(),
        }
    }
}
