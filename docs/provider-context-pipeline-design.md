# Provider 上下文管线设计(从 0 综合:astrcodey × dsh × vvbot)

> 状态:提案。综合三家证据:astrcodey 的存储/turn 管线与 2026-08 冻结事故、
> dsh(deepseek-harness)源码、vvbot 的同类事故复盘
> (`Vvbot/docs/solutions/logic-errors/frozen-incomplete-tool-protocol-session-20260709.md`)
> 与投递语义修复 PR VitaDynamics/Vvbot#893(归属、排队、durable 重建、delivery gate)。

## 0. 事故共同教训

astrcodey 与 vvbot 各自独立踩中同一个坑:协议合法性(「assistant 的 tool_calls 后必须紧跟
tool results」)只在**请求组装期**用截断保证,于是一条按到达顺序落盘的用户插话把未结算
轮次埋进历史中部,之后每个请求的上下文都在同一位置被截断——`input_tokens` 恒定、模型
原地复读、日志无限增长。两家的临时修法不同(vvbot 硬裁剪前缀,astrcodey 重排插话),
但根因共识是:**不变式放错了位置**。

## 1. 核心设计原则(从 0)

**P1 durable transcript 永远 provider-valid——写入侧保证,读取侧零归一化。**
读取侧不再是合法性边界,任何「重排/截断/修补」都不允许出现在请求组装路径上。
合法性由写入侧的两条规则合成:

- **配对规则**:每个 `ToolCallRequested` 必须有且仅有一个终结事件
  (Completed/Failed/Cancelled),且终结事件落盘前,不得有任何非 Tool 消息插入其间。
- **吸收规则**:turn 进行中到达的用户输入,**接受与吸收分离**(vvbot#893 的完整语义):
  1. 所有输入——无论目标 turn 是否在跑——先落 `UserInputAccepted`(durable,
     **不进 transcript**);steering 输入绑定活跃 `turn_id`,无归属输入进 pending 队列;
  2. 吸收只发生在 **step 边界**(上一轮工具结果已全部配对落盘),且只能由被归属的
     turn 按 `accepted_seq` 吸收一次,吸收时才生成进入 transcript 的 `UserMessage`;
     归属校验必须按 `accepted_seq` 而非消息计数(vvbot#893 的根因就是按集合扫描
     未归属输入,导致一句输入被执行两次);
  3. pending 队列完全由 projection 派生(accepted 减去 absorbed),turn 结束与进程
     启动时重建,不维护易失副本;
  4. per-session delivery gate 串行化 接受/注入/入队/turn 末 reconciliation。
     astrcodey 已有 `delivery_gates` + `SessionOperationGuard`,直接复用。

**P2 派生视图一次归一、处处复用(dsh `deriveMessages`)。**
读模型持有原始 transcript(含 origin 元数据,供 fingerprint/compaction/审计);
provider 视图是按需派生的**增量缓存**,以投影 revision 为失效键。请求组装、token 估算、
fingerprint 各自明确选用哪个视图,不允许同一批消息在一个 step 里被归一化两次以上。

**P3 两类流量物理分离,背压语义显式。**(已落地,live 直发 + durable lane。)

**P4 派生状态一律软状态。** 快照/索引/checkpoint 永不权威,损坏即弃重建。

**P5 fsync 纪律显式建模。** 常驻 writer + turn 边界 fsync + sticky 不确定性状态机
(现状保留;dsh 的 200ms write-behind 崩溃窗口语义更松,不换)。

## 2. 目标链路

```text
写入侧(不变式在此成立)
  用户输入 ──► 接受事件(不入 transcript)
       │            │ step 边界且轮次已配对
       │            ▼
       │       吸收:用户消息事件落盘 ──┐
       │                              │
  工具轮次:requested → completed/failed/cancelled(原子轮次,禁止插入)
       │                              │
       ▼                              ▼
  sink lane(FIFO)──► journal ──► event log(协议合法性在此已为真)
                                       │ reduce
                                       ▼
                              读模型(原始 transcript + revision)
                                       │ 派生,按 revision 缓存
                                       ▼
                        provider 视图(归一化只在此发生一次)
                                       │
                        ┌──────────────┼───────────────┐
                        ▼              ▼               ▼
                   请求组装        token 估算      fingerprint/compact
                   (零拷贝共享)   (锚点增量)      (用原始 transcript)
```

## 3. 各层职责与落点

### 3.1 写入侧配对与吸收(astrcode-session / astrcode-server)

astrcodey 现状:queue 链路已有完整的接受侧机制(`UserInputAccepted` durable 事件、
projection 派生的 `pending_inputs`、`accepted_seq` 回链、队列重试、delivery gate);
**inject 链路绕开了它**——`inject_internal` 把 `UserMessage`(`accepted_seq: None`)
直接写进 transcript,本次事故日志证实这正是插话进入未结算工具轮次的通道(seq 946)。

改造:inject 与 queue 合并为同一条 accepted→absorbed 管线。

- `InjectIfRunningElseStart`/`InjectOnly` 不再直接写 `UserMessage`,而是落
  `UserInputAccepted`(steering 时绑定活跃 `turn_id`);turn 在 step 边界吸收归属
  自己的 accepted 输入,此时才提交 `UserMessage { accepted_seq }`——吸收点天然位于
  上一轮结果已配对之后,插话永远无法进入未结算轮次。
- 吸收检测从「按可见 user 消息计数」(`steer.rs` 的现状,对归属不敏感)改为按
  `accepted_seq` 精确对账,消除 vvbot#893 的双重消费变体。
- `ToolCallRequested` 与终结事件在 journal 层构成**轮次**:轮次未闭合时,任何会进入
  transcript 的非 Tool 事件(用户消息、系统提示变更)都被 turn 层暂存,轮次闭合后
  立即按序补落。暂存必须有界(一个 step 的执行窗口),崩溃时由 finalize/abort 路径冲刷。
- turn 开始时跑 **repair**(借鉴 vvbot `repair_interrupted_turn`):对日志尾部未应答的
  tool call 追加 durable `ToolCallFailed`,使历史在任何崩溃时序后都回到合法状态。
  对「已被埋在历史中部的」非法序列:由 3.3 的派生规则重排合法化,不修日志——
  append-only 不改写。
- 现行读侧重排(`truncate_incomplete_tool_entries` 的缓冲移动)保留为**兜底断言**:
  正常路径永不触发,触发即告警(它触发本身就是写入侧违规的信号)。

### 3.2 读模型与派生视图(astrcode-session-projection / astrcode-context)

- `SessionReadModel` 增加单调 `revision`(每次 apply +1)。
- 新增 `ProviderView`:对 `model_context` 的派生缓存,`(revision, 视图)` 单槽;
  请求组装时 revision 未变则零成本复用。归一化(过滤空消息、合并 assistant 分片、
  轮次配对校验)只在这里发生一次。
- 消息体 `Arc<LlmMessage>` 共享(已落地),视图缓存命中时整步零消息拷贝。
- fingerprint 与 compaction 继续用原始 transcript(含 origin),与 provider 视图并存。

### 3.3 派生规则(唯一的归一化语义)

1. 过滤 provider 不可见内容;剔除 System 角色(system prompt 单独通道)。
2. 合并相邻 assistant 文本/工具分片(copy-on-write,不写穿共享)。
3. 轮次配对:轮次闭合后于其间的非 Tool 消息,移动到该轮结果之后(本事故的重排规则,
   收敛到此处为唯一实现);轮次到日志尾仍未闭合(崩溃尾部),裁掉该轮,保留后续消息。
4. 规则 3 的每次触发都 `tracing::warn!`——写入侧健康时它应是死的。

### 3.4 可观测性

- **frozen 检测**(vvbot 运维教训):连续 N 个 step `input_tokens` 恒定 → warn。
  这是此类 bug 最便宜的外部信号,接进现有 `astrcode::perf` target。
- `astrcode::perf` 的 stage 计时保留;`TurnLoop` 级 counter:每 step 的视图缓存命中率。

### 3.5 恢复与重放

- 打开即全量 replay(单遍校验,实测 10k 事件 ~7ms),不做快照(P4 的实证结论)。
- SSE 重连:cursor 高水位重放 + stale live 排空 + Lagged→显式 rehydrate(现状保留)。
- pending 输入恢复:现状已从 projection 的 `pending_inputs` 派生重建;吸收规则落地后
  语义强化为「accepted 未 absorbed」,turn 结束与 startup sweep 双触发(对齐
  vvbot#893 的 turn-end reconciliation + 启动恢复)。

## 4. 明确排除(记录取舍)

- Cordis 式全 proxy 插件内核(dsh 自己在热路径绕行通用总线)。
- 每 token 一条 durable 事件(dsh 为回放保真付 ~56× 信封写放大;live 通道已够)。
- SQLite 双后端(为随机读付行级写放大,JSONL + 单遍打开已足够快)。
- 200ms write-behind 聚批(崩溃窗口语义比 turn 边界 fsync 松)。
- 读路径合成假 tool result(vvbot 复盘明确否决:durable 与视图双轨)。

## 5. 迁移路径(与现状的差距分解)

| 步 | 内容 | 现状 |
|---|---|---|
| 1 | live/durable 分离、Arc 共享、单遍打开 | 已完成 |
| 2 | 重排语义收敛 + 钉板测试 | 已完成(本事故的修复) |
| 3 | `revision` 计数 + `ProviderView` 单槽缓存,消除每 step 重复归一化 | **不做了**(见下) |
| 4 | inject 并入 accepted→absorbed 管线:归属 `turn_id`、step 边界吸收、按 `accepted_seq` 对账(写入侧排序,治本) | 已完成 |
| 5 | turn 开始 repair(尾部未应答 call 补 `ToolCallFailed`) | 已完成(stale/startup 路径既有实现 + 直触回归测试) |
| 6 | frozen 检测告警(N 个 step input_tokens 恒定) | 已完成(连续 3 个 step 恒定 → `astrcode::perf` warn) |

第 4 步是治本(此后重排兜底永不触发);5、6 是韧性。4、5、6 已落地。

**第 3 步的收官决定(2026-08-16):不做。** Arc 共享化(PR-1/2)落地后,归一化
只剩扫描(无消息体拷贝),基准显示 2000 条消息下 `request_messages` 约 18µs、
可见性过滤约 7µs——revision 缓存每 step 最多省两次扫描(~30µs),而代价是
把缓存语义穿进 `ContextAssembler` 扩展点(prepare_messages 对自定义
assembler 是自由语义,不能假设「输入已归一化」)。收益不抵扩展契约风险。
若未来 transcript 规模让扫描可测,再按本节的 ProviderView 方案重启。

另外:`truncate_incomplete_tool_entries` 的截断路径已加 `tracing::warn!`,
作为写入侧违规的可观测信号(设计 3.3 规则 4)。

## 6. 验收

- 故障注入测试:构造「插话进未结算轮次」的事件序列,断言 provider 视图完整且顺序合法
  (已有)+ 注入后下一个 turn 的 repair 行为(新增)。
- 基准:`context_snapshot` bench + `astrcode::perf` 真实负载对照,prepare 耗时不随
  transcript 长度增长(revision 命中)。
- 全量 `cargo clippy --workspace --all-targets --all-features -- -D warnings` +
  `cargo test --workspace --all-features`。
