# astrcode-s5r (Python)

astrcode **S5R 3.0** 扩展线缆协议的薄 Python SDK:仅标准库,Python ≥ 3.10,零依赖。
语义以 Rust 参考实现(`crates/astrcode-extension-worker` 与
`crates/astrcode-extension-sdk::wire`)为准;协议文档见
[`docs/s5r-protocol.md`](../../docs/s5r-protocol.md)。

- 传输:子进程 stdio,帧格式 `"<decimal length>\n<JSON>"`(上限 16 MiB,header 32 字节,
  严格校验);stdout 只写协议帧,日志写 stderr。
- 握手:Host `initialize`(版本/身份/feature/operation 目录校验)→ Worker 返回
  `InitializeManifest` → Host `activate` → 双向 `invoke`。
- 严格解析:envelope 与嵌套 payload 拒绝未知字段;未知错误码无损透传。

## 快速上手

```python
from s5r import ToolDefinition, Worker, tool_text

worker = Worker("my-extension", "0.1.0")

@worker.tool(ToolDefinition(
    name="echo",
    description="Echo the input text",
    parameters={
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
    },
))
async def echo(arguments, ctx):
    return tool_text(str(arguments["text"]))

if __name__ == "__main__":
    worker.run_stdio()
```

完整可运行示例见 [`examples/echo_extension.py`](examples/echo_extension.py)。
协议错误(版本不符、帧畸形等)会以未捕获异常使进程非零退出,这正是宿主期望的行为;
EOF(stdin 关闭)时干净退出。

### extension.json

```json
{
  "extension_id": "my-extension",
  "protocol": { "s5r": "3.0" },
  "command": ["python3", "/abs/path/to/echo_extension.py"],
  "env": { "PYTHONPATH": "/abs/path/to/sdks/python/src" }
}
```

也可以 `pip install` 本目录(无第三方依赖),此时无需 `PYTHONPATH`。

## API 概览

### Worker(注册面,对标 `astrcode_extension_worker::worker::Worker`)

| 方法 | 说明 |
|---|---|
| `Worker(extension_id, version)` | extension_id 必须与 extension.json 一致 |
| `tool(definition, handler, planner=None)` | 注册 tool;planner 默认空 `ToolPlan`;可作装饰器 |
| `hook(on, mode, handler)` | 模式可选的 lifecycle hook(带 Rust 同规则校验) |
| `on_pre_tool_use` / `on_tool_input_transform` / `on_prompt_build` / `on_provider_contribution` | 固定 blocking hook |
| `on_after_provider_response` | 固定 advisory hook |
| `on_pre_compact` / `on_post_compact` | compact hook(blocking) |
| `on_continue_after_stop(max_per_turn, handler)` | `-1` 表示无限 |
| 以上 hook 方法的 keyword-only `priority` | 跨扩展调度优先级(非负整数,缺省 0;宿主降序调度,同级保持注册顺序) |
| `continuation_hook_handler(on, handler)` | 仅由 hook continuation 调用 |
| `command(SlashCommand(...), handler)` | slash command(execute + completion) |
| `custom_event(...)` / `on_custom_event(...)` | custom event 声明与订阅 |
| `http_route(ExtensionHttpRoute..., handler)` | HTTP route:manifest 声明与 handler 同源(含路径/冲突校验);handler 收 `ExtensionHttpRequest` 字典与 `WorkerCallContext`,返回 `{"status", "body"}` |
| `capability(...)` / `require_transport(...)` | manifest 能力声明 |
| `on_activate(handler)` | activate 配置处理 |
| `on_shutdown(handler)` | 驱动结束后的 best-effort 清理(仅 activation 成功后执行;hook 内 `HostClient` 不可用) |
| `background_host()` | activate 完成后交付 `BackgroundHost` 的 future;无 turn 作用域,仅 root session 域 |
| `run_stdio()` / `await serve(transport)` | 入口;后者便于测试接入自定义 transport |

Handler 返回 `HandlerResult`(`HandlerResult.ok()`、`HandlerResult.of(effect, data)`、
`tool_text(content, is_error)`);planner 返回 `ToolPlan([ResourceAccess...])`。

### HostClient(handler 内调用宿主,对标 `HostClient`)

域方法:`events()`、`models()`(unary / `*_events` 流式 / `*_collected`)、
`session_control()`、`session_history()`、`session_state()`、`session_inspect()`、
`workspace()`、`tool_results()`、`process()`、`network()`、`extension_http()`,
以及 `HostClient.host_supports(HostOperation.X)` 本地预检。

请求/响应是共享 wire DTO 的 JSON 字典(见 `crates/astrcode-extension-sdk/src/wire/host/`),
SDK 不重复声明类型。Host API 通过 `contextvars` 绑定到当前入站调用(对标 Rust 的
task-local `with_host_api`);在 handler 外调用会抛 `context_unavailable`。

```python
state = await HostClient.session_state().read({"key": "goal"})
output = await HostClient.models().main_chat_collected(
    {"messages": [{"role": "user", "content": "hi"}]}
)
```

`session_control()` 还包含 turn 级输入控制:`queue_or_start`(running 时 FIFO 排队、
turn 结束自动开新 turn;idle 直接开新 turn)与 `defer_context`(仅在有活跃 turn 时于
下一 step 边界注入,不唤醒;无活跃 turn 时返回 `no_active_turn`)。两者请求/响应复用
`inject_or_start` 的 `HostSessionInputRequest`/`HostSessionDeliveryOutput` 形状
(`{"target_session_id", "content"}` → `{"status": "started"|"injected"|"queued", ...}`)。
handler 内可用 `ctx.defer_context(content)` 快捷形式,自动以当前 invocation 的
session 为 target(对标 Rust 的 `WorkerInvocationContext::defer_context`)。

### 参数解析与错误

- `parse_tool_arguments(arguments, Dataclass)` / `parse_hook_input(event, Dataclass)`。
- handler 抛 `S5rError(ErrorPayload(code, message))` 即结构化失败;`WireErrorCode`
  收录全部已知错误码(含 `NO_ACTIVE_TURN = "no_active_turn"`),未知 code 字符串无损透传。
- `ctx.cancel_token`:`is_cancelled()` / `reason` / `await wait_cancelled()`。

## 协议覆盖

已覆盖:长度前缀帧(含畸形/超限拒绝)、initialize/activate 与 feature negotiation、
`handler.invoke` 的 tool(plan/execute 两阶段)/hook/command/custom-event/HTTP route 分发、
双向 invoke(含 `parent_invoke_id` nested 校验)、model stream 接收(事件顺序校验)、
双向 cancel(墓碑吸收迟到终态)、`s5r.runtime.ping` 与全部 `s5r.conformance.*` 内建操作
(与 Rust worker 相同)。

后台能力:`on_shutdown` 清理 hook(driver 结束后执行,HostClient 不可用)与
`background_host()`(activate 后交付无 turn 作用域的 `BackgroundHost`,仅 root session 域:
`create_root` / `submit_root_turn` / `root_state` / `dispose_root`,invoke 不带
`parent_invoke_id`,宿主按 detached context 处理)。

## 测试

```bash
cd sdks/python
PYTHONPATH=src python3 -m unittest discover -s tests
```

## Conformance 验收

Rust 侧的线缆验收二进制可以直接对本 SDK 的 worker 运行(initialize/activate、unary、
stream、cancel、nested invoke、未知错误码透传、clean shutdown、畸形/超大帧拒绝):

```bash
# 在仓库根目录执行(已验证通过:输出 "S5R 3.0 conformance passed")
PYTHONPATH=sdks/python/src cargo run -p astrcode-extension-sdk --features conformance \
  --bin s5r-conformance -- \
  --extension-id s5r-echo-example -- python3 sdks/python/examples/echo_extension.py
```

拒绝探针会向 stderr 打印 traceback 并以非零码退出,这是预期行为(协议规定 stdout
只承载协议帧,日志走 stderr)。
