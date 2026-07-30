use std::{future::Future, panic::AssertUnwindSafe};

use futures_util::FutureExt;

/// Spawn a background task and log panics.
///
/// The returned handle owns the supplied future: aborting the handle cancels the
/// future instead of detaching it.
pub fn spawn_traced(
    name: &'static str,
    fut: impl Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if AssertUnwindSafe(fut).catch_unwind().await.is_err() {
            tracing::error!(task = name, "background task panicked");
        }
    })
}

#[cfg(test)]
mod tests {
    use std::future;

    use tokio::sync::oneshot;

    use super::spawn_traced;

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test]
    async fn aborting_handle_cancels_supplied_future() {
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let handle = spawn_traced("abort_test", async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            future::pending::<()>().await;
        });

        started_rx.await.unwrap();
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        dropped_rx.await.unwrap();
    }
}
