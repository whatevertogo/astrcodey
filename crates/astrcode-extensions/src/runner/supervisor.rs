use std::{sync::Arc, time::Duration};

use astrcode_extension_sdk::extension::ExtensionError;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub(super) const EXTENSION_INVOCATION_CAPACITY: u32 = 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SupervisorState {
    Initializing,
    Ready,
    Draining,
    Failed(String),
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SupervisorSnapshot {
    pub(super) extension_id: String,
    pub(super) generation: u64,
    pub(super) state: SupervisorState,
}

#[derive(Clone)]
pub(super) struct ExtensionAdmission {
    extension_id: Arc<str>,
    permits: Arc<Semaphore>,
    draining: CancellationToken,
    snapshot: watch::Receiver<SupervisorSnapshot>,
}

impl ExtensionAdmission {
    pub(super) async fn acquire(&self) -> Result<OwnedSemaphorePermit, ExtensionError> {
        if self.draining.is_cancelled() {
            return Err(self.draining_error());
        }

        let permit = tokio::select! {
            biased;
            () = self.draining.cancelled() => return Err(self.draining_error()),
            permit = Arc::clone(&self.permits).acquire_owned() => {
                permit.map_err(|_| self.draining_error())?
            },
        };
        if self.draining.is_cancelled() {
            drop(permit);
            return Err(self.draining_error());
        }
        tracing::trace!(
            extension_id = %self.extension_id,
            available_permits = self.permits.available_permits(),
            "extension invocation admitted"
        );
        Ok(permit)
    }

    pub(super) fn snapshot(&self) -> SupervisorSnapshot {
        self.snapshot.borrow().clone()
    }

    pub(super) fn draining_token(&self) -> CancellationToken {
        self.draining.clone()
    }

    pub(super) fn draining_error(&self) -> ExtensionError {
        ExtensionError::Draining {
            extension_id: self.extension_id.to_string(),
        }
    }
}

#[derive(Clone)]
pub(super) struct ExtensionSupervisorControl {
    admission: ExtensionAdmission,
    commands: mpsc::UnboundedSender<SupervisorCommand>,
}

impl ExtensionSupervisorControl {
    pub(super) fn same_generation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.admission.permits, &other.admission.permits)
    }

    pub(super) async fn begin_draining(&self) -> Result<(), ExtensionError> {
        let (completed, completion) = oneshot::channel();
        self.commands
            .send(SupervisorCommand::BeginDraining { completed })
            .map_err(|_| self.unavailable_error())?;
        completion.await.map_err(|_| self.unavailable_error())?
    }

    fn unavailable_error(&self) -> ExtensionError {
        ExtensionError::Internal(format!(
            "extension {} supervisor is unavailable",
            self.admission.extension_id
        ))
    }
}

pub(super) struct ExtensionSupervisor {
    control: ExtensionSupervisorControl,
    task: Option<JoinHandle<()>>,
}

impl ExtensionSupervisor {
    pub(super) fn spawn(extension_id: String) -> Self {
        let permits = Arc::new(Semaphore::new(EXTENSION_INVOCATION_CAPACITY as usize));
        let draining = CancellationToken::new();
        let initial = SupervisorSnapshot {
            extension_id: extension_id.clone(),
            generation: 0,
            state: SupervisorState::Initializing,
        };
        let (snapshot_tx, snapshot) = watch::channel(initial.clone());
        let (commands, receiver) = mpsc::unbounded_channel();
        let admission = ExtensionAdmission {
            extension_id: Arc::from(extension_id),
            permits: Arc::clone(&permits),
            draining: draining.clone(),
            snapshot,
        };
        let task = tokio::spawn(run_supervisor(
            receiver,
            snapshot_tx,
            initial,
            permits,
            draining,
        ));
        Self {
            control: ExtensionSupervisorControl {
                admission,
                commands,
            },
            task: Some(task),
        }
    }

    pub(super) fn control(&self) -> ExtensionSupervisorControl {
        self.control.clone()
    }

    pub(super) fn admission(&self) -> ExtensionAdmission {
        self.control.admission.clone()
    }

    pub(super) fn mark_ready(&self, generation: u64) {
        let _ = self
            .control
            .commands
            .send(SupervisorCommand::MarkReady { generation });
    }

    pub(super) async fn finish(mut self, failure: Option<String>) {
        let (completed, completion) = oneshot::channel();
        let _ = self
            .control
            .commands
            .send(SupervisorCommand::Finish { failure, completed });
        let _ = tokio::time::timeout(Duration::from_secs(2), completion).await;
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            tracing::warn!(%error, "extension supervisor task failed");
        }
    }
}

impl Drop for ExtensionSupervisor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

enum SupervisorCommand {
    MarkReady {
        generation: u64,
    },
    BeginDraining {
        completed: oneshot::Sender<Result<(), ExtensionError>>,
    },
    Finish {
        failure: Option<String>,
        completed: oneshot::Sender<()>,
    },
}

#[tracing::instrument(
    name = "extension.lifecycle",
    skip(commands, snapshots, permits, draining, snapshot),
    fields(extension_id = %snapshot.extension_id, generation = snapshot.generation)
)]
async fn run_supervisor(
    mut commands: mpsc::UnboundedReceiver<SupervisorCommand>,
    snapshots: watch::Sender<SupervisorSnapshot>,
    mut snapshot: SupervisorSnapshot,
    permits: Arc<Semaphore>,
    draining: CancellationToken,
) {
    let mut drain_permits = None;
    let mut drain_task: Option<
        JoinHandle<Result<OwnedSemaphorePermit, tokio::sync::AcquireError>>,
    > = None;
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    if let Some(task) = drain_task.take() {
                        task.abort();
                    }
                    break;
                };
                match command {
                    SupervisorCommand::MarkReady { generation } => {
                        if matches!(
                            snapshot.state,
                            SupervisorState::Initializing | SupervisorState::Ready
                        ) {
                            snapshot.generation = generation;
                            snapshot.state = SupervisorState::Ready;
                            snapshots.send_replace(snapshot.clone());
                        }
                    },
                    SupervisorCommand::BeginDraining { completed } => {
                        if !matches!(snapshot.state, SupervisorState::Draining) {
                            snapshot.state = SupervisorState::Draining;
                            snapshots.send_replace(snapshot.clone());
                            drain_task = Some(tokio::spawn(
                                Arc::clone(&permits)
                                    .acquire_many_owned(EXTENSION_INVOCATION_CAPACITY),
                            ));
                            draining.cancel();
                        }
                        let _ = completed.send(Ok(()));
                    },
                    SupervisorCommand::Finish { failure, completed } => {
                        if let Some(task) = drain_task.take() {
                            task.abort();
                        }
                        permits.close();
                        snapshot.state = failure
                            .map(SupervisorState::Failed)
                            .unwrap_or(SupervisorState::Stopped);
                        snapshots.send_replace(snapshot.clone());
                        let _ = completed.send(());
                        break;
                    },
                }
            },
            drained = async {
                match &mut drain_task {
                    Some(task) => Some(task.await),
                    None => std::future::pending().await,
                }
            } => {
                drain_task = None;
                match drained {
                    Some(Ok(Ok(all_permits))) => {
                        permits.close();
                        drain_permits = Some(all_permits);
                    },
                    Some(Ok(Err(error))) => {
                        tracing::warn!(%error, "extension admission drain failed");
                    },
                    Some(Err(error)) => {
                        tracing::warn!(%error, "extension admission drain task failed");
                    },
                    None => {},
                }
            },
        }
    }
    drop(drain_permits);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn draining_rejects_queued_and_stale_generation_invocations() {
        let supervisor = ExtensionSupervisor::spawn("test.extension".into());
        supervisor.mark_ready(7);
        tokio::task::yield_now().await;
        let admission = supervisor.admission();
        assert_eq!(admission.snapshot().generation, 7);

        let held = admission.acquire().await.unwrap();
        let control = supervisor.control();
        let drain = tokio::spawn(async move { control.begin_draining().await });
        tokio::task::yield_now().await;

        assert!(matches!(
            admission.acquire().await,
            Err(ExtensionError::Draining { .. })
        ));
        drain.await.unwrap().unwrap();
        assert!(matches!(
            admission.acquire().await,
            Err(ExtensionError::Draining { .. })
        ));
        assert_eq!(admission.snapshot().state, SupervisorState::Draining);

        supervisor.finish(None).await;
        assert_eq!(admission.snapshot().state, SupervisorState::Stopped);
        drop(held);
    }
}
