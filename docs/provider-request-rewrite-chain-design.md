# Provider 请求改写链设计(provider_request_chain,统一链式原语)

> 状态:提案(2026-08-18)。来源:`docs/reviews/extension-gap-analysis.md`「三、当前缺口清单」
> P0 #1–#4(换模型 / 整段重写 prompt / 改工具列表 / 重试接管)。deepseek 侧参照:
> `agent/request` waterfall 重写整个 `LlmCallConfig`(core/agent:244)、`system-prompt/assemble`
> (core/system-prompt:31)、`agent/request-error`(core/agent:260,`llm-retry` 示范插件)。
> 本文所有 astrcodey 侧 file:line 已对照主线核实。

## 0. 问题:四个缺口的共同根因

dsh 用一个 waterfall 机制(veto-able 中间件链,`vendor/cordis/src/events.ts:225-243`)
同时获得换模型、改 prompt、改工具、重试接管、流包裹五种能力;astrcodey 的 19 个 hook 点
每个都是定制工程,provider 请求路径只暴露 `before_provider_request` 一个改写时机,且:

1. **provider 在 hook 前已固定**:`TurnLoop.llm` 是构造期字段(turn_runner.rs:69),turn
   开始时由 `llm_for_model_id` 解析(session_turn.rs:131-146)、`pin_turn_generation` 原子钉住
   (session_runtime_services.rs:226-254);`PreparedProviderRequest.llm` 只是字段拷贝
   (turn_runner.rs:530)。
2. **hook 结果只有消息维度的 4 个变体**:`ProviderResult::Allow/Block/ReplaceMessages/
   AppendMessages`(extension-sdk hooks/results.rs:116),输入无 tools(s5r/hooks.rs:49-57)、
   model 只读。
3. **tools 完全绕过 hook**:`LlmRequest::new(send_messages, tools.to_vec())` 在
   `start_provider_stream` 内拼装(turn_runner.rs:891),链无法触及。
4. **错误路径零介入点**:错误沿 `TurnError` 直接上抛;仅有的重试是 provider 内部 HTTP
   重试(astrcode-ai/common.rs:417-445,不可见)与 reactive compaction(仅
   ContextWindowExceeded,step_body:327-331/346-350 双捕获)。
5. **链信息被压扁**:`emit_provider` 逐 handler fold 后把整条链压成单个
   `ReplaceMessages`(runner/mod.rs:1705-1715),多 handler 组合语义不可见。

结论:不补 4 个定制 hook,而是把现有 `before_provider_request` 升级为**类型化链式原语**——
host 维护链状态逐 handler fold,效果是封闭枚举,组合语义显式定型(下文 §4)。

## 1. 核心决策(速览)

| 问题 | 决策 | 依据 |
|---|---|---|
| 原语形态 | 扩展现有 `before_provider_request`,不新建 hook | messages/tools/model 决策在 prepare_stage 同刻发生;两阶段 contribution 结算已绑定此 hook 点(runner/mod.rs:1724/1801) |
| 效果类型 | 单 handler `ProviderRequestEffect` + 聚合 `ProviderRequestChain` 分离 | `PreToolUseResult`→`PreToolUseAdmission` 先例(results.rs:56/77) |
| Provider 解析 | 链后解析;`TurnLoop.llm` 保留为 session 默认 | override 是请求级效果,与 ReplaceMessages 同刻度 |
| 工具改写 | 只能收窄(⊆ 当前可见集) | `SessionToolSelection::restrict`(session.rs:261)单调收窄原则 |
| 重试 | 新 hook 点 `provider_request_error`,只拦 `start_provider_stream` 错误路径 | 流中途重试违反 transcript 完整性 |
| Prompt 整段重写 | 请求级(`ReplaceMessages` 覆盖 system 消息);`prompt_build` 保持 append-only | `is_stable()` KV-cache 前缀承诺(prompt_engine.rs:260) |
| hook 模式 | 强制 Blocking | 请求改写必须同步完成 |
| 能力 | 复用 `provider_request`,不新增 | 同一信任域 |
| wire | feature `provider_rewrite_v1`,最后独立阶段 | `deactivate_v1` 先例(docs/s5r-deactivate-design.md) |

## 2. 目标链路(prepare_stage 重排)

现状(crates/astrcode-session/src/turn_runner.rs:520-587):

```text
snapshot_model(528) → compaction plan(533,用 session 默认 limits)
  → prepare_provider_history(535-543) → request_messages + dedup reminder(545-549)
  → ProviderRequestId::new(550)
  → before_provider_request hook(551-553 → apply 833-878)   ← 只有消息改写
  → token budget(554-579,llm 是 530 处的 pre-hook 拷贝)
  → PreparedProviderRequest { llm, request_id, messages, max_output_tokens, acks }
  → (tools 旁路)llm_stage(344)→ start_provider_stream → LlmRequest::new(891)
```

重排后(★ 为改动点):

```text
1. snapshot_model + context_snapshot                    (不变)
2. compaction plan(用 session 默认 provider limits)    (不变——见 §10 已知限制)
3. prepare_provider_history                             (不变)
4. request_messages + dedup reminder                    (不变)
5. ProviderRequestId::new                               (不变)
6. ★ chain fold:apply_provider_request_chain()          (原 hook 位点,产出 ProviderRequestChain)
7. ★ provider 解析:chain.model_override
       → Some(id) ⇒ host.llm_for_model_id(id)            (复用 session_turn.rs:131-146 路由,含 small_llm 分支)
       → None     ⇒ session 默认(Arc::clone(host.llm))
       未注册 model_id ⇒ TurnError(错误信息列出 candidate_models)
8. ★ tools = chain.tools_override ∩ 校验后 ⊆ 可见集,否则 unwrap_or(visible_tools)
9. token budget(554-579)改用解析后 llm 的 model_limits() / minimum_output_tokens()
10. ★ PreparedProviderRequest 增加 tools 字段;llm_stage / start_provider_stream
       改用 prepared.tools(替代 891 处旁路拼接)
```

`TurnLoop.llm` 字段**不动**:compaction(107-119 compaction_host)、构造与 usage fallback
继续用它;override 只影响本次请求的 `PreparedProviderRequest.llm`。

小窗口安全网已存在,无需新增:`step_body:327-331` 已把 prepare 阶段的
`ContextWindowExceeded`(含 override 到小窗口后 `request_max_output_tokens` 返回 None 的
路径,572-579)路由进 `recover_or_fail` → reactive compaction。

## 3. 类型设计

```rust
// crates/astrcode-extension-sdk/src/extension/hooks/results.rs(草案)

/// 单 handler 返回的请求改写效果。封闭枚举,in-process 不 serde(wire 见 §7)。
pub enum ProviderRequestEffect {
    Allow,
    Block { reason: String },
    /// 整段替换发送列表(含 system 消息——请求级 prompt 整段重写走这里,#2)
    ReplaceMessages { messages: Vec<LlmMessage> },
    AppendMessages { messages: Vec<LlmMessage> },
    /// 覆盖本次请求的模型(#1);None = 回退 session 默认(取消先前 override)
    OverrideModel { model_id: Option<String> },
    /// 请求级工具列表(#3)。只允许收窄,见 §4。
    ReplaceTools { tools: Vec<ToolDefinition> },
    /// 请求级输出预算覆盖,物化时钳制到 provider 上限
    OverrideMaxOutputTokens { tokens: usize },
}

/// 链 fold 后的聚合状态(遵循单 handler 结果与聚合决策分离的先例)。
pub struct ProviderRequestChain {
    pub model_override: Option<String>,
    pub tools_override: Option<Vec<ToolDefinition>>,
    pub max_output_tokens_override: Option<usize>,
    pub messages: MessagesRewrite,
    pub blocked: Option<String>,
}

pub enum MessagesRewrite {
    Unchanged,
    Replaced(Vec<LlmMessage>),
    Appended(Vec<LlmMessage>),
}
```

Context 扩展(`contexts.rs`,`pub(crate)` 构造不变,host-attributed 原则不破):

- `ProviderContext` 新增只读字段:`model`(当前值——**已累积 override 后**,供后续
  handler 判断)、`candidate_models: Vec<String>`(host 支持集,含 small_llm 路由目标)、
  `tools`(链当前工具列表快照)。
- `ProviderPayload` 新增 `pub(crate)` mutator:`override_model` / `replace_tools` /
  `override_max_output_tokens`(对齐既有 `replace_messages/append_messages`,
  contexts.rs:376-383)。

后续 handler 读到的是已累积状态——与 `transform_tool_input` 的 fold 语义一致
(runner/mod.rs:1546),不同于 `emit_pre_tool_use` 的「全员看同一份输入」(1571):
改写链是**协作改写**,守卫链才是**独立表决**,两者语义本就该不同。

## 4. 组合语义(本设计的核心定型)

按 priority 降序派发(HandlerIndex 既有语义:index.rs,Reverse(priority) + 扩展 id 序 +
注册序 tie),逐 handler fold:

| 字段 | 规则 | 先例依据 |
|---|---|---|
| `Block` | **any-wins,立即短路**(后续 handler 不再执行) | PreToolUse any-Block-wins(runner/mod.rs:1571) |
| `messages` | **顺序 fold**:Replace 重置基线,Append 追加;多个 Append 叠加 | 现有 apply 逻辑(turn_runner.rs:854-874) |
| `model_override` | **last-wins**(OverrideModel{None} 可回退) | 标量字段,无部分合并语义 |
| `tools_override` | **last-wins,且必须 ⊆ 物化时的可见工具集**;违例 → 运行时降级为交集 + tracing warn | `SessionToolSelection::restrict`(session.rs:261)+ configure_tools 只收窄原则:扩展不能借请求级改写注入 host 未授权工具 |
| `max_output_tokens_override` | last-wins,物化时 `min(tokens, provider.max_output_tokens)` | 标量字段 |
| handler 异常/超时 | 该 handler 视为 `Allow`(非 Block),记 error 事件 | emit_provider 现状 |

物化规则(链结束 → `PreparedProviderRequest`):

- `blocked = Some(reason)` ⇒ `TurnError::ProviderBlocked`(现行为,853)。
- messages:Replace 结果仍过 `provider_visible_messages` 过滤(854-864);Append 过
  `provider_visible_shared_messages`(865-874)。
- provider 解析见 §2 步骤 7;tools 见步骤 8;全部请求级、非 durable(与现状一致,
  turn_runner.rs:833-878 的既有承诺)。

给扩展作者的优先级指引(非强制):model router 类用高优先级(先决定模型,后续链在最终
模型下改写);prompt 重写类用低优先级(看到最终工具与模型状态)。

## 5. 重试接管(#4):新 hook 点 `provider_request_error`

**拦截位点**:仅 `start_provider_stream` 的错误路径(turn_runner.rs:880-911)。
当前错误分两个 match 臂:`ContextWindowExceeded` 专属臂(897-899,返回类型化
`TurnError`,由 step_body:346-350 路由进 reactive compaction)与通用臂(900-909,
`durable_error` + `end_turn_with_error_typed`)。hook 插在通用臂内、**`durable_error`
之前**——只有最终决策为 Fail 时才落 durable 错误事件,被 Retry 的尝试不得在 transcript
留下错误记录。两条明确不拦:

- **流中途错误不进 hook**(`consume_llm_stream`,llm_stage 607-613):中途失败已有部分
  transcript 事件,重试 = 重放已发布事件,违反 transcript 完整性原则
  (docs/provider-context-pipeline-design.md P1)。
- **`ContextWindowExceeded` 永远到不了 hook**:它在 897-899 的专属臂被先匹配并返回
  类型化 `TurnError`,不进入 900-909 的通用臂——它不是瞬态错误,已有专属恢复路径。

**重试层级**(文档明示):provider 内部 HTTP retry(429/5xx/408,默认 5 次,不可见,
astrcode-ai/common.rs:417-445)→ `provider_request_error` hook(本设计)→ turn 级
reactive compaction(仅 CtxOverflow,一次/turn)。hook 只看到内部重试耗尽后的错误。

**类型**:

```rust
pub enum ProviderRequestErrorEffect {
    /// 默认:维持现有失败语义
    Fail,
    /// 建议重试;delay 是建议非承诺,host 钳制并加 jitter
    Retry { delay_ms: Option<u64>, reason: String },
    /// 重试并附带链效果(典型:429 时 OverrideModel 降级)
    RetryWithEffect { effect: Box<ProviderRequestEffect>, reason: String },
}

pub struct ProviderRequestErrorPayload {
    pub request: LlmRequestSnapshot,   // 扩展现有类型(turn_runner.rs:87-92),补 tools 字段
    pub model_id: String,              // 实际解析后的模型(含 override)
    pub attempt: u32,                  // 本 turn 内第几次尝试(从 1 起)
    pub error: LlmErrorSummary,        // 分类:rate_limit / server / network / other
}
```

**组合规则**:**any-Fail-wins > first-Retry-wins**(优先级序取第一个非 Fail)。
「别重试」是一票否决(对齐 any-Block-wins:明确的否定优先);否则高优先级 handler 的
Retry 策略胜出。

**重试预算**:注册期声明 `max_retries: u32`(默认 0 = 不重试),host 每 turn 跨 handler
全局计数,超预算降级为 Fail。先例:`continue_after_stop` 的 `max_per_turn` 注册期上限
(docs/extension-hook-matrix.md)——防扩展借重试制造无限循环。

**RetryWithEffect 重入链**:重新生成 `ProviderRequestId`(现状 retry/compaction 本就换新
id,types.rs:131 注释)并**完整重跑 §2 步骤 6-10**——effect 中的 `OverrideModel` 进链后
与其他 handler 正常 fold,不做旁路注入。重跑也重新收集两阶段 contributions。

**pending acknowledgements**:请求失败重试时,对上一 attempt 的 pending contributions
执行 **settle-as-failed**(acknowledge 阶段新增 `Failed` 结算状态,区别于现有的
applied 语义),扩展据此清理自身速率限制器/缓存。这是两阶段机制
(`acknowledge_provider_request`,runner/mod.rs:1801)需要的最小扩展;现有代码假设
「每个 prepared id 都会 Applied」,需审计该假设的全部触点(见 §10)。

## 6. 与既有机制的关系

- **HookMode**:chain 强制 Blocking。`registration_validation.rs:46-58` 的
  `fixed_hook_mode` 表增补:`BeforeProviderRequest` 从可选改为固定 Blocking——请求改写
  必须同步完成,无 fire-and-forget 语义(Advisory 模式下 host 无法忽略「半个」改写)。
  现状该 hook 是 mode-flexible 且 Blocking 已是唯一被 `hook_mode_is_supported` 实际
  允许的模式(60-69),收紧为固定值是显式化而非行为变更。
- **capability**:复用 `provider_request`(注册期已校验,registrar.rs:450-508);
  `provider_request_error` 同能力——都影响请求成败路径,同一信任域。
- **两阶段 contribution**:prepare/acknowledge 语义不变;settle-as-failed 见 §5。
- **审计**:`PreToolUseResult` 先例说明单 handler 与聚合分离后,dispatch 顺序日志
  (`log_handler_dispatch_order`,index.rs:292)继续适用;chain 物化时记一条包含各字段
  改动摘要(消息数、是否 override、目标模型)的 debug 事件。

## 7. wire 阶段(feature `provider_rewrite_v1`,最后独立阶段)

in-process 先行(§3 类型本就无 serde,wire 是纯增量),按
`docs/s5r-deactivate-design.md` 模板:

- feature 名 `provider_rewrite_v1`,经 `negotiate_features`(protocol.rs:424)协商。
- `HandlerEffect`(wire/effects.rs:6-24)新增变体:`OverrideModel` / `ReplaceTools` /
  `OverrideMaxOutputTokens` / `ProviderRequestErrorDecision`;data shape 与 in-process
  payload 一一映射,保持 `deny_unknown_fields` 纪律。
- `ProviderHookInput`(s5r/hooks.rs:49-57)扩展:`tools`、`candidate_models` 字段
  (messages/model 已有)。
- 兼容矩阵:老 host + 新 worker → 新变体 `TryFrom` 失败返回
  `WireErrorCode::Unsupported`(`PreCompactResult::Block` 先例,s5r/hooks.rs:219-229),
  worker 侧降级 Allow;老 worker + 新 host → host 只收到旧 4 变体,链退化为纯消息改写
  (功能子集,安全)。
- `provider_request_error` 的 wire 订阅面与 `max_retries` 声明同步进 manifest。
- Python SDK(sdks/python)镜像枚举与 feature 检测,同 PR 内完成。

## 8. 迁移路径(五阶段,每阶段可独立发布)

| 阶段 | 内容 | 主要文件 | 测试 |
|---|---|---|---|
| 1 | `ProviderRequestEffect` + `ProviderRequestChain` 类型与 fold 语义;`emit_provider` 返回 chain 而非扁平 ReplaceMessages(修复 1705-1715 的链压扁)。**行为等价重构** | extension-sdk/hooks/results.rs、contexts.rs;extensions/runner/mod.rs | 单测:fold 顺序、any-Block 短路;`testing.rs` `HookContextBuilder::build_provider`(382)快照回归——现有行为不变 |
| 2 | `OverrideModel` + provider 解析后移 + `PreparedProviderRequest.tools` + budget 改用解析后 limits | turn_runner.rs、session_turn.rs(向 runner 暴露 llm_for_model_id)、turn_stages.rs | MockExtensionHost:换模型路由(含 small_llm 分支)、未注册模型报错、小窗口 override 触发 reactive compaction(安全网) |
| 3 | `ReplaceTools`(收窄校验与交集降级)+ `OverrideMaxOutputTokens`(钳制) | 同上 + 收窄校验落点 | 越权工具降级测试、钳制边界测试 |
| 4 | `provider_request_error`:payload/effect/预算/settle-as-failed;`start_provider_stream` 重构为带 attempt 循环 | turn_runner.rs;runner/mod.rs 新 emit;registration(max_retries) | 模拟 429/5xx:Retry、RetryWithEffect 重入链换模型、Fail 一票否决、预算耗尽、ack settle-as-failed |
| 5 | wire:feature 协商、新 HandlerEffect 变体、ProviderHookInput 扩展、Python SDK 镜像 | wire/effects.rs、s5r/hooks.rs、wire/protocol.rs、s5r_handler.rs(parse 侧)、sdks/python | 新旧版本交叉矩阵(Unsupported 降级);s5r-conformance 扩展用例 |

每阶段收尾跑该阶段测试 + `testing.rs` 既有 hook 快照防回归;阶段 2 起补
`cargo clippy -p astrcode-session -p astrcode-extensions -p astrcode-extension-sdk
--all-targets -- -D warnings`。

## 9. 明确排除(记录取舍)

1. **请求级 provider 参数**(temperature/top_p 等):`LlmRequest` 不含 params
   (llm.rs:683-702),改动波及全部 provider 实现,无已论证需求。
2. **流式 chunk 包装/检查**(旧 #27 / 新 P0 #5):维持 decision-pending,本设计不触碰
   `LlmEvent` 流。
3. **durable prompt replace**:`prompt_build` 保持 append-only(4 桶 merge,types.rs:73-93);
   整段重写在请求级由 `ReplaceMessages` 承担。durable replace 会破坏 `is_stable()`
   KV-cache 前缀承诺(prompt_engine.rs:260-267)与 prompt 可追溯性(贡献按注册序 merge,
   无单一 owner 有权替换全局)。
4. **backoff 策略所有权**:`delay_ms` 仅为建议;钳制/jitter/总预算归 host,不暴露精确
   调度控制给扩展。
5. **流中途错误接管**:见 §5,transcript 完整性优先。
6. **工具扩宽**:`ReplaceTools` 只能收窄;扩宽必须走 host 授权的 `configure_tools`
   durable 路径(session.rs:247-270)。
7. **跨请求持久状态**:chain 效果全部请求级;模型/工具的持久变更走既有 session 机制
   (configure_tools / session 配置),不借链走私。

## 10. 风险与已知限制

- **emit_provider 扁平化改 fold 是语义增强**:今天多个 ReplaceMessages 只保留最终全量
  (1705-1715);改 fold 后多 handler 链变为可组合。若某扩展依赖「我的 Replace 被另一个
  Replace 覆盖」的旧 observable 行为会看到变化——该依赖本身不合理,但在迁移说明中
  标注(阶段 1)。
- **compaction 与 usage fallback 用 session 默认 provider 计数**:override 到不同窗口的
  模型时,auto-compaction 规划(§2 步骤 2)与 usage 统计可能偏差。接受为已知限制:
  发请求前的 budget 检查用解析后 limits(步骤 9),运行时溢出由 reactive compaction
  兜底(已核实 step_body:327-331 含 prepare 阶段路由)。
- **acknowledge_provider_request 的 Applied 假设**(runner/mod.rs:1801):settle-as-failed
  新状态需审计现有实现是否假设每个 prepared id 必然 Applied(阶段 4 任务)。
- **override 与 mid-turn 模型一致性**:链的 model_override 只影响单次请求;若 turn 内
  step 间在 override 与默认模型间摆动,token 统计与 fingerprint 的模型维度可能出现
  混合口径。观测后再决定是否需要 turn 级 override 粘滞(暂不做,进排除清单同款理由)。
- **candidate_models 的暴露面**:`llm_for_model_id` 的路由表(small_llm 判定)目前是
  session 内部逻辑,暴露给扩展时只给 model_id 列表、不给路由细节,避免把
  `RuntimeGeneration` 内部结构变成契约。

## 11. 验收

- 阶段测试见 §8 每行列;关键行为断言:
  - 单 handler 现有 4 变体行为逐字节不变(阶段 1 快照回归);
  - OverrideModel 后实际请求经解析后 provider 发出(可由 fake provider 断言收到的
    模型身份);未注册 model 报错含 candidate 列表;
  - ReplaceTools 注入未见工具 → 降级交集 + warn 日志断言;
  - RetryWithEffect 重入链后新 ProviderRequestId 与新 provider 生效;Fail 一票否决;
    max_retries 耗尽后维持现有失败语义;
  - 被 Retry 的失败尝试不产生 durable 错误事件(仅最终 Fail 落 `durable_error`,
    fake publisher 断言);
  - settle-as-failed 后扩展收到 Failed 结算(mock contribution handler 断言)。
- 全量收尾:`cargo clippy --all-targets --all-features -- -D warnings` +
  `cargo test --all-features`(阶段 2 起跨 crate;阶段 5 加 Python SDK 单测与
  s5r-conformance)。
- 文档同步:`docs/extension-hook-matrix.md` 增补 chain 与 provider_request_error 两行
  (文件头声明的契约同步义务);`docs/s5r-protocol.md` 在阶段 5 同步 feature 与新变体。
