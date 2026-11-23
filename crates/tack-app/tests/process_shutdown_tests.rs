//! Process-level signal handling: prove graceful exit statuses rather than
//! default signal death, and preserve ordinary RPC stdin-EOF behavior.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

async fn ready_rpc() -> (tempfile::TempDir, Child) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(["--mode", "rpc", "--offline", "--agent-dir"])
        .arg(dir.path())
        .current_dir(dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"{\"id\":\"ready\",\"type\":\"get_state\"}\n{\"id\":\"ready2\",\"type\":\"get_state\"}\n")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    child.stdin = Some(stdin);
    let stdout = child.stdout.take().unwrap();
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    for _ in 0..2 {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(line.contains("get_state"), "unexpected RPC output: {line}");
    }
    (dir, child)
}

async fn signal_and_wait(signal: &str, expected_code: i32) {
    let (_dir, mut child) = ready_rpc().await;
    let status = Command::new("kill")
        .arg(signal)
        .arg(child.id().unwrap().to_string())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let status = tokio::time::timeout(std::time::Duration::from_secs(6), child.wait())
        .await
        .expect("graceful shutdown exceeded its bound")
        .unwrap();
    assert_eq!(status.code(), Some(expected_code), "status: {status:?}");
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(status.signal(), None, "process died from the signal itself");
}

#[tokio::test]
async fn rpc_sighup_runs_graceful_shutdown() {
    signal_and_wait("-HUP", 129).await;
}

#[tokio::test]
async fn rpc_sigterm_runs_graceful_shutdown() {
    signal_and_wait("-TERM", 143).await;
}

#[tokio::test]
async fn rpc_sigterm_kills_active_bash_tree_before_watchdog() {
    let (dir, mut child) = ready_rpc().await;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"{\"id\":\"work\",\"type\":\"bash\",\"command\":\"sleep 30 & echo $! > descendant.pid; sleep 30\"}\n",
        )
        .await
        .unwrap();
    let pid_path = dir.path().join("descendant.pid");
    for _ in 0..50 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let descendant = std::fs::read_to_string(&pid_path)
        .expect("bash descendant was not started")
        .trim()
        .to_string();

    let started = std::time::Instant::now();
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(child.id().unwrap().to_string())
            .status()
            .await
            .unwrap()
            .success()
    );
    let status = tokio::time::timeout(std::time::Duration::from_secs(6), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(143));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "cleanup used the forced-exit watchdog rather than draining bash"
    );

    for _ in 0..50 {
        let alive = Command::new("kill")
            .arg("-0")
            .arg(&descendant)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap()
            .success();
        if !alive {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("descendant process {descendant} survived RPC shutdown");
}

#[tokio::test]
async fn rpc_stdin_eof_still_exits_zero() {
    let (_dir, mut child) = ready_rpc().await;
    drop(child.stdin.take());
    let status = tokio::time::timeout(std::time::Duration::from_secs(6), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(0));
}
