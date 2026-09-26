//! Bash execution backends. The default runs commands locally via the
//! resolved shell; ACP mode plugs in a client-terminal executor (tack-app)
//! through the same trait.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::shell::{CommandTransport, ShellConfig, kill_process_tree};

#[derive(Clone, Copy, Debug, Default)]
pub struct BashExecOutcome {
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub timed_out: bool,
}

/// Raw command execution. Output bytes stream through `on_output` (stdout and
/// stderr merged, arrival order).
#[async_trait]
pub trait BashExecutor: Send + Sync + std::fmt::Debug {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
        cancel: CancellationToken,
        on_output: &(dyn for<'a> Fn(&'a [u8]) + Send + Sync),
    ) -> Result<BashExecOutcome, String>;
}

/// Local execution through the resolved shell (port of pi's local bash path).
#[derive(Clone, Debug)]
pub struct LocalBashExecutor {
    pub shell: Arc<ShellConfig>,
    /// OS sandbox (backend + policy). Commands run unsandboxed when None;
    /// only applies to the Argv transport.
    pub sandbox: Option<(crate::sandbox::SandboxBackend, crate::sandbox::SandboxSpec)>,
    /// Extra environment applied to every command (e.g. a per-worktree
    /// CARGO_TARGET_DIR for isolated sub-agents). Applied last, so it
    /// wins over the inherited process env and the derived defaults.
    pub env: Vec<(String, std::ffi::OsString)>,
}

#[async_trait]
impl BashExecutor for LocalBashExecutor {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
        cancel: CancellationToken,
        on_output: &(dyn for<'a> Fn(&'a [u8]) + Send + Sync),
    ) -> Result<BashExecOutcome, String> {
        let shell = &self.shell;
        // Fail a pipeline when ANY stage fails: without pipefail the exit
        // code comes from the last stage and `cargo test 2>&1 | tail`
        // reports success for failing tests. bash-only (not POSIX —
        // dash/sh would fail the `set` itself).
        let pipefail_command;
        let command = if shell.is_bash() {
            pipefail_command = format!("set -o pipefail\n{command}");
            pipefail_command.as_str()
        } else {
            command
        };
        // Sandboxing wraps the argv (bwrap/seatbelt prefix), so it only
        // applies to the Argv transport; the legacy WSL stdin path stays
        // unsandboxed.
        let sandbox_argv = match (&self.sandbox, &shell.transport) {
            (Some((backend, spec)), CommandTransport::Argv) => Some(crate::sandbox::plan(
                Some(backend),
                Some(spec),
                shell,
                command,
                cwd,
            )),
            _ => None,
        };
        let mut cmd = match &sandbox_argv {
            Some((program, args)) => {
                let mut cmd = tokio::process::Command::new(program);
                cmd.args(args);
                cmd.stdin(std::process::Stdio::null());
                cmd
            }
            None => {
                let mut cmd = tokio::process::Command::new(&shell.shell);
                match shell.transport {
                    CommandTransport::Argv => {
                        cmd.args(&shell.args).arg(command);
                        cmd.stdin(std::process::Stdio::null());
                    }
                    CommandTransport::Stdin => {
                        cmd.args(&shell.args);
                        cmd.stdin(std::process::Stdio::piped());
                    }
                }
                cmd
            }
        };
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(cwd);
        // TS getShellEnv: managed bin dir (downloaded fd/rg) on PATH.
        if let Some((key, path)) = crate::shell::path_with_managed_bin() {
            cmd.env(key, path);
        }
        // Cargo under a read-only-by-policy home (sandbox): fall back to
        // a writable CARGO_HOME so builds keep working.
        if let Some((key, value)) =
            crate::sandbox::cargo_home_env(self.sandbox.as_ref().map(|(_, spec)| spec))
        {
            cmd.env(key, value);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(not(windows))]
        {
            // New process group so kill_process_tree can kill children.
            cmd.process_group(0);
        }

        let spawn_label = match &sandbox_argv {
            Some((program, _)) => format!("sandbox wrapper {}", program.display()),
            None => format!("shell {}", shell.shell.display()),
        };
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn {spawn_label}: {e}"))?;
        let pid = child.id().unwrap_or(0);

        // Windows sandbox: attach the process tree to a Job Object (resource
        // containment + reliable tree-kill). Best-effort: on failure the
        // command runs with plain taskkill semantics.
        #[cfg(windows)]
        let job = match &self.sandbox {
            Some((crate::sandbox::SandboxBackend::WindowsJob, spec)) => {
                crate::windows_job::assign(pid, spec)
            }
            _ => None,
        };

        if shell.transport == CommandTransport::Stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(command.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        // Bounded channels (64 chunks × 8KB = 512KB per stream): with an
        // unbounded channel a slow consumer (throttled snapshot updates)
        // lets output pile up in memory without any backpressure on the
        // reader tasks. send().await gives natural backpressure instead.
        const CHUNK_CHANNEL_CAPACITY: usize = 64;
        let (chunk_tx, mut chunk_rx) =
            tokio::sync::mpsc::channel::<Vec<u8>>(CHUNK_CHANNEL_CAPACITY);
        let out_task = tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if chunk_tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let (chunk_tx2, mut chunk_rx2) =
            tokio::sync::mpsc::channel::<Vec<u8>>(CHUNK_CHANNEL_CAPACITY);
        let err_task = tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if chunk_tx2.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut outcome = BashExecOutcome::default();
        let wait = child.wait();
        tokio::pin!(wait);
        let mut timeout_sleep = timeout.map(|t| Box::pin(tokio::time::sleep(t)));
        // Once a reader task finishes (pipe EOF) its channel reports `None`
        // immediately; the branch must be disabled or the select loop spins.
        let mut out_open = true;
        let mut err_open = true;
        let exit_status = loop {
            tokio::select! {
                status = &mut wait => {
                    break Some(status.map_err(|e| format!("failed to wait on shell: {e}"))?);
                }
                _ = cancel.cancelled() => {
                    outcome.cancelled = true;
                    #[cfg(windows)]
                    if let Some(job) = &job {
                        job.terminate();
                    }
                    kill_process_tree(pid);
                    let _ = wait.await;
                    break None;
                }
                _ = async {
                    match &mut timeout_sleep {
                        Some(s) => s.await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    outcome.timed_out = true;
                    #[cfg(windows)]
                    if let Some(job) = &job {
                        job.terminate();
                    }
                    kill_process_tree(pid);
                    let _ = wait.await;
                    break None;
                }
                chunk = chunk_rx.recv(), if out_open => {
                    match chunk {
                        Some(data) => on_output(&data),
                        None => out_open = false,
                    }
                }
                chunk = chunk_rx2.recv(), if err_open => {
                    match chunk {
                        Some(data) => on_output(&data),
                        None => err_open = false,
                    }
                }
            }
        };

        // Drain output still queued behind a slow consumer: `wait` can win
        // the select race while the reader tasks have already forwarded the
        // final chunks into the channels. Without this drain that tail is
        // silently dropped. But do NOT await the reader tasks to pipe EOF:
        // a backgrounded grandchild (`sleep 100 \u0026`) inherits the pipes and
        // holds them open after the shell exits, which would hang exec past
        // any timeout. Data already in flight arrives immediately, so stop
        // draining once the channels have been quiet for DRAIN_QUIET.
        // DRAIN_MAX bounds the whole drain: a grandchild that detached from
        // the process group (or was reparented) and keeps writing would
        // otherwise reset the quiet window forever and never let exec return.
        const DRAIN_QUIET: Duration = Duration::from_millis(100);
        const DRAIN_MAX: Duration = Duration::from_secs(5);
        let drain_deadline = tokio::time::Instant::now() + DRAIN_MAX;
        loop {
            if !out_open && !err_open {
                break;
            }
            tokio::select! {
                chunk = chunk_rx.recv(), if out_open => {
                    match chunk {
                        Some(data) => on_output(&data),
                        None => out_open = false,
                    }
                }
                chunk = chunk_rx2.recv(), if err_open => {
                    match chunk {
                        Some(data) => on_output(&data),
                        None => err_open = false,
                    }
                }
                _ = tokio::time::sleep_until(drain_deadline) => break,
                _ = tokio::time::sleep(DRAIN_QUIET) => break,
            }
        }
        out_task.abort();
        err_task.abort();

        outcome.exit_code = exit_status.and_then(|s| s.code());
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn executor() -> LocalBashExecutor {
        let shell = crate::shell::resolve_shell(None).unwrap();
        LocalBashExecutor {
            shell: Arc::new(shell),
            sandbox: None,
            env: Vec::new(),
        }
    }

    /// Working directory for exec tests: /tmp does not exist on Windows
    /// (CreateProcess fails with ERROR_DIRECTORY, os error 267), so use
    /// the platform temp dir instead.
    fn exec_cwd() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    /// Regression: `wait` can win the select race while final output chunks
    /// are still queued in the channel behind a slow consumer — the tail
    /// must be drained, not dropped.
    #[tokio::test]
    async fn output_queued_behind_slow_consumer_is_not_lost() {
        let executor = executor();
        for i in 0..3 {
            let total = Arc::new(AtomicUsize::new(0));
            let t = total.clone();
            let on_output = move |data: &[u8]| {
                t.fetch_add(data.len(), Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
            };
            // Finite producer that finishes cleanly (no SIGPIPE, so the
            // exit code is 0 with and without pipefail).
            let outcome = executor
                .exec(
                    "head -c 500000 /dev/zero | tr '\\0' '1'",
                    &exec_cwd(),
                    Some(Duration::from_secs(30)),
                    CancellationToken::new(),
                    &on_output,
                )
                .await
                .unwrap();
            let got = total.load(Ordering::SeqCst);
            assert_eq!(got, 500_000, "iter {i}: lost {} bytes", 500_000 - got);
            assert_eq!(outcome.exit_code, Some(0));
        }
    }

    /// pipefail: the pipeline's exit code must surface ANY failing stage,
    /// not just the last — `cargo test 2>&1 | tail` must not mask a
    /// failing test run. bash only (sh gets no pipefail injection).
    #[tokio::test]
    async fn pipefail_surfaces_upstream_failure() {
        let executor = executor();
        let outcome = executor
            .exec(
                "false | true",
                &exec_cwd(),
                Some(Duration::from_secs(5)),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        if executor.shell.is_bash() {
            assert_eq!(outcome.exit_code, Some(1), "pipefail must surface false");
        } else {
            assert_eq!(outcome.exit_code, Some(0), "sh keeps last-stage semantics");
        }
        // And the success path is unaffected.
        let outcome = executor
            .exec(
                "echo ok | cat",
                &exec_cwd(),
                Some(Duration::from_secs(5)),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(0));
    }

    /// A command that closes its stdout early must still run to completion
    /// (and the select loop must not spin on the closed channel).
    #[tokio::test]
    async fn closed_stdout_still_completes_and_times_out() {
        let executor = executor();
        let outcome = executor
            .exec(
                "exec 1>&- 2>&-; sleep 0.1",
                &exec_cwd(),
                Some(Duration::from_secs(10)),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(0));

        // Timeout still fires with closed pipes.
        let start = std::time::Instant::now();
        let outcome = executor
            .exec(
                "exec 1>&- 2>&-; sleep 30",
                &exec_cwd(),
                Some(Duration::from_millis(300)),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(outcome.timed_out, "{outcome:?}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "took {:?}",
            start.elapsed()
        );
    }

    /// A command that backgrounds a child (`sleep 5 &`) and exits leaves
    /// the grandchild holding the stdout pipe open. exec must return when
    /// the SHELL exits — not when the orphaned grandchild closes the pipe.
    #[cfg(unix)]
    #[tokio::test]
    async fn backgrounded_grandchild_holding_pipe_does_not_hang_exec() {
        let executor = executor();
        let start = std::time::Instant::now();
        let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let c = collected.clone();
        let outcome = executor
            .exec(
                "sleep 5 & echo done",
                &exec_cwd(),
                Some(Duration::from_secs(30)),
                CancellationToken::new(),
                &move |data: &[u8]| c.lock().unwrap().extend_from_slice(data),
            )
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "exec hung on grandchild-held pipe: {:?}",
            start.elapsed()
        );
        let out = collected.lock().unwrap();
        assert!(out.windows(4).any(|w| w == b"done"), "lost output: {out:?}");
    }

    #[tokio::test]
    async fn exit_code_propagates() {
        let executor = executor();
        let outcome = executor
            .exec(
                "exit 42",
                &exec_cwd(),
                None,
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(42));
    }

    /// Regression: a backgrounded grandchild that keeps WRITING to the
    /// inherited pipe used to reset the drain-quiet window on every chunk,
    /// so exec never returned. The drain deadline (5s) must bound it.
    #[cfg(unix)]
    #[tokio::test]
    async fn continuously_writing_grandchild_hits_drain_deadline() {
        let executor = executor();
        let start = std::time::Instant::now();
        let outcome = executor
            .exec(
                // Grandchild writes forever; the shell exits immediately.
                "(while :; do echo tick; done) & echo done",
                &exec_cwd(),
                Some(Duration::from_secs(60)),
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(30),
            "drain was not bounded: {elapsed:?}"
        );
    }
}
