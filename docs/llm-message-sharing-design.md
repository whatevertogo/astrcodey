# 模型历史零拷贝设计:LlmMessage 共享化(Phase 2.1 RFC)

> 状态:PR-1(projection/context/turn 内部共享化)与 PR-2(`LlmRequest`/provider trait/
> SDK `ProviderContext` 边界)均已实施(2026-08-16)。验收基准在
> `astrcode-context/benches/context_snapshot.rs`。

## 实施记录(PR-2 收尾时补记)

- **SDK 边界落地形态**:`ProviderPayload.messages` 改 `Arc<[Arc<LlmMessage>]>`;新增
  `shared_messages()` 零拷贝访问器,原 `messages()` 改为按值 deep clone 并标
  `#[deprecated]`(保留一个版本)。扩展输出(`ReplaceMessages`/`AppendMessages`)保持
  owned,边界处包 `Arc`;s5r 传输序列化 `Vec<&LlmMessage>`,JSON 逐字节不变,未引入
  serde `rc` feature。
- **不变式封装评估(不做的理由)**:`SequencedLlmMessage.message` 保持 pub。改为私有字段
  + 访问器会触及 22 个文件(含 10+ 个跨 crate 测试的构造/读取点),超过预设阈值;且读模型
  对 crate 外只以 `&SessionReadModel` 暴露,外部无法对条目 `Arc::make_mut`,可变入口本就
  收敛在 projection reducer 内。不变式继续由类型注释 + reducer 纪律维持。
- **归一化可见性扫描不短路**:`has_provider_visible_content` 的 `trim()` 本来就是两侧
  早退;O(字节) 只出现在全空白/巨前缀空白文本,而证明「trim 后为空」必须扫描,无法安全
  短路。基准实测(2000 条 × 约 1.8KB 普通文本):可见性过滤约 7µs,`request_messages`
  约 18µs,无优化空间;早期疑似瓶颈是基准数据用空格填充文本造成的假象。

## 问题

「事件 → 读模型」已是增量(`SessionProjection` 逐事件 apply,读只 clone `Arc`);瓶颈在「读模型 → provider 请求」:每个 step 对全量 transcript 做深拷贝。Phase 1.2 收敛后仍剩两次结构性拷贝:

1. `context_snapshot()`(`astrcode-session/src/projection_context.rs`)从读模型逐条 clone 消息构建 `ContextSnapshot`;
2. 发送路径:`prepare_provider_history` 的 `snapshot.messages.clone()`(为追加 deferred 提醒而持有可变 Vec)、`LlmRequestSnapshot` 与 hook 边界的 `send_messages.clone()`(`turn_runner.rs:844-846` 注释已自知:SDK 的 `ProviderContext` 按值持有 `Vec<LlmMessage>`)。

长 transcript(数千条、含大工具结果)下,这是每 step 重复支付的 O(全量内容) 内存带宽。根因是 `LlmMessage` 在所有权上按值流转,而消息事实上不可变:transcript append-only,改写只发生在 compaction 的整段替换(`TranscriptRewritten`)。

## 目标

读模型到请求的路径 O(新增消息),消息对象跨 step 零拷贝共享;不削弱 wire 序列化与持久化格式(线上格式不变)。

## 方案选型

### 方案 A:消息体 Arc 化(推荐)

引入共享消息类型,读模型与运行时路径统一持有:

- `astrcode-session-projection`:`SequencedLlmMessage.message: LlmMessage` → `Arc<LlmMessage>`;
- `ContextSnapshot.messages: Vec<Arc<LlmMessage>>`;`request_messages` 返回 `Vec<Arc<LlmMessage>>`;
- `LlmRequest.messages: Vec<Arc<LlmMessage>>`(astrcode-core);
- serde:`Arc<T>` 序列化透明;反序列化需 `serde` 的 `rc` feature——**但持久化/wire 类型保持现有按值 DTO,`Arc` 只出现在运行时类型**,边界处一次性转换,避免动持久化格式。
- SDK 边界(`ProviderContext`、`TurnHooks`)按值持有 `Vec<LlmMessage>` 的部分保持按值,边界处 `Arc::unwrap_or_clone`——clone 只发生在扩展 hook 真正需要时。

效果:step 间消息零拷贝;每 step 只有新增消息参与构建;compaction 整段替换天然兼容(替换产生新 `Arc`,旧快照持有旧 `Arc`,无写时冲突)。

风险与成本:
- 改动面:`LlmMessage` 的消费点遍布 workspace(grep 量级:数百处 `message.clone()`/按值传递)。需要分两个 PR:先 projection + context + turn 内部,再动 `LlmRequest`/SDK。
- 纪律:共享后任何原地 `&mut` 修改都会变成跨快照可见的 bug——`LlmMessage` 的修改入口(如 hook 的 ReplaceMessages)必须先 `Arc::make_mut`/重建。消息构造后不可变这一不变式需要在类型注释里显式化。
- 不引入第三方依赖(不选 `im`/`rpds`:消息是尾部追加+整段替换,`Vec<Arc<LlmMessage>>` 的 clone 已是 O(指针数),持久结构的哨兵/分支开销在此场景无收益)。

### 方案 B:请求侧惰性物化(备选,工作量小)

不动类型;`ContextSnapshot` 持有读模型的 `Arc<SessionReadModel>` 与 cursor,`messages()` 惰性构建并对相同 (cursor, prompt) 复用缓存。能把「无变化 step」的重复构建消掉,但每次 compaction/新消息后仍全量 clone,且 hook 按值边界不变。收益约为 A 的 40%,可作为 A 落地前的过渡。

## 建议路径

1. PR-1(内部):projection `SequencedLlmMessage` 与 `ContextSnapshot`/`request_messages` 改 `Arc<LlmMessage>`;turn 路径(`prepare_provider_history`、gate、token 估算)全部改借用/共享。`LlmRequest` 暂不动,发送前 `.iter().map(Arc::unwrap_or_clone)`?——不,发送前做 `Vec<LlmMessage>` 一次性 clone 会抵消收益,所以 PR-1 的边界就到 `llm_stage` 组装请求处,接受该处一次 clone,消掉其余全部。
2. PR-2(边界):`LlmRequest.messages: Vec<Arc<LlmMessage>>` + provider wire 层按 `&LlmMessage` 序列化(本就如此);SDK `ProviderContext` 增加按引用/共享的访问器,按值访问器标 deprecated 保留一个版本。
3. 验收:Phase 0 的 `astrcode::perf` 计时点对照(prepare_stage 耗时应随 transcript 长度解耦);新增一个 2000 消息 transcript 的 turn-prepare 微基准放进 `astrcode-session/benches/`。

## 明确不做

- 不引入 `im`/`rpds` 持久数据结构(见方案 A 风险条)。
- 不改持久化与 wire 格式;`Arc` 不出现在 `DurableEvent`/snapshot/HTTP DTO。
- 不做 hook 按值语义的破坏式变更(兼容一个版本)。
