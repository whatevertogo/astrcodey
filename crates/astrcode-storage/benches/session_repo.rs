//! astrcode-storage 热路径基准:append、durable sync、冷打开、摘要列表。
//!
//! 运行:`cargo bench -p astrcode-storage --features testing`
//! 基线存档:`cargo bench ... -- --save-baseline <name>`,对比:`-- --baseline <name>`。

use std::time::Duration;

use astrcode_core::{
    event::{
        DurableEvent, DurableEventPayload, PersistedSystemPrompt, SessionStarted,
        SystemPromptSource,
    },
    tool::SessionToolSelection,
    types::{SessionId, new_message_id},
};
use astrcode_storage::{
    SessionEventJournal, SessionReader, SessionStore, testing::filesystem_session_repository,
};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

fn started_event(session_id: &SessionId) -> DurableEvent {
    DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::SessionStarted(SessionStarted {
            working_dir: "/workspace".into(),
            model_id: "model-a".into(),
            parent: None,
            tool_selection: SessionToolSelection::default(),
            source_extension: None,
            initial_system_prompt: PersistedSystemPrompt {
                text: "system".into(),
                fingerprint: "fingerprint".into(),
                extra_system_prompt: None,
                source: SystemPromptSource::Native,
            },
        }),
    )
}

fn user_event(session_id: &SessionId, text: &str) -> DurableEvent {
    DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::UserMessage {
            message_id: new_message_id(),
            text: text.into(),
            attachments: vec![],
            accepted_seq: None,
        },
    )
}

fn user_batch(session_id: &SessionId, batch: usize, base: u64) -> Vec<DurableEvent> {
    (0..batch)
        .map(|index| user_event(session_id, &format!("message-{}", base + index as u64)))
        .collect()
}

fn bench_append(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("append_events");
    for batch in [1usize, 32, 128] {
        group.bench_with_input(BenchmarkId::from_parameter(batch), &batch, |b, &batch| {
            let dir = tempfile::tempdir().unwrap();
            let repo = filesystem_session_repository(dir.path().into());
            let session_id = SessionId::new("bench-append");
            runtime
                .block_on(repo.create_session(started_event(&session_id)))
                .unwrap();
            let mut appended = 0u64;
            b.to_async(&runtime).iter(|| {
                let events = user_batch(&session_id, batch, appended);
                appended += batch as u64;
                let repo = &repo;
                async move { repo.append_events(events).await.unwrap() }
            });
        });
    }
    group.finish();
}

fn bench_append_and_sync(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("append_events_and_sync");
    for batch in [1usize, 32] {
        group.bench_with_input(BenchmarkId::from_parameter(batch), &batch, |b, &batch| {
            let dir = tempfile::tempdir().unwrap();
            let repo = filesystem_session_repository(dir.path().into());
            let session_id = SessionId::new("bench-sync");
            runtime
                .block_on(repo.create_session(started_event(&session_id)))
                .unwrap();
            let mut appended = 0u64;
            b.to_async(&runtime).iter(|| {
                let events = user_batch(&session_id, batch, appended);
                appended += batch as u64;
                let repo = &repo;
                async move { repo.append_events_and_sync(events).await.unwrap() }
            });
        });
    }
    group.finish();
}

/// 预建一个含 `event_count` 条用户消息的 session,返回 (tempdir, session_id, last_seq)。
fn prepare_session(event_count: u64) -> (tempfile::TempDir, SessionId, u64) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let repo = filesystem_session_repository(dir.path().into());
    let session_id = SessionId::new("bench-open");
    runtime
        .block_on(repo.create_session(started_event(&session_id)))
        .unwrap();
    runtime
        .block_on(repo.append_events(user_batch(&session_id, event_count as usize, 0)))
        .unwrap();
    runtime
        .block_on(repo.sync_durable_events(&session_id))
        .unwrap();
    let last_seq = runtime
        .block_on(repo.session_read_model(&session_id))
        .unwrap()
        .stats
        .last_seq;
    (dir, session_id, last_seq)
}

fn bench_cold_open(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("cold_open");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(8));

    for event_count in [1_000u64, 10_000] {
        let (dir, session_id, last_seq) = prepare_session(event_count);
        let base = dir.path().to_path_buf();
        group.bench_function(format!("{event_count}/without_snapshot"), |b| {
            b.to_async(&runtime).iter_batched(
                || filesystem_session_repository(base.clone()),
                |repo| {
                    let session_id = session_id.clone();
                    async move {
                        repo.session_read_model(&session_id).await.unwrap();
                    }
                },
                BatchSize::SmallInput,
            );
        });

        let repo = filesystem_session_repository(base.clone());
        runtime
            .block_on(repo.checkpoint(&session_id, &last_seq.to_string().into()))
            .unwrap();
        drop(repo);
        group.bench_function(format!("{event_count}/with_snapshot"), |b| {
            b.to_async(&runtime).iter_batched(
                || filesystem_session_repository(base.clone()),
                |repo| {
                    let session_id = session_id.clone();
                    async move {
                        repo.session_read_model(&session_id).await.unwrap();
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_list_all_summaries(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("list_all_session_summaries");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    for session_count in [10usize, 100, 500] {
        let dir = tempfile::tempdir().unwrap();
        let repo = filesystem_session_repository(dir.path().into());
        runtime.block_on(async {
            for index in 0..session_count {
                let session_id = SessionId::new(format!("bench-list-{index}"));
                repo.create_session(started_event(&session_id))
                    .await
                    .unwrap();
                repo.append_events(user_batch(&session_id, 2, 0))
                    .await
                    .unwrap();
                repo.sync_durable_events(&session_id).await.unwrap();
            }
        });
        drop(repo);
        let base = dir.path().to_path_buf();
        group.bench_with_input(
            BenchmarkId::from_parameter(session_count),
            &session_count,
            |b, _| {
                b.to_async(&runtime).iter_batched(
                    || filesystem_session_repository(base.clone()),
                    |repo| async move {
                        repo.list_all_session_summaries().await.unwrap();
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_append,
    bench_append_and_sync,
    bench_cold_open,
    bench_list_all_summaries
);
criterion_main!(benches);
