# Extension 系统

扩展是 astrcode 的核心可扩展性机制。Skills、Agent Profiles、自定义工具、slash 命令和生命周期 hook 都应该通过扩展接入，而不是塞进 agent loop。

## 当前运行时边界

- 扩展在 server bootstrap 时从 `~/.astrcode/extensions/` 和当前工作区 `.astrcode/extensions/` 加载。
- 项目级扩展排在全局扩展前面执行。
- 原生 `.dll` / `.so` 扩展默认禁用；必须设置 `ASTRCODE_ENABLE_NATIVE_EXTENSIONS=1` 才会加载。
- 原生扩展是进程内代码，和宿主同信任边界；JSON-RPC / out-of-process 插件协议保留为后续边界，不在当前实现里混用。

## 生命周期事件（9 个）

| 级别 | 事件 | 说明 |
|------|------|------|
| Session | `SessionStart` | 会话/运行时启动时 |
| Session | `SessionShutdown` | 会话/运行时关闭时 |
| Turn | `TurnStart` | Turn 开始时 |
| Turn | `TurnEnd` | Turn 结束时 |
| Tool | `PreToolUse` | 工具调用前，可阻断或修改输入 |
| Tool | `PostToolUse` | 工具调用后，可修改结果 |
| Provider | `BeforeProviderRequest` | 请求 LLM 前，可修改消息 |
| Provider | `AfterProviderResponse` | LLM 响应后观察点 |
| Input | `UserPromptSubmit` | 用户提交 prompt 后、组装消息前 |

## HookMode

- `Blocking`: 同步执行，可 `Allow`、`Block`，也可以返回当前事件允许的修改效果。
- `NonBlocking`: fire-and-forget，只做日志、分析、通知等观察性工作，不影响主流程。
- `Advisory`: 同步执行但结果只作参考，当前不会改变主流程。

同一事件有多个 Blocking hook 时：第一个 `Block` 立即停止；多个修改效果按执行顺序覆盖，最后一个适用修改生效。

## 工具扩展

扩展注册工具需要两部分：

1. `ToolDefinition`: 暴露给 LLM 的名称、描述和 JSON Schema。
2. Tool handler: 当 LLM 调用该工具时真正执行逻辑。

Rust 扩展实现 `Extension::tools()` 返回定义，并覆盖 `Extension::execute_tool()` 执行工具。原生扩展通过 FFI 调用 `register_tool()` 注册定义，再调用 `register_tool_handler()` 绑定执行回调。

工具执行进入 session 级 `ToolRegistry`。扩展工具会被 `ExtensionRunner`
包装成普通 `Tool` trait object，因此会和内置工具走同一条
`PreToolUse -> execute -> PostToolUse` pipeline。

更完整的 tool / extension 边界见 [Tool Loading Boundary](tool-loading.md)。

## Manifest

`extension.json` 当前支持这些字段：

```json
{
  "id": "my-extension",
  "name": "My Extension",
  "version": "0.1.0",
  "description": "Optional human-readable description",
  "astrcode_version": "0.1.0",
  "library": "my_extension.dll",
  "subscriptions": [
    { "event": "session_start", "mode": "blocking" }
  ],
  "tools": []
}
```

`version`、`description`、`astrcode_version` 是诊断/展示元数据；当前只有 `id`、`name`、`library` 是加载必需项。

## 简单 Rust 扩展示意

```rust
use astrcode_core::tool::{ToolDefinition, ToolOrigin, ToolResult};

struct EchoExtension;

#[async_trait::async_trait]
impl Extension for EchoExtension {
    fn id(&self) -> &str { "echo" }

    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> { vec![] }

    async fn on_event(
        &self,
        _event: ExtensionEvent,
        _ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        Ok(HookEffect::Allow)
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "echo".into(),
            description: "Echo text".into(),
            parameters: serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}}),
            origin: ToolOrigin::Extension,
        }]
    }

    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        _working_dir: &str,
        _ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != "echo" {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        Ok(ToolResult {
            call_id: String::new(),
            content: arguments["text"].as_str().unwrap_or("").to_string(),
            is_error: false,
            error: None,
            metadata: Default::default(),
            duration_ms: None,
        })
    }
}
```
