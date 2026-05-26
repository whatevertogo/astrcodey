//! Worker handler 注册辅助：减少闭包样板、支持类型化参数。

use std::{future::Future, sync::Arc};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    s5r::{ErrorPayload, HandlerResult},
    worker::registry::{CommandHandlerFn, HookHandlerFn, ToolHandlerFn, WorkerCallContext},
};

/// 从 tool 事件 JSON 反序列化 LLM 传入的 `arguments`。
pub fn parse_tool_arguments<T: DeserializeOwned>(event: &Value) -> Result<T, ErrorPayload> {
    let args = event
        .get("input")
        .and_then(|i| i.get("arguments"))
        .or_else(|| event.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    serde_json::from_value(args)
        .map_err(|e| ErrorPayload::new("invalid_arguments", format!("parse tool arguments: {e}")))
}

/// 从 hook 事件 JSON 反序列化 `input` 载荷。
pub fn parse_hook_input<T: DeserializeOwned>(event: &Value) -> Result<T, ErrorPayload> {
    let input = event.get("input").cloned().unwrap_or_else(|| event.clone());
    serde_json::from_value(input)
        .map_err(|e| ErrorPayload::new("invalid_input", format!("parse hook input: {e}")))
}

/// 无参 tool handler：`async move |ctx| { ... }`。
pub fn tool_handler<F, Fut>(f: F) -> ToolHandlerFn
where
    F: Fn(WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// 带反序列化参数的 tool handler：`async move |args, ctx| { ... }`。
pub fn tool_handler_args<A, F, Fut>(f: F) -> ToolHandlerFn
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A, WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |event, ctx| match parse_tool_arguments::<A>(&event) {
        Err(e) => Box::pin(async move { Err(e) }),
        Ok(args) => Box::pin(f(args, ctx)),
    })
}

/// 无参 hook handler。
pub fn hook_handler<F, Fut>(f: F) -> HookHandlerFn
where
    F: Fn(WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// 带反序列化 hook input 的 handler。
pub fn hook_handler_args<A, F, Fut>(f: F) -> HookHandlerFn
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A, WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |event, ctx| match parse_hook_input::<A>(&event) {
        Err(e) => Box::pin(async move { Err(e) }),
        Ok(input) => Box::pin(f(input, ctx)),
    })
}

/// 无参 command handler。
pub fn command_handler<F, Fut>(f: F) -> CommandHandlerFn
where
    F: Fn(WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// 将 [`ErrorPayload`] 转为失败的 [`HandlerResult`]（保留 code 于 data）。
pub fn handler_err(err: ErrorPayload) -> HandlerResult {
    HandlerResult {
        ok: false,
        effect: None,
        data: Some(serde_json::json!({
            "code": err.code,
            "hint": err.hint,
            "retryable": err.retryable,
            "details": err.details,
        })),
        error: Some(err.message),
        continuations: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct GreetArgs {
        name: String,
    }

    #[test]
    fn parse_tool_arguments_from_nested_input() {
        let event = serde_json::json!({
            "input": { "arguments": { "name": "s5r" } }
        });
        let args: GreetArgs = parse_tool_arguments(&event).unwrap();
        assert_eq!(args.name, "s5r");
    }

    #[tokio::test]
    async fn tool_handler_args_deserializes() {
        let handler = tool_handler_args(|args: GreetArgs, _ctx| async move {
            Ok(HandlerResult::effect(
                "ok",
                serde_json::json!({ "content": format!("hi {}", args.name) }),
            ))
        });
        let event = serde_json::json!({
            "input": { "arguments": { "name": "world" } }
        });
        let ctx = WorkerCallContext {
            extension_id: "ext".into(),
            cancel_token: crate::runtime::CancelToken::default(),
        };
        let out = handler(event, ctx).await.unwrap();
        assert!(out.ok);
    }
}
