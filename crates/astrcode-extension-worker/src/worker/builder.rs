//! Worker handler 注册辅助：减少闭包样板、支持类型化参数。

use std::{future::Future, sync::Arc};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    WireErrorCode,
    extension::{ExtensionHttpRequest, ExtensionHttpResponse},
    s5r::{ErrorPayload, HandlerResult},
    worker::registry::{
        CommandHandlerFn, ContinuationHandlerFn, CustomEventHandlerFn, HookHandlerFn,
        HttpHandlerFn, ToolHandlerFn, WorkerCallContext, WorkerCommandContext,
        WorkerCustomEventContext, WorkerInvocationContext,
    },
};

/// 从 tool 事件 JSON 反序列化 LLM 传入的 `arguments`。
pub fn parse_tool_arguments<T: DeserializeOwned>(event: &Value) -> Result<T, ErrorPayload> {
    let args = event
        .get("input")
        .and_then(|i| i.get("arguments"))
        .or_else(|| event.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    serde_json::from_value(args).map_err(|e| {
        ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("parse tool arguments: {e}"),
        )
    })
}

/// 从 hook 事件 JSON 反序列化 `input` 载荷。
pub fn parse_hook_input<T: DeserializeOwned>(event: &Value) -> Result<T, ErrorPayload> {
    let input = event.get("input").cloned().unwrap_or_else(|| event.clone());
    serde_json::from_value(input).map_err(|e| {
        ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("parse hook input: {e}"),
        )
    })
}

/// 无参 tool handler：`async move |ctx| { ... }`。
pub fn tool_handler<F, Fut>(f: F) -> ToolHandlerFn
where
    F: Fn(WorkerInvocationContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// 带反序列化参数的 tool handler：`async move |args, ctx| { ... }`。
pub fn tool_handler_args<A, F, Fut>(f: F) -> ToolHandlerFn
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A, WorkerInvocationContext) -> Fut + Send + Sync + 'static,
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
    F: Fn(WorkerInvocationContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// 带反序列化 hook input 的 handler。
pub fn hook_handler_args<A, F, Fut>(f: F) -> HookHandlerFn
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A, WorkerInvocationContext) -> Fut + Send + Sync + 'static,
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
    F: Fn(WorkerCommandContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// Handler for a continuation emitted by another worker handler.
pub fn continuation_handler<F, Fut>(f: F) -> ContinuationHandlerFn
where
    F: Fn(WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// Typed-input handler for a continuation emitted by another worker handler.
pub fn continuation_handler_args<A, F, Fut>(f: F) -> ContinuationHandlerFn
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A, WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |event, ctx| match parse_hook_input::<A>(&event) {
        Err(error) => Box::pin(async move { Err(error) }),
        Ok(input) => Box::pin(f(input, ctx)),
    })
}

/// Handler for a session-scoped custom-event delivery.
pub fn custom_event_handler<F, Fut>(f: F) -> CustomEventHandlerFn
where
    F: Fn(WorkerCustomEventContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// Typed-input handler for a session-scoped custom-event delivery.
pub fn custom_event_handler_args<A, F, Fut>(f: F) -> CustomEventHandlerFn
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A, WorkerCustomEventContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HandlerResult, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |event, ctx| match parse_hook_input::<A>(&event) {
        Err(error) => Box::pin(async move { Err(error) }),
        Ok(input) => Box::pin(f(input, ctx)),
    })
}

/// 类型化 HTTP handler。
pub fn http_handler<F, Fut>(f: F) -> HttpHandlerFn
where
    F: Fn(ExtensionHttpRequest, WorkerCallContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExtensionHttpResponse, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |request, ctx| Box::pin(f(request, ctx)))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::s5r::HandlerEffect;

    #[derive(Deserialize)]
    struct GreetArgs {
        name: String,
    }

    #[tokio::test]
    async fn typed_tool_handler_preserves_arguments_and_call_facts() {
        let handler = tool_handler_args(|args: GreetArgs, ctx| async move {
            assert_eq!(ctx.extension_id(), "ext");
            assert_eq!(ctx.session_id(), "session-1");
            assert_eq!(ctx.turn_id(), None);
            assert_eq!(ctx.tool_call_id(), Some("tool-call-1"));
            assert_eq!(ctx.working_dir(), std::path::Path::new("/workspace"));
            assert!(!ctx.cancel_token().is_cancelled());
            Ok(HandlerResult::effect(
                HandlerEffect::Ok,
                serde_json::json!({ "content": format!("hi {}", args.name) }),
            ))
        });
        let event = serde_json::json!({
            "input": {
                "arguments": { "name": "world" },
                "session_id": "session-1",
                "tool_call_id": "tool-call-1",
                "working_dir": "/workspace"
            }
        });
        let ctx = crate::worker::registry::WorkerCallFacts::from_event(
            "ext".into(),
            crate::worker::CancelToken::default(),
            &event,
        )
        .unwrap()
        .into_invocation("tool")
        .unwrap();
        let out = handler(event, ctx).await.unwrap();
        assert_eq!(out.effect, HandlerEffect::Ok);
        assert_eq!(
            out.data_value("content"),
            Some(&serde_json::json!("hi world"))
        );

        let hook_event = serde_json::json!({ "input": {
            "tool_call_id": "hook-tool-call-1",
            "session_id": "session-1",
            "working_dir": "/workspace"
        }});
        let hook_ctx = crate::worker::registry::WorkerCallFacts::from_event(
            "ext".into(),
            crate::worker::CancelToken::default(),
            &hook_event,
        )
        .unwrap()
        .into_invocation("hook")
        .unwrap();
        assert_eq!(hook_ctx.tool_call_id(), Some("hook-tool-call-1"));
    }
}
