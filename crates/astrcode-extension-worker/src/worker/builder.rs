//! Worker handler 注册辅助：减少闭包样板、支持类型化参数。

use std::{future::Future, sync::Arc};

use astrcode_extension_sdk::s5r::hooks::{
    ContinueAfterStopHookInput, PostCompactHookInput, PostToolUseHookInput, PreCompactHookInput,
    PromptBuildHookInput, ProviderContributionHookInput, ProviderHookInput, ToolUseHookInput,
    prompt_contributions_to_wire, provider_contribution_to_wire,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    WireErrorCode,
    extension::{
        ContinueAfterStopResult, ExtensionHttpRequest, ExtensionHttpResponse, PostToolUseResult,
        PreCompactResult, PreToolUseResult, PromptContributions, ProviderResult,
        ToolInputTransformResult,
    },
    wire::{ErrorPayload, HandlerResult, ProviderContributionData},
    worker::registry::{
        CommandHandlerFn, ContinuationHandlerFn, CustomEventHandlerFn, HookHandlerFn,
        HttpHandlerFn, ToolHandlerFn, ToolPlannerFn, WorkerCallContext, WorkerCommandContext,
        WorkerCustomEventContext, WorkerInvocationContext, WorkerToolPlanContext,
    },
};

/// 反序列化 S5R tool invocation 已校验过的 `arguments`。
pub fn parse_tool_arguments<T: DeserializeOwned>(arguments: Value) -> Result<T, ErrorPayload> {
    serde_json::from_value(arguments).map_err(|e| {
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
    Arc::new(move |event, ctx| match parse_tool_arguments::<A>(event) {
        Err(e) => Box::pin(async move { Err(e) }),
        Ok(args) => Box::pin(f(args, ctx)),
    })
}

/// Side-effect-free tool planner.
pub fn tool_planner<F, Fut>(f: F) -> ToolPlannerFn
where
    F: Fn(WorkerToolPlanContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<crate::tool::ToolPlan, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |_event, ctx| Box::pin(f(ctx)))
}

/// Side-effect-free tool planner with typed arguments.
pub fn tool_planner_args<A, F, Fut>(f: F) -> ToolPlannerFn
where
    A: DeserializeOwned + Send + 'static,
    F: Fn(A, WorkerToolPlanContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<crate::tool::ToolPlan, ErrorPayload>> + Send + 'static,
{
    Arc::new(move |event, ctx| match parse_tool_arguments::<A>(event) {
        Err(error) => Box::pin(async move { Err(error) }),
        Ok(arguments) => Box::pin(f(arguments, ctx)),
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
    Arc::new(move |ctx| Box::pin(f(ctx)))
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

/// 生成固定输入/输出类型的 hook handler 构造器:反序列化宿主 hook 载荷,
/// 并把 SDK hook 结果枚举映射为宿主可解析的 wire `HandlerResult`。
macro_rules! typed_hook_handler {
    ($(#[$meta:meta])* $name:ident, $input:ty, $output:ty, |$result:ident| $convert:expr) => {
        $(#[$meta])*
        pub fn $name<F, Fut>(f: F) -> HookHandlerFn
        where
            F: Fn($input, WorkerInvocationContext) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = Result<$output, ErrorPayload>> + Send + 'static,
        {
            Arc::new(move |event, ctx| match parse_hook_input::<$input>(&event) {
                Err(error) => Box::pin(async move { Err(error) }),
                Ok(input) => {
                    let future = f(input, ctx);
                    Box::pin(async move {
                        let $result = future.await?;
                        $convert
                    })
                },
            })
        }
    };
}

typed_hook_handler!(
    /// 类型化 `pre_tool_use` 准入 hook handler。
    pre_tool_use_handler,
    ToolUseHookInput,
    PreToolUseResult,
    |result| Ok(HandlerResult::from(result))
);

typed_hook_handler!(
    /// 类型化 `tool_input_transform` hook handler。
    tool_input_transform_handler,
    ToolUseHookInput,
    ToolInputTransformResult,
    |result| Ok(HandlerResult::from(result))
);

typed_hook_handler!(
    /// 类型化 `post_tool_use` hook handler;与 `Worker::hook(LifecycleEvent::PostToolUse, ..)` 组合使用。
    post_tool_use_handler,
    PostToolUseHookInput,
    PostToolUseResult,
    |result| Ok(HandlerResult::from(result))
);

typed_hook_handler!(
    /// 类型化 provider hook handler,适用于 `before_provider_request` 与 `after_provider_response`。
    provider_handler,
    ProviderHookInput,
    ProviderResult,
    |result| Ok(HandlerResult::from(result))
);

typed_hook_handler!(
    /// 类型化 `provider_contribution` hook handler;`None` 表示本请求无贡献,
    /// `acknowledge` 阶段的返回值被忽略并固定应答 `ok`。
    provider_contribution_handler,
    ProviderContributionHookInput,
    Option<ProviderContributionData>,
    |result| provider_contribution_to_wire(result)
);

typed_hook_handler!(
    /// 类型化 `continue_after_stop` hook handler。
    continue_after_stop_handler,
    ContinueAfterStopHookInput,
    ContinueAfterStopResult,
    |result| Ok(HandlerResult::from(result))
);

typed_hook_handler!(
    /// 类型化 `prompt_build` hook handler;空贡献自动折叠为 `ok`。
    prompt_build_handler,
    PromptBuildHookInput,
    PromptContributions,
    |result| prompt_contributions_to_wire(result)
);

typed_hook_handler!(
    /// 类型化 `pre_compact` hook handler;`Block` 在 S5R 上不受支持,会被提前拒绝。
    pre_compact_handler,
    PreCompactHookInput,
    PreCompactResult,
    |result| HandlerResult::try_from(result)
);

typed_hook_handler!(
    /// 类型化 `post_compact` 通知 hook handler;返回值被忽略并固定应答 `ok`。
    post_compact_handler,
    PostCompactHookInput,
    (),
    |result| {
        let () = result;
        Ok(HandlerResult::ok())
    }
);

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::wire::HandlerEffect;
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Deserialize)]
    struct GreetArgs {
        name: String,
    }

    #[tokio::test]
    async fn typed_pre_tool_use_handler_parses_input_and_maps_decision() {
        let handler = pre_tool_use_handler(|input: ToolUseHookInput, ctx| async move {
            assert_eq!(input.tool_name, "shell");
            assert_eq!(ctx.session_id(), "session-1");
            Ok(PreToolUseResult::Block {
                reason: format!("blocked {}", input.tool_name),
            })
        });
        let event = || {
            json!({ "input": {
                "session_id": "session-1",
                "working_dir": "/workspace",
                "model": { "profile_name": "default", "model": "m", "provider_kind": "openai" },
                "tool_call_id": "call-1",
                "tool_name": "shell",
                "tool_input": { "command": "rm -rf /" },
                "available_tools": []
            }})
        };
        let make_ctx = || {
            crate::worker::registry::WorkerCallFacts::from_event(
                "ext".into(),
                crate::worker::CancelToken::default(),
                &event(),
            )
            .unwrap()
            .into_invocation("hook")
            .unwrap()
        };

        let result = handler(event(), make_ctx()).await.unwrap();
        assert_eq!(result.effect, HandlerEffect::Block);
        assert_eq!(result.data["reason"], json!("blocked shell"));

        // 缺字段的载荷在反序列化处失败,不会进入 handler。
        let error = handler(json!({ "input": {} }), make_ctx())
            .await
            .unwrap_err();
        assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidInput));
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
        let out = handler(serde_json::json!({ "name": "world" }), ctx)
            .await
            .unwrap();
        assert_eq!(out.effect, HandlerEffect::Ok);
        assert_eq!(
            out.data.get("content"),
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
