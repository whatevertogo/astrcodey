//! s5r 扩展 E2E guest — 使用 Worker SDK（manifest 与 handler 一体注册）。

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use astrcode_extension_sdk::{
    builder::tool,
    s5r::{
        ErrorPayload,
        effects::{CallContinuation, HandlerResult},
    },
    worker_prelude::*,
};
use serde::Deserialize;
use serde_json::{Value, json};

static PIPELINE_STEPS: AtomicU32 = AtomicU32::new(0);
static PIPELINE_LLM_OK: AtomicBool = AtomicBool::new(false);

const EXT_ID: &str = "s5r-guest-demo";

#[derive(Deserialize)]
struct GreetArgs {
    name: String,
}

#[derive(Deserialize)]
struct AddArgs {
    a: i64,
    b: i64,
}

#[derive(Deserialize)]
struct AskLlmArgs {
    prompt: String,
}

#[derive(Deserialize)]
struct PreToolInput {
    tool_name: String,
    tool_input: Value,
}

#[derive(Deserialize)]
struct PipelineStepInput {
    step: u64,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("s5r guest failed: {} ({})", error.message, error.code);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ErrorPayload> {
    let mut worker = Worker::new(EXT_ID)
        .version("0.1.0")
        .capability("small_model")
        .capability("emit_events")
        .capability("workspace_read")
        .extension_event(json!({
            "event_type": "s5r_guest.probe",
            "schema_version": 1,
            "durable": true,
            "max_payload_bytes": 4096
        }));

    worker.tool(
        tool("ping")
            .description("Returns pong")
            .parameters(json!({ "type": "object", "properties": {} }))
            .build(),
        tool_handler(|_ctx| async { Ok(tool_text("pong", false)) }),
    );

    worker.tool(
        tool("greet")
            .description("Greet")
            .parameters(json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }))
            .build(),
        tool_handler_args(|args: GreetArgs, _ctx| async move {
            Ok(tool_text(format!("hello, {}!", args.name), false))
        }),
    );

    worker.tool(
        tool("add")
            .description("Add")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "b": { "type": "integer" }
                },
                "required": ["a", "b"]
            }))
            .build(),
        tool_handler_args(|args: AddArgs, _ctx| async move {
            Ok(tool_text(format!("{} + {} = {}", args.a, args.b, args.a + args.b), false))
        }),
    );

    worker.tool(
        tool("ask_llm")
            .description("Ask small LLM")
            .parameters(json!({
                "type": "object",
                "properties": { "prompt": { "type": "string" } },
                "required": ["prompt"]
            }))
            .build(),
        tool_handler_args(|args: AskLlmArgs, _ctx| async move {
            let out = HostClient::call(
                "astrcode.llm.small_chat",
                json!({ "messages": [{ "role": "user", "content": args.prompt }] }),
            )
            .await?;
            let content = out["content"].as_str().unwrap_or("(no content)");
            Ok(tool_text(content, false))
        }),
    );

    worker.tool(
        tool("pipeline_status")
            .description("Pipeline status")
            .parameters(json!({ "type": "object" }))
            .build(),
        tool_handler(|_ctx| async move {
            let steps = PIPELINE_STEPS.load(Ordering::SeqCst);
            let llm_ok = PIPELINE_LLM_OK.load(Ordering::SeqCst);
            Ok(tool_text(format!("steps={steps} llm_ok={llm_ok}"), false))
        }),
    );

    worker.tool(
        tool("read_workspace")
            .description("Read probe.txt")
            .parameters(json!({ "type": "object" }))
            .build(),
        tool_handler(|_ctx| async move {
            let out = HostClient::call(
                "astrcode.workspace.read",
                json!({ "path": "probe.txt" }),
            )
            .await?;
            let content = out["content"].as_str().unwrap_or("");
            Ok(tool_text(format!("read probe.txt: {content}"), false))
        }),
    );

    worker.tool(
        tool("slow")
            .description("Slow tool for cancel E2E")
            .parameters(json!({ "type": "object" }))
            .build(),
        tool_handler(|ctx| async move {
            for _ in 0..200 {
                if ctx.cancel_token.is_cancelled() {
                    return Ok(tool_text("cancelled", true));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(tool_text("done", false))
        }),
    );

    worker.command(
        "demo",
        "Demo slash command",
        command_handler(|_ctx| async {
            Ok(HandlerResult::effect(
                "ok",
                json!({ "kind": "display", "content": "s5r guest demo works!", "is_error": false }),
            ))
        }),
    );

    worker.hook(
        "pre_tool_use",
        "blocking",
        hook_handler_args(|input: PreToolInput, _ctx| async move {
            if input.tool_name == "emit_hook_probe" {
                let _ = HostClient::call(
                    "astrcode.event.emit",
                    json!({
                        "event_type": "s5r_guest.probe",
                        "schema_version": 1,
                        "payload": { "from": "pre_tool_use" }
                    }),
                )
                .await;
                return Ok(HandlerResult::ok());
            }
            if input.tool_name == "bash" {
                let cmd = input.tool_input["command"].as_str().unwrap_or("");
                if cmd.contains("rm -rf") {
                    return Ok(HandlerResult::effect(
                        "block",
                        json!({ "reason": "dangerous rm -rf blocked by s5r-guest-demo" }),
                    ));
                }
            }
            Ok(HandlerResult::ok())
        }),
    );

    worker.hook(
        "turn_end",
        "non_blocking",
        hook_handler(|_ctx| async {
            Ok(HandlerResult {
                ok: true,
                effect: Some("ok".into()),
                data: None,
                error: None,
                continuations: vec![CallContinuation::Hook {
                    on: "pipeline_step".into(),
                    input: json!({ "step": 1 }),
                }],
            })
        }),
    );

    worker.hook(
        "pipeline_step",
        "non_blocking",
        hook_handler_args(|input: PipelineStepInput, _ctx| async move {
            match input.step {
                1 => {
                    PIPELINE_STEPS.store(1, Ordering::SeqCst);
                    Ok(HandlerResult {
                        ok: true,
                        effect: Some("ok".into()),
                        data: None,
                        error: None,
                        continuations: vec![CallContinuation::Hook {
                            on: "pipeline_step".into(),
                            input: json!({ "step": 2 }),
                        }],
                    })
                }
                2 => {
                    PIPELINE_STEPS.store(2, Ordering::SeqCst);
                    let _ = HostClient::call_stream(
                        "astrcode.llm.small_chat",
                        json!({ "messages": [{ "role": "user", "content": "continuation pipeline" }] }),
                    )
                    .await?;
                    PIPELINE_LLM_OK.store(true, Ordering::SeqCst);
                    Ok(HandlerResult::ok())
                }
                _ => Err(ErrorPayload::new(
                    "unknown_step",
                    format!("unknown pipeline step: {}", input.step),
                )),
            }
        }),
    );

    worker.run_stdio().await
}
