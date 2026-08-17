# AstrCode 项目待办事项

## 中优先级

- [ ] **插件系统 / 扩展（s5r + SDK）**
  - [ ] **宿主能力补齐（`HostRouter` / wire）**
    - [x] `astrcode.session.control.create` 透出 `tool_selection`（外置 agent 禁嵌套 `agent`）
    - [x] 外置扩展安全路径下的同步子 Agent — 已落地后台路径：`Worker::background_host()` + root session 域（`create_root` 显式 `working_dir`、unscoped `submit_root_turn(wait_for_result: true)`、`root_state` / `dispose_root`）；handler 内 `wait_for_result: true` 维持拒绝（有意，防 admission permit 死锁）
    - [x] 实现 `astrcode.process.spawn`、`astrcode.network.client`（并发、总超时、取消与 I/O 大小均有上限）
    - [x] 实现 public HTTP 路由与跨插件公开路由分发（含 s5r manifest/handler E2E）
    - [x] 实现 workspace `list` / `grep` / `glob` 与 session 中断、注入、取消、执行视图
  - [ ] **外置扩展与内置能力对齐**
    - [x] `S5rToolHandler` 透传 turn 取消 → `InvokeContext.cancel_token`（`peer.rs` → `registry.rs` → `host_router.rs` → `session.rs` 全链路已打通）
    - [ ] s5r 支持 `tool_metadata` / `ToolDiscovery`（对标 MCP 动态工具、agent prompt 元数据）— 内置扩展完整支持，s5r wire 协议尚无 `tool_metadata` / `ToolDiscovery` 字段
    - [x] custom event 统一经声明校验后的 `EventClient::emit` / `CustomEventEmitter::emit` 发射；裸 sink 仅留在 doc-hidden runtime internal boundary
  - [ ] **SDK 开发体验（Worker）**
    - [x] `tool_handler` / `tool_handler_args`、manifest 与 handler 一体注册（`Worker::tool`）
    - [x] Handler 错误类型 `ErrorPayload`；`HostApi` + task-scoped `with_host_api` transport seam 可测
    - [x] [`extension-author-guide.md`](extension-author-guide.md)（含外置 agent-tool 指引）
    - [ ] `#[handler]` 过程宏（可选，进一步减样板）
    - [ ] Python SDK 对齐 `BackgroundHost` 等价物（Rust 已先行；Python `ContextVar` 模型下的对应物是 asyncio task 外的显式 binding）
    - [ ] Handler 运行时进度 / 日志上报通道（协议 + SDK API）
    - [ ] 合并 / 澄清 `prelude` 与 `worker_prelude` 文档入口（README 链到 author guide）— 两个模块都存在且 author guide 已有解释，但 SDK crate 无 README 链接
  - [ ] **内置扩展 vs 外置部署策略**
    - [ ] 明确各内置 crate（agent-tools / mcp / skill / todo / mode / goal / memory / channels / web-tools）的外置替代矩阵与默认开关
    - [ ] MCP 保持独立桥接层，不与 s5r 合并（文档中写清边界）
  - [ ] **测试与 CI**
    - [ ] 外置 agent-tool 最小 E2E（`session_control` + `prompt_build` + 后台 submit_turn）
    - [ ] CI 可选构建 `examples/` 或模板外置扩展工程
- [ ] 审批插件安全流程（通过 hook 实现）
  - [ ] 危险操作确认机制
  - [ ] 策略引擎集成点
  - [ ] 审计日志增强
- [ ] ACP 协议完善
- [ ] CodeGraph
- [ ] **Conversation timeline C：SQLite 物化读模型**
  - [ ] **触发条件**：真实长会话 soak 仍出现不可接受的内存增长，或 JSONL 深度翻页延迟随历史长度明显增长；触发前保留当前有界 JSONL reader，避免提前引入双存储复杂度
  - [ ] **稳定边界**：HTTP 继续只依赖 [`ConversationTimelineReader`](../crates/astrcode-server/src/http/conversation_timeline.rs)，保持现有 state/items DTO、不透明版本化 cursor、SSE 增量与前端窗口协议不变；SQLite 实现不得泄漏表结构到 wire contract
  - [ ] **数据所有权**：durable JSONL 仍是唯一事实来源；SQLite 只是可删除、可重建的 read model，由单一 projector 按 `(session_id, event_seq, item_index)` 幂等消费，持久化 projection watermark 与 schema version
  - [ ] **查询模型**：按 session 与 timeline position 建索引，直接读取 bounded suffix；普通分页不得回扫 JSONL，也不得反序列化完整 `SessionForked.messages`
  - [ ] **一致性**：定义 snapshot watermark 与 SSE replay cursor 的握手；覆盖投影落后、重复投递、进程崩溃、日志截尾、fork、compaction、旧 schema 迁移与重建中断
  - [ ] **迁移路径**：实现 SQLite reader → 从 JSONL backfill → shadow read 与 JSONL 结果对比 → 按 session 灰度切换 → 保留一键回退 JSONL 与全量 rebuild；不做双写事实源
  - [ ] **验收标准**：首屏和任意深度分页的内存保持有界，分页延迟不再随日志总长度线性增长；重启/重建后 block 顺序、状态、cursor、fork 历史与 SSE 无缺失、无重复；完成真实 6 GB 复现场景的 Electron renderer 与后端 RSS soak

## 扩展能力对齐(deepseek-harness 对照,2026-08 缺口分析)

> 完整分析见 `docs/reviews/extension-gap-analysis.md`。P0 已完成(queue_or_start/defer_context、per-tool execution timeout、presentation intent、Python s5r SDK);以下为 P1(纯新增)与 P2(大工程/需架构决策)。

### P1 — 纯新增能力

- [ ] **request-error 接管**:新增 `on_provider_error` hook,扩展可对 LLM 调用失败返回 Retry/Abort;与 reactive compaction 重试(`turn_runner.rs:325`)协调顺序
- [ ] **around-execute hook**:包裹工具执行(换 signal/超时/重试/metrics),现有只有 pre/post;注意与 plan/execute 资源租约的交互
- [ ] **concludeTurn**:工具正常收官当前 turn(区别于 `cancel_turn` 的 abort 语义,`lifecycle.rs:75`)
- [ ] **LSP host 领域 client**:locations/hover;按 wire 协议 + SDK + host_router 三处改动套路新增
- [ ] **settings 宿主服务**:命名空间 + schema 注册 + get/update/watch + 变更推送(现仅静态 `ExtensionConfig`)
- [ ] **credentials 宿主服务**:凭据引用(resolve/describe/set/unset),替代各扩展自管密钥
- [ ] **jobs 统一后台任务抽象**:持久句柄 + 完成事件;整合 Session 寿命进程、后台 submit_turn、ExtensionTasks 三件近似物
- [ ] **LLM provider 注册 seam**:新 capability + registerAdapter/registerModelDiscovery 对等物,放开 `provider_catalog.rs` 编译期常量表(生态生死项)
- [ ] **subagent 外部委派**:ACP client 侧 provider,可委派给 codex/claude-code 等外部 agent CLI(现仅有 ACP server 侧)
- [ ] **session_control 补强**:`when_idle` await 式操作、`cancel_turn` 带 `keep_inbox` 选项
- [ ] **per-call 并发分类器**:`ToolDefinition` 可选 `is_concurrency_safe(args)` 回调(现为静态 `ExecutionMode`;s5r 需权衡往返开销)
- [ ] **skill provider 注册**:开放 skill catalog 贡献 seam(现来源固定磁盘)
- [ ] **goal/planMode 程序化服务面**:跨扩展编排用的类型化契约(现为内置扩展,只能靠 session_state/自定义事件交互)
- [ ] **结构化多内容块工具结果**:`ToolResult` 从单一 String 演进为类型化 content blocks(是 UI 呈现 intent 完整版的前提)
- [ ] **外部 hooks 桥**:读 Claude Code / Codex `hooks.json` shell hook 配置映射到现有 hook 点(生态迁移 shim)

### P2 — 大工程 / 需架构决策

- [ ] **Code Mode**:`present_as` + codeRuntime + `run_code` 传输 + code-dispatch-log(模型交互协议、执行沙箱、日志管线三层)
- [ ] **OS 级 sandbox**:landlock / sandbox-exec(现为策略级圈禁;有不可信扩展场景再做)
- [ ] **PTY terminals**:持久 PTY 会话(现持久进程走管道,无 portable-pty 依赖)
- [ ] **workflowEngine**:workflow 脚本引擎 + WorkflowRun + workflow/* 事件
- [ ] **插件包管理/分发**:install 子命令、profile/bundle 组合、版本解析(现为手动拷目录 + `runtime.extensionStates` 开关)
- [ ] **agent 自修改**:运行时动态加载/卸载扩展 + 审批流(现仅人用的 reload_extensions 路由;安全敏感需先设计)
- [ ] **逐 chunk stream 拦截**:与 transcript 完整性原则冲突,先决策(现 after-response 只改 final_text 展示)
- [ ] **system prompt 组装管线**:命名有序 section / 变量插值 / 整体改写;与 KV cache 前缀稳定(`prompt_engine.rs:260`)权衡,先决策
- [ ] **session title provider / telemetry / spillStore**:小 seam,按需开
- [ ] **扩展配置 schema 声明**:让宿主/UI 能理解扩展配置(现 `ExtensionConfig` 为黑盒 JSON)
- [ ] **approval answerer 可替换**:headless 自动审批(现 answerer 固定为前端 UI)

## 较低优先级

- [ ] 会话 Fork 分支点管理
- [ ] 引入 fd、rg 等外部依赖，可选配置工具执行策略（builtin / external / auto）
- [ ] AgentTeam 插件
  - [ ] AgentSendTool
  - [ ] 聊天室
  - [ ] 主 agent task 分发
- [ ] 文档完善
  - [ ] API 文档自动生成
  - [x] 扩展开发指南（[`extension-author-guide.md`](extension-author-guide.md)、[`extension-system.md`](extension-system.md)、[`s5r-protocol.md`](s5r-protocol.md)）
  - [x] README 中英文同步（crate 统计、web-tools 扩展、行数更新）
  - [x] 发布指南（[`release.md`](release.md)，含版本同步、npm/GitHub 分发与 weekly release 行为）
