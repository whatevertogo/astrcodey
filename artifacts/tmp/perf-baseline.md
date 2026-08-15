# astrcode-storage 性能基线(Phase 0)

- 日期:2026-08-15
- 机器:macOS(本机 SSD)
- 命令:`cargo bench -p astrcode-storage --features testing`
- 说明:criterion bench profile(release 优化);中位数,括号内为 95% CI 区间。

| 基准 | 规模 | 耗时 |
|---|---|---|
| append_events | 1 事件 | 8.1 µs [8.03–8.19] |
| append_events | 32 事件 | 73.8 µs [73.2–74.6] |
| append_events | 128 事件 | 245.9 µs [244–248] |
| append_events_and_sync | 1 事件 | 3.56 ms [3.49–3.62] |
| append_events_and_sync | 32 事件 | 4.00 ms [3.95–4.06] |
| cold_open 1000 事件 | 无 snapshot | 839 µs [816–857] |
| cold_open 1000 事件 | 有 snapshot | 1.09 ms [1.08–1.10] |
| list_all_session_summaries | 10 session | 446 µs [440–452] |
| list_all_session_summaries | 100 session | 4.17 ms [4.05–4.29] |
| list_all_session_summaries | 500 session | 66.5 ms [32.4–100.9] |

## 读数

- buffered append 吞吐约 2 µs/事件,写路径不是瓶颈。
- durable sync 被 fsync 主导(~3.5 ms/次),与批量大小几乎无关 → 聚批收益已在。
- **意外发现:1000 事件规模下 snapshot 恢复(1.09 ms)比全量 replay(839 µs)还慢**——快照是整个 read model 的 pretty JSON,反序列化成本高于逐行事件。快照的收益阈值比预想高,或快照格式值得压缩(后续候选,不在本计划内)。
- 列表在 500 session 时 ~66 ms 且方差大(32–101 ms),500+ 规模开始可感知 → Phase 2.2(session 索引)的触发条件暂未达到,但已不远。

## turn 链路计时点(Phase 0.2)

- `astrcode::perf` target,debug 级:`turn stage timing`(prepare/llm/complete/tool_calls,`turn_runner.rs`)、`durable commit blocks the session event lane`(`session_event_sink.rs`)、`provider count_tokens call`(`compaction/pipeline.rs`)。
- 开启方式:`RUST_LOG=astrcode::perf=debug`。

## Phase 1 后复测(2026-08-16,同一命令)

| 基准 | 基线 | Phase 1 后 | 判定 |
|---|---|---|---|
| append_events/1 | 8.10 µs | 8.53 µs | 噪音内(+5%) |
| append_events/32 | 73.8 µs | 71.8 µs | 持平 |
| append_events/128 | 245.9 µs | 244.5 µs | 持平 |
| append_events_and_sync/1 | 3.56 ms | 2.91 ms | fsync 方差,无回归 |
| append_events_and_sync/32 | 4.00 ms | 4.02 ms | 持平 |
| cold_open 1000 无 snapshot | 839 µs | 842 µs | 持平 |
| cold_open 1000 有 snapshot | 1.09 ms | 1.13 ms | 持平 |
| list/10 | 446 µs | 482 µs | 噪音内 |
| list/100 | 4.17 ms | 4.20 ms | 持平 |
| list/500 | 66.5 ms(方差大) | 24.8 ms | 基线受系统负载影响,不可归因于本阶段改动 |

注:Phase 1 的 1.1–1.4 均不改存储层热路径,存储基准持平是预期;turn 链路(count_tokens 调用消除、clone 收敛、live 直发、工具估算 memo)的收
益需要用 `astrcode::perf` 计时点在真实负载下对照,微基准覆盖不到。
