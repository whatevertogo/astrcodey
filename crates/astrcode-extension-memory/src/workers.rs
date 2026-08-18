//! Generation-owned workers for memory persistence and background extraction.

use std::{
    collections::VecDeque,
    sync::{Arc, OnceLock},
};

use astrcode_extension_sdk::{
    extension::{ExtensionError, ExtensionTasks},
    host::ExtensionHost,
};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::{
    config::MemoryConfig,
    pipeline,
    scope::ScopedMemoryStores,
    store::{AppendResult, MemoryStorePool},
    turn_recall::SessionPrefsCache,
};

const STORE_QUEUE_CAPACITY: usize = 32;

type StoreReply<T> = oneshot::Sender<Result<T, ExtensionError>>;

enum StoreCommand {
    Upsert {
        working_dir: String,
        category: String,
        content: String,
        replaces: String,
        reply: StoreReply<bool>,
    },
    Append {
        working_dir: String,
        category: String,
        content: String,
        reply: StoreReply<AppendResult>,
    },
    Delete {
        working_dir: String,
        pattern: String,
        reply: StoreReply<Vec<String>>,
    },
    PreloadPreferences {
        working_dir: String,
        session_id: String,
        reply: StoreReply<()>,
    },
}

struct PipelineRequest {
    session_id: String,
    working_dir: String,
}

#[derive(Default)]
struct PipelineQueueState {
    ready: VecDeque<PipelineRequest>,
}

#[derive(Default)]
struct PipelineQueue {
    state: Mutex<PipelineQueueState>,
    changed: Notify,
}

struct PipelineWork<'a> {
    request: PipelineRequest,
    queue: &'a PipelineQueue,
}

impl Drop for PipelineWork<'_> {
    fn drop(&mut self) {
        self.queue.complete();
    }
}

impl PipelineQueue {
    fn submit(&self, request: PipelineRequest) {
        let mut state = self.state.lock();
        if let Some(queued) = state
            .ready
            .iter_mut()
            .find(|queued| queued.working_dir == request.working_dir)
        {
            *queued = request;
            return;
        }
        state.ready.push_back(request);
        self.changed.notify_one();
    }

    async fn next(&self) -> PipelineWork<'_> {
        loop {
            let changed = self.changed.notified();
            let request = {
                let mut state = self.state.lock();
                state.ready.pop_front()
            };
            if let Some(request) = request {
                return PipelineWork {
                    request,
                    queue: self,
                };
            }
            changed.await;
        }
    }

    fn complete(&self) {
        let state = self.state.lock();
        if !state.ready.is_empty() {
            self.changed.notify_one();
        }
    }
}

/// Author-side handle for the workers owned by one immutable extension generation.
#[derive(Default)]
pub(crate) struct MemoryWorkers {
    store: OnceLock<mpsc::Sender<StoreCommand>>,
    pipeline: Arc<PipelineQueue>,
}

impl MemoryWorkers {
    pub(crate) fn start(
        &self,
        tasks: &ExtensionTasks,
        store_pool: Arc<MemoryStorePool>,
        host: ExtensionHost,
        config: Arc<RwLock<MemoryConfig>>,
        session_prefs: Arc<SessionPrefsCache>,
    ) -> Result<(), ExtensionError> {
        let (store_tx, store_rx) = mpsc::channel(STORE_QUEUE_CAPACITY);
        self.store.set(store_tx).map_err(|_| {
            ExtensionError::Internal("memory workers were already started".to_string())
        })?;

        let store_tasks = tasks.clone();
        let store_pool_for_worker = Arc::clone(&store_pool);
        tasks.spawn("memory-store-worker", async move {
            run_store_worker(store_tasks, store_pool_for_worker, session_prefs, store_rx).await;
        });

        let pipeline_tasks = tasks.clone();
        let pipeline_queue = Arc::clone(&self.pipeline);
        tasks.spawn("memory-pipeline-worker", async move {
            run_pipeline_worker(pipeline_tasks, pipeline_queue, store_pool, host, config).await;
        });
        Ok(())
    }

    pub(crate) async fn upsert(
        &self,
        working_dir: String,
        category: String,
        content: String,
        replaces: String,
    ) -> Result<bool, ExtensionError> {
        let (reply, result) = oneshot::channel();
        self.send(
            StoreCommand::Upsert {
                working_dir,
                category,
                content,
                replaces,
                reply,
            },
            result,
        )
        .await
    }

    pub(crate) async fn append(
        &self,
        working_dir: String,
        category: String,
        content: String,
    ) -> Result<AppendResult, ExtensionError> {
        let (reply, result) = oneshot::channel();
        self.send(
            StoreCommand::Append {
                working_dir,
                category,
                content,
                reply,
            },
            result,
        )
        .await
    }

    pub(crate) async fn delete(
        &self,
        working_dir: String,
        pattern: String,
    ) -> Result<Vec<String>, ExtensionError> {
        let (reply, result) = oneshot::channel();
        self.send(
            StoreCommand::Delete {
                working_dir,
                pattern,
                reply,
            },
            result,
        )
        .await
    }

    pub(crate) async fn preload_preferences(
        &self,
        working_dir: String,
        session_id: String,
    ) -> Result<(), ExtensionError> {
        let (reply, result) = oneshot::channel();
        self.send(
            StoreCommand::PreloadPreferences {
                working_dir,
                session_id,
                reply,
            },
            result,
        )
        .await
    }

    pub(crate) fn request_pipeline(&self, session_id: String, working_dir: String) {
        self.pipeline.submit(PipelineRequest {
            session_id,
            working_dir,
        });
    }

    async fn send<T>(
        &self,
        command: StoreCommand,
        result: oneshot::Receiver<Result<T, ExtensionError>>,
    ) -> Result<T, ExtensionError> {
        let sender = self.store.get().ok_or_else(worker_unavailable)?;
        sender
            .send(command)
            .await
            .map_err(|_| worker_unavailable())?;
        result.await.map_err(|_| worker_unavailable())?
    }
}

fn worker_unavailable() -> ExtensionError {
    ExtensionError::Internal("memory generation worker is unavailable".to_string())
}

async fn run_store_worker(
    tasks: ExtensionTasks,
    store_pool: Arc<MemoryStorePool>,
    session_prefs: Arc<SessionPrefsCache>,
    mut commands: mpsc::Receiver<StoreCommand>,
) {
    let cancellation = tasks.cancellation();
    loop {
        let command = tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            command = commands.recv() => match command {
                Some(command) => command,
                None => break,
            },
        };
        command.execute(&tasks, &store_pool, &session_prefs).await;
    }
}

impl StoreCommand {
    async fn execute(
        self,
        tasks: &ExtensionTasks,
        store_pool: &Arc<MemoryStorePool>,
        session_prefs: &Arc<SessionPrefsCache>,
    ) {
        match self {
            Self::Upsert {
                working_dir,
                category,
                content,
                replaces,
                reply,
            } => {
                let result = run_store_operation(
                    tasks,
                    "memory-save-upsert",
                    Arc::clone(store_pool),
                    working_dir,
                    move |stores| stores.upsert(&category, &content, Some(&replaces)),
                )
                .await;
                let _ = reply.send(result);
            },
            Self::Append {
                working_dir,
                category,
                content,
                reply,
            } => {
                let result = run_store_operation(
                    tasks,
                    "memory-save-append",
                    Arc::clone(store_pool),
                    working_dir,
                    move |stores| stores.append(&category, &content),
                )
                .await;
                let _ = reply.send(result);
            },
            Self::Delete {
                working_dir,
                pattern,
                reply,
            } => {
                let result = run_store_operation(
                    tasks,
                    "memory-delete",
                    Arc::clone(store_pool),
                    working_dir,
                    move |stores| stores.delete_by_content(&pattern),
                )
                .await;
                let _ = reply.send(result);
            },
            Self::PreloadPreferences {
                working_dir,
                session_id,
                reply,
            } => {
                let session_prefs = Arc::clone(session_prefs);
                let result = run_store_operation(
                    tasks,
                    "memory-preload-preferences",
                    Arc::clone(store_pool),
                    working_dir,
                    move |stores| {
                        session_prefs
                            .preload_for_session(&session_id, || stores.all_user_preference_lines())
                    },
                )
                .await;
                let _ = reply.send(result);
            },
        }
    }
}

async fn run_store_operation<T: Send + 'static>(
    tasks: &ExtensionTasks,
    task_name: &'static str,
    store_pool: Arc<MemoryStorePool>,
    working_dir: String,
    operation: impl FnOnce(ScopedMemoryStores) -> std::io::Result<T> + Send + 'static,
) -> Result<T, ExtensionError> {
    tasks
        .run_to_completion(
            task_name,
            tokio::task::spawn_blocking(move || {
                let stores = store_pool.get_scoped(&working_dir)?;
                operation(stores)
            }),
        )
        .await
        .map_err(|error| ExtensionError::Internal(error.to_string()))?
        .map_err(|error| ExtensionError::Internal(error.to_string()))?
        .map_err(|error| ExtensionError::Internal(error.to_string()))
}

async fn run_pipeline_worker(
    tasks: ExtensionTasks,
    queue: Arc<PipelineQueue>,
    store_pool: Arc<MemoryStorePool>,
    host: ExtensionHost,
    config: Arc<RwLock<MemoryConfig>>,
) {
    let cancellation = tasks.cancellation();
    let session_inspect = match host.session_inspect() {
        Ok(inspect) => inspect,
        Err(error) => {
            tracing::warn!(%error, "memory pipeline: session inspection unavailable");
            return;
        },
    };
    let models = match host.models() {
        Ok(models) => models,
        Err(error) => {
            tracing::warn!(%error, "memory pipeline: models unavailable");
            return;
        },
    };

    loop {
        let work = tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            request = queue.next() => request,
        };
        let request = &work.request;
        let scoped = match store_pool.get_scoped(&request.working_dir) {
            Ok(scoped) => scoped,
            Err(error) => {
                tracing::warn!(%error, "memory pipeline: scoped store failed");
                continue;
            },
        };
        let current_config = config.read().clone();
        let run = pipeline::run(
            &scoped,
            session_inspect.clone(),
            &models,
            &request.session_id,
            &current_config,
        );
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            result = run => {
                if let Err(error) = result {
                    tracing::warn!(
                        %error,
                        session_id = %request.session_id,
                        "memory pipeline failed"
                    );
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use astrcode_extension_sdk::extension::internal::{
        cancel_extension_tasks, extension_tasks, wait_extension_tasks,
    };
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn generation_workers_finish_accepted_writes_revoke_on_shutdown_and_coalesce_pipeline() {
        let queue = PipelineQueue::default();

        queue.submit(PipelineRequest {
            session_id: "s1".into(),
            working_dir: "/a".into(),
        });
        queue.submit(PipelineRequest {
            session_id: "s2".into(),
            working_dir: "/b".into(),
        });
        queue.submit(PipelineRequest {
            session_id: "s3".into(),
            working_dir: "/a".into(),
        });

        let active = queue.next().await;
        assert_eq!(active.request.session_id, "s3");
        assert_eq!(active.request.working_dir, "/a");

        drop(active);
        let pending = queue.next().await;
        assert_eq!(pending.request.session_id, "s2");
        assert_eq!(pending.request.working_dir, "/b");
        drop(pending);

        queue.submit(PipelineRequest {
            session_id: "s4".into(),
            working_dir: "/d".into(),
        });
        let after_early_exit = queue.next().await;
        assert_eq!(after_early_exit.request.session_id, "s4");
        drop(after_early_exit);

        let temp = TempDir::new().unwrap();
        let store_pool = Arc::new(MemoryStorePool::new());
        store_pool.set_root(temp.path().join("memory")).unwrap();
        let working_dir = temp.path().join("workspace").to_string_lossy().into_owned();
        let tasks = extension_tasks("memory-worker-test");
        let (commands, receiver) = mpsc::channel(STORE_QUEUE_CAPACITY);
        let worker_tasks = tasks.clone();
        let worker_store_pool = Arc::clone(&store_pool);
        tasks.spawn("memory-store-worker-test", async move {
            run_store_worker(
                worker_tasks,
                worker_store_pool,
                Arc::new(SessionPrefsCache::default()),
                receiver,
            )
            .await;
        });

        let (first_reply, abandoned_result) = oneshot::channel();
        commands
            .send(StoreCommand::Append {
                working_dir: working_dir.clone(),
                category: "general".into(),
                content: "first value".into(),
                reply: first_reply,
            })
            .await
            .unwrap();
        drop(abandoned_result);

        let (second_reply, second_result) = oneshot::channel();
        commands
            .send(StoreCommand::Upsert {
                working_dir: working_dir.clone(),
                category: "general".into(),
                content: "updated value".into(),
                replaces: "first value".into(),
                reply: second_reply,
            })
            .await
            .unwrap();
        assert!(second_result.await.unwrap().unwrap());
        let memory = store_pool
            .get_scoped(&working_dir)
            .unwrap()
            .project
            .read_memory()
            .unwrap();
        assert!(memory.contains("updated value"));
        assert!(!memory.contains("first value"));

        cancel_extension_tasks(&tasks);
        assert!(wait_extension_tasks(&tasks, Duration::from_secs(1)).await);
        let (late_reply, _late_result) = oneshot::channel();
        assert!(
            commands
                .send(StoreCommand::Delete {
                    working_dir,
                    pattern: "updated value".into(),
                    reply: late_reply,
                })
                .await
                .is_err()
        );
    }
}
