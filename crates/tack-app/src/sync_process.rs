//! Synchronous subprocess execution with a hard deadline.
//!
//! `std::process::Command::output()` has no timeout: a wedged child (a
//! hung fsmonitor daemon, a credential prompt nobody answers, a network
//! filesystem) blocks the caller forever — and the caller is often an
//! async worker thread, where an unbounded wait also disables every
//! timeout and cancellation scheduled on it. Use
//! [`output_with_timeout`] for one-shot probes (`git rev-parse`,
//! `<tool> --version`, ...) from such contexts.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// `Command::output()` with a deadline. On timeout the child is killed
/// and `ErrorKind::TimedOut` is returned. Stdin is always nulled (these
/// probes must never block on a prompt) and stdout/stderr are piped.
pub(crate) fn output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes on helper threads: a full pipe buffer would block
    // the child mid-write and it would never exit (the classic output()
    // deadlock shape — output() itself spawns the same drainers).
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let stdout = out_thread.join().unwrap_or_default();
                let stderr = err_thread.join().unwrap_or_default();
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("command timed out after {}s", timeout.as_secs()),
                ));
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}
