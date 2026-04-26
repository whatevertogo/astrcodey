use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use astrcode_core::{StorageEvent, StoredEvent, store::StoreResult};
use astrcode_host_session::ports::SessionRecoveryCheckpoint;
use tokio::sync::{Mutex, Notify, oneshot};

use super::{event_log::EventLog, paths::resolve_existing_session_path_from_projects_root};

const BATCH_DRAIN_WINDOW: Duration = Duration::from_millis(50);

pub(crate) type SharedAppenderRegistry = Arc<Mutex<HashMap<String, Arc<BatchAppender>>>>;

struct PendingAppend {
    event: StorageEvent,
    reply: oneshot::Sender<StoreResult<StoredEvent>>,
}

#[derive(Default)]
struct BatchAppenderState {
    queue: VecDeque<PendingAppend>,
    flush_scheduled: bool,
    draining: bool,
    paused: bool,
}

pub(crate) struct BatchAppender {
    session_id: String,
    projects_root: Option<PathBuf>,
    state: Mutex<BatchAppenderState>,
    notify: Notify,
    log: StdMutex<Option<EventLog>>,
}

impl BatchAppender {
    pub(crate) fn new(session_id: String, projects_root: Option<PathBuf>) -> Self {
        Self {
            session_id,
            projects_root,
            state: Mutex::new(BatchAppenderState::default()),
            notify: Notify::new(),
            log: StdMutex::new(None),
        }
    }

    pub(crate) async fn append(self: &Arc<Self>, event: StorageEvent) -> StoreResult<StoredEvent> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().await;
        state.queue.push_back(PendingAppend { event, reply: tx });
        if !state.paused && !state.flush_scheduled {
            state.flush_scheduled = true;
            drop(state);
            self.spawn_flush_cycle();
        } else {
            drop(state);
        }
        rx.await.map_err(|_| {
            crate::internal_io_error(format!(
                "batch appender for session '{}' dropped response channel",
                self.session_id
            ))
        })?
    }

    pub(crate) async fn checkpoint_with_payload<T, F>(
        self: &Arc<Self>,
        checkpoint: SessionRecoveryCheckpoint,
        work: F,
    ) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Path, &SessionRecoveryCheckpoint) -> StoreResult<T> + Send + 'static,
    {
        self.pause_and_drain().await;
        let appender = Arc::clone(self);
        let result = tokio::task::spawn_blocking(move || {
            let path = appender.event_log_path()?;
            let mut guard = appender.log.lock().map_err(|_| {
                crate::internal_io_error(format!(
                    "batch appender log mutex poisoned for session '{}'",
                    appender.session_id
                ))
            })?;
            let previous = guard.take();
            drop(guard);
            drop(previous);
            work(&path, &checkpoint)
        })
        .await
        .map_err(|error| {
            crate::internal_io_error(format!(
                "checkpoint task for session '{}' failed to join: {error}",
                self.session_id
            ))
        })?;
        self.resume_after_barrier().await;
        result
    }

    fn spawn_flush_cycle(self: &Arc<Self>) {
        let appender = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(BATCH_DRAIN_WINDOW).await;
            appender.flush_pending_batch().await;
        });
    }

    async fn flush_pending_batch(self: Arc<Self>) {
        let pending = {
            let mut state = self.state.lock().await;
            if state.paused || state.queue.is_empty() {
                state.flush_scheduled = false;
                state.draining = false;
                self.notify.notify_waiters();
                return;
            }
            state.draining = true;
            state.flush_scheduled = false;
            state.queue.drain(..).collect::<Vec<_>>()
        };
        let events = pending
            .iter()
            .map(|pending| pending.event.clone())
            .collect::<Vec<_>>();
        let result = {
            let appender = Arc::clone(&self);
            tokio::task::spawn_blocking(move || appender.append_batch_blocking(&events)).await
        };
        match result {
            Ok(Ok(stored_events)) => {
                for (pending, stored) in pending.into_iter().zip(stored_events) {
                    let _ = pending.reply.send(Ok(stored));
                }
            },
            Ok(Err(error)) => {
                let message = error.to_string();
                for pending in pending {
                    let _ = pending
                        .reply
                        .send(Err(crate::internal_io_error(message.clone())));
                }
            },
            Err(error) => {
                let message = format!(
                    "batch appender for session '{}' failed to join: {error}",
                    self.session_id
                );
                for pending in pending {
                    let _ = pending
                        .reply
                        .send(Err(crate::internal_io_error(message.clone())));
                }
            },
        }

        let should_reschedule = {
            let mut state = self.state.lock().await;
            state.draining = false;
            let should_reschedule =
                !state.paused && !state.flush_scheduled && !state.queue.is_empty();
            if should_reschedule {
                state.flush_scheduled = true;
            }
            self.notify.notify_waiters();
            should_reschedule
        };
        if should_reschedule {
            self.spawn_flush_cycle();
        }
    }

    async fn pause_and_drain(&self) {
        loop {
            let notified = {
                let mut state = self.state.lock().await;
                if !state.paused && !state.draining && state.queue.is_empty() {
                    state.paused = true;
                    None
                } else {
                    Some(self.notify.notified())
                }
            };
            if let Some(notified) = notified {
                notified.await;
                continue;
            }
            return;
        }
    }

    async fn resume_after_barrier(self: &Arc<Self>) {
        let should_schedule = {
            let mut state = self.state.lock().await;
            state.paused = false;
            let should_schedule = !state.flush_scheduled && !state.queue.is_empty();
            if should_schedule {
                state.flush_scheduled = true;
            }
            self.notify.notify_waiters();
            should_schedule
        };
        if should_schedule {
            self.spawn_flush_cycle();
        }
    }

    fn append_batch_blocking(&self, events: &[StorageEvent]) -> StoreResult<Vec<StoredEvent>> {
        let mut guard = self.log.lock().map_err(|_| {
            crate::internal_io_error(format!(
                "batch appender log mutex poisoned for session '{}'",
                self.session_id
            ))
        })?;
        if guard.is_none() {
            *guard = Some(self.open_event_log()?);
        }
        guard
            .as_mut()
            .expect("event log should be available")
            .append_batch(events)
    }

    fn open_event_log(&self) -> StoreResult<EventLog> {
        match &self.projects_root {
            Some(projects_root) => EventLog::open_in_projects_root(projects_root, &self.session_id),
            None => EventLog::open(&self.session_id),
        }
    }

    fn event_log_path(&self) -> StoreResult<PathBuf> {
        match &self.projects_root {
            Some(projects_root) => {
                resolve_existing_session_path_from_projects_root(projects_root, &self.session_id)
            },
            None => super::paths::resolve_existing_session_path(&self.session_id),
        }
    }
}
