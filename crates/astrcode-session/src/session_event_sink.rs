//! Shared ordered publication for all session events.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use astrcode_core::{
    event::{DurableEvent, Event, LiveEvent, StoredEvent},
    types::SessionId,
};
use astrcode_storage::{SessionEventJournal, StorageError};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::perf_snapshot;

const EVENT_PUBLISH_CAPACITY: usize = 1024;

pub trait SessionEventObserver: Send + Sync {
    fn publish(&self, event: Arc<Event>);
}

#[derive(Debug, thiserror::Error)]
pub enum SessionEventPublishError {
    #[error("session event publisher is closed")]
    Closed,
    #[error("session event publisher queue is full; dropped {dropped} live events")]
    Full { dropped: u64 },
    #[error("session event publisher task failed: {0}")]
    Task(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

pub struct SessionEventSink {
    observer: Arc<dyn SessionEventObserver>,
    state: Mutex<SinkState>,
}

#[derive(Default)]
struct SinkState {
    closed: bool,
    inactive_sessions: HashSet<SessionId>,
    lanes: HashMap<SessionId, Arc<SessionEventLane>>,
}

impl SessionEventSink {
    pub fn new(observer: Arc<dyn SessionEventObserver>) -> Self {
        Self {
            observer,
            state: Mutex::new(SinkState::default()),
        }
    }

    fn lane(
        &self,
        session_id: &SessionId,
        journal: Arc<dyn SessionEventJournal>,
    ) -> Result<Arc<SessionEventLane>, SessionEventPublishError> {
        let mut state = self.state.lock();
        if state.closed || state.inactive_sessions.contains(session_id) {
            return Err(SessionEventPublishError::Closed);
        }
        Ok(Arc::clone(
            state.lanes.entry(session_id.clone()).or_insert_with(|| {
                Arc::new(SessionEventLane::start(
                    session_id.clone(),
                    journal,
                    Arc::clone(&self.observer),
                ))
            }),
        ))
    }

    pub fn activate(&self, session_id: &SessionId) -> Result<(), SessionEventPublishError> {
        let mut state = self.state.lock();
        if state.closed {
            return Err(SessionEventPublishError::Closed);
        }
        state.inactive_sessions.remove(session_id);
        Ok(())
    }

    pub async fn create(
        &self,
        journal: Arc<dyn SessionEventJournal>,
        event: DurableEvent,
    ) -> Result<StoredEvent, SessionEventPublishError> {
        self.lane(&event.session_id, journal)?
            .commit(DurableCommit::Create, event)
            .await
    }

    pub async fn append(
        &self,
        journal: Arc<dyn SessionEventJournal>,
        event: DurableEvent,
    ) -> Result<StoredEvent, SessionEventPublishError> {
        self.lane(&event.session_id, journal)?
            .commit(DurableCommit::Append, event)
            .await
    }

    pub fn publish_live(
        &self,
        journal: Arc<dyn SessionEventJournal>,
        event: LiveEvent,
    ) -> Result<(), SessionEventPublishError> {
        self.lane(&event.session_id, journal)?.publish_live(event)
    }

    pub async fn sync(
        &self,
        journal: Arc<dyn SessionEventJournal>,
        session_id: &SessionId,
    ) -> Result<(), SessionEventPublishError> {
        self.lane(session_id, journal)?.sync().await
    }

    pub async fn release(
        &self,
        journal: &dyn SessionEventJournal,
        session_id: &SessionId,
    ) -> Result<(), SessionEventPublishError> {
        let lane = {
            let mut state = self.state.lock();
            state.inactive_sessions.insert(session_id.clone());
            state.lanes.remove(session_id)
        };
        match lane {
            Some(lane) => lane.shutdown().await,
            None => journal
                .sync_durable_events(session_id)
                .await
                .map_err(Into::into),
        }
    }

    pub async fn shutdown(&self) {
        let lanes = {
            let mut state = self.state.lock();
            state.closed = true;
            state
                .lanes
                .drain()
                .map(|(_, lane)| lane)
                .collect::<Vec<_>>()
        };
        for lane in lanes {
            if let Err(error) = lane.shutdown().await {
                tracing::warn!(%error, "failed to stop session event lane");
            }
        }
    }
}

enum PublishCommand {
    Durable {
        kind: DurableCommit,
        event: DurableEvent,
        reply: oneshot::Sender<Result<StoredEvent, SessionEventPublishError>>,
    },
    Live(LiveEvent),
    Sync {
        reply: oneshot::Sender<Result<(), SessionEventPublishError>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), SessionEventPublishError>>,
    },
}

#[derive(Clone, Copy)]
enum DurableCommit {
    Create,
    Append,
}

struct SessionEventLane {
    commands: mpsc::Sender<PublishCommand>,
    dropped_live: AtomicU64,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SessionEventLane {
    fn start(
        session_id: SessionId,
        journal: Arc<dyn SessionEventJournal>,
        observer: Arc<dyn SessionEventObserver>,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(EVENT_PUBLISH_CAPACITY);
        let task = tokio::spawn(run_lane(session_id, journal, observer, receiver));
        Self {
            commands,
            dropped_live: AtomicU64::new(0),
            task: Mutex::new(Some(task)),
        }
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, SessionEventPublishError>>) -> PublishCommand,
    ) -> Result<T, SessionEventPublishError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(build(reply_tx))
            .await
            .map_err(|_| SessionEventPublishError::Closed)?;
        reply_rx
            .await
            .map_err(|_| SessionEventPublishError::Closed)?
    }

    async fn commit(
        &self,
        kind: DurableCommit,
        event: DurableEvent,
    ) -> Result<StoredEvent, SessionEventPublishError> {
        self.request(|reply| PublishCommand::Durable { kind, event, reply })
            .await
    }

    fn publish_live(&self, event: LiveEvent) -> Result<(), SessionEventPublishError> {
        self.commands
            .try_send(PublishCommand::Live(event))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SessionEventPublishError::Full {
                    dropped: self.dropped_live.fetch_add(1, Ordering::Relaxed) + 1,
                },
                mpsc::error::TrySendError::Closed(_) => SessionEventPublishError::Closed,
            })
    }

    async fn sync(&self) -> Result<(), SessionEventPublishError> {
        self.request(|reply| PublishCommand::Sync { reply }).await
    }

    async fn shutdown(&self) -> Result<(), SessionEventPublishError> {
        let result = self
            .request(|reply| PublishCommand::Shutdown { reply })
            .await;
        let task = self.task.lock().take();
        if let Some(task) = task {
            task.await
                .map_err(|error| SessionEventPublishError::Task(error.to_string()))?;
        }
        result
    }
}

async fn run_lane(
    session_id: SessionId,
    journal: Arc<dyn SessionEventJournal>,
    observer: Arc<dyn SessionEventObserver>,
    mut commands: mpsc::Receiver<PublishCommand>,
) {
    tracing::debug!(%session_id, "session event lane started");
    let mut shutdown_reply = None;
    while let Some(command) = commands.recv().await {
        match command {
            PublishCommand::Durable { kind, event, reply } => {
                let commit = match kind {
                    DurableCommit::Create => journal.create_session(event).await,
                    DurableCommit::Append => journal.append_event(event).await,
                };
                let result = commit
                    .map_err(SessionEventPublishError::from)
                    .inspect(|stored| publish_durable(observer.as_ref(), kind, stored));
                let _ = reply.send(result);
            },
            PublishCommand::Live(event) => {
                let event = Event::from(event);
                perf_snapshot::capture_event("session.emit_live", &event);
                observer.publish(Arc::new(event));
            },
            PublishCommand::Sync { reply } => {
                let result = journal
                    .sync_durable_events(&session_id)
                    .await
                    .map_err(SessionEventPublishError::from);
                let _ = reply.send(result);
            },
            PublishCommand::Shutdown { reply } => {
                commands.close();
                shutdown_reply = Some(reply);
            },
        }
    }
    if let Some(reply) = shutdown_reply {
        let result = journal
            .sync_durable_events(&session_id)
            .await
            .map_err(SessionEventPublishError::from);
        let _ = reply.send(result);
    }
    tracing::debug!(%session_id, "session event lane stopped");
}

fn publish_durable(observer: &dyn SessionEventObserver, kind: DurableCommit, stored: &StoredEvent) {
    let event = Event::from(stored);
    let capture_name = match kind {
        DurableCommit::Create => "session.create",
        DurableCommit::Append => "session.append_event",
    };
    perf_snapshot::capture_event(capture_name, &event);
    observer.publish(Arc::new(event));
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::atomic::{AtomicBool, AtomicU64},
        task::Poll,
    };

    use astrcode_core::event::{DurableEventPayload, EventPayload, LiveEventPayload};
    use tokio::sync::{Semaphore, mpsc};

    use super::*;

    struct ChannelObserver(mpsc::UnboundedSender<Arc<Event>>);

    impl SessionEventObserver for ChannelObserver {
        fn publish(&self, event: Arc<Event>) {
            let _ = self.0.send(event);
        }
    }

    struct ControlledJournal {
        next_seq: AtomicU64,
        gate_next: AtomicBool,
        fail_next: AtomicBool,
        append_started: Semaphore,
        append_release: Semaphore,
        sync_count: AtomicU64,
    }

    impl ControlledJournal {
        fn new() -> Self {
            Self {
                next_seq: AtomicU64::new(0),
                gate_next: AtomicBool::new(true),
                fail_next: AtomicBool::new(false),
                append_started: Semaphore::new(0),
                append_release: Semaphore::new(0),
                sync_count: AtomicU64::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionEventJournal for ControlledJournal {
        async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
            self.append_event(event).await
        }

        async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
            if self.gate_next.swap(false, Ordering::AcqRel) {
                self.append_started.add_permits(1);
                self.append_release.acquire().await.unwrap().forget();
            }
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err(StorageError::Unsupported("injected failure".into()));
            }
            let seq = self.next_seq.fetch_add(1, Ordering::AcqRel);
            Ok(StoredEvent::new(seq, event))
        }

        async fn sync_durable_events(&self, _session_id: &SessionId) -> Result<(), StorageError> {
            self.sync_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    async fn assert_pending<F: Future>(future: std::pin::Pin<&mut F>) {
        let mut future = future;
        std::future::poll_fn(|context| match future.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("event command completed before the commit gate opened"),
        })
        .await;
    }

    #[tokio::test]
    async fn sink_orders_events_hides_failed_commits_and_closes_cleanly() {
        let session_id = SessionId::new("ordered-session");
        let journal = Arc::new(ControlledJournal::new());
        let journal_port: Arc<dyn SessionEventJournal> = journal.clone();
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let sink = Arc::new(SessionEventSink::new(Arc::new(ChannelObserver(events_tx))));

        let first_sink = Arc::clone(&sink);
        let first_journal = Arc::clone(&journal_port);
        let first_session_id = session_id.clone();
        let first = tokio::spawn(async move {
            first_sink
                .append(
                    first_journal,
                    DurableEvent::session(first_session_id, DurableEventPayload::TurnStarted),
                )
                .await
        });
        journal.append_started.acquire().await.unwrap().forget();

        sink.publish_live(
            Arc::clone(&journal_port),
            LiveEvent::session(session_id.clone(), LiveEventPayload::AgentRunStarted),
        )
        .unwrap();
        let mut second = Box::pin(sink.append(
            Arc::clone(&journal_port),
            DurableEvent::session(
                session_id.clone(),
                DurableEventPayload::TurnCompleted {
                    finish_reason: "stop".into(),
                },
            ),
        ));
        assert_pending(second.as_mut()).await;

        journal.append_release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap().seq, 0);
        assert_eq!(second.await.unwrap().seq, 1);
        let events = [
            events_rx.recv().await.unwrap(),
            events_rx.recv().await.unwrap(),
            events_rx.recv().await.unwrap(),
        ];
        assert_eq!(events[0].seq, Some(0));
        assert!(matches!(events[1].payload, EventPayload::Live(_)));
        assert_eq!(events[2].seq, Some(1));

        journal.fail_next.store(true, Ordering::Release);
        assert!(matches!(
            sink.append(
                Arc::clone(&journal_port),
                DurableEvent::session(session_id.clone(), DurableEventPayload::TurnStarted,),
            )
            .await,
            Err(SessionEventPublishError::Storage(_))
        ));
        assert!(events_rx.try_recv().is_err());

        sink.release(journal.as_ref(), &session_id).await.unwrap();
        assert_eq!(journal.sync_count.load(Ordering::Acquire), 1);
        assert!(matches!(
            sink.publish_live(
                Arc::clone(&journal_port),
                LiveEvent::session(session_id.clone(), LiveEventPayload::AgentRunStarted),
            ),
            Err(SessionEventPublishError::Closed)
        ));

        sink.activate(&session_id).unwrap();
        sink.publish_live(
            journal_port,
            LiveEvent::session(session_id.clone(), LiveEventPayload::AgentRunStarted),
        )
        .unwrap();
        assert!(matches!(
            events_rx.recv().await.unwrap().payload,
            EventPayload::Live(_)
        ));
        sink.shutdown().await;
        assert_eq!(journal.sync_count.load(Ordering::Acquire), 2);
        assert!(matches!(
            sink.activate(&session_id),
            Err(SessionEventPublishError::Closed)
        ));
    }
}
