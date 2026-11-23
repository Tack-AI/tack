//! Detached OAuth refresh lifecycle.
//!
//! A caller may disappear while a provider is rotating a refresh token. The
//! network request and credential write therefore run in a tracked task owned
//! by the process, not by the caller's future. Graceful shutdown waits for all
//! started tasks after stopping new session work.

use std::future::Future;
use std::sync::Mutex;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

static PENDING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Run a refresh to completion even if every returned receiver is dropped.
pub fn spawn_tracked<T, F>(future: F) -> oneshot::Receiver<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let output = future.await;
        // A cancelled caller deliberately does not receive the result. The
        // refresh itself has still completed and persisted its credentials.
        let _ = tx.send(output);
    });
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|task| !task.is_finished());
    pending.push(handle);
    rx
}

/// Wait for every started refresh, ignoring individual task failures.
///
/// Callers must stop new session work first. Individual refresh errors are
/// reported to their callers (or logged by the refresh operation); shutdown
/// only needs the persistence attempt to settle.
pub async fn settled() {
    loop {
        let tasks = {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            if pending.is_empty() {
                return;
            }
            std::mem::take(&mut *pending)
        };
        for task in tasks {
            if let Err(error) = task.await {
                tracing::warn!(%error, "tracked OAuth refresh task did not finish cleanly");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[tokio::test]
    async fn dropped_caller_does_not_cancel_tracked_work() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let receiver = super::spawn_tracked(async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
            42
        });
        started_rx.await.unwrap();
        drop(receiver);
        release_tx.send(()).unwrap();
        super::settled().await;
    }

    #[tokio::test]
    async fn settlement_also_finishes_after_task_panic() {
        let receiver = super::spawn_tracked(async move {
            panic!("fixture panic");
        });
        assert!(receiver.await.is_err());
        super::settled().await;
    }
}
