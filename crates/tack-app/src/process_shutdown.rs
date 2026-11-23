//! Process termination signals that must still run asynchronous cleanup.
//!
//! Ctrl+C and Ctrl+Z retain their mode-specific behavior. This helper covers
//! SIGHUP/SIGTERM on Unix and `CTRL_CLOSE_EVENT` on Windows, where the OS only
//! gives the process a few seconds to restore the terminal and stop plugins.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const FORCED_EXIT_GRACE: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TerminationReason {
    Hangup = 1,
    Terminate = 2,
    ConsoleClose = 3,
}

impl TerminationReason {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Hangup | Self::ConsoleClose => 129,
            Self::Terminate => 143,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Hangup),
            2 => Some(Self::Terminate),
            3 => Some(Self::ConsoleClose),
            _ => None,
        }
    }
}

/// Owns the signal listener. Normal completion aborts it; once triggered, the
/// forced-exit watchdog is deliberately detached so outer `main.rs` cleanup
/// cannot disarm it by dropping this value.
pub struct ProcessShutdown {
    token: CancellationToken,
    reason: Arc<AtomicU8>,
    listener: JoinHandle<()>,
}

impl std::fmt::Debug for ProcessShutdown {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessShutdown")
            .field("reason", &self.reason())
            .finish_non_exhaustive()
    }
}

impl Default for ProcessShutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessShutdown {
    pub fn new() -> Self {
        let token = CancellationToken::new();
        let reason = Arc::new(AtomicU8::new(0));
        let listener_token = token.clone();
        let listener_reason = reason.clone();
        let listener = tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut hangup = match signal(SignalKind::hangup()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "cannot listen for SIGHUP");
                        return;
                    }
                };
                let mut terminate = match signal(SignalKind::terminate()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "cannot listen for SIGTERM");
                        return;
                    }
                };
                let reason = tokio::select! {
                    _ = hangup.recv() => TerminationReason::Hangup,
                    _ = terminate.recv() => TerminationReason::Terminate,
                };
                trigger(reason, listener_token, listener_reason);
            }
            #[cfg(windows)]
            {
                use tokio::signal::windows::ctrl_close;
                let mut close = match ctrl_close() {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "cannot listen for console close");
                        return;
                    }
                };
                close.recv().await;
                trigger(
                    TerminationReason::ConsoleClose,
                    listener_token,
                    listener_reason,
                );
            }
            #[cfg(not(any(unix, windows)))]
            {
                let _ = (listener_token, listener_reason);
                std::future::pending::<()>().await;
            }
        });
        Self {
            token,
            reason,
            listener,
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.reason().map(TerminationReason::exit_code)
    }

    pub fn reason(&self) -> Option<TerminationReason> {
        TerminationReason::from_u8(self.reason.load(Ordering::Acquire))
    }
}

impl Drop for ProcessShutdown {
    fn drop(&mut self) {
        if self.reason().is_none() {
            self.listener.abort();
        }
    }
}

fn trigger(reason: TerminationReason, token: CancellationToken, stored_reason: Arc<AtomicU8>) {
    if stored_reason
        .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    token.cancel();
    let code = reason.exit_code();
    // A hung terminal write, extension, keyring, or provider must not defeat
    // the operating system's bounded console-close budget.
    tokio::spawn(async move {
        tokio::time::sleep(FORCED_EXIT_GRACE).await;
        std::process::exit(code);
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn reason_exit_codes_match_conventional_signal_statuses() {
        assert_eq!(super::TerminationReason::Hangup.exit_code(), 129);
        assert_eq!(super::TerminationReason::ConsoleClose.exit_code(), 129);
        assert_eq!(super::TerminationReason::Terminate.exit_code(), 143);
    }
}
