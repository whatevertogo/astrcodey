//! IPC 扩展 E2E guest — 覆盖工具 / 命令 / 钩子 / host/invoke。

use std::{
    io::{BufRead, BufReader, Write},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use astrcode_protocol::framing::{JsonRpcMessage, from_jsonl_line, to_jsonl_line};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const EXT_ID: &str = "ipc-guest-demo";
const IPC_VERSION: &str = "1.0";

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

struct Rpc {
    reader: BufReader<std::io::Stdin>,
    writer: std::io::Stdout,
    next_id: u64,
}

impl Rpc {
    fn new() -> Self {
        Self {
            reader: BufReader::new(std::io::stdin()),
            writer: std::io::stdout(),
            next_id: 1,
        }
    }

    fn bump_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn write_request(&mut self, id: u64, method: &str, params: Value) {
        let msg = JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        };
        let line = to_jsonl_line(&msg).expect("serialize");
        self.writer.write_all(line.as_bytes()).expect("write");
        self.writer.flush().expect("flush");
    }

    fn write_response(&mut self, id: u64, result: Value) {
        let msg = JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        };
        let line = to_jsonl_line(&msg).expect("serialize");
        self.writer.write_all(line.as_bytes()).expect("write");
        self.writer.flush().expect("flush");
    }

    fn read_until(&mut self, expected_id: u64) -> Result<Value, String> {
        loop {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .map_err(|e| format!("read stdin: {e}"))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: JsonRpcMessage =
                from_jsonl_line(line).map_err(|e| format!("parse JSONL: {e}"))?;
            if msg.id != Some(expected_id) {
                continue;
            }
            if let Some(err) = msg.error {
                return Err(format!("JSON-RPC {}: {}", err.code, err.message));
            }
            return Ok(msg.result.unwrap_or(Value::Null));
        }
    }

    fn host_invoke(&mut self, capability: &str, input: Value) -> Option<Value> {
        let id = self.bump_id();
        self.write_request(
            id,
            "host/invoke",
            json!({
                "capability": capability,
                "input": input,
                "stream": false,
            }),
        );
        match self.read_until(id) {
            Ok(result) => result.get("output").cloned(),
            Err(e) => {
                eprintln!("host/invoke {capability} failed: {e}");
                None
            }
        }
    }
}

fn main() {
    let mut rpc = Rpc::new();
    loop {
        let mut line = String::new();
        if rpc.reader.read_line(&mut line).is_err() {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: JsonRpcMessage = match from_jsonl_line(line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("parse error: {e}");
                continue;
            }
        };
        let Some(method) = msg.method else { continue };
        let Some(id) = msg.id else { continue };
        match method.as_str() {
            "extension/initialize" => rpc.write_response(id, initialize_result()),
            "extension/handler.invoke" => {
                let result = handler_invoke(&mut rpc, msg.params);
                rpc.write_response(id, serde_json::to_value(result).unwrap());
            },
            "extension/ping" => rpc.write_response(id, json!({ "ok": true })),
            "extension/shutdown" => {},
            other => {
                let msg = JsonRpcMessage {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    method: None,
                    params: None,
                    result: None,
                    error: Some(astrcode_protocol::framing::JsonRpcError {
                        code: -32601,
                        message: format!("unknown method: {other}"),
                        data: None,
                    }),
                };
                let line = to_jsonl_line(&msg).expect("serialize");
                let _ = rpc.writer.write_all(line.as_bytes());
                let _ = rpc.writer.flush();
            },
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "extension_id": EXT_ID,
        "version": "0.1.0",
        "protocol": { "ipc": IPC_VERSION },
        "capabilities": ["small_model", "emit_events", "workspace_read"],
        "tools": [
            {
                "name": "ping",
                "description": "Returns pong",
                "parameters": { "type": "object", "properties": {} },
                "mode": "sequential"
            },
            {
                "name": "greet",
                "description": "Greet",
                "parameters": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }
            },
            {
                "name": "add",
                "description": "Add",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "a": { "type": "integer" },
                        "b": { "type": "integer" }
                    },
                    "required": ["a", "b"]
                }
            },
            {
                "name": "ask_llm",
                "description": "Ask small LLM via host/invoke",
                "parameters": {
                    "type": "object",
                    "properties": { "prompt": { "type": "string" } },
                    "required": ["prompt"]
                }
            },
            {
                "name": "pipeline_status",
                "description": "Pipeline status",
                "parameters": { "type": "object" }
            },
            {
                "name": "read_workspace",
                "description": "Read probe.txt via workspace.read",
                "parameters": { "type": "object" }
            }
        ],
        "commands": [{ "name": "demo", "description": "Demo slash command" }],
        "hooks": [
            { "on": "pre_tool_use", "mode": "blocking" },
            { "on": "turn_end", "mode": "non_blocking" }
        ],
        "extension_events": [{
            "event_type": "ipc_guest.probe",
            "schema_version": 1,
            "durable": true,
            "max_payload_bytes": 4096
        }]
    })
}

fn handler_invoke(rpc: &mut Rpc, params: Option<Value>) -> HandlerResult {
    let params = params.unwrap_or(Value::Null);
    let handler_id = params
        .get("handler_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let event = params.get("event").unwrap_or(&Value::Null);
    dispatch_handler(rpc, handler_id, event)
}

fn dispatch_handler(rpc: &mut Rpc, handler_id: &str, event: &Value) -> HandlerResult {
    if handler_id.ends_with(":tool:ping") {
        return HandlerResult::effect("ok", json!({ "content": "pong" }));
    }
    if handler_id.ends_with(":tool:greet") {
        return handle_tool_greet(event);
    }
    if handler_id.ends_with(":tool:add") {
        return handle_tool_add(event);
    }
    if handler_id.ends_with(":tool:ask_llm") {
        return handle_tool_ask_llm(rpc, event);
    }
    if handler_id.ends_with(":tool:pipeline_status") {
        return handle_tool_pipeline_status();
    }
    if handler_id.ends_with(":tool:read_workspace") {
        return handle_tool_read_workspace(rpc, event);
    }
    if handler_id.ends_with(":command:demo") {
        return HandlerResult::effect(
            "ok",
            json!({ "kind": "display", "content": "ipc guest demo works!", "is_error": false }),
        );
    }
    if handler_id.ends_with(":hook:pre_tool_use") {
        return handle_pre_tool_use(rpc, event);
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
        return handle_pipeline_step(rpc, event);
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

fn handle_tool_ask_llm(rpc: &mut Rpc, event: &Value) -> HandlerResult {
    let args = event.get("input").and_then(|i| i.get("arguments")).unwrap_or(event);
    let prompt = args["prompt"].as_str().unwrap_or("");
    let input = json!({ "messages": [{ "role": "user", "content": prompt }] });
    match rpc.host_invoke("astrcode.llm.small_chat", input) {
        Some(out) => {
            let content = out["content"].as_str().unwrap_or("(no content)");
            HandlerResult::effect("ok", json!({ "content": content }))
        }
        None => HandlerResult::err("astrcode.llm.small_chat failed"),
    }
}

fn handle_tool_read_workspace(rpc: &mut Rpc, event: &Value) -> HandlerResult {
    let input = event.get("input").unwrap_or(event);
    let working_dir = input["working_dir"].as_str().unwrap_or(".");
    let path = format!("{working_dir}/probe.txt");
    match rpc.host_invoke(
        "astrcode.workspace.read",
        json!({ "path": "probe.txt" }),
    ) {
        Some(out) => {
            let content = out["content"].as_str().unwrap_or("");
            HandlerResult::effect(
                "ok",
                json!({ "content": format!("read {path}: {content}") }),
            )
        }
        None => HandlerResult::err("astrcode.workspace.read failed"),
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

fn handle_pre_tool_use(rpc: &mut Rpc, event: &Value) -> HandlerResult {
    let input = event.get("input").unwrap_or(event);
    let tool_name = input["tool_name"].as_str().unwrap_or("");
    if tool_name == "emit_hook_probe" {
        let _ = rpc.host_invoke(
            "astrcode.event.emit",
            json!({
                "event_type": "ipc_guest.probe",
                "schema_version": 1,
                "payload": { "from": "pre_tool_use" }
            }),
        );
        return HandlerResult::ok();
    }
    if tool_name == "bash" {
        let cmd = input["tool_input"]["command"].as_str().unwrap_or("");
        if cmd.contains("rm -rf") {
            return HandlerResult::effect(
                "block",
                json!({ "reason": "dangerous rm -rf blocked by ipc-guest-demo" }),
            );
        }
    }
    HandlerResult::ok()
}

fn handle_pipeline_step(rpc: &mut Rpc, event: &Value) -> HandlerResult {
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
        }
        2 => {
            PIPELINE_STEPS.store(2, Ordering::SeqCst);
            let input = json!({ "messages": [{ "role": "user", "content": "continuation pipeline" }] });
            match rpc.host_invoke("astrcode.llm.small_chat", input) {
                Some(_) => {
                    PIPELINE_LLM_OK.store(true, Ordering::SeqCst);
                    HandlerResult::ok()
                }
                None => HandlerResult::err("astrcode.llm.small_chat failed in pipeline"),
            }
        }
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
