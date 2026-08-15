# AstrCode Crate 说明

本文档按 workspace 成员总结每个 Rust crate 的职责、主要模块、依赖边界和测试线索。

范围来自根目录 `Cargo.toml` 的 `[workspace].members`。`crates/astrcode-extensions/tests/s5r-guest` 和 `eval-tasks/fixtures/*` 也有 `Cargo.toml`，但它们是测试/评测 fixture，不是 workspace 成员，本文不作为正式 crate 展开。

## 总览

AstrCode 当前 workspace 有 29 个成员：`crates/` 下 28 个 crate，加上 `src-tauri` 的桌面壳 `astrcode-desktop`。

整体分层可以按依赖方向理解：

1. 基础领域与线缆契约层：`astrcode-core`、`astrcode-session-projection`、`astrcode-protocol`；S5R 线缆契约在 `astrcode-extension-sdk::wire` 内保持独立模块边界。
2. 基础能力实现层：`astrcode-ai`、`astrcode-storage`、`astrcode-context`、`astrcode-log`。
3. 会话运行时层：`astrcode-session`。
4. 扩展系统层：作者面 `astrcode-extension-sdk`、外部进程运行时 `astrcode-extension-worker`、宿主 `astrcode-extensions`，以及 `astrcode-bundled-extensions` 和各 `astrcode-extension-*` 内置扩展。
5. 服务与客户端层：`astrcode-server`、`astrcode-client`。
6. 用户入口层：`astrcode-cli`、`astrcode-desktop`。
7. 辅助评测层：`astrcode-eval`。

一个关键边界：所有 `astrcode-extension-*` 内置扩展都只依赖 `astrcode-extension-sdk`，不直接依赖 server/session/storage 等宿主内部 crate。这保证内置扩展和外置扩展走同一套公开 SDK 契约。

依赖方向由 `scripts/check-deps.py` 检查。脚本使用的校验层级偏向依赖可达性（例如 desktop 作为最终应用入口独立约束），不完全等同于上面的产品架构分层；文档中的分层用于理解职责边界。

## Workspace 成员索引

| Crate | 路径 | 类型 | 主要用途 |
|---|---|---|---|
| `astrcode-core` | `crates/astrcode-core` | lib | 稳定领域契约：事件、工具、配置、LLM 与基础 session 类型 |
| `astrcode-protocol` | `crates/astrcode-protocol` | lib | JSON-RPC、HTTP DTO、事件通知、协议版本等 wire 类型 |
| `astrcode-ai` | `crates/astrcode-ai` | lib | OpenAI/Anthropic provider、流式解码、重试 |
| `astrcode-storage` | `crates/astrcode-storage` | lib | JSONL EventLog、session 仓库、配置与 artifact 存储 |
| `astrcode-session-projection` | `crates/astrcode-session-projection` | lib | session read model、summary 和纯事件 reducer |
| `astrcode-context` | `crates/astrcode-context` | lib | prompt 组装、上下文裁剪、token 预算、compact |
| `astrcode-session` | `crates/astrcode-session` | lib | session/turn 运行时、工具管线、权限链、compact 持久化 |
| `astrcode-extension-sdk` | `crates/astrcode-extension-sdk` | lib | 扩展作者 API、Host 客户端，以及逻辑隔离的 S5R wire/peer 契约 |
| `astrcode-extension-worker` | `crates/astrcode-extension-worker` | lib | s5r worker 运行时：`run_stdio`、handler dispatch、远程 `HostClient` |
| `astrcode-extensions` | `crates/astrcode-extensions` | lib | 扩展加载、hook 分发、host router、s5r 子进程运行 |
| `astrcode-bundled-extensions` | `crates/astrcode-bundled-extensions` | lib | 第一方内置扩展组合根 |
| `astrcode-extension-agent-tools` | `crates/astrcode-extension-agent-tools` | lib | `agent` 子 Agent 委派工具 |
| `astrcode-extension-coding` | `crates/astrcode-extension-coding` | lib | 只经 SDK Host 能力实现的文件、搜索与受监管 shell 工具 |
| `astrcode-extension-mcp` | `crates/astrcode-extension-mcp` | lib | MCP server 发现、预热、工具搜索和工具调用 |
| `astrcode-extension-skill` | `crates/astrcode-extension-skill` | lib | Claude-style Skill 发现、`Skill` 工具、skill slash command |
| `astrcode-extension-session-commands` | `crates/astrcode-extension-session-commands` | lib | 声明 `/compact`、`/model` 及其类型化 Host command intent |
| `astrcode-extension-todo-tool` | `crates/astrcode-extension-todo-tool` | lib | `todoWrite` session-local 进度列表 |
| `astrcode-extension-mode` | `crates/astrcode-extension-mode` | lib | code/plan 模式切换与 plan artifact |
| `astrcode-extension-ask-user` | `crates/astrcode-extension-ask-user` | lib | `askUser` 挂起交互、受保护 HTTP 与实时事件 |
| `astrcode-extension-goal` | `crates/astrcode-extension-goal` | lib | Codex-style session goal、token 预算与自动续跑 |
| `astrcode-extension-memory` | `crates/astrcode-extension-memory` | lib | 用户/项目记忆、记忆索引、召回、保存/删除工具 |
| `astrcode-extension-channels` | `crates/astrcode-extension-channels` | lib | Telegram channel 入口扩展 |
| `astrcode-extension-web-tools` | `crates/astrcode-extension-web-tools` | lib | `web-search` 和 `fetch-url` 网络工具 |
| `astrcode-server` | `crates/astrcode-server` | lib + bins | stdio/HTTP/ACP 后端、session manager、turn scheduler |
| `astrcode-client` | `crates/astrcode-client` | lib | typed JSON-RPC client、transport、事件流 |
| `astrcode-cli` | `crates/astrcode-cli` | bin | `astrcode` CLI、TUI、exec、server 子命令 |
| `astrcode-log` | `crates/astrcode-log` | lib | tracing 初始化、stderr/file 双层日志、日志清理 |
| `astrcode-eval` | `crates/astrcode-eval` | lib | 评测 case、runner、judge、metrics、report |
| `astrcode-desktop` | `src-tauri` | bin | Tauri v2 桌面壳、sidecar HTTP server 管理 |

## `astrcode-core`

路径：`crates/astrcode-core`

职责：定义平台最底层的共享类型和 trait，是其他 crate 消费或实现的公共契约。它不承载具体业务流程，例如不负责运行 turn、不读写 EventLog、不发 HTTP 请求。任何跨 crate 的领域对象应优先在这里定义；任何只属于具体实现的逻辑不应上移到这里。

主要模块：

- `config`：配置系统。拆分为 raw/effective/defaults/resolve 等模块，区分原始配置、解析后配置、默认值和合并逻辑；`defaults` 同时拥有产品级用户目录与数据目录。
- `types`：核心标识符和共享数据类型，例如 session/project 相关 id。
- `event`：分离的 durable/live payload、未分配序号的事件信封与 `StoredEvent`，是 EventLog 和 UI 投影的事实来源。
- `llm`：`LlmProvider` 抽象、消息、内容块、模型限制、流式事件和收集工具。
- `tool`：工具 trait、定义、显式执行结果、session access、host services、工具 origin 和 execution mode；`tool::access` 描述权限检查需要的资源访问，`tool::selection` 定义 session 工具可见性。
- `compaction`：compact trigger 和 strategy 领域类型。
- `permission`：权限审批模式和审批结果等通用类型。
- `message_attachment`：消息附件契约；read 工具图片和 artifact 读取契约位于 `tool`。

依赖边界：无 workspace 内部依赖，只依赖 serde、tokio、uuid、chrono、thiserror、tracing 等基础库。它是 workspace 的根契约层，下游应通过完整模块路径导入，例如 `astrcode_core::event::EventPayload`。

测试线索：主要是模块内单元测试。改动这里影响面最大，通常需要按受影响契约跑对应下游 crate 测试，必要时跑全 workspace clippy/test。

## `astrcode-protocol`

路径：`crates/astrcode-protocol`

职责：定义 wire protocol。它只放跨进程/跨 HTTP/跨前端边界的数据类型和 JSON-RPC framing，不放业务逻辑。

主要模块：

- `commands`：客户端发给 server 的 JSON-RPC command，例如创建 session、提交 prompt、注入消息、审批工具等。
- `events`：server 发给客户端的 notification、session snapshot、message DTO、extension command/status DTO。
- `http`：HTTP API DTO，例如 session CRUD、prompt、compact、fork、conversation snapshot/delta、config、models、extensions。
- `framing`：JSON-RPC message envelope、ack/error、JSONL 编解码、command/notification 与 JSON-RPC 的映射。
- `transport`：transport 错误类型。
- `version`：initialize request/response 和协议版本协商。
- `agent_session_link`：父子 agent session 关联 DTO。

依赖边界：只依赖 `astrcode-core`。由于这里是 wire 类型，`serde(rename_all = "camelCase")` 等序列化规则应集中在这里。

测试线索：`framing.rs`、`commands.rs`、`events.rs`、`http.rs`、`version.rs`、`agent_session_link.rs` 有协议转换和序列化相关单元测试。修改字段名或 enum variant 需要同步前端/API 兼容性检查。

## `astrcode-ai`

路径：`crates/astrcode-ai`

职责：实现 LLM provider 集成和流式响应处理。对上层暴露 `create_provider`，根据 provider kind、client config、模型 id 和限制创建 `Arc<dyn LlmProvider>`。

主要模块：

- `providers/openai.rs`：OpenAI 兼容 provider。包含标准内容累积器 `StandardAccumulator`、可替换 `ChatAccumulator` trait、SSE/chat response 处理。
- `providers/anthropic.rs`：Anthropic provider 适配。
- `common`：provider 间共享请求/响应辅助。
- `retry`：`RetryPolicy` 和指数退避重试。
- `stream_decoder`：多字节安全 UTF-8/SSE 流式解码。
- `serialization`：provider wire 序列化辅助。
- `tool_result_wire`：工具结果向 provider wire 内容的映射。

依赖边界：依赖 `astrcode-core` 的 `LlmProvider` 契约；不依赖 session/server，因此模型调用可被 session runtime 注入和替换。

测试线索：OpenAI provider、stream decoder、retry 等模块有单元测试。改 provider wire format 时应补对应 provider 的序列化/流式解析测试。

## `astrcode-storage`

路径：`crates/astrcode-storage`

职责：实现持久化层：append-only JSONL EventLog、session repository、配置存储和大工具结果 artifact；持有唯一的可变 session projection，并通过 copy-on-write `Arc` 共享不可变读快照；read model 的纯投影逻辑由 `astrcode-session-projection` 提供。

主要模块：

- `event_log`：JSONL EventLog 读写，维护事件 seq 和 append-only 持久化。
- `session_repo`：文件系统 session repository，负责 session 目录、列表、fork/recycle/delete/restore，
  以及版本化的 durable event-consumer checkpoint。consumer state 使用同步后原子替换，保存
  quarantine/skip 单调总数和最近 128 条有界审计。
- `config_store`：配置文件原子读写。
- `tool_artifacts`：大工具结果文件名、写入、切片和摘要引用。
- `traits`：独立调用方使用 journal、事件读取、状态读取、路径和 artifact 窄端口；
  `SessionStore` 组合完整 repository 所需的生命周期和 compact snapshot 能力。
- `in_memory`：`testing` feature 下的内存存储，供测试使用。

依赖边界：依赖 `astrcode-core` 和 `astrcode-session-projection`；不依赖 session/server。按“右侧可以依赖左侧”表示，运行时主方向为 `core → session-projection → storage → session → server`；协议支线为 `core → protocol → server` 和 `protocol → frontend`，`context → session`、`extension-sdk → session` 是另外两条显式输入边。

测试线索：`event_log/tests.rs`、`session_repo/tests.rs` 和 `in_memory/tests.rs` 覆盖日志校验、尾部恢复、projection 恢复与并发追加；`tool_artifacts.rs` 有模块内测试。任何事件 payload 或持久化格式变更都应同时验证 replay 和 session repository。

## `astrcode-session-projection`

路径：`crates/astrcode-session-projection`

职责：拥有 session 的纯读模型，不做任何存储 I/O。`SessionReadModelProjection` 以事件序列为唯一输入，把 durable facts fan-out 到 model context、展示、执行和 child-agent 子投影；消息按原始顺序统一容纳 user、assistant、tool 与 synthetic context，不按角色拆 projection。

主要模块：

- `model`：组合 `SessionReadModel`，定义 identity、stats、`SessionSummary` 和跨子投影查询。
- `model_context`：system prompt、provider-visible 消息、usage、compact 元数据，以及 rewrite fingerprint 校验和前缀替换。
- `presentation`：首条用户消息、artifact、错误与 recap 等展示事实。
- `execution`：phase、未结 turn、pending input、tool call、approval 和 active step。
- `agents`：父 session 可见的 child-agent link 与终态。
- `reducer`：事件序列校验、batch/replay/summary，并按固定顺序把已校验事件分派给各子投影。

依赖边界：只依赖 `astrcode-core` 和序列化基础库，不依赖 storage、session runtime 或 server。

测试线索：模块测试覆盖多样事件 replay、tool 终态、approval、compact、agent session 和 read model 序列化恢复。

## `astrcode-context`

路径：`crates/astrcode-context`

职责：负责 provider-ready 上下文构建：system prompt 组装、token 预算、消息裁剪、LLM compact 和 compact 后上下文补充。

主要模块：

- `prompt_engine`：具体的 system prompt 组装、规则文件加载和 provider message 分组。稳定 section 与动态 section 分开输出，格式异常的自定义 prompt 会完整回退为单条 system message。
- `context_assembler`：根据模型限制和 `ContextSettings` 裁剪历史消息，生成 LLM 请求上下文。
- `token_budget`：token 粗估和预算门控。
- `compaction`：compact 主流程。包含 XML contract 解析、摘要格式、确定性 fallback、merge、post-compact 上下文恢复等。
- `post_compact_enricher`：compact 后自动找回最近 read 过但已被压缩移出的文件上下文。

依赖边界：依赖 `astrcode-core`；不持有 provider，不直接执行 LLM 请求。compact 的 LLM 调用通过闭包注入，保持上下文层无状态。

测试线索：`tests/token_budget_gate.rs` 覆盖预算门控；`prompt_engine.rs`、`context_assembler.rs`、`compaction/*`、`post_compact_enricher.rs` 有单元测试。改 compact contract 或 prompt section 顺序时必须跑相关测试。

## `astrcode-session`

路径：`crates/astrcode-session`

职责：会话运行时核心。它把持久 session、运行时状态、LLM 请求、工具管线、权限链、compact、事件发布串成一个 turn 执行流程。

主要公开入口：

- `Session`、`SessionCreateParams`、`SessionError`：session handle 和创建参数。
- `SessionRuntimeState`：同一 session 的进程内共享状态和事件排序边界。
- `SessionRuntimeServices`、`SessionExtensionPorts`：server composition root 注入的 context、窄 extension ports、post-compact enrichment 等能力；工具 catalog 只来自当前 extension generation。
- `ToolRegistry`：由单个 turn 显式持有的不可变工具快照；session 只应用 durable selection，不拥有发现缓存。
- `TurnHandle`、`TurnOutput`、`RunTurnResult`：turn 运行和停止控制。

主要模块：

- `session`：session 对象、payload 级事件写入、恢复、订阅和读模型访问。
- `session_event_sink`：每个 session 一条 durable/live 有序发布管线。
- `session_state`：应用层只读状态端口及 storage adapter。
- `session_runtime`：进程内 runtime state，持有 event sink、file observation store、审批状态和不可变工具快照缓存。
- `session_runtime_services`：跨 session 共享的运行时服务。
- `session_extension_ports`：runtime snapshot、tool catalog、prompt contribution、turn hooks、session operations 五个窄端口。
- Extension Runtime 按 generation 与 scope 缓存完整 catalog，并拥有 partial TTL 与 single-flight；session 仅把选择结果冻结进本轮 registry。
- `runtime_stability`：在扩展发布期间以有截止时间的封顶退避等待稳定 generation。
- `session_setup`：缓存未命中时构建工具快照，保留 partial diagnostics，并装配 prompt 快照。
- `tool_registry`：工具定义、执行句柄和 prompt metadata 的不可变快照。
- `turn_context`：单 turn 上下文、事件 tx、错误类型。
- `turn_runner`、`turn_stages`：agent turn 主循环和阶段划分。
- `llm_stream`：LLM 流处理；当轮 provider 请求快照由 `turn_stages` 持有。
- `tool_pipeline`：工具调用预处理、执行、提交；并行/串行工具调度。
- `tool_exec`：单个工具执行、file observation store、interrupt 结果。
- `tool_json_repair`：修复模型输出的工具 JSON 参数。
- `tool_results`：工具结果 inline/persist 策略、artifact summary。
- `tool_deduplicator`、`deferred_tools`：工具调用去重和延迟发现工具。
- `permission/*`：默认权限链，包括 yolo、配置 allow/deny/ask、cwd 外写入、git 路径、敏感文件、shell broad access、子 session 限制、approval history。
- `compaction/pipeline`：manual、auto、reactive 共用的同步 compact 状态机；负责 hook、重读 projection、摘要、token、enrich、durable 提交和唯一终态通知。
- `compaction/turn`、`compaction/manual`：分别负责 turn 内阈值/reactive 编排和空闲 manual 入口。
- `compaction/persistence`：追加 `TranscriptRewritten`、fsync EventLog，并以 best-effort 创建 checkpoint。
- `compaction/circuit_breaker`：只维护 auto LLM compact 的熔断状态。
- `projection_context`：从 projection 映射当前 provider `ContextSnapshot`，不把 context 组装职责下沉到 projection crate。
- `steer`：运行中注入用户消息。
- `turn_publish`、`payload`：turn 事件 ingress、storage projection 读取和 payload 构建。

Compact 调用链只传递 `CompactStrategy`；trigger 与 manual 的 `keep_recent_turns` 由 strategy
唯一派生，避免多个可冲突参数描述同一次 compact。turn 归属由 hook call context 携带，
pipeline 不再接收第二份 `turn_id`。前端同样不复制 conversation phase：服务端
`ConversationControlStateDto.phase` 是远端事实，本地只叠加请求尚未到达 live event 的
`compactSubmitting` 短窗口。

依赖边界：依赖 `astrcode-core`、`astrcode-context`、`astrcode-extension-sdk`、`astrcode-storage` 和 `astrcode-session-projection`。它刻意不依赖 `astrcode-extensions`、`astrcode-server` 或任何具体扩展；server 在 composition root 通过 `SessionRuntimeServices` 注入这些能力。

测试线索：`tests/session_resume.rs`、`tests/ssot_turn_history.rs`、`tests/compact_persist_conflict.rs` 覆盖恢复、历史唯一事实源和 compact 并发；`compaction/pipeline/tests.rs` 覆盖 hook、fsync、checkpoint 与唯一终态顺序；模块内测试覆盖权限链、工具 JSON 修复、工具结果、turn 发布等。

## `astrcode-extension-sdk`

路径：`crates/astrcode-extension-sdk`

职责：扩展作者的稳定公开契约。进程内内置扩展直接依赖它；s5r 子进程扩展以
`astrcode-extension-worker` 为运行时入口，并在需要 worker prelude 未导出的 SDK 模块时直接依赖它。
两类扩展都不依赖宿主内部 crate。

主要模块：

- `extension`：扩展 trait、hook、registrar、HTTP/event 契约、host-attributed call context、namespaced paths 和能力声明。
- `tool`：core 工具契约与扩展专用的私有字段 `ToolContext`。
- `host`：按能力和调用上下文裁剪的类型化宿主域，包括 model、session history/control、workspace、network、process 和 extension HTTP。
- `runtime_ports`：Session 消费的 tool catalog、prompt contribution、turn hooks 和 session operations 窄端口；`ToolCatalogScope` 只携带 discovery 真正使用的 working directory。
- `llm`、`permission`、`config`：按扩展需要 re-export 的稳定 core 契约。
- `hostpaths`：扩展可用的相对路径解析和路径边界校验，并暴露产品级用户目录与数据目录。
- `frontmatter`、`shell`：扩展文件解析和宿主 shell 检测。
- `builder`：进程内扩展 handler 辅助函数。
- `manifest`：扩展 manifest 校验。
- `wire`：S5R wire DTO、稳定错误码、Host operation catalog、frame 与角色化 `Peer` 状态机；是物理并入 SDK、逻辑独立的线缆边界。
- `s5r`：面向作者的 S5R 类型与 `HandlerResult` 领域转换。
- `session`：扩展侧 session 相关 re-export。
- `prelude`：面向进程内扩展的便捷导入集合；s5r worker 的 `worker_prelude` 与运行时组装位于 `astrcode-extension-worker`。
- `testing`：只在 `testing` feature 下导出的 context builder、mock host 与 registration/lifecycle harness。

依赖边界：依赖 `astrcode-core`。worker prelude 组合 `sdk::wire` DTO 与 handler 所需的精选 SDK/domain 类型，但不暴露宿主实现 crate；bundled（进程内）
作者面可共享更多 core 类型。两类扩展都通过 capability-gated typed host API 访问宿主能力；
运行时负责注入不可伪造的 extension/session/turn 归属。

测试线索：`builder.rs`、`manifest.rs` 有单元测试。修改 SDK 类型等同修改扩展 ABI，需要同步内置扩展和 s5r 测试。

## `astrcode-extension-worker`

路径：`crates/astrcode-extension-worker`

职责：s5r worker（子进程）侧运行时。磁盘扩展通过它把 handler 暴露给宿主；authoring 与 wire 契约均由 SDK 拥有。

主要模块：

- `worker`：`Worker` 入口和 `run_stdio` 主循环，驱动握手、invoke 与取消。
- `worker/registry`：`HandlerRegistry` 按 handler id 分发工具/命令/hook/custom event/HTTP handler。
- `worker/host`：远程 `HostClient` 和各能力域客户端，经嵌套 invoke 调用宿主能力。
- `worker/builder`：worker handler 注册辅助函数。
- `worker_prelude`：磁盘扩展作者的一站式导入集合。
- `testing`：只在 `testing` feature 下导出的 scoped `HostApi` transport seam；raw invoke 不进入默认生产 API。

依赖边界：只依赖 `astrcode-extension-sdk`，不依赖宿主内部 crate。

测试线索：`worker/registry.rs`、`worker/host.rs` 等模块内测试覆盖 handler dispatch、取消和宿主调用错误路径。

## `astrcode-extensions`

路径：`crates/astrcode-extensions`

职责：扩展和 hook 系统的宿主实现。它负责加载内置/外置扩展、构建 host router、分发生命周期事件、执行 blocking/non-blocking/advisory hook，并运行 s5r 子进程扩展。

主要模块：

- `loader`：扩展源 `ExtensionSource`、加载上下文、加载结果和多源加载。
- `runner`：扩展生命周期、generation 发布、partial tool catalog、hook 分发、工具/命令/状态注册执行；维护 typed decision hook 的优先级排序和短路语义。
- `host_router`：把扩展请求路由到宿主能力，例如 session、storage、provider、event 等。
- `extension_manifest`：本地扩展 manifest 解析和验证。
- `s5r_handler`：S5R handler 返回值解析。
- `s5r_ext`：s5r 子进程扩展协议、session、加载和运行。

依赖边界：依赖 `astrcode-core`、`astrcode-context`、`astrcode-session-projection`、
`astrcode-storage` 和 `astrcode-extension-sdk`。它不依赖某个
具体内置扩展；内置扩展组合在 `astrcode-bundled-extensions`。

测试线索：`tests/loader_integration_test.rs`、`tests/s5r_e2e_test.rs`、`tests/workspace_read_security.rs` 覆盖加载、s5r 端到端和 workspace read 安全边界；`tests/s5r-guest` 是测试用 guest 程序。

## `astrcode-bundled-extensions`

路径：`crates/astrcode-bundled-extensions`

职责：第一方内置扩展的组合根。`astrcode-extensions` 负责运行时机制，本 crate 负责决定哪些第一方扩展被链接进二进制，以及按什么顺序加载。

主要内容：

- `BundledExtensionSource`：实现 `ExtensionSource`，根据配置状态返回启用的扩展。
- `bundled_extensions`：按优先级返回扩展列表，早注册的扩展在工具名冲突时有优先权。
- `bundled_extension_ids`：返回当前 feature 编译进来的扩展 id。
- `extension_enabled`：统一配置显式值和默认启用策略。`memory`、`channels` 默认关闭，其他扩展默认启用。

Feature：

- `default` 包含 agent-tools、coding、mcp、skill、session-commands、todo-tool、mode、ask-user、goal、memory、channels、web-tools。
- 单独 feature 对应每个内置扩展，便于裁剪二进制。

依赖边界：依赖内置扩展 crate 和 `astrcode-extensions`/`astrcode-extension-sdk`。这是少数允许直接依赖所有内置扩展的 composition crate。

测试线索：单元测试覆盖默认启用策略和显式配置优先级。

## `astrcode-extension-session-commands`

路径：`crates/astrcode-extension-session-commands`

职责：作为普通第一方扩展声明 `/compact` 与 `/model` 的名称、描述、transport availability、idle admission 和类型化 Host execution。handler 只解析参数并返回 `SessionCommandIntent`；它不依赖 session/server，也不复制 compact 或 model selector 状态机。

关键边界：只依赖 `astrcode-extension-sdk`。扩展 manifest 必须声明 `SessionCommand` capability，命令 declaration 必须声明匹配的 `SessionCommandKind`；runner 在 handler 结果边界再次校验 capability 和 intent kind。server 在现有 session operation guard 内解释 intent，且在执行 handler 前统一应用 transport 与 busy admission。

测试线索：server 的多样命令测试覆盖 list/execute/complete 的统一 availability、interactive model selector、非交互拒绝、compact 参数错误，以及 busy compact 不调用 handler、不写用户消息。

## `astrcode-extension-coding`

路径：`crates/astrcode-extension-coding`

职责：作为普通第一方扩展提供 `read`、`read_tool_result`、`write`、`edit`、`patch`、`glob`、`grep` 和 `shell`。schema、展示语义和工具级编排属于本 crate；路径约束、原子文件操作、artifact ID 解析、受监管进程、增量输出与生命周期所有权属于 Host。交互式 PTY 不在产品能力中：迁移前评估的 portable-pty 无法让 Host 在 spawn 前跨平台建立可证明的进程树所有权，因此最终没有保留该依赖或降级实现。

关键边界：只依赖 `astrcode-extension-sdk`，不直接访问 filesystem、artifact storage 或 process API。每个工具都先返回 `ToolPlan`，再由 session 完成权限与 lease，执行时 Host 对具体 operation 二次校验 lease。工具参数不包含 `maxOutputTokens` 一类平台调优字段；Host 有界增量读取与 session 统一 inline/artifact 预算对所有扩展提供相同结果控制。持久化结果通过 `ToolResultClient` 和 session-scoped opaque `artifactId` 读取，不伪装成 workspace path。

测试线索：SDK registration 测试覆盖工具集合、严格 schema、能力声明和 plan；宿主集成测试覆盖 workspace/process 的真实 Host 路由。

## `astrcode-extension-agent-tools`

路径：`crates/astrcode-extension-agent-tools`

职责：提供 `agent` 工具，用于派生子 Agent 执行委派任务。它通过 SDK 的 session operations 创建子 session、提交 turn，并把结果作为父 session 工具结果返回。

主要模块：

- `lib.rs`：扩展入口、`agent` 工具定义、参数解析、prompt contribution、工具 metadata、执行逻辑。
- `agent.rs`：发现 agent 配置，读取工作区内 agent 定义并缓存。

关键行为：

- 注册工具：`agent`。
- 能力声明：`SessionControl`、`SmallModel`。
- 支持同步等待和后台运行：`waitForResult=true` 阻塞到子 turn 完成；`false` 返回 task id，完成后由后台通知进入父 session。
- 子 Agent 固定使用配置的小模型；agent 文件内模型字段当前不生效。
- 子 session 默认 ephemeral，且通过 child tool policy 禁止子 Agent 再调用 `agent`，避免无限委派。
- `PromptBuild` 阶段注入可用 agent 列表，指导模型选择 `subagentType`。

依赖边界：只依赖 `astrcode-extension-sdk`。实际创建/提交/回收 session 由宿主通过 SDK 注入。

测试线索：单元测试覆盖 agent 列表格式化、工具 schema、camelCase 参数、缺失小模型错误等。

## `astrcode-extension-mcp`

路径：`crates/astrcode-extension-mcp`

职责：把 MCP server 暴露为 AstrCode 工具。它负责从配置发现 MCP server、预热进程池、列出工具、规范化工具名、提供 `tool_search_tool` 延迟发现，并执行具体 MCP 工具调用。

主要模块：

- `config`：读取全局和 workspace MCP 配置，生成 server 配置和诊断。
- `pool`：MCP process pool，负责启动、复用、健康检查、shutdown、call/list tools。
- `protocol`：MCP JSON-RPC 协议类型、tool/call result 渲染。
- `names`：MCP server/tool 名规范化为 `mcp__server__tool`。
- `search`：BM25/关键词式工具搜索和 `tool_search_tool` 输出。
- `http_client`：HTTP transport client 支持。
- `lib.rs`：扩展入口、缓存、预热 gate、tool discovery、handler、prompt contribution。

关键行为：

- 注册动态工具：一个 `tool_search_tool` 加若干 `mcp__...` 具体工具。
- 工具发现按 working_dir 缓存，启动/SessionStart/SessionResume 时预热。
- 首轮 discovery 会等待初始预热或同步 refresh，避免工具表为空。
- `tool_search_tool` 使用 deferred discovery metadata；模型先检索 schema，再调用具体工具。
- 重名或非法规范化工具名会记录 diagnostics 并跳过。

能力声明：`WorkspaceRead`、`ProcessSpawn`、`NetworkClient`。

依赖边界：只依赖 `astrcode-extension-sdk`。MCP 进程和网络访问通过扩展能力声明受宿主管控。

测试线索：单元测试覆盖 MCP 工具名转换、`tool_search_tool` 属性、prompt discovery 指令；`pool`、`protocol`、`search` 也有测试。

## `astrcode-extension-skill`

路径：`crates/astrcode-extension-skill`

职责：实现 Claude-style skill 发现和 `Skill` 工具。它把 skill index 注入 prompt，但只有模型明确调用 `Skill` 或 slash command 时才读取完整 `SKILL.md`，避免 prompt 常驻过大。

主要内容：

- 扩展入口 `extension()` 返回 `astrcode-skill`。
- 注册工具：`Skill`。
- 注册 command discovery：为每个 skill 暴露 slash command。
- 注册 `PromptBuild`：当 `Skill` 工具存在时，注入 skill 简短索引。
- `SkillShared`：按 working_dir 缓存发现结果。
- skill 发现：查找 `SKILL.md`，解析 frontmatter 和正文，控制索引长度。
- 工具调用：根据参数定位 skill，返回完整内容或错误。

能力声明：`WorkspaceRead`。

依赖边界：只依赖 `astrcode-extension-sdk`。frontmatter、hostpaths 等通过 SDK re-export 使用。

测试线索：单元测试覆盖 skill 发现、frontmatter、索引截断、工具参数、slash command 等。

## `astrcode-extension-todo-tool`

路径：`crates/astrcode-extension-todo-tool`

职责：提供 session-local 进度 todo 列表。它不做全局任务管理，只在当前 session 的扩展数据目录下维护一个 progress artifact，并通过 provider hook 在长时间未更新时提醒模型。

主要内容：

- 注册工具：`todoWrite`。
- 持久化位置：`<session>/extension_data/astrcode-todo-tool/todos/progress.json`。
- reminder 状态：`.reminder-state.json`。
- `ProgressListStore`：读取、替换、保存 todo 列表。
- `ProgressReminder`：在 `BeforeProviderRequest` 统计周期，必要时追加隐藏提醒消息。
- `TodoPostToolUseHandler`：`todoWrite` 后重置 stale 计数。
- 工具 UI：输出 `RenderSpec` 和 compact summary metadata。

关键规则：

- 每次调用提交完整列表，不是 patch。
- 最多一个 `in_progress`。
- 所有项目完成时清空存储。
- 多步骤列表全部完成但没有 verification/test/check 语义时，返回验证提醒。

能力声明：`ProviderRequest`、`ToolIntercept`。session-local todo state 使用默认 namespaced
session data path，不需要额外持久化 capability。

依赖边界：只依赖 `astrcode-extension-sdk`。session 数据目录由调用上下文的 `ctx.paths().session_data_dir()` 按扩展标识完成命名空间隔离。

测试线索：单元测试覆盖替换、校验、清空、verification nudge、provider reminder、post-tool 重置。

## `astrcode-extension-mode`

路径：`crates/astrcode-extension-mode`

职责：提供 agent 运行模式系统，目前内置 code/plan。模式通过扩展注入，不由核心系统硬编码。

主要模块：

- `catalog`：mode catalog、mode id、mode spec，定义 code/plan 模式能力和限制。
- `tools`：`switchMode`、`upsertSessionPlan` 的 tool definition 和 handler。
- `store`：mode state 和 plan artifact 的读写。
- `prompts`：进入/退出 plan 的 prompt 内容。
- `lib.rs`：扩展注册、pre-tool-use 限制、provider transition message、slash command、keybinding、status item。

关键行为：

- 工具：`switchMode`、`upsertSessionPlan`。
- Slash command：`/mode`，可切换或指定 `code`/`plan`。
- 快捷键：`shift+tab` 切换模式。
- 状态栏：`mode` 状态项显示当前模式。
- 持久化：`<session>/extension_data/astrcode-mode/mode/mode-state.json` 和 `plan/plan.md`。
- plan 模式的工具限制由 `PreToolUse` blocking hook 执行；yolo mode 下放行。
- 模式切换指令通过 `BeforeProviderRequest` 追加 user message，而不是改变 system prompt，利于 KV cache 稳定。

能力声明：`ProviderRequest`、`ToolIntercept`。session-local mode state 使用默认 namespaced
session data path，不需要额外持久化 capability。

依赖边界：只依赖 `astrcode-extension-sdk`。

测试线索：`tools.rs`、`store.rs`、`catalog.rs` 覆盖模式切换、计划写入和状态持久化。

## `astrcode-extension-ask-user`

路径：`crates/astrcode-extension-ask-user`

职责：独立拥有 `askUser` 交互。工具调用保持运行中，直到回答、拒绝、turn 取消、扩展停止或 300 秒超时后返回普通终态结果。

主要模块：

- `model`：问题、选项、答案和 pending HTTP 契约。
- `registry`：以 `(session_id, call_id)` 为键的并发安全 pending registry，保证解决操作只有一个赢家。
- `lib.rs`：工具注册、authenticated HTTP 路由、session/extension shutdown 清理，以及 live-only `ask_user.pending` / `ask_user.resolved` 事件。

关键边界：Web 客户端通过 `/api/extensions/astrcode-ask-user/sessions/{sessionId}/questions` REST 状态和扩展 SSE 事件恢复交互；这些事件不写入 durable EventLog，也不复用通用后端 UI DSL。

能力声明：`AuthenticatedHttp`、`EmitEvents`。

依赖边界：只依赖 `astrcode-extension-sdk`。

测试线索：端到端测试覆盖 pending 查询、回答、拒绝、超时、取消、shutdown、重复解决、错误 session 和认证边界。

## `astrcode-extension-goal`

路径：`crates/astrcode-extension-goal`

职责：提供 Codex-style session goal 状态机，让模型可以创建、查询、完成或标记阻塞一个当前目标，并在目标仍 active 时通过 `ContinueAfterStop` 请求宿主继续一个 agent step。

关键行为：

- 扩展 id：`astrcode-goal`。
- 工具：`getGoal`、`createGoal`、`updateGoal`。
- Slash command：`/goal`，可显示、创建、暂停、恢复、清空、complete 或 blocked 当前 goal；`/goal budget <n>` 在预算耗尽后调整总额并恢复 active。
- 持久化：`<session>/extension_data/astrcode-goal/goal/goal-state.json`。
- 续跑：注册 `ContinueAfterStopOptions::unlimited()` 的 blocking-only `ContinueAfterStop` decision hook；hook 只设置下一步续跑意图，真正的目标上下文由 `BeforeProviderRequest` 以非持久 provider-visible user message 注入，避免写入 durable transcript。
- Token 预算：声明 `SessionHistory` 后通过类型化 host client 汇总 `TokenUsageRecorded`，按 non-cached input + output 统计 goal token（分项缺失时从 provider `total_tokens` 扣除 cached input）；达到 `tokenBudget` 时系统把 goal 标为 `budget_limited`，再通过一次非持久 hidden provider message 让模型收尾，随后停止自动续跑。`budget_limited` 与 `paused` 的 goal 都可经 `/goal resume` 或 `/goal budget <n>` 恢复。

能力声明：`SessionHistory`、`ProviderRequest`、`TurnContinuationControl`。session-local goal
state 使用默认 namespaced session data path。

依赖边界：只依赖 `astrcode-extension-sdk`。session 数据目录、事件读取和 LLM/tool 类型都通过 SDK re-export 使用。

测试线索：`store.rs` 覆盖状态持久化和状态转移；`lib.rs` 覆盖 token 预算、prompt 注入文本和 token fallback 汇总。

## `astrcode-extension-memory`

路径：`crates/astrcode-extension-memory`

职责：持久化记忆扩展。它把用户偏好、项目事实和 turn 后召回从核心 agent loop 中移出，通过 extension hook 和小模型能力实现记忆保存、提取、索引和注入。

主要模块：

- `config`：memory 扩展配置。
- `store`：用户/项目 memory 文件和索引存储池。
- `index`：`memory_index.json`，支持 BM25/子串搜索和相似条目 upsert。
- `pipeline`：批量提取、更新 MEMORY.md 的 pipeline。
- `prompts`：记忆提取/召回 prompt。
- `scope`：用户记忆和项目记忆 scope 解析。
- `handlers`：`memory_save`、`memory_delete`、`memory_list`、prompt build、session start 等 handler。
- `turn_recall`：TurnEnd 项目事实召回、BeforeRequest 注入、session prefs cache。

关键行为：

- 用户记忆：`~/.astrcode/extension_data/astrcode.memory/`，偏向跨项目用户偏好。
- 项目记忆：`~/.astrcode/extension_data/astrcode.memory/projects/<key>/`。
- `SessionStart` 和 `memory_save` 后可触发批量提取并更新 `MEMORY.md`。
- `PromptBuild` 注入用户偏好，按 session 缓存。
- `TurnEnd` 对当轮对话召回项目事实，下一 turn 首次 LLM 请求注入。
- 工具：`memory_save`、`memory_delete`、`memory_list`。
- 事件：`memory.created`(`memory_save` 新增成功时发射)、`memory.deleted`(`memory_delete` 删除条目时发射)。

能力声明：`SmallModel`、`SessionInspect`、`EmitEvents`、`ProviderRequest`。启动时如果没有
small model 会失败；跨 session 的历史读取通过 `SessionInspect` client 完成。

依赖边界：只依赖 `astrcode-extension-sdk`。

测试线索：各子模块有单元测试，重点覆盖索引、store、pipeline、handler 和 recall。当前
authoring API 重构把数据根目录从旧的 `~/.astrcode/memory/` 与
`~/.astrcode/projects/<key>/extension_data/astrcode.memory/` 统一迁到上面的 extension-owned
目录；这是明确的不兼容变更，不做旧目录双读或自动迁移。

## `astrcode-extension-channels`

路径：`crates/astrcode-extension-channels`

职责：外部 channel 入口扩展，目前实现 Telegram bot polling。它把外部聊天消息映射为 AstrCode root session 的 turn，并把回复发回聊天。

主要内容：

- 扩展 id：`astrcode-channels`。
- 配置类型：`ChannelsConfig`，当前包含 `telegram` 子配置。
- Telegram 配置：启用开关、bot token 或 env 引用、allowlist、allow all、命令注册、streaming 预留、工作目录、超时、最大回复长度。
- `ChannelRuntime`：保存配置、chat 到 session 的映射、session operations 和 Telegram API。
- `poll_telegram`：long polling `getUpdates`，处理 shutdown token。
- `TelegramApi` trait 和 `HttpTelegramApi` 实现。
- inbound 处理：鉴权 chat id，处理 `/start`/`/help`，创建或复用 chat session，提交 turn，拆分回复。

能力声明：`InputDelivery`、`NetworkClient`。

依赖边界：只依赖 `astrcode-extension-sdk`。网络访问和 root session 输入投递分别由
`NetworkClient` 和 `InputDelivery` 授权。

测试线索：单元/异步测试覆盖 nested config、拒绝 flat config、token env 引用、命令、回复拆分、session 复用、未授权 chat 拒绝等。

## `astrcode-extension-web-tools`

路径：`crates/astrcode-extension-web-tools`

职责：提供网络信息获取工具：`web-search` 和 `fetch-url`。适合需要当前公开网页信息的任务，避免把网络能力放入核心工具层。

主要模块：

- `config`：扩展 id、工具名、搜索/抓取配置。
- `web_search`：搜索请求、结果类型、查询执行、搜索结果渲染。
- `fetch_url`：URL 抓取、正文提取、可选小模型处理、结果渲染。
- `url_guard`：阻止 localhost、私网、非 HTTP(S)、二进制等不安全或不适合的 URL。
- `http`：HTTP client 封装。
- `cache`：fetch URL 缓存，支持 TTL、条数和字节上限。
- `preapproved`：预批准域名或来源策略。
- `lib.rs`：扩展入口、共享配置、小模型注入和工具注册。

关键行为：

- `web-search` 并行执行，参数支持 `query`、`maxResults`、`allowedDomains`、`blockedDomains`。
- `fetch-url` 并行执行，参数为 `url` 和 `prompt`。
- `fetch-url` 明确拒绝 authenticated/private/local/binary URL。
- 抓取结果可用 small LLM 依据 prompt 做提取/总结。
- 工具结果 metadata 只携带 URL、数量、缓存状态等展示/诊断信息。

能力声明：`NetworkClient`、`SmallModel`。

依赖边界：只依赖 `astrcode-extension-sdk`。

测试线索：`web_search.rs`、`fetch_url.rs`、`url_guard.rs`、`cache.rs` 覆盖查询、抓取、缓存和安全边界。

## `astrcode-server`

路径：`crates/astrcode-server`

职责：后端 runtime。它把 session runtime、storage、context、tools、extensions、AI provider、协议 transport 组合成可服务 CLI/TUI/HTTP/Desktop/ACP 的后端。

目标：

- lib：`astrcode_server`
- bin：`astrcode-server`，stdio/JSON-RPC server 入口。
- bin：`astrcode-http-server`，HTTP/SSE server 入口。

主要模块：

- `bootstrap`：server system 启动、配置解析、`ServerRuntime`、`BootstrapOptions`、启动错误。
- `config_manager`：配置读取与 reload，并在 composition root 组装 provider、context、extensions、tools 和 post-compact 能力。
- `handler`：JSON-RPC command actor 和业务 handler。包含 prompt、compact、recap、session lifecycle、notifications、model selection、slash、snapshot、errors 等。
- `transport`：stdio transport 和 JSON-RPC 初始化/错误响应。
- `acp`：Agent Client Protocol 适配和事件转换。
- `http`：Axum HTTP server、认证、SSE stream、REST routes、conversation projection。
- `http/stream/replay`：校验 cursor、限制 replay 数量并决定是否要求 rehydrate。
- `http/stream/child_sessions`：维护父会话可见的 child lineage、compact leaf 与阶段投影。
- `session_manager`：session 创建、恢复、列表、删除、fork/recycle 等管理。
- `session_operations`：给扩展使用的、带 session 树访问校验的操作 facade。
- `turn_scheduler`：输入投递策略、turn 启动、队列、注入、完成后启动下一条。
- `turn_registry`：进程内活跃 turn 索引和 stale repair。
- `child_session`：子 session lifecycle、后台 agent 完成、回收策略。
- `server_event_bus`：session broadcast 到 client notification 的桥接。
- `task_utils`：带 tracing 的任务 spawn。
- `test_support`：`testing` feature 下的测试辅助 re-export。

关键行为：

- `TurnScheduler::deliver_input` 支持 `InjectIfRunningElseStart`、`QueueIfRunningElseStart`、`StartNew` 三种策略。
- HTTP 路由提供 session CRUD、session 工具选择、prompt、inject、compact、abort、fork、extensions、config、models、SSE 等。
- broadcast lag 时通过 rehydrate/snapshot 机制让客户端恢复。
- extension runtime 在 bootstrap 后绑定到 session manager、event bus、host services。

依赖边界：依赖几乎所有核心实现 crate：`astrcode-ai`、`astrcode-bundled-extensions`、`astrcode-context`、`astrcode-core`、`astrcode-extensions`、`astrcode-log`、`astrcode-protocol`、`astrcode-session`、`astrcode-session-projection`、`astrcode-storage`。它是后端 composition root；具体工具仅通过 bundled extension composition 接入。

测试线索：`tests/http_routes.rs`、`tests/session_operations_test.rs`、`tests/turn_scheduler_behavior_test.rs`、`tests/extension_integration_test.rs` 覆盖 HTTP、session operations、turn scheduler 和扩展集成；模块内测试覆盖 child session、session manager、HTTP projection 等。

## `astrcode-client`

路径：`crates/astrcode-client`

职责：typed JSON-RPC client SDK，供 CLI/TUI 或其他 Rust 客户端连接 `astrcode-server`。

主要模块：

- `client`：`AstrcodeClient<T: ClientTransport>`，封装 initialize、session、prompt、stream、config 等命令发送。
- `transport`：`ClientTransport` trait 和 transport 抽象。
- `stream`：服务端事件流异步接收器。
- `error`：`ClientError`。

依赖边界：依赖 `astrcode-core` 和 `astrcode-protocol`。不依赖 server 实现，便于测试和替换 transport。

测试线索：`tests/client_test.rs` 和 `client.rs` 内 mock transport 测试覆盖客户端命令行为。

## `astrcode-cli`

路径：`crates/astrcode-cli`

职责：命令行入口和 TUI。默认 workspace member 是它，产物名为 `astrcode`。

主要模块：

- `main.rs`：CLI 参数解析和子命令入口。
- `exec.rs`：非交互式执行模式。
- `transport.rs`：CLI 与 server 的 transport glue。
- `tui/mod.rs`：TUI 模块根。
- `tui/app.rs`、`tui/app/handle_event.rs`：TUI app 状态和事件处理。
- `tui/frame`：frame event stream。
- `tui/composer.rs`：输入区/编辑器。
- `tui/custom_terminal.rs`、`terminal.rs`、`terminal_probe.rs`：CLI 自身的终端 I/O、viewport 与能力探测；这是 TUI 展示层，不是 Coding Extension 已删除的 PTY process/`terminal` 工具。
- `tui/keybinding.rs`：键位处理。
- `tui/render`：scrollback 和 visual render spec。
- `tui/streaming`：流式输出 chunking、commit tick、controller。
- `tui/store`：transcript、session picker、child agent 状态。
- `tui/ext`：扩展消息、工具 UI fallback/builtin 渲染。
- `tui/clipboard_image.rs`：剪贴板图片输入。
- `tui/insert_history.rs`、`tui/command`：历史插入和 slash command。
- `tui/theme.rs`、`viewport.rs`、`tool_vocab.rs`：显示主题、视口和工具文案。

Feature：

- `dev-mode`：启用可选依赖 `astrcode-eval`。

依赖边界：依赖 client、server、protocol、core、log，并可选依赖 eval。CLI 同时可作为前端和本地 server 的启动入口；扩展作者契约由 server 在协议边界映射，TUI 不直接依赖 extension-sdk 或 context。

测试线索：`tests/end_to_end.rs` 覆盖端到端行为；TUI 子模块有较多单元测试，尤其是 viewport、render spec、streaming chunking、session picker、child agent store。

## `astrcode-log`

路径：`crates/astrcode-log`

职责：初始化全局 tracing subscriber，提供 stderr 和 file 两层日志输出。

主要内容：

- `LogOptions`：日志目录、stderr filter、file filter、是否启用 file/stderr。
- `init()`：默认初始化，返回必须保活的 `WorkerGuard`。
- `init_with()`：自定义初始化。
- `default_log_dir()`：默认 `~/.astrcode/logs/`。
- 文件日志：`astrcode-YYYYMMDD-HHMMSS-PID.log`，另写 `latest.logpath` 指向最新日志。
- 环境变量：`ASTRCODE_LOG` 覆盖 stderr level，`ASTRCODE_LOG_FILE` 覆盖 file level。
- 清理策略：保留 30 天日志，并兼容清理 legacy daily log 文件名。

依赖边界：依赖 `astrcode-core` 的产品级默认数据目录。它被 CLI/server/desktop 等入口使用。

测试线索：单元测试覆盖日志文件名不复用、latest pointer、旧日志清理。

## `astrcode-eval`

路径：`crates/astrcode-eval`

职责：Agent 任务评测框架。它面向 benchmark/eval，不参与主运行时核心路径。

主要模块：

- `case`：`EvalCase`、setup、judge config、case set 加载。
- `setup`：评测工作区准备。
- `runner`：`EvalRunner` 执行 case。
- `client`：`EvalClient`，和被测服务交互。
- `judge`：评分 judge。
- `metrics`：指标收集。
- `report`：`EvalResult`、`EvalReport`、`EvalSummary`。
- `adapter`：`BenchmarkAdapter` trait，用于接入不同 benchmark。

依赖边界：依赖 `astrcode-core` 的领域类型和 `astrcode-protocol` 的 typed wire
契约；它是协议消费者，不定义或复制服务端 DTO。`astrcode-cli` 通过 `dev-mode`
feature 可选依赖它。

测试线索：评测 fixture 位于 `eval-tasks/fixtures/*`，包含 `buggy-rust`、`implement-trie` 等独立项目。

## `astrcode-desktop`

路径：`src-tauri`

职责：Tauri v2 桌面壳。它不是 `crates/` 下的库 crate，而是 workspace 成员 binary，负责启动/协调本地 HTTP sidecar，并向 React 前端暴露 Tauri commands。

主要模块：

- `main.rs`：Tauri app builder，注册插件和 commands。
- `commands.rs`：Tauri command。包含启动 sidecar server、窗口最小化/最大化/关闭、sidecar state、HTTP token/端口响应等。
- `instance.rs`：单实例协调、锁文件、已有实例唤起/复用。
- `paths.rs`：`~/.astrcode`、instance lock/info 路径。
- `build.rs`：Tauri build hook。

关键行为：

- sidecar 模式运行 `astrcode-http-server`，动态选择本地端口和 auth token。
- 前端通过 HTTP API + SSE 与后端通信，并使用 `tauri-plugin-http` 绕过平台 webview 网络栈差异。
- 用 `fs2` 文件锁和 instance info 管理单实例。

依赖边界：不依赖 workspace 内部 Rust crate；通过 sidecar 进程和 HTTP 协议与后端交互。

测试线索：当前该 crate 主要依赖编译检查和桌面集成验证；改 commands 或 sidecar 协议时应同步前端调用方。
