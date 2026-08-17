# 磁盘扩展作者入门指南

面向通过 **s5r 子进程** 接入 AstrCode 的扩展作者（非进程内 bundled 扩展）。

## 我该用哪个 prelude？

| 模块 | 运行边界 | 注册与上下文 | 宿主调用与错误 |
|------|----------|--------------|----------------|
| `astrcode_extension_sdk::prelude` | 仓库内、**进程内** bundled Rust 扩展 | 实现 `Extension`；用 `manifest()` + `Registrar`；handler 接收宿主构造的 `ToolContext` / hook context | `ctx.host()` 返回 owned typed domain clients；返回 `ExtensionError` / lossless `HostError` |
| `astrcode_extension_worker::worker_prelude` | `extension.json` 启动的**磁盘**独立进程 | `Worker` 同时生成握手 manifest 与 handler registry；handler 接收按入口拆分的强类型 context | task-local 静态 `HostClient` 经 S5R invoke；返回 `ErrorPayload` |

边界由 SDK re-export 强制：bundled prelude 不导出 `Worker`、静态 `HostClient` 或 S5R wire 类型；
worker prelude 不导出 `Extension`、`Registrar` 或 bundled 的生产 context。共享 DTO 从 SDK 稳定模块
re-export，但两套 runtime 入口、context 和错误返回类型不能互换。

测试入口也分开：bundled 使用 `astrcode_extension_sdk::testing` 的 context builders、
`MockExtensionHost`、`RegistrationHarness` 与 `ExtensionLifecycleHarness`；worker 单元测试通过
`astrcode_extension_worker::testing::with_host_api` 在异步作用域内注入 `HostApi`，协议验收使用真实子进程 E2E。
两者都只在对应 crate 的 `testing` feature 开启时导出，生产依赖无需携带测试构造入口。

## 最小示例

```rust
use astrcode_extension_worker::worker_prelude::*;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("extension failed: {} ({})", e.message, e.code);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ErrorPayload> {
    let mut worker = Worker::new("my-ext", "0.1.0");

    worker.tool(
        tool("ping")
            .description("Returns pong")
            .parameters(serde_json::json!({"type": "object"}))
            .build(),
        tool_planner(|_| async { Ok(ToolPlan::default()) }),
        tool_handler(|_ctx| async { Ok(tool_text("pong", false)) }),
    )?;

    worker.run_stdio().await
}
```

`extension.json` 只负责**发现与启动**，工具/钩子定义在代码里注册并自动生成握手 manifest：

```json
{
  "extension_id": "my-ext",
  "protocol": { "s5r": "3.0" },
  "command": ["./my-ext"]
}
```

## 为什么 manifest 与 handler 要一起注册？

旧写法在 JSON `manifest()` 里写一遍 `tools`，又在 `register_tool("ping", ...)` 注册一遍，名称不一致时**静默失败**（宿主有工具、子进程无 handler）。

现用 `worker.tool(def, planner, handler)`：**同一次调用**写入 manifest、纯资源规划器与执行
handler。planner 不是可选兼容钩子；每一次工具调用都固定经过“最终参数 → plan → 权限 →
resource lease → execute”。

SDK builder 创建的工具默认不启用 provider strict。只有确认 Schema 满足所有目标 provider 的
strict JSON Schema 子集时才调用 `.strict()`：

```rust
tool("ping")
    .description("Returns pong")
    .parameters(serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }))
    .strict()
    .build()
```

宿主会将 strict 声明写入 S5R manifest，并在 profile 同时声明 `supportsStrictToolUse` 时为 OpenAI
或 Anthropic 编译对应的 strict Schema；工具定义本身不需要复制两份 provider 专用 Schema。
Provider strict 只支持完整 JSON Schema 的子集，而且不同 provider 的子集并不相同；例如
`oneOf`、`patternProperties` 等关键字不能假定跨 provider 可用。显式 strict 但无法安全编译的
结构会在发请求前返回带工具名与 Schema 路径的本地错误，避免悄悄削弱扩展作者声明的契约。
MCP 工具保持 non-strict；Anthropic 达到单次请求 strict 聚合上限时，溢出工具只在 wire 副本上
确定性降级并记录 warning。

## 类型化参数

避免手写 `event["input"]["arguments"]["name"]`：

```rust
use serde::Deserialize;

#[derive(Deserialize)]
struct GreetArgs { name: String }

worker.tool(
    tool("greet").description("Greet").parameters(/* JSON Schema */).build(),
    tool_planner_args(|_args: GreetArgs, _ctx| async move {
        Ok(ToolPlan::default())
    }),
    tool_handler_args(|args: GreetArgs, _ctx| async move {
        Ok(tool_text(format!("hello, {}!", args.name), false))
    }),
)?;
```

## 资源规划不是权限检查

planner 与 execute 接收同一份已经完成 JSON repair 的最终参数。planner 只能把参数解释为
`ToolPlan`，不能调用 `HostClient`、写事件、创建任务或访问扩展持久目录；worker runtime 在 plan
阶段不会安装 Host API。路径型资源必须使用 `ResourceAccess::{read_file, search_file,
write_file, read_write_file}`，非文件 Host 能力使用 `ToolPlan::host(HostResource::...)`。只有不经过
Host、无法由 lease 强制约束的外部副作用才使用 `ToolPlan::opaque()`；它会要求显式审批，但不会
因此获得任意 Host capability。

```rust
#[derive(Deserialize)]
struct SaveArgs { path: String, content: String }

worker.capability(ExtensionCapability::WorkspaceWrite);
worker.tool(
    tool("save").description("Save text").parameters(/* strict schema */).build(),
    tool_planner_args(|args: SaveArgs, ctx| async move {
        let path = ctx.working_dir().join(args.path);
        Ok(ToolPlan::new([ResourceAccess::write_file(path)]))
    }),
    tool_handler_args(|args: SaveArgs, _ctx| async move {
        let output = HostClient::workspace().write(HostWorkspaceWriteRequest {
            path: args.path,
            content: args.content,
            create_dirs: false,
        }).await?;
        Ok(tool_text(format!("wrote {} bytes", output.bytes_written), false))
    }),
)?;
```

Host 不信任 planner 的声明：session 用 plan 做权限决策并签发不可由扩展构造的 lease；真正的
workspace/process/network/session/model/event operation 在 HostRouter 再按 lease 校验具体访问。
声明不足会在执行边界失败，声明过宽则会触发更宽权限，因此 planner 应精确、确定且无副作用。

钩子同理：`hook_handler_args` + `parse_hook_input`（反序列化 `event["input"]`）。
固定模式 hook 有类型化构造器,输入是宿主载荷的强类型镜像、输出是 SDK 的 hook 结果枚举,
优先于手写 JSON:`pre_tool_use_handler`、`tool_input_transform_handler`、
`post_tool_use_handler`、`provider_handler`、`provider_contribution_handler`、
`continue_after_stop_handler`、`prompt_build_handler`、`pre_compact_handler`、
`post_compact_handler`(输入 DTO 见 `astrcode_extension_sdk::s5r::hooks`)。注意
`PreCompactResult::Block` 在 S5R 上不受支持,类型化构造器会提前拒绝。

## 调用宿主能力

本节的静态 `HostClient` 只属于 `worker_prelude`，依赖当前 S5R handler 的 task-local 调用范围。
bundled 扩展不要调用它；bundled handler 应从 `ctx.host()` 取得 `ModelClient`、
`SessionControlClient`、`WorkspaceClient` 等 owned domain client。

主模型与小模型分别声明、分别调用：

```rust
use astrcode_extension_worker::worker_prelude::*;

// 主模型（当前 session 的 activeModel）
worker.capability(ExtensionCapability::MainModel);
let out = HostClient::models().main_chat(vec![
    LlmMessage::user("summarize this"),
]).await?;

// 小模型（activeSmallModel）
worker.capability(ExtensionCapability::SmallModel);
let out = HostClient::models().small_chat(vec![
    LlmMessage::user("tag this line"),
]).await?;

// 只限制本次模型生成；默认 main_chat/small_chat 不覆盖 provider 上限
let request = llm_chat_request(vec![
    LlmMessage::user("summarize in at most 512 tokens"),
]).with_max_output_tokens(512);
let out = HostClient::models().small_chat_request(request).await?;

// 受限子进程（总超时包含并发排队，cwd 必须在 workspace 内）
worker.capability(ExtensionCapability::ProcessSpawn);
let output = HostClient::process().spawn(HostProcessRequest::new("rustc")).await?;

// 受限出站 HTTP(S)
worker.capability(ExtensionCapability::NetworkClient);
let response = HostClient::network().send(
    HostNetworkRequest::get("https://example.com")
).await?;
// response.final_url 是完成受限重定向后的地址。

// 创建或精确编辑工作区内的非敏感文件
worker.capability(ExtensionCapability::WorkspaceWrite);
let written = HostClient::workspace().write(HostWorkspaceWriteRequest {
    path: "notes/result.txt".into(),
    content: "done".into(),
    create_dirs: true,
}).await?;

// 跨会话检查（返回 SDK 定义的稳定 DTO，不暴露内部 SessionReadModel）
worker.capability(ExtensionCapability::SessionInspect);
let sessions = HostClient::session_inspect().list().await?;
let model = HostClient::session_inspect()
    .read_model(&sessions.sessions[0].session_id)
    .await?;

// 创建归属本扩展的独立 root session；省略 working_dir 时回退到调用上下文的工作目录
worker.capability(ExtensionCapability::InputDelivery);
let root = HostClient::session_control()
    .create_root(HostCreateRootSessionRequest::default())
    .await?;

// 注册公开 JSON 路由；路由和 handler 从同一注册调用生成 manifest
worker.capability(ExtensionCapability::PublicHttp);
worker.http_route(
    ExtensionHttpRoute::public(ExtensionHttpMethod::Post, "/my-plugin/{id}"),
    http_handler(|request, _ctx| async move {
        Ok(ExtensionHttpResponse::json(200, serde_json::json!({
            "id": request.path_params.get("id"),
            "body": request.body,
        })))
    }),
)?;

// 调用另一插件的公开路由
worker.capability(ExtensionCapability::PublicHttpDispatch);
let response = HostClient::extension_http().dispatch_public(
    ExtensionHttpDispatchRequest::new(ExtensionHttpMethod::Post, "/other-plugin/run")
        .json_body(serde_json::json!({ "job": 1 }))
).await?;
```

`max_output_tokens` 必须大于 0；Host 把它作为本次 provider request 的上限，provider 仍会将其
限制在模型配置的最大输出内。这是模型生成预算，不是 tool result 的字节截断参数。

`create_root` 的 `working_dir` 显式指定时必须是已存在的目录（宿主做 canonicalize 与存在性
校验）；该 root session 内的宿主工具将在指定目录运行，**超出 workspace 圈禁**，宿主会记录
`(extension_id, working_dir)` 归因日志。只在确需驱动其它目录的顶层会话时显式指定。

在具有 turn attribution 的 hook 或 tool handler 中，`ModelClient` 使用 Session 为该 turn 固定的
main/small provider binding；运行中 reload 不会把旧 turn 切到新 provider。只有 startup 或其他
明确没有 turn attribution 的调用使用 live fallback。扩展不应缓存或自行解析 provider handle。

`network.client` 的作者 API 以原始字节返回响应 body，S5R 线缆使用 base64。请求的
body 解码后不得超过 10 MiB，`max_bytes` 不得超过 10 MiB，`timeout_ms` 必须位于
`1..=60_000`；`Manual` 不跟随重定向，
但仍会返回受 `max_bytes` 限制的原始 3xx 响应 body。同名响应头的重复值不会保留。worker 与
web-tools 共享宿主的全局并发上限，当前协议不提供 extension 级公平配额。
进程请求的 `stdin` 以 UTF-8 字节计最多 1 MiB。

`workspace_write`、`process_spawn` 与 `network_client` 都是敏感授权；只在插件确实需要时声明。
前者拒绝越界、symlink 和密钥类路径；进程能力只提供 Host-supervised stdin/stdout/stderr pipes，
Unix 由 process group、Windows 由 Job Object 管理进程树，不提供 PTY 或 resize，也不是操作系统
sandbox；后者允许访问宿主网络可达的 HTTP(S) 地址。这些能力均有并发、总超时和 I/O 大小限制，
并响应会话取消。

S5R 同时支持 `public_http` 和按扩展 id 隔离的 `authenticated_http`（兼容保留名称，当前
HTTP 运输不校验 bearer token）；两者都不能
注册在 `/api` 下。s5r 工具默认串行；显式声明 `ExecutionMode::Parallel` 时，宿主会在同一
worker 内启用最多 8 个并行调用，并按 request id 隔离 session/working directory 上下文。
S5R 3.0 初始化会协商 `nested_invoke_v1`、`model_stream_v1` 与 `custom_event_v1`；宿主只把
归因所需的 `nested_invoke_v1` 设为 required。流式调用在实际使用时检查
`model_stream_v1`；声明 custom event 或 subscription 的 manifest 必须协商
`custom_event_v1`，否则宿主在发布注册前拒绝加载。
只有双方 feature 约束满足后 peer 才进入 Initialized；宿主完成全局注册校验并发送 Activate 后
才进入 Ready。Activate 前 Worker 不安装 Host API，也不处理 handler。
嵌套调用通过 `parent_invoke_id` 关联父请求，并继承父请求的取消与授权上下文。
`HostClient::host_supports(HostOperation)` 可查询 Host 版本是否实现 operation；typed client 会用
同一目录预检，但查询成功不代表 manifest 已授权、当前上下文完整或 backend 可用。
`public_http_dispatch` 仍拒绝同步调用自己的公开
路由，因为路由和非并行 handler 需要取得顺序执行通道，重入会形成等待环。

作为边界对照，进程内 bundled 工具通过私有字段 `ToolContext` 的 accessor 读取调用事实；这不是
磁盘 worker 可导入或构造的 context：

- `ctx.main_model_id()` — 需声明 `main_model`
- `ctx.small_model_id()` — 需声明 `small_model`
- `ctx.available_tools()` — 当前 turn 可见工具定义
- `ctx.paths()` / `ctx.host()` — 已绑定 extension attribution 的路径与类型化宿主客户端

它不会暴露 core `ToolExecutionContext`、裸 `SessionOperations` 或事件 sink。

bundled host 错误使用 `HostError`：它无损保留 `code` / `message` / `hint` / `retryable` /
`details`，并通过 `class()` 提供常见错误分类。worker 线缆边界继续使用同字段的
`ErrorPayload`；不要在 worker handler 签名中改用 `ExtensionError`。

完整 wire 名与能力对照见 [extension-system.md](extension-system.md)。

## 错误处理

Handler 返回 `Result<HandlerResult, ErrorPayload>`，与宿主侧一致（`code` / `message` / `hint` / `retryable`）：

```rust
use astrcode_extension_sdk::WireErrorCode;

return Err(ErrorPayload::new(WireErrorCode::InvalidInput, "name is required")
    .with_hint("pass {\"name\": \"...\"} in arguments"));
```

`ErrorPayload::with_hint()` 可链式补充可操作建议；可重试错误再使用
`ErrorPayload::retryable(true)` 明确标记。

## 取消

worker 的长时间 tool 应等待或轮询 `WorkerInvocationContext::cancel_token()`；宿主取消经 S5R
`Cancel` 消息传递，并可通过 `cancel_token().reason()` 读取首个非敏感取消原因标识。
bundled handler 则读取 `ctx.cancellation()`，后台循环使用 `ctx.tasks().cancellation()` 或把调用取消
令牌克隆进受管任务。两种 token 来源不能跨 prelude 混用。

## 工具超时

工具执行策略与发送给模型的 `ToolDefinition` 分离。需要限制 `execute` 阶段时，在作者 API 中用
`Duration` 声明预算：

```rust
use std::time::Duration;

worker_tool("deploy")
    .description("Deploy the current release")
    .parameters(json!({"type": "object"}))
    .timeout(Duration::from_secs(600))
    .build()
```

S5R manifest 在线缆边界使用 `timeout_ms`；未声明时沿用 S5R 的 120 秒默认值。bundled 扩展使用
同一个 builder（`astrcode_extension_sdk::builder::tool`），未声明时不增加 tool execute 超时。
`plan` 始终受宿主控制面超时约束；审批与 admission 等待不消耗 execute 预算；主 handler 与全部
continuation 共享同一次预算。超时到达后 LLM 会看到带 `timeoutMs` 元数据的结构化错误结果。

## 结果呈现 intent

工具结果可以声明呈现 intent，让前端和 TUI 选用对应的内置渲染（终端、diff、搜索结果、文件读取），
而不是落到通用渲染。intent 写在 `ToolResult` 的 metadata 里，不进 LLM prompt，也不影响运行时控制流：

```rust
use astrcode_extension_sdk::tool::{ToolPresentation, ToolResult};

// bundled handler 直接返回 ToolResult：
Ok(ToolResult::success(output).with_presentation(ToolPresentation::Terminal))

// worker handler 用 tool_result 携带 metadata 返回：
Ok(tool_result(
    ToolResult::success(output).with_presentation(ToolPresentation::Terminal),
))
```

可选值：`Generic`（默认，等同不声明）、`Terminal`、`Diff`、`Search`、`Read`。无法识别的 intent
字符串按未声明处理，因此旧版本宿主/前端遇到新 intent 时安全回退到通用渲染。声明 intent 只是选择
渲染种类；配套的展示字段（如终端渲染读取的 `command`、`exitCode`）仍按对应内置工具的约定放进
metadata，缺失时渲染组件会降级显示。

## Durable custom event 与幂等

session durable custom event 是 **at-least-once** 投递：handler 成功返回只代表本次副作用完成，
宿主随后还要用 CAS 提交 consumer checkpoint。进程可能恰好在这两步之间崩溃，因此同一个
`event_id` 会再次到达。`consumer_version` 是 consumer key 的一部分；只有在处理语义确实改变、
需要独立 checkpoint 时才递增，不能把它当重试计数。

调用外部 HTTP 时，直接把稳定事件 ID 作为幂等键：

```rust
let event_id = ctx.event_id().to_string();
let response = http_client
    .post("https://example.com/callback")
    .header("Idempotency-Key", event_id)
    .json(ctx.payload())
    .send()
    .await?;
response.error_for_status()?;
```

本地副作用则在同一数据库事务内记录已处理事件；唯一约束负责吸收重投：

```sql
CREATE TABLE processed_extension_events (
    event_id TEXT PRIMARY KEY,
    processed_at TEXT NOT NULL
);

BEGIN;
INSERT INTO processed_extension_events(event_id, processed_at)
VALUES (?1, CURRENT_TIMESTAMP)
ON CONFLICT(event_id) DO NOTHING;
-- 只有 INSERT 实际写入一行时才执行同一事务内的业务变更。
COMMIT;
```

失败会按 250 ms 到 30 s 退避重试；连续第 20 次失败后事件只会被持久化 quarantine/DLQ
一次并推进 checkpoint。人工 skip 也会推进 checkpoint，但必须通过管理入口留下审计记录。
retry 等待不占全局 delivery permit，同一 consumer 仍严格串行，不会跳过前一个事件。
consumer state 保存 quarantine/skip 的单调总数和最近 128 条审计；单条错误文本最多 4 KiB，
因此长期运行不会让控制文件无界增长。状态更新先同步临时文件再原子替换；Unix 还会同步目录元数据。

bundled handler 显式返回 `CustomEventDisposition::Ack`、`::retry(reason)` 或
`::dead_letter(reason)`。worker handler 返回相同 disposition 的 `HandlerResult` 表示：

```rust
Ok(CustomEventDisposition::Ack.into())
// Ok(CustomEventDisposition::retry("upstream unavailable").into())
// Ok(CustomEventDisposition::dead_letter("payload is permanently invalid").into())
```

`Err(...)` 与显式 `Retry` 一样进入重试；`DeadLetter` 必须先持久化 quarantine 并推进 checkpoint
才算消费完成，不能用它掩盖临时故障。

## 调试

- **stderr**：宿主会持续 drain 子进程 stderr 以避免阻塞，但当前不保存或转发这些行；
  调试时不要向 stdout 写日志，stdout 专用于 S5R 帧。
- **握手失败**：检查 `protocol.s5r` 是否为 `3.0`、`extension_id` 是否符合命名规则，以及
  required feature 是否都在双方协商交集中；package manifest 的 `extension_id` 必须与
  Worker 身份一致。注册冲突会发生在 Initialize 之后、Activate 之前；目录名只建议保持一致，
  宿主不把目录名当作身份来源。
- **工具不出现**：确认 `worker.tool()` 已调用且 `run_stdio()` 未提前退出。
- **E2E 参考**：`crates/astrcode-extensions/tests/s5r-guest/`

## 测试 Worker `HostClient`

测试依赖需显式启用 `features = ["testing"]`；该 feature 不应加入生产依赖。

```rust
use std::sync::Arc;
use astrcode_extension_sdk::WireErrorCode;
use astrcode_extension_worker::{
    testing::{HostApi, with_host_api},
    worker_prelude::{ErrorPayload, HostClient, LlmMessage},
};
use serde_json::Value;

struct MockHost;
#[async_trait::async_trait]
impl HostApi for MockHost {
    async fn call(&self, cap: &str, _input: Value) -> Result<Value, ErrorPayload> {
        match cap {
            "astrcode.llm.main_chat" => Ok(serde_json::json!({
                "content": "mocked",
                "model": "test-main"
            })),
            _ => Err(ErrorPayload::new(WireErrorCode::UnknownCapability, cap)),
        }
    }
}

let output = with_host_api(Arc::new(MockHost), async {
    HostClient::models()
        .main_chat(vec![LlmMessage::user("hello")])
        .await
}).await?;
```

单元测试把类型化领域 client 调用包在作用域内；并发测试各自持有 mock，不修改进程级全局状态。
`tokio::spawn` 创建的新任务不会继承这个测试作用域；需要在新任务内再次调用 `with_host_api`。
`HostApi` 和 raw invoke 只是
启用 `testing` feature 后的 `astrcode_extension_worker::testing` transport seam，不在
`worker_prelude` 中。集成测试使用真实子进程 +
`s5r_e2e_test`。

## 进一步阅读

- [s5r-protocol.md](s5r-protocol.md) — 线缆消息与握手方向
- [extension-system.md](extension-system.md) — 宿主加载、能力表、架构

---

## 外置 agent-tool 类插件

内置的 `astrcode-extension-agent-tools` 是**进程内**扩展（`Extension` trait +
`ctx.host().session_control()` 类型化调用）。
若你要做**磁盘外置**、独立二进制分发的 agent 委派插件，走 **s5r Worker**，通过 `HostClient` 调用 `astrcode.session.control.*`。

### 先选路径

| 目标 | 推荐 |
|------|------|
| 与内置 agent-tools 完全等价（同步等待子 Agent、`tool_selection` 禁嵌套 agent 等） | 在仓库内新增 `astrcode-extension-*` **bundled**  crate，用 `prelude` |
| 独立安装包、`extension.json` 启动、用户目录分发 | s5r **Worker** + 下文结构 |
| 仅需「后台派生子 Agent + 完成后通知」 | 外置 Worker **可行**（`wait_for_result: false`） |
| 必须由外置插件**同步等待**长任务结果 | 用 `Worker::background_host()` + root session 域（见下文「后台任务与外置 agent」）；handler 内 `wait_for_result: true` 仍被宿主拒绝 |

### 目录与安装

用户级：

```text
~/.astrcode/extensions/my-agent-tools/
  extension.json
  my-agent-tools.exe    # Windows；Unix 无 .exe
```

项目级：`<repo>/.astrcode/extensions/my-agent-tools/`（同上）。

`extension.json` 声明发现期身份与启动方式，**不要**在这里重复写 tools 列表。`extension_id`
必须与 `Worker` 生成的握手 manifest 一致：

```json
{
  "extension_id": "my-agent-tools",
  "protocol": { "s5r": "3.0" },
  "command": ["C:/path/to/my-agent-tools.exe"]
}
```

### 独立工程骨架

```text
my-agent-tools/
  Cargo.toml
  src/
    main.rs
    agents.rs      # 扫描 ~/.astrcode/agents、.astrcode/agents/*.md
```

`Cargo.toml`：

```toml
[package]
name = "my-agent-tools"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "my-agent-tools"
path = "src/main.rs"

[dependencies]
astrcode-extension-worker = { path = "…/astrcode/crates/astrcode-extension-worker" }
astrcode-extension-sdk = { path = "…/astrcode/crates/astrcode-extension-sdk" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

`astrcode-extension-worker` 提供 `Worker`、`worker_prelude` 与 S5R 运行时，是磁盘扩展的直接依赖；
本示例另从 SDK 的 `tool` 模块导入 `ExecutionMode`，因此同时列出 `astrcode-extension-sdk`。

`Worker::new` 的 id 建议与目录名一致，例如 `"my-agent-tools"`。

### 必须声明的能力与钩子

```rust
let mut worker = Worker::new("my-agent-tools", "0.1.0");
worker
    .capability(ExtensionCapability::SessionControl);

// 向主 Agent 注入 [Agents] 列表（等同内置 on_prompt_build）
worker.on_prompt_build(
    hook_handler(|_ctx| async move {
        let agents_md = discover_agents_markdown(); // 自行扫描 .md
        Ok(HandlerResult::effect(
            "prompt_contributions",
            serde_json::json!({ "agents": [agents_md] }),
        ))
    }),
)?;
```

`Worker::hook(event, mode, handler)` 用于运行时确实支持多种调度方式的 hook：
`advisory` 会按顺序等待但只记录失败，`non_blocking` 由宿主管理后台执行。
`prompt_build`、`pre_compact`、`post_compact`、`after_provider_response` 和
`continue_after_stop` 使用固定模式的 typed `on_*` 方法；宿主会拒绝不受支持的组合。

`prompt_build` 返回 `HandlerEffect::PromptContributions`，宿主会同时校验 effect 和 payload。

### `agent` 工具（核心）

参数 schema 与内置一致（`camelCase`，给 LLM 用）：

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentArgs {
    prompt: String,
    description: String,
    subagent_type: Option<String>,
    #[serde(default = "default_true")]
    wait_for_result: bool,
}
const fn default_true() -> bool { true }
```

注册（**并行**执行，避免阻塞其它 tool）：

```rust
use astrcode_extension_sdk::tool::ExecutionMode;

worker.tool(
    tool("agent")
        .description("Delegate to a subagent…")
        .parameters(/* 与内置 AGENT_TOOL_PARAMETERS 相同 */)
        .execution_mode(ExecutionMode::Parallel)
        .build(),
    tool_handler_args(|args: AgentArgs, ctx| async move {
        run_agent_via_host(&args, &ctx).await
    }),
)?;
```

### 通过 HostClient 派生子会话

`tool_handler_args` 在反序列化 arguments 的同时验证宿主经 `handler.invoke` 传入的调用事实，
tool 和 hook handler 共用 `WorkerInvocationContext`，可直接读取：

- `ctx.session_id()`：当前 session，必定存在；
- `ctx.tool_call_id()`：宿主归属的当前 tool call；没有 call id 时为 `None`；
- `ctx.working_dir()`：宿主校验后的工作目录，必定存在；
- `ctx.turn_id()`：当前 tool call 所属 turn 的真实 ID；宿主没有 turn attribution 时为 `None`。

command 和 custom event 分别使用 `WorkerCommandContext`、
`WorkerCustomEventContext`。缺少入口必需事实时，worker runtime 在作者 handler 运行前返回
`context_unavailable`。HTTP 与 continuation 不承诺 session/workspace，使用只含 extension id 和取消
信号的 `WorkerCallContext`；continuation 应注册 `continuation_handler[_args]`。

创建子会话：

```rust
let parent_session_id = ctx.session_id();
let mut request = HostCreateSessionRequest::new(agent_name);
request.system_prompt = Some(system_prompt);
request.model_preference = Some(model);
request.ephemeral = true;
request.tool_selection = Some(SessionToolSelectionDto::all_except(["agent"]));

let created = HostClient::session_control().create_child(request).await?;
let child_id = created.session_id;
```

子会话始终继承父 session 的 workspace，创建请求不接受独立的工作目录。

`tool_selection` 是子会话工具可见性策略。外置 agent 默认建议使用
`SessionToolSelectionDto::all_except(["agent"])`，避免子 agent 继续嵌套创建 agent；若需要更严格的工具边界，可改用
`SessionToolSelectionDto::only(["tool_a", "tool_b"])` 白名单。

提交 turn（**外置扩展请用异步**，避免 peer 死锁）：

```rust
let mut request = HostSubmitTurnRequest::background(child_id, args.prompt);
request.notify_parent_on_complete = Some(format!(
    "Subagent '{}' finished: {}", agent_name, args.description
));
let submitted = HostClient::session_control().submit_turn(request).await?;
// HostSubmitTurnOutput::Backgrounded { .. } → 返回说明文本给主 Agent
```

若用户传 `waitForResult: true`，外置实现应降级为 `false` 并说明「外置插件仅支持后台子 Agent」，或返回带 hint 的 `ErrorPayload`；需要同步等待结果的整段 pipeline 改用下文 `BackgroundHost` 的 root session 域。

`tool_call_id` 由宿主从当前调用上下文写入 create/submit 的内部请求，worker 不能在 wire 请求中指定或伪造。

### 后台任务与外置 agent（`BackgroundHost`）

`HostClient` 只在 handler 调用作用域内可用（task-local），`tokio::spawn` 创建的任务里调用
会得到 `context_unavailable`。需要在 handler 之外长期驱动宿主的插件（轮询循环、外置 agent
pipeline）使用 `Worker::background_host()`：

```rust
let mut worker = Worker::new("my-agent-tools", "0.1.0");
worker.capability(ExtensionCapability::InputDelivery);

let host_rx = worker.background_host();
tokio::spawn(async move {
    // transport handshake 完成后交付
    let host = host_rx.await.expect("background host handle");
    let sessions = host.root_sessions();
    let root = sessions
        .create_root(HostCreateRootSessionRequest {
            working_dir: Some("/path/to/worktree".into()),
        })
        .await?;
    // 默认 wait_for_result: true,同步等待 turn 完成,结果文本随响应返回;
    // turn 失败时返回 Err(ErrorPayload),其 message 即失败详情
    let output = sessions
        .submit_root_turn(HostRootSubmitTurnRequest::new(&root.session_id, "review"))
        .await?;
    // ... 长 pipeline 循环结束后回收:
    sessions
        .dispose_root(HostSessionTargetRequest {
            target_session_id: root.session_id,
        })
        .await
});
worker.run_stdio().await
```

约束与语义：

- `BackgroundHost` 由根部 peer handle 构造，**结构上不带父调用上下文**；宿主按 detached
  context 处理，要求 Session/Workspace 上下文的操作（child create、workspace 等）失败关闭。
  因此它只暴露 root session 域（`create_root` / `submit_root_turn` / `root_state` /
  `dispose_root`）与 `host_supports`。
- 后台 `submit_root_turn` 允许 `wait_for_result: true`（默认值）：后台任务不持有 handler
  admission permit，不会与被提交 turn 的回调形成互等。**handler 内**的
  `submit_turn(wait_for_result: true)` 仍被宿主拒绝；长任务不要塞进 handler——宿主→worker
  的 handler 调用 120s 超时是有意的，handler 只做短交互（status tool、command），长
  pipeline 放后台任务。
- 后台无调用上下文，`create_root` 必须显式给出 `working_dir`（省略且上下文也没有时返回
  `context_unavailable`）。
- worker 崩溃后正在跑的 turn 会变孤儿：插件重启后凭 `root_state` 检查自己持有的 root
  session（`active_turn_id` 残留、phase 归 idle 即已完成/失败），再 `dispose_root` 回收；
  宿主侧不做 GC。
- 每个并发等待中的 turn 占用一个宿主重入槽（上限 8），超出报 `ReentrancyExceeded`；串行
  pipeline 不受影响。旧宿主没有 root 域操作时，用
  `host_supports(HostOperation::SessionRootDispose)` 等预检并保留既有回退路径。

### 会话状态与扩展事件

会话内的扩展私有状态使用 typed client；同一个 handler 可直接写后读：

```rust
HostClient::session_state()
    .write(HostSessionStateWriteRequest {
        key: "last-run".into(),
        content: "complete".into(),
    })
    .await?;
let state = HostClient::session_state()
    .read(HostSessionStateReadRequest {
        key: "last-run".into(),
    })
    .await?;
```

key 只接受 1–128 个 ASCII 字母、数字、`_`、`.`、`-`，且不能是 `.` 或 `..`；缺失值通过
`state.content == None` 表达，state value 以 UTF-8 字节计最多 1 MiB。发射 manifest
中已声明的事件同样使用 typed client：

```rust
HostClient::events()
    .emit(HostEventEmitRequest {
        event_type: "my_extension.completed".into(),
        schema_version: 1,
        payload: serde_json::json!({ "status": "ok" }),
    })
    .await?;
```

每个事件声明的 `max_payload_bytes` 必须位于 1 byte..=1 MiB，发射时仍按该声明值校验。

### Agent 定义文件从哪来？

内置扩展通过 SDK 的宿主路径 API 扫描 `~/.claude/agents`、`~/.astrcode/agents` 以及对应的项目目录。
Agent frontmatter 支持 `tools` 白名单和 `disallowedTools` 黑名单；省略两者时继承父 session 的工具边界，
两者同时存在时黑名单优先。各内置 Agent 的附加限制定义在各自 Markdown frontmatter 中：
`explore` 使用只读白名单，`reviewer` 额外获得仅用于检查和验证的 `Bash`，`execute` 通过
`disallowedTools: Task` 禁止递归委派；
Claude 名称 `Task` 会被规范化为 AstrCode 工具名 `agent`。最终有效工具集仍会与整条父 session
边界取交集，不能通过 Agent 配置扩大权限。自定义 Agent 若省略两者则不会附加隐藏限制。

外置二进制可：

- 在插件内**复制/简化**扫描逻辑（只依赖 `std` + 简单 frontmatter 解析），或
- 把若干内置 agent 编进 `include_str!`（参考 `astrcode-extension-agent-tools/src/builtin_agents/`）。

### 与 AstrCode 的衔接（数据流）

```mermaid
sequenceDiagram
    participant Host as AstrCode 宿主
    participant Ext as 外置 Worker 子进程
    Host->>Ext: handler.invoke(tool: agent)
    Ext->>Host: astrcode.session.control.create
    Ext->>Host: astrcode.session.control.submit_turn
    Host-->>Ext: HandlerResult
    Ext-->>Host: tool 文本结果
    Note over Host: prompt_build 钩子注入 Agents 列表
```

宿主加载：磁盘扩展源读取 `extension.json` → 启动子进程 → `Initialize` 交换 manifest → `S5rExtension::register` 把 tools/hooks 注册进 `ExtensionRunner`，之后与内置扩展一样参与 `pre_tool_use` / `prompt_build` / LLM tool 调用。

### 参考代码

| 用途 | 位置 |
|------|------|
| 内置 agent 完整逻辑（进程内） | `crates/astrcode-extension-agent-tools/` |
| s5r Worker 写法 | `crates/astrcode-extensions/tests/s5r-guest/src/main.rs` |
| E2E | `cargo test -p astrcode-extensions --test s5r_e2e_test` |

### 本地调试

```bash
# 1. 编译插件
cargo build --release

# 2. 放到扩展目录并写 extension.json

# 3. 启动 AstrCode 后看日志；插件调试输出写 stderr，不要写 stdout
RUST_LOG=astrcode_extensions=debug astrcode ...
```

若工具未出现：检查 `extension_id`、握手是否成功、是否声明 `session_control`、handler 是否在 `worker.tool()` 注册。
