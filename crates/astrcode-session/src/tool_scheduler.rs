//! 冲突图工具调度器：基于资源访问声明决定并行/串行执行。

use std::future::Future;

use astrcode_core::tool_access::{ResourceAccess, conflicts};
use astrcode_support::channel_policy::TOOL_SCHEDULER_FINISH_CAPACITY;
use tokio::sync::{mpsc, oneshot};

type BoxStart = Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

struct ActiveTask {
    id: u64,
    accesses: Vec<ResourceAccess>,
}

struct QueuedTask {
    id: u64,
    accesses: Vec<ResourceAccess>,
    start: BoxStart,
}

/// 基于资源冲突判定与并发上限调度工具调用。
pub struct ToolScheduler {
    active: Vec<ActiveTask>,
    queued: Vec<QueuedTask>,
    max_concurrent: usize,
    next_id: u64,
    finish_tx: mpsc::Sender<u64>,
    finish_rx: mpsc::Receiver<u64>,
}

impl ToolScheduler {
    pub fn new(max_concurrent: usize) -> Self {
        let (finish_tx, finish_rx) = mpsc::channel(TOOL_SCHEDULER_FINISH_CAPACITY);
        Self {
            active: Vec::new(),
            queued: Vec::new(),
            max_concurrent: max_concurrent.max(1),
            next_id: 0,
            finish_tx,
            finish_rx,
        }
    }

    /// 提交一个工具执行任务。返回的 oneshot 在任务完成后 resolve。
    pub fn submit<F, Fut, R>(
        &mut self,
        accesses: Vec<ResourceAccess>,
        execute: F,
    ) -> oneshot::Receiver<R>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        let task_id = self.next_id;
        self.next_id += 1;

        let finish_tx = self.finish_tx.clone();
        let start: BoxStart = Box::new(move || {
            Box::pin(async move {
                let result = execute().await;
                let _ = result_tx.send(result);
                let _ = finish_tx.send(task_id).await;
            })
        });

        if self.is_at_capacity() || self.is_conflict_blocked(&accesses, &self.queued) {
            self.queued.push(QueuedTask {
                id: task_id,
                accesses,
                start,
            });
        } else {
            self.start_task(task_id, accesses, start);
        }

        result_rx
    }

    /// 处理已完成任务的 finish 通知，级联启动队列中不再被阻塞的任务。
    pub fn drain_finished(&mut self) {
        while let Ok(task_id) = self.finish_rx.try_recv() {
            self.finish(task_id);
        }
    }

    /// 等待任务完成，同时在等待期间处理 finish 通知以启动排队任务。
    pub async fn await_result<R>(
        &mut self,
        rx: oneshot::Receiver<R>,
    ) -> Result<R, oneshot::error::RecvError> {
        let mut rx = rx;
        loop {
            self.drain_finished();
            let finish_rx = &mut self.finish_rx;
            tokio::select! {
                biased;
                result = &mut rx => {
                    let result = result?;
                    self.drain_finished();
                    return Ok(result);
                },
                Some(_task_id) = finish_rx.recv() => {
                    self.drain_finished();
                },
            }
        }
    }

    fn finish(&mut self, task_id: u64) {
        self.active.retain(|task| task.id != task_id);
        self.start_queued();
    }

    fn start_queued(&mut self) {
        let pending: Vec<QueuedTask> = self.queued.drain(..).collect();
        let mut still_queued = Vec::new();
        for task in pending {
            if self.is_at_capacity() || self.is_conflict_blocked(&task.accesses, &still_queued) {
                still_queued.push(task);
            } else {
                self.start_task(task.id, task.accesses, task.start);
            }
        }
        self.queued = still_queued;
    }

    fn start_task(&mut self, id: u64, accesses: Vec<ResourceAccess>, start: BoxStart) {
        self.active.push(ActiveTask { id, accesses });
        tokio::spawn(start());
    }

    fn is_at_capacity(&self) -> bool {
        self.active.len() >= self.max_concurrent
    }

    fn is_conflict_blocked(&self, accesses: &[ResourceAccess], ahead: &[QueuedTask]) -> bool {
        self.active
            .iter()
            .any(|task| conflicts(accesses, &task.accesses))
            || ahead.iter().any(|task| conflicts(accesses, &task.accesses))
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.active.len()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use astrcode_core::tool_access::ResourceAccess;

    use super::*;

    #[tokio::test]
    async fn non_conflicting_reads_run_in_parallel() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = ToolScheduler::new(4);

        let a = {
            let counter = Arc::clone(&counter);
            scheduler.submit(
                vec![ResourceAccess::read_file("/a.rs")],
                move || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    counter.fetch_sub(1, Ordering::SeqCst);
                    "a"
                },
            )
        };
        let b = scheduler.submit(vec![ResourceAccess::read_file("/b.rs")], || async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            "b"
        });

        let a_result = scheduler.await_result(a).await;
        let b_result = scheduler.await_result(b).await;
        assert_eq!(a_result.unwrap(), "a");
        assert_eq!(b_result.unwrap(), "b");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn write_blocks_read_on_same_path() {
        let mut scheduler = ToolScheduler::new(4);
        let order = Arc::new(tokio::sync::Mutex::new(Vec::<&'static str>::new()));

        let write = {
            let order = Arc::clone(&order);
            scheduler.submit(
                vec![ResourceAccess::write_file("/a.rs")],
                move || async move {
                    order.lock().await.push("write-start");
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    order.lock().await.push("write-end");
                    "w"
                },
            )
        };
        let read = {
            let order = Arc::clone(&order);
            scheduler.submit(
                vec![ResourceAccess::read_file("/a.rs")],
                move || async move {
                    order.lock().await.push("read");
                    "r"
                },
            )
        };

        let _ = scheduler.await_result(write).await;
        let _ = scheduler.await_result(read).await;

        let events = order.lock().await.clone();
        assert_eq!(events, vec!["write-start", "write-end", "read"]);
    }

    #[tokio::test]
    async fn unrelated_write_and_read_can_overlap() {
        let mut scheduler = ToolScheduler::new(4);

        let write = scheduler.submit(vec![ResourceAccess::write_file("/a.rs")], || async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            "w"
        });
        let read = scheduler.submit(vec![ResourceAccess::read_file("/b.rs")], || async { "r" });

        let read_result = scheduler.await_result(read).await.unwrap();
        assert_eq!(read_result, "r");
        assert_eq!(scheduler.active_count(), 1);
        assert_eq!(scheduler.await_result(write).await.unwrap(), "w");
    }

    #[tokio::test]
    async fn independent_session_spawn_tools_run_in_parallel() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut scheduler = ToolScheduler::new(4);

        let first = {
            let counter = Arc::clone(&counter);
            scheduler.submit(Vec::new(), move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                counter.fetch_sub(1, Ordering::SeqCst);
                "first"
            })
        };
        let second = scheduler.submit(Vec::new(), || async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            "second"
        });

        let second_result = scheduler.await_result(second).await.unwrap();
        assert_eq!(second_result, "second");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.await_result(first).await.unwrap(), "first");
    }

    #[tokio::test]
    async fn resource_all_tools_run_serially() {
        let order = Arc::new(tokio::sync::Mutex::new(Vec::<&'static str>::new()));
        let mut scheduler = ToolScheduler::new(4);

        let first = {
            let order = Arc::clone(&order);
            scheduler.submit(vec![ResourceAccess::all()], move || async move {
                order.lock().await.push("first-start");
                tokio::time::sleep(Duration::from_millis(30)).await;
                order.lock().await.push("first-end");
                "first"
            })
        };
        let second = {
            let order = Arc::clone(&order);
            scheduler.submit(vec![ResourceAccess::all()], move || async move {
                order.lock().await.push("second");
                "second"
            })
        };

        let _ = scheduler.await_result(first).await;
        let _ = scheduler.await_result(second).await;

        assert_eq!(
            *order.lock().await,
            vec!["first-start", "first-end", "second"]
        );
    }
}
