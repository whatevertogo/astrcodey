# AstrCode 扩展系统

> 以当前代码为准（`astrcode-extension-sdk`、
> `astrcode-extension-worker`、`astrcode-extensions`、`astrcode-server`）。

---

## 1. 概览

| 层级 | 实现 | 说明 |
|------|------|------|
| **内置扩展** | `astrcode-bundled-extensions` + 各 `astrcode-extension-*` | 进程内 Rust，使用与 worker 同源的作用域化宿主 API |
| **磁盘扩展** | s5r 子进程 | `~/.astrcode/extensions/`、`<project>/.astrcode/extensions/` |
| **外部工具** | `astrcode-extension-mcp` | MCP 子进程/HTTP，**不**实现 `Extension` trait |

磁盘扩展使用 **s5r** 协议：stdio 长度前缀帧 + JSON `WireMessage`（非 JSON-RPC）。详见 [s5r-protocol.md](s5r-protocol.md)。

**插件作者入门**：[extension-author-guide.md](extension-author-guide.md)

**内置 Rust 扩展实现规范**：
[bundled-extension-authoring-design.md](bundled-extension-authoring-design.md)

---

## 2. 代码地图

| 模块 | 职责 |
|------|------|
| `astrcode-extension-sdk::extension` | `Extension` trait、能力、钩子、Registrar |
| `astrcode-extensions::loader` | 发现 `extension.json`、启动 s5r 子进程 |
| `astrcode-extensions::s5r_ext` | `S5rExtension`、Peer 会话、宿主 `invoke` 路由 |
| `astrcode-extensions::host_router` | 唯一 `astrcode.*` 宿主能力实现 |
| `astrcode-extensions::runner` | 统一运行时 manifest、registration 校验、索引发布与生命周期 |
| `astrcode-extensions::extension_manifest` / `s5r_handler` | typed S5R manifest 规范化与 handler 返回值解析 |
| `astrcode-extension-sdk::s5r` | wire 协议类型的作者向 re-export 和 `HandlerResult` 领域转换 |
| `astrcode-extension-sdk::wire` | S5R wire DTO、稳定错误码（`WireErrorCode`）、宿主操作 catalog |
| `astrcode-extension-sdk::wire::{peer, peer_runtime, frame}` | `Peer` 握手状态机、帧传输、取消、流式 |
| `astrcode-extension-worker` | Worker 入口、`HandlerRegistry`、远程 `HostClient` |

参考实现：`crates/astrcode-extensions/tests/s5r-guest/`  
E2E：`cargo test -p astrcode-extensions --test s5r_e2e_test`

Hook 语义矩阵见 [extension-hook-matrix.md](extension-hook-matrix.md)。

---

## 3. 内置扩展（进程内）

bundled 扩展依赖 `astrcode-extension-sdk`，从 `astrcode_extension_sdk::prelude` 导入进程内作者
接口。身份与 capability 放在 `manifest()`；定义与 handler 在 `register()` 中成对注册；I/O、配置
读取和后台任务从 `start()` 或 handler 发起。生产 context 均由宿主构造，作者只通过 accessor 读取。

### 3.1 最小 manifest / register / start

```rust
use std::sync::Arc;

use astrcode_extension_sdk::prelude::*;

struct PingExtension;

#[async_trait::async_trait]
impl Extension for PingExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("example-ping")
            .version(env!("CARGO_PKG_VERSION"))
            .description("Minimal bundled extension")
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            tool("ping").description("Return pong").build(),
            tool_handler(
                |_ctx| async { Ok(ToolPlan::default()) },
                |_ctx| async { Ok(ToolResult::success("pong")) },
            ),
        );
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        Ok(())
    }
}

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(PingExtension)
}
```

`Registrar::finish(manifest)` 在宿主安装边界校验局部声明，并返回
`(ExtensionManifest, ExtensionRegistrations)`。runner 再做全局冲突检查并发布不可变 generation；
作者不直接调用 `finish()`，测试使用 `RegistrationHarness` 走同一路径。

### 3.2 `ToolContext`

工具 handler 只接收 owned `ToolContext`。字段私有；工具参数统一经 `arguments<T>()` 解码，错误包含
工具名与 serde path。

```rust
use astrcode_extension_sdk::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EchoArgs {
    text: String,
}

struct EchoHandler;

#[async_trait::async_trait]
impl ToolHandler for EchoHandler {
    async fn plan(&self, ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let _: EchoArgs = ctx.arguments()?;
        Ok(ToolPlan::default())
    }

    async fn execute(&self, ctx: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let args: EchoArgs = ctx.arguments()?;
        let _session_dir = ctx.paths().session_data_dir()?;
        Ok(ToolResult::success(args.text).into())
    }
}
```

`ToolPlanContext` 只暴露最终参数与不可变调用事实，不提供 Host、event、task 或持久化入口。
session 用 plan 完成权限决策并签发 resource lease；`execute` 中的每次 Host 调用再校验该 lease。
因此 closure adapter 也要求同时提供 planner，不存在隐式 trusted/native 入口。

`ctx.extension_id()`、session/turn、working directory、路径 namespace、capability scope 和取消令牌
均来自 runtime attribution。`ToolContext` 不暴露 core `ToolExecutionContext`、裸
`SessionOperations` 或 event sink。

### 3.3 公共与专用 context

所有 handler context 都包含同一组公共调用事实；具体 handler 再增加自己的最小输入。

| 范围 | Context | 主要 accessor / 专用事实 |
|------|---------|--------------------------|
| 公共调用 | `ExtensionCallContext` | `extension_id`、可选 session/turn、可选 working dir、`paths`、`host`、`events`、`tasks`、`cancellation` |
| 启动 | `ExtensionStartContext` | `config()`、`startup_working_dir()`；没有隐式 session |
| 工具 | `ToolContext` | tool name、call id、raw/typed arguments、main/small model id、available tools |
| 命令 | `CommandContext` / `CommandCompletionContext` | command name、argument、model；补全额外包含 cursor |
| HTTP | `HttpContext` | route、已校验 request、可选 caller extension；`json<T>()` 解码 body |
| 动态发现 | `ToolDiscoveryContext` / `CommandDiscoveryContext` | workspace 与 discovery generation |
| Hook | `PreToolUseContext`、`PostToolUseContext`、`ProviderContext`、`PromptBuildContext`、`CompactContext`、`ContinueAfterStopContext`、`UserMessageEnvelopeContext`、`LifecycleContext` | `ToolInputTransformHandler` 与 `PreToolUseHandler` 共用只读的 `PreToolUseContext`，但前者只变换输入、后者只做 Allow/Ask/Block 准入；其他 context 也只增加所属 hook 的输入 |

`SlashCommand` 是 list、execute、completion 共用的完整声明：参数 schema、idle 要求、completion、priority、transport availability 和 execution owner 只定义一次。命令冲突只比较显式 priority，再用 extension ID/name 确定稳定顺序；runtime 不按某个产品 Extension ID 推断“skill”来源或隐式优先级。普通命令由 extension handler 完成；`Host(SessionCommandKind)` 命令只能由声明 `session_command` capability 的扩展注册，并且 handler 只能返回种类匹配的 `SessionCommandIntent`。server 在 handler 前执行 transport/busy admission，再在同一个 session operation guard 内解释 intent，因此扩展不能重入 operation gate 或复制 Host 状态机。非空 `/name` 始终按命令语法解析；未知或被禁用的命令返回类型化错误，不会降级写成用户消息，只有裸 `/` 仍可作为普通输入。

生产代码不能用 struct literal 构造这些 context；测试在 SDK 依赖启用 `testing` feature 后，
从 `astrcode_extension_sdk::testing` 使用 builder。

### 3.4 类型化 host 与 capability

`ctx.host()` 返回当前调用已绑定的 `ExtensionHost`。领域 accessor 返回 owned client；除
`models()` 外，accessor 本身返回 `Result<Client, HostError>`。特别是
`workspace()` 也是 `Result<WorkspaceClient, HostError>`，调用前会区分权限、backend 与 workspace
上下文缺失。

| 入口 | Capability | 调用范围与示例 |
|------|------------|----------------|
| `host.models()` | `main_model` / `small_model` | `main_chat`、`small_chat` 默认使用 provider 输出上限；`llm_chat_request(...).with_max_output_tokens(n)` 配合对应的 `*_request` 入口可限制单次生成。`*_chat_events` 返回渐进式 `ModelStream`，`*_chat_collected` 返回最终 content 与 model。 |
| `host.session_control()?` | `session_control` 或 `input_delivery` | `create_root`、`submit_root_turn`、`root_state` 使用 `input_delivery`；子 session 的创建、提交、注入、中断、取消、状态、工具配置、回收与重新激活使用 session-scoped `session_control`。`cancel_turn` 返回 `HostSessionCancelOutput { cancelled }`。 |
| `host.session_history()?` | `session_history` | 当前 session 及其已授权后代的 `list_summaries`、`transcript`、`provider_messages`、`token_usage`、`events_page` 与 `snapshot`。 |
| `host.session_inspect()?` | `session_inspect` | 全局跨 session 只读能力；只授予确需全局观察的扩展。 |
| `host.tool_results()?` | `tool_result_read` | `read(HostToolResultReadRequest)` 分页读取当前 session 的持久化工具结果；artifact ID 是不含路径语义的 opaque token。 |
| `host.workspace()?` | `workspace_read` / `workspace_write` | 必须有 workspace context；read/list/grep/glob 与 write/edit 分别重新校验所需能力。 |
| `host.process()?` | `process_spawn` | 在受限 workspace cwd 启动 Host-supervised pipes process；不提供 PTY/resize，也不是 OS sandbox。 |
| `host.network()?` | `network_client` | 受限公网 HTTP(S)，拒绝本机、内网和链路本地目标；body 在作者 API 中是原始字节，线缆使用 base64。`max_bytes <= 10 MiB`，`timeout_ms` 为 `1..=60_000`，`Manual` 返回有界 3xx body。 |
| `host.extension_http()?` | `public_http_dispatch` | 调用另一扩展的公开路由；同步自调用被拒绝。 |
| `ctx.events()` | `emit_custom_events` | 只能发射 manifest 已声明的事件。 |
| `ctx.paths().session_data_dir()` | 无额外 capability | 需要 session context，目录已按 extension id 隔离。 |

`HostError` 无损保存 `code`、`message`、`hint`、`retryable` 和 `details`；
`HostError::code_enum()` 将已知 code 解析为 `WireErrorCode`，未知 code 返回 `None`，原始字符串仍
保留在 `HostError::code` 中。

具有 turn attribution 的 hook/tool context 会携带本 turn 固定的 main/small provider binding；
`host.models()` 的 session-scoped 调用始终使用该 binding，即使 live provider 已 reload。只有 startup
或其他明确没有 turn attribution 的调用使用 live fallback。这个约束防止单 turn 内模型漂移，
不表示 core config generation 与 Extension publication 已经原子化。

工具结果读取要求 session context，不能借 artifact ID 跨 session 访问。`max_bytes` 必须在
`4..=20_000`，默认 20,000；`byte_offset` 是 UTF-8 字节偏移，必须使用上页返回的
`next_byte_offset` 继续读取。Host 只返回完整 UTF-8 字符，调用方不应自行推算下一页游标。

### 3.5 测试入口

```rust
use astrcode_extension_sdk::{
    extension::ExtensionCapability,
    host::{HostOperation, HostWorkspaceReadOutput, HostWorkspaceReadRequest},
    testing::{MockExtensionHost, RegistrationHarness, ToolContextBuilder},
};

#[tokio::test]
async fn bundled_extension_uses_the_real_authoring_boundaries() {
    let registered = RegistrationHarness::register(&PingExtension).unwrap();
    assert_eq!(registered.manifest().id(), "example-ping");

    let mock = MockExtensionHost::new()
        .grant(ExtensionCapability::WorkspaceRead)
        .workspace_context(true)
        .respond(
            HostOperation::WorkspaceRead,
            serde_json::json!(HostWorkspaceReadOutput { content: "hello".into() }),
        );
    let ctx = ToolContextBuilder::new("example-ping", "ping")
        .workspace("/workspace")
        .host(mock.host())
        .build();
    let output = ctx
        .host()
        .workspace()
        .unwrap()
        .read(HostWorkspaceReadRequest {
            path: "README.md".into(),
            max_bytes: None,
        })
        .await
        .unwrap();

    assert_eq!(output.content, "hello");
    assert_eq!(mock.invocations().len(), 1);
}
```

`CommandContextBuilder`、`HttpContextBuilder` 和 `HookContextBuilder` 构造其它私有 context；
`ExtensionLifecycleHarness` 验证 suspended task、start/config/cancel/drain/stop 与 startup rollback
顺序。builder 的默认值没有 session、workspace、capability 或可用 backend，测试必须显式 grant 与
配置响应。

### 3.6 当前 bundled 扩展

| 扩展 ID | Crate | 默认 | 说明 |
|---------|-------|------|------|
| `astrcode-coding` | `astrcode-extension-coding` | 启用 | `read`、`read_tool_result`、`write`、`edit`、`patch`、`glob`、`grep`、`shell` |
| `astrcode-agent-tools` | `astrcode-extension-agent-tools` | 启用 | 子 Agent 委派与发现 |
| `astrcode-mcp` | `astrcode-extension-mcp` | 启用 | MCP 客户端（stdio/HTTP） |
| `astrcode-skill` | `astrcode-extension-skill` | 启用 | 斜杠命令 Skill 发现与调度 |
| `astrcode-session-commands` | `astrcode-extension-session-commands` | 启用 | 声明 `/compact`、`/model` 和类型化 Host command intent |
| `astrcode-todo-tool` | `astrcode-extension-todo-tool` | 启用 | Todo 进度追踪工具 |
| `astrcode-mode` | `astrcode-extension-mode` | 启用 | Code / Plan 模式切换 |
| `astrcode-goal` | `astrcode-extension-goal` | 启用 | Codex-style session goal 与自动续跑 |
| `astrcode.memory` | `astrcode-extension-memory` | **关闭** | 项目级 Markdown 记忆 |
| `astrcode-channels` | `astrcode-extension-channels` | **关闭** | Telegram 通道桥接 |
| `astrcode-web-tools` | `astrcode-extension-web-tools` | 启用 | `web-search` / `fetch-url` 内置 Web 工具 |

通过 `config.toml` 的 `runtime.extensionStates` 覆盖默认开关。配置示例见 [configuration.md](configuration.md#web-tools-extension)。

## 4. 磁盘 s5r 扩展

### 4.1 目录布局

```
~/.astrcode/extensions/my-ext/
  extension.json
  my-ext-binary

<project>/.astrcode/extensions/my-ext/
  extension.json
  ...
```

### 4.2 extension.json

| 字段 | 必填 | 说明 |
|------|------|------|
| `extension_id` | 是 | 启动进程前确定的权威扩展 ID；Host 在 Initialize 中指定，Worker 必须确认一致 |
| `protocol.s5r` | 是 | `"3.0"` |
| `command` | 是 | 字符串数组：`[可执行文件, ...参数]` |
| `env` | 否 | 额外环境变量 |

```json
{
  "extension_id": "my-extension",
  "protocol": { "s5r": "3.0" },
  "command": ["./my-extension"]
}
```

### 4.3 握手与调用

1. 宿主启动子进程后发送 `Initialize`，声明预期扩展 ID、feature 与完整 Host operation 支持目录
2. `Worker::run_stdio()` 返回 Worker 身份、协商 feature 和严格类型的 `InitializeManifest`
3. 宿主完成 Registrar 校验及跨扩展冲突检查后发送 `Activate`，Worker 回复 Ready
4. Ready 后宿主经 `handler.invoke` 调用工具 / 命令 / 钩子，子进程经 `astrcode.*` 调用宿主

S5R 消息和 session host-operation DTO 的字段统一使用 `snake_case`。面向 HTTP/前端的 DTO
可在其独立边界使用 `camelCase`，不得把该命名约定带回 S5R payload。

宿主只把 `nested_invoke_v1` 列为 required，因为 handler invoke 的归因依赖
`parent_invoke_id`；`model_stream_v1` 与 `custom_event_v1` 属于可选能力，双方支持时协商。
未协商 `model_stream_v1` 的流式调用返回 `unsupported_feature`；声明 custom event 或
subscription 却未协商 `custom_event_v1` 的 manifest 在发布注册前被拒绝。在 handler 作用域内发起的
嵌套 invoke 会携带父请求 ID，宿主据此恢复该请求自己的 session、working directory、
取消令牌和授权上下文。未完成激活、feature 交集不满足任一方 required 集合、或携带
未知父请求的调用都会在边界拒绝。
handler 自行创建的脱离 Tokio task 不继承 task-local 调用作用域；当前 Worker API 不支持
这类任务在原 handler 返回后继续使用会话级 HostClient 能力。

---

## 5. 宿主能力

见 `HostRouter`；除默认 session state API 外，子进程 invoke 的 capability 须以
`astrcode.` 开头，且 manifest 中已声明对应 capability。

宿主从唯一的 `HOST_OPERATION_SPECS` 生成握手支持目录，并用同一目录查找、授权和选择类型化能力域；
LLM、session、context、workspace、process、network 与公开扩展 HTTP 各自持有窄后端并实现行为，
`HostRouter` 仅负责解析、授权和分发。

握手目录不按 Worker manifest、当前 backend 或调用上下文过滤，只说明当前 Host 版本实现该
operation。`host_supports == true` 不是授权或可用性承诺；每次调用仍按 manifest capability、
可信 `InvokeContext` 和 backend 状态逐层检查。

默认可用、无需 manifest capability：

| API | 说明 |
|------|------|
| `astrcode.session.state.read` | 读取当前 session 下按 extension id 隔离的状态。 |
| `astrcode.session.state.write` | 写入当前 session 下按 extension id 隔离的状态。 |

`session_state` 不是有效 capability，插件不要在 manifest 中声明它。

须在 manifest 声明后才可调用：

安装并启用扩展表示宿主接受其 manifest 中声明的能力；运行时不允许扩展再动态提升能力。
因此第三方扩展启用前必须审查 `session_inspect`、`session_control`、`network_client`、
`process_spawn` 与 workspace 写能力，其中 `session_inspect` 是明确的全局读取权限。

| Manifest capability | API | 说明 |
|------|------|------|
| `main_model` | `astrcode.llm.main_chat` | 调用当前会话主模型。 |
| `small_model` | `astrcode.llm.small_chat` | 调用宿主小模型。 |
| `session_history` | `astrcode.session.history.*` / `astrcode.session.read_events` | 列出当前 session lineage 的稳定摘要，读取 transcript、provider messages、token usage、snapshot 或按游标读取 durable events；仅在 session-scoped 调用上下文中可用，不能读取无关顶层 session。 |
| `input_delivery` | `astrcode.session.root.*` | 创建 extension-owned 顶层 session、向其提交输入并读取生命周期状态；用于 channel 等进程级入口，不授予任意 session 控制。 |
| `session_control` | `astrcode.session.control.*` | 创建、提交、注入、中断、取消、查询执行状态或回收子会话。中断并提交在 session delivery gate 内切换 turn。 |
| `session_inspect` | `astrcode.session.inspect.*` | 宿主级全局读取权限：跨 session lineage 列出所有宿主可见会话，读取快照、完整投影或 provider 可见消息。只应授予需要全局观察或后台接续会话的扩展。 |
| `public_http` | 公开路由注册 | 注册无需 bearer token 的 JSON HTTP 路由；禁止占用 `/api` 命名空间。 |
| `authenticated_http` | 管理路由注册 | 注册复用宿主 bearer token、按扩展 id 隔离的 JSON HTTP 路由。 |
| `public_http_dispatch` | `astrcode.extension.http.public` | 从插件内部调用另一插件的公开路由；同步自调用会被拒绝以避免 s5r 重入死锁。 |
| `emit_custom_events` | `astrcode.event.emit` | 发射 manifest 已声明的扩展事件。 |
| `consume_custom_events` | custom event subscription | 注册并消费符合 source filter 的扩展事件。 |
| `provider_request` | Provider 与 user-message hooks | 读取或改写 provider 请求、观察 provider 响应，并变换 durable user-message envelope；after-response 返回值不会改写 turn。 |
| `tool_intercept` | Blocking tool transform/admission 与 post-tool hooks | `ToolInputTransform` 先按确定顺序折叠输入；Session normalize 后，全部 `PreToolUse` handler 在同一 canonical arguments 上聚合 Ask 或返回 Block；`PostToolUse` 处理结果。 |
| `turn_continuation_control` | Continue-after-stop hook | 决定 LLM 自然停止后是否继续一个 agent step。 |
| `tool_result_read` | `astrcode.tool_result.read` | 读取当前 session 拥有的 opaque 工具结果 artifact；按 `4..=20_000` UTF-8 字节分页，并使用响应中的 `next_byte_offset` 续读。 |
| `workspace_read` | `astrcode.workspace.read/list/grep/glob` | 有界读取、目录遍历、正则搜索和 glob；相对路径限定工作区内并拒绝 symlink 组件，绝对路径按文件系统解析；均拒绝 `..` 穿越与密钥类路径，默认忽略 `.git`/`node_modules`。 |
| `workspace_write` | `astrcode.workspace.write` / `astrcode.workspace.edit` | 创建、替换或精确编辑非敏感文件；相对路径限定工作区内并拒绝 symlink 组件，绝对路径按文件系统解析；写入目标为 symlink 时拒绝；均拒绝 `..` 穿越与密钥类路径。 |
| `process_spawn` | `astrcode.process.*` | 运行并管理受监管的 stdin/stdout/stderr pipes process；cwd 默认工作区，可传绝对路径（按文件系统解析）或相对路径（限定工作区内）。并发、总时长、stdin 和输出均受限；Unix 用 process group、Windows 用 Job Object 管理完整进程树；不提供 PTY 或 resize operation。 |
| `network_client` | `astrcode.network.client` | 向公网发起 HTTP(S) 请求。worker 与进程内扩展共用同一个宿主出站网络服务；统一拒绝本机、内网、链路本地地址及解析到这些地址的域名。作者 API 接收原始响应字节，线缆使用 base64；`max_bytes <= 10 MiB`，`timeout_ms` 为 `1..=60_000`，`Manual` 不跟随重定向但返回有界 3xx body。同名响应头不保留重复值，并通过 `final_url` 返回最终地址。并发、总时长和重定向次数均受限；并发上限全局共享，当前不承诺 extension 级公平配额。 |

`workspace_write`、`process_spawn` 与 `network_client` 均为敏感授权。`process_spawn` 是进程执行授权，不是操作系统级沙箱；它只提供 Host-supervised pipes，Unix descendant-tree 已有真实回归，Windows Job Object 的真实 Windows CI 仍待验收。`network_client` 仅提供受限公网访问，
不用于调用宿主本机或内网服务。只应给确实需要这些权限的插件声明相应 capability。
Worker 使用与 bundled 同名的类型化领域方法，例如
`HostClient::process().spawn(...)` 与 `HostClient::network().send(...)`。通用 raw invoke 仅保留在
启用 `testing` feature 后的 `astrcode_extension_worker::testing` transport seam，不属于作者
prelude，也不进入默认生产 API。

`session_inspect.read_model` 不会直接暴露核心的 `SessionReadModel`。宿主在
`host_router::session_inspect` 边界显式映射到 `sdk::wire` DTO，内部 enum 的调整不会静默改变
插件线缆契约。

HTTP 路由由 `Worker::http_route(route, http_handler(...))` 同时写入握手 manifest 与
handler 注册表。宿主在安装时校验 scope capability、路径格式、全局公开路由冲突和
每路由请求体上限（默认 64 KiB，最高 1 MiB）；handler 响应体与执行时间同样有界。

宿主也会在启动扩展前校验 hook 注册与 capability 声明是否一致。扩展事件、
compact、provider、blocking tool intercept、continue-after-stop 等敏感注册缺少对应
capability 时会直接拒绝加载。生命周期事件中只有 `TurnStart` 和
`UserPromptSubmit` 可以使用 Blocking；session、step 和结束类事件可选择同步等待但
fail-open 的 Advisory，或由宿主管理生命周期的 NonBlocking 通知。

所有来源最终都解析成 runner 内唯一的 `ResolvedExtensionManifest`。索引、快照、
能力检查和冲突检查只读取这份运行时清单，不再分别维护扩展实例、注册记录和任务表。
`start()` 中登记的后台任务会保持 suspended；单个注册在索引发布后激活，loader
批量同步则在整批成功 registration 都可见后统一激活。启动失败或超时的任务不会被
轮询，宿主会直接丢弃任务并执行启动回滚。

来源同步分为 discovery 与 load 两阶段。每个候选携带稳定的 `source_key` 和内容
fingerprint 以及无需启动进程即可读取的权威 extension ID；reconcile 直接保留未变化的运行实例，
禁用候选不会启动子进程。替换候选只在旧 generation retirement 与 must-finish barrier 完成后
启动并初始化；Worker 在响应前校验 Host 指定的 ID，Host 也复核响应身份。加载器只加载新增或变更候选，并停止
已经消失的来源。磁盘来源的 fingerprint 覆盖 `extension.json`、显式路径命令程序及
命令中引用的本地文件，因此普通 reload 不会启动未变化的 s5r 子进程。增量完成后 runner 会
恢复来源顺序，再统一激活新任务，handler 优先级与全量加载保持一致。

### 事件运维接口

以下接口位于 bearer 认证后的 `/api` 管理面，不是扩展作者的 host capability：

- `GET /api/sessions/{id}/event-consumers` 返回 custom-event subscription 的 pause、
  checkpoint、pending 与失败计数。
- `POST /api/sessions/{id}/event-consumers/control` 接受 `extensionId`、`subscriptionId`
  和 `pause`、`resume`、`replay_from_beginning`、`skip_to_stream_head` 之一。重置 checkpoint
  前会先暂停并等待当前 handler 退出；超时返回 `409`，不会让旧执行路径越过新 checkpoint。
  consumer state 只保留最近 128 条 quarantine/skip 审计和单调总数；写入会先同步临时文件
  再原子替换，Unix 还会同步目录元数据。

---

## 6. 磁盘扩展编写入口

磁盘扩展从 `astrcode_extension_worker::worker_prelude` 导入 `Worker`、handler adapter 与
`HostClient`，参考 `tests/s5r-guest/src/main.rs` 与 `s5r_e2e_test.rs`。不要混入 bundled
`prelude` 的 `Extension`、`Registrar` 或生产 context。

**agent-tool 类外置插件**（子 Agent 委派）：见 [extension-author-guide.md — 外置 agent-tool](extension-author-guide.md#外置-agent-tool-类插件)。

### ContinueAfterStop 预算

`ContinueAfterStop` 是 blocking-only decision hook，注册时可声明
`ContinueAfterStopOptions`。默认不做 host 级次数限制，是否继续主要交给 handler
自己的状态机决定；需要 host 代为限制时声明 `ContinueAfterStopOptions::limited(n)`，
需要明确表达无限续跑时声明 `ContinueAfterStopOptions::unlimited()`。

磁盘 s5r 扩展的握手 manifest 可在 `continue_after_stop` hook 的 `options.max_per_turn` 上携带数字字段；缺省表示不限制，`-1` 也表示无限续跑，非负数表示每 turn 上限。宿主调用 hook 时会在 input 中传入 `continuations_this_turn`，表示当前 turn 已经发生的自动续跑次数。

### Typed decision hooks

进程内扩展还可以注册 typed decision hook：

| Hook | 用途 |
|------|------|
| `on_user_message_envelope(priority, handler)` | 用户消息写入 durable transcript 前的改写或阻断。 |

该 hook 不接收 `HookMode`，宿主总是按优先级同步等待。它暂不暴露给磁盘
s5r manifest；s5r manifest 中声明 `user_message_envelope` 会在握手校验阶段失败。

协议细节见 [s5r-protocol.md](s5r-protocol.md)。
