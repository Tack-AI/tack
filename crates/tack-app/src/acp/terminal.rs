//! ACP client-terminal bash backend (pi's `terminal/*` usage pattern):
//! create → poll output → wait_for_exit (racing timeout/cancel) → kill on
//! abort → always release.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tack_tools::executor::{BashExecOutcome, BashExecutor};
use tokio_util::sync::CancellationToken;

use super::session::{BridgeRequest, TerminalExitInfo};

/// Runs bash commands in client-owned terminals via the bridge.
#[derive(Debug)]
pub struct AcpTerminalExecutor {
    pub bridge: tokio::sync::mpsc::UnboundedSender<BridgeRequest>,
}

impl AcpTerminalExecutor {
    async fn call<R>(
        &self,
        build: impl FnOnce(tokio::sync::oneshot::Sender<R>) -> BridgeRequest,
    ) -> Result<R, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.bridge
            .send(build(tx))
            .map_err(|_| "ACP bridge closed".to_string())?;
        rx.await
            .map_err(|_| "ACP bridge dropped response".to_string())
    }
}

#[async_trait]
impl BashExecutor for AcpTerminalExecutor {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
        cancel: CancellationToken,
        on_output: &(dyn for<'a> Fn(&'a [u8]) + Send + Sync),
    ) -> Result<BashExecOutcome, String> {
        // terminal/create: command as a single string; clients wrap in a shell.
        let terminal_id = self
            .call(|tx| BridgeRequest::TerminalCreate {
                command: command.to_string(),
                cwd: cwd.to_path_buf(),
                respond: tx,
            })
            .await??;

        let result = self
            .run_terminal(&terminal_id, timeout, cancel, on_output)
            .await;

        // terminal/release (MUST release per spec).
        let _ = self
            .call(|tx| BridgeRequest::TerminalRelease {
                terminal_id: terminal_id.clone(),
                respond: tx,
            })
            .await;

        result
    }
}

impl AcpTerminalExecutor {
    async fn run_terminal(
        &self,
        terminal_id: &str,
        timeout: Option<Duration>,
        cancel: CancellationToken,
        on_output: &(dyn for<'a> Fn(&'a [u8]) + Send + Sync),
    ) -> Result<BashExecOutcome, String> {
        let wait = self.call(|tx| BridgeRequest::TerminalWaitExit {
            terminal_id: terminal_id.to_string(),
            respond: tx,
        });
        tokio::pin!(wait);

        let mut timeout_sleep = timeout.map(|t| Box::pin(tokio::time::sleep(t)));
        let mut poll = Box::pin(tokio::time::sleep(Duration::from_millis(200)));
        let mut last_output_len = 0usize;
        let mut outcome = BashExecOutcome::default();

        loop {
            tokio::select! {
                result = &mut wait => {
                    let exit: Result<Result<TerminalExitInfo, String>, String> = result;
                    match exit {
                        Ok(Ok(info)) => {
                            outcome.exit_code = info.exit_code;
                            break;
                        }
                        Ok(Err(e)) => return Err(e),
                        Err(e) => return Err(e),
                    }
                }
                _ = cancel.cancelled() => {
                    outcome.cancelled = true;
                    let _ = self.call(|tx| BridgeRequest::TerminalKill {
                        terminal_id: terminal_id.to_string(),
                        respond: tx,
                    }).await;
                    // Bounded: a client that never answers wait_for_exit after
                    // a kill must not hang the bash tool forever.
                    let _ = tokio::time::timeout(Duration::from_secs(5), &mut wait).await;
                    break;
                }
                _ = async {
                    match &mut timeout_sleep {
                        Some(s) => s.await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    outcome.timed_out = true;
                    let _ = self.call(|tx| BridgeRequest::TerminalKill {
                        terminal_id: terminal_id.to_string(),
                        respond: tx,
                    }).await;
                    let _ = tokio::time::timeout(Duration::from_secs(5), &mut wait).await;
                    break;
                }
                _ = &mut poll => {
                    poll = Box::pin(tokio::time::sleep(Duration::from_millis(200)));
                    if let Ok(Ok(snap)) = self.call(|tx| BridgeRequest::TerminalOutput {
                        terminal_id: terminal_id.to_string(),
                        respond: tx,
                    }).await {
                        let bytes = snap.as_bytes();
                        if bytes.len() < last_output_len {
                            // The client truncated retained output from the
                            // front (output_byte_limit): offsets shifted, so
                            // incremental slicing would re-emit old content.
                            // Resync; the bytes in the truncation window are
                            // lost (display-only stream, never parsed).
                            last_output_len = bytes.len();
                        } else if bytes.len() > last_output_len {
                            on_output(&bytes[last_output_len..]);
                            last_output_len = bytes.len();
                        }
                    }
                }
            }
        }

        // Final output drain.
        if let Ok(Ok(snap)) = self
            .call(|tx| BridgeRequest::TerminalOutput {
                terminal_id: terminal_id.to_string(),
                respond: tx,
            })
            .await
        {
            let bytes = snap.as_bytes();
            if bytes.len() > last_output_len {
                on_output(&bytes[last_output_len..]);
            }
        }

        Ok(outcome)
    }
}
