//! Per-session ordered event publication.
//!
//! Durable and live events share one bounded FIFO. Durable events are committed
//! before they become visible to the session fan-out; live events skip storage
//! but keep their position relative to durable events in the same queue.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use astrcode_core::{
    event::{DurableEvent, Event, LiveEvent, StoredEvent},
    types::SessionId,
};
use astrcode_storage::{SessionEventJournal, StorageError};
use parking_lot::Mutex;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};

use crate::perf_snapshot;

const EVENT_PUBLISH_CAPACITY: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum SessionEventPublishError {
    #[error("session event publisher is closed")]
    Closed,
    #[error("session event publisher queue is full; dropped {dropped} live events")]
    Full { dropped: u64 },
    #[error(transparent)]
    Storage(#[from] StorageError),
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

pub(crate) struct SessionEventPublisher {
    session_id: SessionId,
    commands: mpsc::Sender<PublishCommand>,
    worker: Mutex<Option<JoinHandle<()>>>,
    dropped_live: AtomicU64,
}

impl SessionEventPublisher {
    pub(crate) fn start(
        session_id: SessionId,
        journal: Arc<dyn SessionEventJournal>,
        fanout: broadcast::Sender<Arc<Event>>,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(EVENT_PUBLISH_CAPACITY);
        let worker = tokio::spawn(run_publisher(session_id.clone(), journal, fanout, receiver));
        Self {
            session_id,
            commands,
            worker: Mutex::new(Some(worker)),
            dropped_live: AtomicU64::new(0),
        }
    }

    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
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

    pub(crate) async fn create(
        &self,
        event: DurableEvent,
    ) -> Result<StoredEvent, SessionEventPublishError> {
        self.commit(DurableCommit::Create, event).await
    }

    pub(crate) async fn append(
        &self,
        event: DurableEvent,
    ) -> Result<StoredEvent, SessionEventPublishError> {
        self.commit(DurableCommit::Append, event).await
    }

    pub(crate) fn publish_live(&self, event: LiveEvent) -> Result<(), SessionEventPublishError> {
        self.commands
            .try_send(PublishCommand::Live(event))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SessionEventPublishError::Full {
                    dropped: self.dropped_live.fetch_add(1, Ordering::Relaxed) + 1,
                },
                mpsc::error::TrySendError::Closed(_) => SessionEventPublishError::Closed,
            })
    }

    pub(crate) async fn sync_durable(&self) -> Result<(), SessionEventPublishError> {
        self.request(|reply| PublishCommand::Sync { reply }).await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), SessionEventPublishError> {
        let result = self
            .request(|reply| PublishCommand::Shutdown { reply })
            .await;
        let worker = self.worker.lock().take();
        if let Some(worker) = worker {
            if let Err(error) = worker.await {
                tracing::error!(
                    session_id = %self.session_id,
                    panic = %error,
                    "session event publisher worker panicked"
                );
                return Err(SessionEventPublishError::Closed);
            }
        }
        result
    }
}

async fn run_publisher(
    session_id: SessionId,
    journal: Arc<dyn SessionEventJournal>,
    fanout: broadcast::Sender<Arc<Event>>,
    mut commands: mpsc::Receiver<PublishCommand>,
) {
    tracing::debug!(session_id = %session_id, "session event publisher started");
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
                    .inspect(|stored| {
                        publish_durable(&fanout, kind, stored);
                    });
                let _ = reply.send(result);
            },
            PublishCommand::Live(event) => {
                let event = Event::from(event);
                perf_snapshot::capture_event("session.emit_live", &event);
                let _ = fanout.send(Arc::new(event));
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
                if shutdown_reply.is_none() {
                    shutdown_reply = Some(reply);
                } else {
                    let _ = reply.send(Err(SessionEventPublishError::Closed));
                }
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
    tracing::debug!(session_id = %session_id, "session event publisher stopped");
}

fn publish_durable(
    fanout: &broadcast::Sender<Arc<Event>>,
    kind: DurableCommit,
    stored: &StoredEvent,
) {
    let event = Event::from(stored);
    let capture_name = match kind {
        DurableCommit::Create => "session.create",
        DurableCommit::Append => "session.append_event",
    };
    perf_snapshot::capture_event(capture_name, &event);
    let _ = fanout.send(Arc::new(event));
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
        task::Poll,
    };

    use astrcode_core::event::{DurableEventPayload, EventPayload, LiveEventPayload};
    use tokio::sync::{Semaphore, broadcast::error::TryRecvError};

    use super::*;

    struct ControlledCommitter {
        next_seq: AtomicU64,
        gate_first_append: AtomicBool,
        append_started: Semaphore,
        append_release: Semaphore,
        fail_next: AtomicBool,
        sync_count: AtomicU64,
    }

    impl ControlledCommitter {
        fn new() -> Self {
            Self {
                next_seq: AtomicU64::new(0),
                gate_first_append: AtomicBool::new(true),
                append_started: Semaphore::new(0),
                append_release: Semaphore::new(0),
                fail_next: AtomicBool::new(false),
                sync_count: AtomicU64::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionEventJournal for ControlledCommitter {
        async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
            self.append_event(event).await
        }

        async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
            if self.gate_first_append.swap(false, Ordering::AcqRel) {
                self.append_started.add_permits(1);
                self.append_release.acquire().await.unwrap().forget();
            }
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err(StorageError::Unsupported("injected append failure".into()));
            }
            let seq = self.next_seq.fetch_add(1, Ordering::AcqRel);
            Ok(StoredEvent::new(seq, event))
        }

        async fn sync_durable_events(&self, _session_id: &SessionId) -> Result<(), StorageError> {
            self.sync_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    async fn poll_until_pending<F: Future>(future: std::pin::Pin<&mut F>) {
        let mut future = future;
        std::future::poll_fn(|context| match future.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => {
                panic!("publisher command completed while the commit gate was closed")
            },
        })
        .await;
    }

    #[tokio::test]
    async fn publisher_orders_durable_and_live_and_never_fanouts_failed_commits() {
        let session_id = SessionId::new("session-ordered-publisher");
        let committer = Arc::new(ControlledCommitter::new());
        let (fanout, mut events) = broadcast::channel(16);
        let publisher = Arc::new(SessionEventPublisher::start(
            session_id.clone(),
            committer.clone(),
            fanout,
        ));

        let first_publisher = Arc::clone(&publisher);
        let first_session_id = session_id.clone();
        let first = tokio::spawn(async move {
            first_publisher
                .append(DurableEvent::session(
                    first_session_id,
                    DurableEventPayload::TurnStarted,
                ))
                .await
        });
        committer.append_started.acquire().await.unwrap().forget();

        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        publisher
            .publish_live(LiveEvent::session(
                session_id.clone(),
                LiveEventPayload::AgentRunStarted,
            ))
            .unwrap();

        let mut second = Box::pin(publisher.append(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
        )));
        poll_until_pending(second.as_mut()).await;

        committer.append_release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap().seq, 0);
        assert_eq!(second.await.unwrap().seq, 1);

        let first_event = events.recv().await.unwrap();
        let live_event = events.recv().await.unwrap();
        let second_event = events.recv().await.unwrap();
        assert_eq!(first_event.seq, Some(0));
        assert!(matches!(first_event.payload, EventPayload::Durable(_)));
        assert_eq!(live_event.seq, None);
        assert!(matches!(live_event.payload, EventPayload::Live(_)));
        assert_eq!(second_event.seq, Some(1));

        committer.fail_next.store(true, Ordering::Release);
        let error = publisher
            .append(DurableEvent::session(
                session_id.clone(),
                DurableEventPayload::TurnStarted,
            ))
            .await
            .unwrap_err();
        assert!(matches!(error, SessionEventPublishError::Storage(_)));
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        publisher
            .publish_live(LiveEvent::session(
                session_id.clone(),
                LiveEventPayload::AgentRunStarted,
            ))
            .unwrap();
        assert!(matches!(
            events.recv().await.unwrap().payload,
            EventPayload::Live(_)
        ));

        committer.gate_first_append.store(true, Ordering::Release);
        let gated_publisher = Arc::clone(&publisher);
        let gated_session_id = session_id.clone();
        let gated = tokio::spawn(async move {
            gated_publisher
                .append(DurableEvent::session(
                    gated_session_id,
                    DurableEventPayload::TurnStarted,
                ))
                .await
        });
        committer.append_started.acquire().await.unwrap().forget();

        let mut shutdown = Box::pin(publisher.shutdown());
        poll_until_pending(shutdown.as_mut()).await;
        publisher
            .publish_live(LiveEvent::session(
                session_id.clone(),
                LiveEventPayload::AgentRunStarted,
            ))
            .unwrap();
        let mut after_shutdown = Box::pin(publisher.append(DurableEvent::session(
            session_id,
            DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
        )));
        poll_until_pending(after_shutdown.as_mut()).await;

        committer.append_release.add_permits(1);
        assert_eq!(gated.await.unwrap().unwrap().seq, 2);
        assert_eq!(after_shutdown.await.unwrap().seq, 3);
        assert_eq!(events.recv().await.unwrap().seq, Some(2));
        assert!(matches!(
            events.recv().await.unwrap().payload,
            EventPayload::Live(_)
        ));
        assert_eq!(events.recv().await.unwrap().seq, Some(3));
        shutdown.await.unwrap();
        assert_eq!(committer.sync_count.load(Ordering::Acquire), 1);

        let saturated_session_id = SessionId::new("session-saturated-publisher");
        let (commands, _receiver) = mpsc::channel(1);
        let saturated = SessionEventPublisher {
            session_id: saturated_session_id.clone(),
            commands,
            worker: Mutex::new(None),
            dropped_live: AtomicU64::new(0),
        };
        saturated
            .publish_live(LiveEvent::session(
                saturated_session_id.clone(),
                LiveEventPayload::AgentRunStarted,
            ))
            .unwrap();
        assert!(matches!(
            saturated.publish_live(LiveEvent::session(
                saturated_session_id,
                LiveEventPayload::AgentRunStarted,
            )),
            Err(SessionEventPublishError::Full { dropped: 1 })
        ));
    }
}
