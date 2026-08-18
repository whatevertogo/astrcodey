# astrcodey vs deepseek-harness 扩展能力缺口分析

> 基于两边源码逐条核对(54 条),astrcodey 侧所有结论均有 file:line 证据;deepseek 侧依据其 API 签名与文档。
> 结论分四档:【有】【部分有】【没有】,外加架构性差异(不算欠债)。
> 对比评价只针对两边都有的能力。
> 2026-08-18 按 deepseek master `47f9438` 重核:P0 五项已闭合,当前行动清单见「三、当前缺口清单(2026-08-18 重核)」。

## 一、总判断

- **astrcodey 系统性更强的维度**:准入/收窄/持久化/鉴权——类型编码的单调性(工具收窄不可放大)、durable steer(可重放、带鉴权)、provider contribution 的 prepare/acknowledge 两段结算、durable 事件管道(retry/dead_letter/防级联/运维 API)、capability 声明-注册-调用三重校验、路径 canonicalize 圈禁、s5r 协议规格 + conformance harness、prompt 固定顺序对 KV cache 友好。
- **deepseek-harness 系统性更强的维度**:可替换性与生态——每条能力都是可换 Provider 的 seam(e2b 沙箱、外部 agent CLI 委派)、LLM adapter 开放注册、UI 渲染扩展点、包分发体系(npm bundle + profile + patch 层叠)、多语言 SDK、agent 自修改、around 式包裹拦截(execute/request-error/stream)。
- **架构性差异(不建议计入补齐清单)**:cordis 的 `isolate`/`intercept`/依赖注入是为"共享服务总线"解毒;astrcodey 没有共享服务 realm(扩展只见自己 capability 内的窄 client),从源头上不需要。astrcodey 的"能力=扩展,替换=换扩展"对功能层(goal/mode/ask-user/web-tools)已自证可行,对基础设施层(fs/shell 实现)无效。

## 二、逐条缺口(按补齐优先级分组)

### P0 — 已全部闭合(2026-08-18 重核)

| # | 能力 | 闭合方式与证据 |
|---|------|---------------|
| 1 | `inject`(排队不唤醒)/ `followup`(排队并唤醒) | `queue_or_start`/`inject_or_start`/`defer_context` 已进 session.control wire 域(operation.rs:325/305/275),`QueueIfRunningElseStart` 原语对扩展开放 |
| 2 | 多语言扩展 SDK | Python SDK 已落地(sdks/python),刚补齐 4 项 parity:`dispose_root` 类型化方法、http route 注册、`on_shutdown`、`BackgroundHost` |
| 3 | 工具 UI 呈现 intent(deepseek `presentCall/presentResult`) | `ToolPresentation` 经 metadata 跨界(tool.rs:308-323,`PRESENTATION_METADATA_KEY`) |
| 4 | per-tool `timeoutMs` | `ToolExecutionPolicy.timeout: Option<Duration>`(tool.rs:59-63) |
| 5 | `deferContext`(工具执行中追加 user message) | session.control 域 `defer_context` 操作 + worker 侧 `WorkerInvocationContext.defer_context()` 便捷方法 |

另:后台驱动型扩展能力(s5r-3.0)同期闭合——`BackgroundHost` + session.root 域 4 个新操作(`root.create/state/submit_turn/dispose`,operation.rs:466/476/486/596)。当前协议面 49 个操作、19 个 hook 点 + 9 个类型化构造器;Rust worker SDK 49/49 全覆盖,内置与外部扩展同一 `HostRouter` 同一授权,仅余 2 处硬编码特权(astrcode-coding 敏感路径豁免、Bundled 工具 strict 优先级)与进程内独占注册面(keybinding/status item/discovery/hook priority/user_message_envelope)。

### P1 — 纯新增能力,不触动现有模型

> 2026-08-18 重核:#6/#7/#10 上调入新 P0,#12/#13/#14/#20 并入新 P1「当前缺口清单」节;本节表格保留作逐条证据,行动清单以该节为准。

| # | 能力 | 现状 | 补法要点 |
|---|------|------|---------|
| 6 | **request-error 接管**(LLM 失败扩展自定义 retry) | 完全无钩子,错误直接沿 TurnError 上抛(turn_runner.rs:515 起) | 新增 `on_provider_error` hook,返回 Retry/Abort;与 reactive compaction 重试(turn_runner.rs:325)协调顺序 |
| 7 | **around-execute**(包裹工具执行:换 signal/超时/重试/metrics) | 只有 pre/post,无 around(hook matrix:docs/extension-hook-matrix.md:57-69) | 新增 hook 族或扩展 PostToolUse 语义;注意与 plan/execute 资源租约的交互 |
| 8 | `concludeTurn`(工具正常收官当前 turn) | 只有 abort 语义的 cancel_turn(lifecycle.rs:75) | ToolExecutionResult 加变体或 ToolContext 加方法,走自然 stop 路径 |
| 9 | **LSP seam** | 全仓无 lsp 匹配 | 新 host 领域 client(参考 workspace client 的三处改动:wire 协议、SDK、host_router) |
| 10 | **settings 宿主服务**(命名空间+schema+watch) | 扩展只有静态 ExtensionConfig;ConfigStore 是部署期 seam(core/src/config/mod.rs:66) | 新 host 领域 client + 配置变更推送 |
| 11 | **credentials 服务** | 各扩展自管密钥(web_search.rs:179 读 env) | 新 host 领域 client,凭据引用而非明文 |
| 12 | **jobs 统一后台任务抽象** | 三件近似物各自为政:Session 寿命进程、后台 submit_turn、ExtensionTasks | 统一持久句柄 + 完成事件,可建在现有三者之上 |
| 13 | **LLM provider 注册** | provider 是编译期常量表(config/provider_catalog.rs:31)+ 固定枚举 wire format | 新 capability + 注册 seam;决定第三方能否接私有模型网关,生态生死项 |
| 14 | **subagent 外部委派**(委派给 codex/claude-code 等 CLI) | 子 agent 只能是同运行时子会话;ACP 只有 server 侧(astrcode-server/src/acp/mod.rs:32) | 做 ACP client 侧 provider seam |
| 15 | `whenIdle` / `runMaintenance` / cancel 带 keepInbox | 只能轮询 execution_view;cancel 固定 abort-queue-drain(lifecycle.rs:80) | session_control 加 await 式操作 + cancel 选项 |
| 16 | per-call `isConcurrencySafe(args)` | 只有静态 ExecutionMode(tool.rs:55) | ToolDefinition 加可选分类器回调(s5r 需多一次往返,权衡) |
| 17 | skill provider 注册 | skill 来源固定磁盘(astrcode-extension-skill/lib.rs:37) | 开放 catalog 贡献 seam |
| 18 | goal/planMode 程序化服务面 | 是内置扩展,其他扩展只能走 session_state/自定义事件交互 | 如需跨扩展编排,抽出类型化契约(自定义事件已可用) |
| 19 | 结构化多内容块工具结果(deepseek `ContentBlock[]`) | ToolResult 单一 String + metadata;唯一多模态通道是塞 JSON 的 ReadToolInlinePayload(tool/read_image.rs:11) | ToolResult 演进为类型化 content blocks,是 #3 的前提 |
| 20 | 外部 hooks 桥(Claude Code/Codex hooks.json) | 无任何解析;兼容只到 agent 定义 frontmatter(2026-08 重核:deepseek 侧已成体系——packages/hooks/ 下 hook-protocol + claude-code/codex 双桥,原样执行 hooks.json 映射 6 个原生扩展点) | 纯生态 shim,读旧配置跑 shell 映射到现有 hook 点 |

### P2 — 大工程 / 需要架构决策

> 2026-08 重核:#27/#31 与 #28 的「整体改写」部分上调入新 P0(LLM 请求改写链相关);#22/#23 deepseek 侧进展远超撰写时状态,见行内注与「当前缺口清单」节。

| # | 能力 | 现状 | 备注 |
|---|------|------|------|
| 21 | **Code Mode**(presentAs + codeRuntime + run_code 传输 + code-dispatch-log) | 完全无 | 涉及模型交互协议、执行沙箱、日志管线三层 |
| 22 | **OS 级 sandbox**(landlock/seatbelt) | 只有策略级圈禁(路径 canonicalize + network 域名/凭据限制) | 跨平台复杂;明确有不可信扩展场景再做(deepseek 侧已落地 fail-closed 圈禁 + per-session `sandbox/mode` durable 覆盖:ctx.sandbox/ctx.sandboxPolicy) |
| 23 | **PTY terminals** | 无 portable-pty 依赖,持久进程走管道 | 交互式终端程序的前提(deepseek 侧已有持久 PTY:ctx.terminals + subprocess.spawnTerminal) |
| 24 | workflowEngine | 无 | 从零建设 |
| 25 | **插件包管理/分发**(install/profile/组合) | 手动拷目录 + runtime.extensionStates 开关;无 CLI 子命令 | 生态地基;fingerprint reconcile(docs/extension-system.md:386)已是亮点 |
| 26 | agent 自修改(运行时动态加载插件 + 审批) | 只有人用的 reload_extensions HTTP 路由 | 安全敏感,需要审批流设计 |
| 27 | 逐 chunk stream 拦截 | after-response 只改 final_text 展示,不改 durable turn(docs/extension-system.md:346) | 与 transcript 完整性原则冲突,需决策 |
| 28 | system prompt 组装管线(命名 section/order/变量/整体改写) | PromptContributions 只有 4 个 Vec<String> 桶(hooks/types.rs:73),section 顺序固定是 KV cache 刻意设计(prompt_engine.rs:260) | 灵活性与缓存命中率的权衡,需决策 |
| 29 | sessionTitle provider / telemetry / spillStore | title=首条用户消息截取(session_lifecycle.rs:24);余无 | 小 seam,按需开 |
| 30 | 扩展配置 schema 声明 | ExtensionConfig 是黑盒 JSON(但 serde_path_to_error 报错体验好,runtime.rs:55) | 宿主/UI 想理解配置时才需要 |
| 31 | approval answerer 可替换(headless 自动审批) | answerer 固定为前端 UI,扩展只能发起 Ask | headless 部署的前提 |

### 架构性差异(不补)

- cordis `isolate`/`intercept`/依赖注入:astrcodey 无共享服务总线,需求被架构消解;跨扩展协作唯一通道是 public_http_dispatch(重,但显式)。
- fs/shell 实现可替换(e2b 远程沙箱):与 capability + 宿主权威模型冲突,除非要做远程执行,否则不值得。

## 三、当前缺口清单(2026-08-18 重核)

> 对照 deepseek-harness master `47f9438`(官方扩展点地图见其 `docs/cookbook/extension-cookbook.md:101-129`)与我方当前 API 面(49 个协议操作、19 个 hook 点,`crates/astrcode-extension-sdk/src/wire/operation.rs`)重核。每条注明 deepseek 侧参照、我方现状(file:line)与建议接入方式。上方旧表保留作证据,行动清单以本节为准。

### P0 — 应尽快补

> #1–#4(请求改写链四项)已按「统一链式原语」定型设计,见 `docs/provider-request-rewrite-chain-design.md`(2026-08-18 提案):扩展现有 `before_provider_request` 为类型化 fold 链,`ProviderRequestEffect` 封闭枚举 + 显式组合语义;#5(逐 chunk 流包裹)维持 decision-pending。

| # | 能力 | deepseek 参照 | 我方现状 | 接入方式 |
|---|------|--------------|---------|---------|
| 1 | 请求改写链·换模型 | `agent/request` waterfall 重写整个 `LlmCallConfig`(core/agent:244) | `before_provider_request` 输入含 messages(含系统消息)+ 只读 model,结果只有 Replace/Append/Block Messages;provider 实例在 hook 前已固定(turn_runner.rs:530),hook 应用为请求级非 durable(turn_runner.rs:846-876) | `ProviderResult` 加 `OverrideModel` 变体 + provider 解析挪到 hook 后 |
| 2 | 请求改写链·整段重写 prompt | `system-prompt/assemble` waterfall 整体改写(core/system-prompt:31) | `prompt_build` 纯追加四个 section,输入无当前 prompt 文本(extension/hooks/types.rs:74-83) | `PromptContributions`/`PromptBuildResult` 加 replace 语义 |
| 3 | 请求改写链·改工具列表 | 同 #1(工具集在 `LlmCallConfig` 内) | hook 输入无 tools;tools 在 hook 后才拼入请求(turn_runner.rs:891);`configure_tools` 是 session 级按名过滤、只能收窄,非请求级 | `ProviderHookInput` 加 `tools` 字段 + `ProviderResult::ReplaceTools` 变体 |
| 4 | 请求改写链·重试接管 | `agent/request-error` waterfall 自定义 retry(core/agent:260),`llm-retry` 为示范插件 | 完全无介入点,错误沿 `TurnError` 直接上抛 | 新增 `ProviderEvent::RequestError` hook,结果枚举 `Retry{delay}`/`Fail`/`RetryWithOverride`;与 reactive compaction 重试协调顺序 |
| 5 | 请求改写链·逐 chunk 流包裹 | `llm/stream` waterfall 逐包包裹(llm/llm:64) | 只有 BeforeRequest/AfterResponse 两个时机;after-response 可 Block、可改最终输出文本,但不改已落盘 assistant 消息 | 新增逐 chunk 流 hook;与 transcript 完整性原则冲突,先决策(原旧表 #27) |
| 6 | session fork | `SessionStore.fork()`(core/session:830-1081) | **已闭合(2026-08-18)**:`astrcode.session.root.fork`(`input_delivery` capability),授权为「本扩展拥有的顶层会话或当前调用上下文会话」,产出归属调用扩展的新 root(prompt 前缀与指纹继承);root.create 同期增加可选 system_prompt/model_preference/tool_selection 定制 | 已落地,无需行动 |
| 7 | 扩展级托管 KV / settings | `ctx.storage`/`storageDomain`(json/sqlite 后端)+ `ctx.settings`(命名空间+schema+watch,settings:479-564) | 扩展只有静态 `ExtensionConfig` 与 session 命名空间 state 读写(operation.rs:495/505);无跨会话托管 KV、无 schema/watch | 新 host 领域 client + wire op 组(参考 workspace client 的 wire 协议/SDK/host_router 三处改动套路) |
| 8 | approval seam(扩展注册审批应答方) | `approval/request` waterfall,answerer 可替换,headless 可自动审批(user-approval:17-30) | answerer 固定为前端 UI,扩展只能在工具守卫里发起 Ask | 新 hook/注册 seam:扩展登记 answerer,宿主按优先级路由审批请求 |
| 9 | 工具执行 around 包裹(超时/重试/metrics) | `tools/execute` around,可换 `exec.signal`(core/tools:163) | 只有 pre/post 两个时机(docs/extension-hook-matrix.md:57-69) | 新增 around 语义 hook 族;注意与 plan/execute 资源租约的交互 |

### P1 — 纯新增,不触动现有模型

| # | 能力 | deepseek 参照 | 我方现状 | 接入方式 |
|---|------|--------------|---------|---------|
| 10 | LLM adapter 开放注册 | `llm.registerAdapter/registerConfigurableProviders/registerModelDiscovery`(llm/llm:338-913) | provider 是编译期常量表(config/provider_catalog.rs:31)+ 固定枚举 wire format | 新 capability + 注册 seam;生态生死项 |
| 11 | subagent 外部 CLI 委派 | `subagents.registerProvider`,codex/claude-code/ACP/dsh-sdk 均有现成 provider(subagent:212-414) | 子 agent 只能是同运行时子会话;ACP 只有 server 侧(astrcode-server/src/acp/mod.rs:32) | ACP client 侧 provider seam |
| 12 | compaction 引擎可换 | `ctx.compaction`(compactIfNeeded/compactNow/compactRegion,可换引擎,compaction:96-164) | 引擎内置在 turn_runner,reactive compaction 不可替换 | 引擎 trait 抽出 + 注册 seam |
| 13 | web provider 注册 | `ctx.web.registerSearchProvider/registerFetchProvider`(web:103-157) | search/fetch provider 固定内置 | 开放 catalog 贡献 seam(同旧 #17 skill provider 套路) |
| 14 | 宿主监管后台任务 + 持久定时 | `ctx.jobs`(持久句柄/完成事件/attachController,jobs:62-176)+ schedule(durable cron,事件日志 fold,schedule:40) | `ExtensionTasks` 进程内独占;worker 只有自带 tokio 任务 + `BackgroundHost`,不受宿主代际监管,无持久 cron | 统一 jobs 抽象(持久句柄 + 完成事件)建在现有三件近似物上;schedule 走 durable 事件 + 新 wire op |
| 15 | Claude Code / Codex hooks.json 生态桥 | packages/hooks/:hook-protocol + claude-code/codex 双桥,原样执行 hooks.json 映射 6 个原生扩展点(hooks-claude-code:206-295) | 无任何解析 | 纯生态 shim:读旧 hooks.json 跑 shell,映射到现有 hook 点 |

### P2 — 定位选择,需产品决策

| # | 能力 | deepseek 参照 | 我方现状 | 备注 |
|---|------|--------------|---------|------|
| 16 | UI slots 扩展面 | `SlotRegistry` + `SlotMap` 声明合并,`dsh.client` 双面包动态加载(client/runtime slots.ts:93) | 前端/TUI renderer 注册表只收内置;`ToolPresentation` 只解决工具呈现 | 前端架构决策,不解决通用 UI 槽位 |
| 17 | OS sandbox seam | `ctx.sandbox` fail-closed 圈禁(landlock/seatbelt/bwrap/windows-acl)+ `ctx.sandboxPolicy` per-session 覆盖(sandbox:158-175) | 只有策略级圈禁(路径 canonicalize + network 域名/凭据限制) | 有不可信扩展场景再做 |
| 18 | HMR / 声明式加载树 | cordis.yml overlay/patch 层叠 + `ctx.hmr` 热重载(vendor/loader、vendor/hmr) | 手动拷目录 + `reload_extensions` 路由 | 与插件包管理/分发(旧 #25)同属生态地基 |
| 19 | PTY 终端 | `ctx.terminals` 持久 PTY + `subprocess.spawnTerminal`(terminal:50) | 无 portable-pty 依赖,持久进程走管道 | 交互式终端程序的前提 |
| 20 | E2B 远程沙箱 | `ctx.e2b` owner + fs-e2b/subprocess-e2b 适配器(e2b:74) | 无;fs/shell 实现不可换 | 与 capability + 宿主权威模型冲突,除非要做远程执行 |

沿用判断:deepseek 侧无 plan/execute 资源租约、无类型化单调收窄、无 durable 事件管道运维面,本次重核未影响这些「我方更强」的结论。

## 四、两边都有的能力:实现对比

| 能力 | astrcodey | deepseek | 谁好 |
|------|-----------|----------|------|
| steer 注入 | durable、可重放、跨 session 鉴权;step 边界生效 | 当前 step 即时生效;不持久 | **astrcodey**(持久+鉴权);deepseek 粒度细半拍 |
| step 前改写消息 | on_before_provider_request:Allow/Block/Replace/Append + provider_contribution prepare/ack 两段结算 | agent/pre-step:reject/enter(messages) | **astrcodey**(表达力+结算生命周期);代价是扩展要理解 request 重试语义 |
| 工具准入守卫 | priority 串行、任一 Block 短路、Ask 分级、按工具名定向 | guard 单调拒绝,声明式单用途 | **astrcodey**(Ask 中间档+定向);另有 plan/execute+资源租约,deepseek 完全没有 |
| 动态工具过滤 | SessionToolSelection 收窄单调性进类型 + discovery gate 渐进披露(一等执行结果) | restrict 命令式过滤 | **astrcodey 明显更强** |
| turn 续跑 | ContinueAfterStop 带 max_per_turn 预算,注册时声明上限 | turn-stopping 可内嵌 steer 消息,原子 | 各有优劣:astrcodey 安全预算好,deepseek 表达力好 |
| 并发分类 | 静态 ExecutionMode,派发零开销 | isConcurrencySafe(args) 运行时精确 | **deepseek** 精确;astrcodey 保守便宜 |
| 事件系统 | durable 管道:声明制、背压、retry/dead_letter、防级联、checkpoint/pause/replay 运维 API | 自由 on/emit + declaration merging,零约束 | 不同物种:不可信子进程场景 **astrcodey 对**;轻量解耦 deepseek 灵活 |
| 配置报错 | serde_path_to_error 带路径 | Schemastery schema 可被宿主/UI 理解 | 单点质量 astrcodey 好;声明式体系 deepseek 好 |
| 子 agent | 子会话=一等会话(独立事件流/工具选择/token 统计/ephemeral 回收) | provider 可换,能委派外部 CLI | 可观察性 astrcodey 好;生态接入 deepseek 好 |
| prompt 注入 | 固定槽位,KV cache 前缀稳定 | 命名有序 section+变量+整体改写 | 缓存 astrcodey 好;灵活性 deepseek 完胜 |
| 扩展协议 | s5r 独立规格 + conformance harness + 稳定错误码 | SDK-first(TS/Python 现成) | 规格化 astrcodey 高;开箱即用 deepseek 胜 |
| goal/planMode/ask-user | 普通扩展建在公开 SDK 上,天然可替换 | 核心服务,程序化操控直接 | 模型内聚性 astrcodey 好;编排集成 deepseek 直白 |

## 五、补齐路线图建议

> 2026-08-18:第一波 P0 五项已全部完成(见「二、逐条缺口」P0 节);当前行动清单以「三、当前缺口清单(2026-08-18 重核)」为准,以下为原始波次规划存档。

1. **第一波(P0 全做)**:暴露 QueueIfRunningElseStart、Python SDK、RenderSpec 跨界、timeoutMs、defer_context——全部是"地基已有,只差暴露"。
2. **第二波(生态生死)**:LLM provider 注册(#13)、插件包管理(#25)、settings/credentials(#10/#11)。
3. **第三波(干预能力)**:request-error(#6)、around-execute(#7)、concludeTurn(#8)、jobs(#12)。
4. **第四波(重投入,按场景决策)**:LSP(#9)、外部 agent 委派(#14)、Code Mode(#21)、sandbox(#22)、PTY(#23)。
5. **需先决策再动**:#27(流拦截 vs transcript 完整性)、#28(prompt 灵活性 vs KV cache)、#26(自修改的安全模型)。
