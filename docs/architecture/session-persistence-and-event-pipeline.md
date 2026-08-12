# Session 持久化与有序事件管线

> 当前实现说明。内部 API 不保留旧接口兼容。

核心原则：

- EventLog 是事实来源；
- projection 是 storage 持有的可重建读模型；
- session publisher 是 durable/live 的应用排序边界；
- live 是允许丢失的实时信号，不能影响 durable 事实是否成功。

## 1. 不变式

1. storage 只接受 `DurableEvent`，live 事件不能进入 EventLog。
2. projection 只消费 storage 分配 `seq` 后的 `StoredEvent`。
3. runtime 创建时就绑定唯一的 `SessionId`、repository 和 model binding，不存在未绑定状态。
4. `Session` 不重复保存 id/model id；`SessionStarted` 和后续 envelope 都从 runtime 取值。
5. 同一 session 的 create、append、live、sync、shutdown 进入同一条有界 FIFO。
6. durable 必须先写 journal、更新 projection，再 fan-out。
7. journal 写失败的 durable 不 fan-out，也不终止 publisher。
8. storage 是进程内唯一的可变 projection 实例所有者；turn/session 只持有共享的不可变
   `Arc<SessionReadModel>` 快照。
9. 同一 session 目录同一时间只有一个 repository owner。
10. delete/recycle 关闭 publisher 时，并发 open 必须等待。

## 2. 数据与责任

| 数据 | 类型 | 持久化 | 所有者 |
|---|---|---:|---|
| 领域事实 | `DurableEvent` / `StoredEvent` | 是 | EventLog |
| 实时信号 | `LiveEvent` | 否 | 仅在途 |
| 读模型 | `SessionReadModel` / `Arc<SessionReadModel>` | checkpoint 可选 | storage |
| 运行时资源 | publisher、broadcast、审批 sender、工具缓存 | 否 | `SessionRuntimeState` |
| 大结果/诊断输入 | tool artifact、compact snapshot | 独立文件 | storage |

`Event` 联合类型只用于进程内 fan-out 和协议映射：

```text
DurableEvent -> journal -> StoredEvent --\
                                        +-> Event -> broadcast
LiveEvent -----------------------------/
```

### 应用端口

应用层使用两个具体端口对象，不为单实现制造 trait：

- `SessionStateSource` 只暴露 read model 和 cursor，内部依赖 `SessionReader` /
  `EventReader`；
- `SessionEventPublisher` 只暴露 create、append、live、sync、shutdown，内部依赖
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

生命周期、checkpoint 和 compact snapshot 只有完整 repository 使用，直接归入
`SessionStore`，不再各建一个单实现 trait。

runtime 持有一次完整 repository 与 model binding；state source 和 publisher 分别从它
派生窄读写端口，`SessionCreateParams` 不再接收第二个 store 或 model id。这使“状态读自
A、事件写入 B”以及“持久化 model 与实际 provider binding 不同”的错配无法构造。
checkpoint、artifact、路径和 child 创建仍通过该 repository 完成，但所有应用 durable
写入只经过 publisher。

## 3. 有序发布

```text
业务调用 / TurnEventIngress
            |
            v
 Session::emit_durable / emit_live
            |
            v
 SessionEventPublisher
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
丢弃数只在 1、2、4、8……次时记录 warning。broadcast 无订阅者或订阅者 lag 也不反向
阻塞 durable。

### Turn ingress

扩展 SDK 和工具上下文需要同步、可 clone 的事件 sender，因此 `TurnEventIngress` 以
`mpsc::channel(256)` 作为边界适配器：

- 异步 `emit` 在队列有容量后入队，并等待宿主返回 publication receipt；
- 仅供同步释放路径使用的 `try_emit` 不等待，队列满时显式返回 `Full`；
- 绑定当前 turn；
- durable 失败时执行有限重试；
- `flush` 等待此前 ingress 事件处理完成。

它不写 storage、不持有 projection、也不 fan-out，因此不是第二条事件管线。若未来修改
extension SDK 的事件 sink 契约，可以再把这个适配层并入 publisher；仅为减少一个 channel
而改公共插件边界，收益不足。

## 4. Projection 所有权

projection 的三种“所有权”必须分开：

- 规则所有权：`astrcode-session-projection` 定义 model、`replay`、`reduce` 和序列校验；
- 实例所有权：storage 的 `SessionMeta` 持有唯一的当前可变 `SessionReadModel`；
- 事实所有权：EventLog 永远是可恢复事实，checkpoint 只是加速器。

打开 session 时：

```text
加载最新 checkpoint
  -> 校验 session_id / cursor
  -> replay checkpoint 之后的事件
  -> checkpoint 无效则全量 replay EventLog
```

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
| publisher FIFO | 应用可见的 durable/live 相对顺序 | 文件 I/O 实现 |
| storage `commit_lane` | journal append 与 projection reduce 的一致顺序 | fan-out、跨进程 |
| `SessionOwnerLease` | session 目录的 repository/进程唯一 owner | `seq`、应用顺序 |

EventLog 还有一个专用 writer channel，用于把阻塞文件 I/O 隔离到 writer thread。它与
publisher 也不是重复：前者管理文件句柄和 JSONL 原子记录，后者管理 durable/live 的应用
语义。合并会迫使 storage 知道 live 和 broadcast，破坏依赖方向。

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
     -> publisher.create(SessionStarted)

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
  -> publisher shutdown（排空、fsync、join）
  -> storage delete/recycle
  -> 清理订阅和外部资源
  -> 唤醒等待 open
```

`Resuming` 与 `Closing` 没有合成一个泛化的 `Transition`：两者完成条件不同，保留枚举
语义可以阻止过期的 resume/close guard 完成错误的状态转换。这是有行为价值的状态，而非
命名重复。

delete/recycle 的关闭阶段运行在持有 owned close guard 的 completion task 中。外层 HTTP、
actor 或工具请求即使在 `.await` 期间被取消，task 仍会完成 publisher 排空、storage 操作
和资源清理，`Closing` 不会被提前释放。

## 7. 对 Vvbot 设计的取舍

本次直接核对了相邻 `Vvbot` 仓库的 `ServerEventSink`、`SessionRepository`、
`SessionStateSource` 和 storage projection：

| Vvbot 做法 | AstrCode 取舍 |
|---|---|
| event sink 按 session 分片 command queue | 采用；publisher 已天然随 per-session runtime 分片 |
| durable 等待有界队列，live `try_send` | 采用；满载时丢 live，并做对数采样 warning |
| store 成功后才 fan-out | 采用 |
| repository 分持 state source 与 event sink | 采用其读写分离原则，但从同一 runtime repository 绑定派生，排除错配 |
| `SessionStateSource` 有多个实现，因此保留 trait | 不照搬；AstrCode 应用层只有一个 adapter，具体端口对象更直接 |
| projection memoize `Arc<ReadModel>` | 采用原则；storage 用 copy-on-write `Arc` 共享不可变快照，不在 turn 中维护可变副本 |
| `EventPayload::is_durable()` 运行时判断 | 不照搬；AstrCode 的 `DurableEvent` / `LiveEvent` 类型分离更强 |
| sink 对同批 durable 做 batch append | 暂不照搬；AstrCode 每次 append 同步更新 storage projection，批处理会扩大原子提交语义 |

ServerEventBus 也没有并入 publisher。它负责 child 到 root conversation 的路由、
streaming snapshot、全局通知和 legacy 映射；publisher 只负责单 session 的提交顺序。

## 8. 可观测性与验证

关键失败都有唯一记录位置：

- live 入队失败：`Session::emit_live` warning；
- publisher worker panic：shutdown 时 error；
- durable 重试耗尽：turn ingress error；
- owner 冲突、日志损坏、projection 损坏：typed storage error；
- checkpoint 无效：warning 后回退 EventLog replay。

关键代码：

| 内容 | 文件 |
|---|---|
| publisher | `crates/astrcode-session/src/event_publisher.rs` |
| read port | `crates/astrcode-session/src/session_state.rs` |
| session facade | `crates/astrcode-session/src/session.rs` |
| runtime binding | `crates/astrcode-session/src/session_runtime.rs` |
| turn ingress | `crates/astrcode-session/src/turn_publish.rs` |
| storage ports | `crates/astrcode-storage/src/traits.rs` |
| projection/lease/commit | `crates/astrcode-storage/src/session_repo.rs` |
| file writer | `crates/astrcode-storage/src/event_log.rs` |
| lifecycle registry | `crates/astrcode-server/src/session_manager.rs` |

大范围修改必须通过：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```
