//! `ContextSnapshot` 构建与请求组装的微基准（PR-1/PR-2:消息体 Arc 共享化）。
//!
//! 对照意义:改动前 `from_transcript` 与 `request_messages` 都对全量 transcript
//! 做深拷贝;改动后 `from_shared_transcript`/`request_messages` 只 clone `Arc`
//! 指针,`from_transcript`(owned 入口)只移动消息并逐个包 `Arc`。
//!
//! 消息体用普通文本(非空白填充):`provider_visible_*` 归一化的可见性检查对
//! 以非空白开头的文本是 O(1) 早退,真实 transcript 即此情形。

use std::sync::Arc;

use astrcode_context::ContextSnapshot;
use astrcode_core::llm::{LlmMessage, SharedTranscriptMessage, TranscriptMessage};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

const MESSAGE_COUNT: usize = 2_000;

fn sample_transcript() -> Vec<TranscriptMessage> {
    (0..MESSAGE_COUNT)
        .map(|index| {
            // 约 1.8KB 的普通文本,模拟含工具结果的真实 transcript 单条消息量级。
            let body = format!(
                "message {index}: {}",
                "lorem ipsum dolor sit amet ".repeat(60)
            );
            let message = if index % 2 == 0 {
                LlmMessage::user(body)
            } else {
                LlmMessage::assistant(body)
            };
            TranscriptMessage::plain(message)
        })
        .collect()
}

fn bench_context_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_snapshot");

    group.bench_function("from_transcript", |b| {
        b.iter_batched(
            sample_transcript,
            |messages| ContextSnapshot::from_transcript(10_000, "system".into(), messages),
            BatchSize::LargeInput,
        );
    });

    group.bench_function("from_shared_transcript", |b| {
        let shared: Vec<SharedTranscriptMessage> = sample_transcript()
            .into_iter()
            .map(|entry| SharedTranscriptMessage {
                message: Arc::new(entry.message),
                origin: entry.origin,
            })
            .collect();
        b.iter(|| ContextSnapshot::from_shared_transcript(10_000, "system".into(), shared.clone()));
    });

    group.bench_function("request_messages", |b| {
        let snapshot =
            ContextSnapshot::from_transcript(10_000, "system".into(), sample_transcript());
        b.iter(|| snapshot.request_messages(snapshot.messages.clone()));
    });

    group.finish();
}

criterion_group!(benches, bench_context_snapshot);
criterion_main!(benches);
