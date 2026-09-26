//! Monitored background-task spawning.
//!
//! A panic inside a plain `tokio::spawn` kills only that task: the process
//! survives but the feature silently stops working (notifications stop, UI
//! fragments freeze) and the `JoinError` vanishes with the dropped handle.
//! The global panic hook already appends full crash details to
//! `<agent_dir>/crash.log`; `spawn_guarded` adds task-name context and
//! yields `None` to awaiting callers instead of a `JoinError` they might
//! unwrap.

use std::future::Future;

use futures_util::FutureExt as _;

/// Spawn a background task whose panic is reported (tracing + stderr with
/// the task name) instead of vanishing into a dropped `JoinHandle`.
///
/// The returned handle resolves to `None` when the task panicked; fire-and-
/// forget callers can drop it as before.
pub fn spawn_guarded<F>(name: &'static str, fut: F) -> tokio::task::JoinHandle<Option<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(async move {
        match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
            Ok(out) => Some(out),
            Err(_) => {
                // Payload/location/backtrace were logged by the panic hook.
                tracing::error!(
                    task = name,
                    "background task panicked; details in crash.log"
                );
                // writeln! (Result) instead of eprintln! (panics on EPIPE).
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "tack: background task '{name}' panicked (details in crash.log)"
                );
                None
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn guarded_task_returns_output() {
        let out = spawn_guarded("ok-task", async { 41 + 1 }).await.unwrap();
        assert_eq!(out, Some(42));
    }

    #[tokio::test]
    async fn panicking_task_yields_none_instead_of_join_error() {
        // The panic hook prints to stderr here; expected in test output.
        let out = spawn_guarded("boom-task", async {
            panic!("simulated background failure");
        })
        .await
        .unwrap();
        assert_eq!(out, None);
    }
}
