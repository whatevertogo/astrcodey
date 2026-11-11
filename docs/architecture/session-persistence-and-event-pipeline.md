# Session 持久化与有序事件管线

> 当前实现说明。内部 API 不保留旧接口兼容。

核心原则：

- EventLog 是事实来源；
- projection 是 storage 持有的可重建读模型；
- `SessionEventSink` 是 durable/live 的应用排序边界；
- live 是允许丢失的实时信号，不能影响 durable 事实是否成功。

## 1. 不变式

1. storage 只接受 `DurableEvent`，live 事件不能进入 EventLog。
2. projection 只消费 storage 分配 `seq` 后的 `StoredEvent`。
3. runtime 创建时就绑定唯一的 `SessionId` 和 repository，不存在未绑定状态。
4. `Session` 不重复保存 id/model id；`SessionStarted` 和后续 envelope 都从 runtime 取值。
5. 同一 session 的 create、append、live、sync、shutdown 进入同一条有界 FIFO。
6. durable 必须先写 journal、更新 projection，再 fan-out。
7. journal 写失败的 durable 不 fan-out，也不终止 event sink。
8. storage 是进程内唯一的可变 projection 实例所有者；turn/session 只持有共享的不可变
   `Arc<SessionReadModel>` 快照。
9. 同一 session 目录同一时间只有一个 repository owner。
10. delete/recycle 关闭 event sink 时，并发 open 必须等待。

## 2. 数据与责任

| 数据 | 类型 | 持久化 | 所有者 |
|---|---|---:|---|
| 领域事实 | `DurableEvent` / `StoredEvent` | 是 | EventLog |
| 实时信号 | `LiveEvent` | 否 | 仅在途 |
| 读模型 | `SessionReadModel` / `Arc<SessionReadModel>` | 否(打开时从 EventLog 重建) | storage |
| 运行时资源 | event sink、审批 sender、file observation、工具缓存 | 否 | `SessionRuntimeState` |
| 大结果/诊断输入 | tool artifact、compact snapshot | 独立文件 | storage |

fork 的用量锚点（真实 token 用量及其覆盖的消息范围）保存在
`StoredEvent.input_token_anchor` 记录头，只允许分叉记录携带。读模型在回放时校验覆盖范围
和模型窗口。发送给客户端的 `Event` 不携带这个记录头。

`Event` 联合类型只用于进程内 fan-out 和协议映射：

```text
DurableEvent -> journal -> StoredEvent --\
                                        +-> Event -> observer
LiveEvent -----------------------------/
```

### 应用端口

应用层使用两个具体端口对象，不为单实现制造 trait：

- `SessionStateSource` 只暴露 read model 和 cursor，内部依赖 `SessionReader` /
  `EventReader`；
- `SessionEventSink` 只暴露 create、append、live、sync、shutdown，内部依赖
  `SessionEventJournal`。

`Session` 对业务调用方只暴露 payload 级入口：

```rust
session.emit_durable(turn_id, durable_payload).await?;
session.emit_live(turn_id, live_payload);
```

调用方不能自行决定 `session_id` 或 durable `seq`。

### Storage 端口

storage 保留有独立调用方的窄能力：

| Trait | 职责 |
|---|---|
| `SessionEventJournal` | create、append、fsync |
| `EventReader` | replay、cursor、session 列表 |
| `SessionReader` | read model、summary |
| `SessionPathResolver` | session 路径 |
| `ToolResultArtifactStore` | 大工具结果 |

生命周期和 compact snapshot 只有完整 repository 使用，直接归入
`SessionStore`，不再各建一个单实现 trait。

runtime 持有一次完整 repository；state source 和 event sink 分别从它
派生窄读写端口，`SessionCreateParams` 不再接收第二个 store 或 model id。这使“状态读自
A、事件写入 B”的错配无法构造。
checkpoint、artifact、路径和 child 创建仍通过该 repository 完成，但所有应用 durable
写入只经过 event sink。

## 3. 有序发布

```text
业务调用 / TurnEventIngress
            |
            v
 Session::emit_durable / emit_live
            |
            v
 SessionEventSink
   bounded mpsc FIFO (1024)
      |         |         |
   Durable     Live    Sync/Shutdown
      |         |         |
 journal     fan-out     fsync
      |
 projection
      |
   fan-out
```

FIFO 定义的是命令成功入队后的顺序。并发 task 的调度先后不是领域顺序；谁先入队，谁先
处理。

### Durable

durable 命令使用 oneshot 返回 commit 结果。成功返回时：

- EventLog 已接受完整 JSONL 记录并分配 `seq`；
- storage projection 已应用同一事件；
- fan-out 已发送该 `StoredEvent`。

普通 append 写入 OS page cache，不等于每条都 `fsync`。turn/lifecycle 边界通过排在
FIFO 中的 sync barrier 保证此前 durable 已提交后再 fsync。

shutdown 被 worker 取出后先关闭 receiver，排空关闭前已经被 channel 接受的命令，再执行
最终 fsync 并回复 shutdown。因并发竞态排在 shutdown 后但已成功入队的请求不会丢失 reply。

### Live

live 使用 `try_send`：成功入队后由 worker 按队列位置 fan-out；队列满时允许丢弃，不能
反向拖住 LLM token 流或 durable producer。

`Session::emit_live` 在一个位置记录关闭或背压。为避免流式 delta 造成日志风暴，累计
丢弃数只在 1、2、4、8……次时记录 warning。下游 observer 不反向阻塞 durable。

### Turn ingress

扩展 SDK 和工具上下文需要同步、可 clone 的事件 sender，因此 `TurnEventIngress` 以
`mpsc::channel(256)` 作为边界适配器：

- 异步 `emit` 在队列有容量后入队，并等待宿主返回 publication receipt；
- 仅供同步释放路径使用的 `try_emit` 不等待，队列满时显式返回 `Full`；
- 绑定当前 turn；
- durable 失败时执行有限重试；
- `flush` 等待此前 ingress 事件处理完成。

它不写 storage、不持有 projection、也不 fan-out，因此不是第二条事件管线。若未来修改
extension SDK 的事件 sink 契约，可以再把这个适配层并入 session event sink；仅为减少一个 channel
而改公共插件边界，收益不足。

## 4. Projection 所有权

projection 的三种“所有权”必须分开：

- 规则所有权：`astrcode-session-projection` 定义 model、`replay`、`reduce` 和序列校验；
- 实例所有权：storage 的 `SessionMeta` 持有唯一的当前可变 `SessionReadModel`；
- 事实所有权：EventLog 永远是可恢复事实，也是唯一的持久化来源。

打开 session 时全量 replay EventLog(`EventLog::open` 单遍完成完整性校验并把
事件交给恢复路径)。曾有的 projection 快照/checkpoint 加速器已于 2026-08 删除:
基准显示在该设计下恢复恒慢于直接 replay 约 1.25 倍(模型 JSON 反序列化与日志
内容同阶,快照省不掉打开时的全量校验扫描)。

此前 turn 内的 `model_cache` 已删除。它复制完整 projection，还需要
invalidate/reload/reduce 三套手工同步路径，容易形成第二个状态源。

替代方案由 storage 的 `SessionProjection` 统一提供：读取只克隆
`RwLock<Arc<SessionReadModel>>` 的 `Arc`。普通 durable 批次先做无副作用校验，日志 append
成功后再通过 `Arc::make_mut` 应用：没有旧快照时直接原地归约，只有读者仍持有旧
快照时才 copy-on-write。`TranscriptRewritten` 批次依赖旧 projection 做前缀指纹校验，
prepare 只复制并推进 system prompt 与 provider transcript 这一窄状态，不构造完整
read-model candidate；校验失败时不会写入日志或发布部分 projection。

## 5. 三个不能合并的顺序域

| 机制 | 保护范围 | 不负责 |
|---|---|---|
| event sink FIFO | 应用可见的 durable/live 相对顺序 | 文件 I/O 实现 |
| storage `commit_lane` | journal append 与 projection reduce 的一致顺序 | fan-out、跨进程 |
| `SessionOwnerLease` | session 目录的 repository/进程唯一 owner | `seq`、应用顺序 |

EventLog 还有一个专用 writer channel，用于把阻塞文件 I/O 隔离到 writer thread。它与
event sink 也不是重复：前者管理文件句柄和 JSONL 原子记录，后者管理 durable/live 的应用
语义。合并会迫使 storage 知道 live 和 observer，破坏依赖方向。

### `commit_lane`

```text
acquire
  -> 按当前 projection 校验下一事件
  -> 校验 EventLog next_seq
  -> append
  -> 校验实际 seq
  -> reduce projection
release
```

即使测试或维护代码直接调用 journal，projection 的应用顺序也不会和日志顺序分叉。

### `SessionOwnerLease`

文件系统 repository 在创建或打开 session 时对
`.astrcode-session-owner.lock` 执行 `try_lock_exclusive`，并在 `SessionMeta` 生命周期内
持有文件句柄。同一 repository 用 `Arc` 实例身份复用 lease；其他 repository 或进程立即
得到明确的 owner 冲突错误。

## 6. 生命周期

### Create/Open

```text
创建：构造已绑定 runtime -> registry 登记 -> 准备 prompt
     -> event_sink.create(SessionStarted)

打开：registry 复用或创建 runtime -> storage 恢复 projection
     -> hydrate runtime prompt
```

prompt 准备失败发生在 EventLog 创建之前，不留下半初始化 session。cold open 的
`Resuming` 状态保证只有一个恢复者。

### Delete/Recycle

```text
关闭输入与 active turn
  -> lifecycle shutdown
  -> registry: Ready -> Closing
  -> event sink shutdown（排空、fsync、join）
  -> storage delete/recycle
  -> 清理订阅和外部资源
  -> 唤醒等待 open
```

`Resuming` 与 `Closing` 没有合成一个泛化的 `Transition`：两者完成条件不同，保留枚举
语义可以阻止过期的 resume/close guard 完成错误的状态转换。这是有行为价值的状态，而非
命名重复。

delete/recycle 的关闭阶段运行在持有 owned close guard 的 completion task 中。外层 HTTP、
actor 或工具请求即使在 `.await` 期间被取消，task 仍会完成 event sink 排空、storage 操作
和资源清理，`Closing` 不会被提前释放。

## 7. 对 Vvbot 设计的取舍

本次直接核对了相邻 `Vvbot` 仓库的 `ServerEventSink`、`SessionRepository`、
`SessionStateSource` 和 storage projection：

| Vvbot 做法 | AstrCode 取舍 |
|---|---|
| event sink 按 session 分片 command queue | 采用；AstrCode sink 已天然随 per-session runtime 分片 |
| durable 等待有界队列，live `try_send` | 采用；满载时丢 live，并做对数采样 warning |
| store 成功后才 fan-out | 采用 |
| repository 分持 state source 与 event sink | 采用其读写分离原则，但从同一 runtime repository 绑定派生，排除错配 |
| `SessionStateSource` 有多个实现，因此保留 trait | 不照搬；AstrCode 应用层只有一个 adapter，具体端口对象更直接 |
| projection memoize `Arc<ReadModel>` | 采用原则；storage 用 copy-on-write `Arc` 共享不可变快照，不在 turn 中维护可变副本 |
| `EventPayload::is_durable()` 运行时判断 | 不照搬；AstrCode 的 `DurableEvent` / `LiveEvent` 类型分离更强 |
| sink 对同批 durable 做 batch append | 暂不照搬；AstrCode 每次 append 同步更新 storage projection，批处理会扩大原子提交语义 |

ServerEventBus 也没有并入 session event sink。它负责 child 到 root conversation 的路由、
streaming snapshot、全局通知和 legacy 映射；event sink 只负责单 session 的提交顺序。

## 8. 可观测性与验证

关键失败都有唯一记录位置：

- live 入队失败：`Session::emit_live` warning；
- event sink worker panic：shutdown 时 error；
- durable 重试耗尽：turn ingress error；
- owner 冲突、日志损坏、projection 损坏：typed storage error。

关键代码：

| 内容 | 文件 |
|---|---|
| event sink | `crates/astrcode-session/src/session_event_sink.rs` |
| read port | `crates/astrcode-session/src/session_state.rs` |
| session facade | `crates/astrcode-session/src/session.rs` |
| runtime binding | `crates/astrcode-session/src/session_runtime.rs` |
| turn ingress | `crates/astrcode-session/src/turn_publish.rs` |
| storage ports | `crates/astrcode-storage/src/traits.rs` |
| projection/lease/commit | `crates/astrcode-storage/src/session_repo/` |
| file writer | `crates/astrcode-storage/src/event_log.rs` |
| lifecycle registry | `crates/astrcode-server/src/session_manager.rs` |

大范围修改必须通过：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```


## 9. 事件领域契约

参考 Vvbot PR #1176 的 `73b57e8a81c21b10f34e26f8f4462217e77801d7`：采用明确的执行结局、参数原文、模型尝试结算和事实/进度分离。另参考 DeepSeek Harness 的 `571902028a75c64c1d767f4ae5fd099485f0ddf0`：已声明调用必须配对结果，展示投影明确区分中断与失败。AstrCode 保留更强的 `DurableEventPayload` / `LiveEventPayload` 类型边界，发布顺序继续由现有 session lane 负责。

### 完整分类

| 领域 | 持久事实 | 实时通知 | 事实的含义 |
| --- | --- | --- | --- |
| 会话配置 | `SessionStarted`、`ModelIdChanged`、`SessionToolsConfigured`、`SystemPromptConfigured` | 无 | 创建状态完整；后续配置只记录被改变的领域。创建和配置复用 `PersistedSystemPrompt`。 |
| 子会话关系 | `AgentSessionSpawned`、`AgentSessionSettled`、`AgentSessionRecycled` | 无 | 父会话拥有链接；结算带成功摘要或失败错误，身份只记录 `child_session_id`。 |
| 回合与步骤 | `TurnStarted`、`TurnEnded`、`StepStarted`、`StepEnded` | `AgentRunStarted`、`AgentRunEnded` | durable 描述逻辑交互，live 描述当前执行器运行。重启后可恢复原 turn，因此两种生命周期仍分开。 |
| 用户输入 | `UserInputAccepted`、`UserMessage`、`UserInputDiscarded` | 无 | `accepted_seq` 关联待处理输入与实际进入回合的正文，或被插件拒绝的原因。每条待处理输入只能结算一次。 |
| 模型输出 | `AssistantMessage`、`AssistantAttemptSettled`、`TokenUsageRecorded` | `AssistantMessageStarted`、`AssistantMessageDelta`、`LlmRetrying`、`LlmRetryRecovered` | 可见正文与无正文尝试分别结算；增量和重试状态不能作为恢复事实。 |
| 工具调用 | `ToolCallRequested`、`ToolCallSettled` | `ToolCallPreviewStarted`、`ToolCallArgumentsDelta`、`ToolCallPreviewDiscarded`、`ToolExecutionStarted`、`ToolOutputDelta` | 请求是模型意图，结算是执行结果；预览不是执行完成。 |
| 工具审批 | `ToolApprovalRequested`、`ToolApprovalResolved` | 无 | 审批展示执行前的有效参数；决定必须能在重启后恢复。 |
| 上下文 | `TurnAbortedContext`、`TranscriptRewritten`、`SessionForked` | `CompactionStarted`、`CompactionEnded` | 上下文的实际改变持久化；压缩进度只通知当前订阅者。前缀指纹和 source cursor 保护仍在提交边界校验。 |
| 展示补充 | `RecapGenerated` | 无 | 供展示的回顾保留来源，不隐式改写模型上下文。 |
| 诊断 | `ErrorOccurred` | `ErrorOccurred` | durable 保存诊断，live 通知可恢复错误；执行是否失败以对应终态为准。 |
| 扩展 | `CustomEvent` | `CustomEvent` | 宿主根据声明选择生命周期；audience、schema、因果链与载荷仍保留在既有契约中。 |

`LlmEvent` 属于 provider（模型接口）的输出边界。`ToolCallArgumentsCompleted` 只表示参数生成完毕，不表示工具执行完成；其余 provider 增量、用量、重试和结束通知继续由流消费者转换成 session 事件。

### 终态和参数不变式

- `TurnEndReason` 区分完成、失败、取消与进程中断。provider 的 `finish_reason` 在入口归一为 `TurnCompletionReason`（完成原因），只附在完成结局上；错误与取消携带各自的数据。
- `StepEndReason` 区分继续、完成、失败与取消。每个已开始的步骤都会尝试写入结束事实；原始执行错误优先于后续写入错误。执行投影只保留可选 `end_reason`，不维护相互独立的 completed/finish_reason 标记。
- `AssistantMessage` 携带 `AssistantAttemptOutcome`；失败/取消保留部分正文、推理和已知用量。无正文时只写 `AssistantAttemptSettled`，不制造可见空消息。重试先结算旧尝试的部分正文和已知用量，再以新 message ID 开始尝试；工具草稿通过 `ToolCallPreviewDiscarded` 撤销。
- `ToolCallSettled` 共用一份 `ToolResult`。正常完成可以带业务错误；失败、取消或中断必须携带错误结果，storage projection 在提交/重放边界拒绝矛盾的载荷。
- `ToolCallRequested.arguments` 记录模型原始 JSON 文本。执行 hook 可改变有效参数，不能覆盖这个事实。解析修复算法由 `astrcode-core::tool::parse_tool_arguments` 共享，执行入口负责诊断，projection 和 HTTP 边界按相同规则解读原文；无法解析的展示参数保留原文字符串。
- 每个工具调用分配独立的运行时 `call_id`，另存 `provider_call_id`。hook、审批、结果和 UI 都引用运行时身份；发给模型的边界恢复 provider 的原始 ID，避免跨步骤、跨回合复用 ID 覆盖旧卡片。
- 工具终态只记录 `call_id`、`outcome` 和 `result`，通过已声明意图取得名称与原始参数。分页 timeline 跨日志块寻找声明再合并结果；未知或重复结算在持久化投影边界被拒绝，崩溃恢复仍可补齐已关闭回合的未回答调用。
- `preparing` 表示生成参数，`pending` 表示已有持久请求，`running` 必须来自实际执行入口或工具输出。审批展示 `effectiveArguments`（hook 调整后的执行参数），原始参数仍属于模型意图。终态不接受迟到的预览或输出增量。
- `AgentSessionSettled` 不重复记录当前必定等于 child 的 final session ID。旧 HTTP 链接 DTO 的 finalSessionId 在边界从 child 身份派生。
- `CompactionEnded` 用一个 outcome 表达完成、跳过或失败；实际上下文修改由 `TranscriptRewritten` 表达。

SSE 重放只过滤已被持久事实覆盖的消息、调用或已结束回合；缓冲区中尚未结算的预览和正文继续投递，避免重连时丢失正在生成的输出。游标从请求位置和已回放记录初始化，随后随连接处理的父会话持久事件推进，实时通知和子会话帧沿用当前值。一个事件产生多帧时，最后一帧才携带新游标。

HTTP 的 assistant block 使用 `complete`、`error`、`cancelled` 区分持久终态；重放和 snapshot 保留同样的状态。未完成正文不会进入“完整回复”的复制/导出操作。core 的 `LiveAccepted` receipt 仅确认事件已进入有序队列，不承诺 observer 已收到。

### 版本边界

新 `StoredEvent` 记录显式写入 `format_version: 2`。缺少版本、旧版本和未知版本都返回读取错误；本次没有迁移或改写已有用户日志。旧日志需要保留旧运行时或另行迁移。stdio 的 `PROTOCOL_VERSION` 同步为 2；外部客户端和使用变更后的宿主事件 receipt 的插件需要同步对应契约。HTTP 使用明确的 DTO 映射，不直接把新的 outcome enum 当作客户端类型。

事件载荷、信封、提交入口和终态类型分别位于 `event/payload.rs`、`event/envelope.rs`、`event/ingress.rs` 和 `event/outcome.rs`。这些划分围绕实际责任，不新增另一套事件发布或状态管理机制。
