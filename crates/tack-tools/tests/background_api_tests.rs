//! Adversarial tests for the typed background-task API added by f2c28ef.
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use tack_tools::background::{BackgroundTaskManager, KillOutcome, TaskStatus};

fn services(cwd: &std::path::Path) -> tack_tools::ToolServices {
    tack_tools::default_services(cwd.to_path_buf())
}

async fn wait_idle(manager: &BackgroundTaskManager, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if !manager.has_running() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn send_message_rejects_bash_unknown_and_finished_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = BackgroundTaskManager::new();
    let id = manager
        .spawn("sleep 30".to_string(), &services(tmp.path()))
        .unwrap();
    let err = manager.send_message(&id, "hi").unwrap_err();
    assert!(err.contains("does not accept messages"), "{err}");

    let err = manager.send_message("bg999", "hi").unwrap_err();
    assert!(err.contains("unknown task"), "{err}");

    let sub = manager.register("subagent: x".to_string());
    sub.set_message_sink(std::sync::Arc::new(|_| {}));
    sub.finish(TaskStatus::Exited(0), None);
    let err = manager.send_message("bg2", "hi").unwrap_err();
    assert!(err.contains("already finished"), "{err}");
    manager.kill(&id);
    assert!(wait_idle(&manager, Duration::from_secs(10)).await);
}

/// A chatty task spilling past the 50KB snapshot cap must mark the output
/// truncated, persist the full output, and have `prune_finished` remove that
/// temp file.
#[tokio::test]
async fn truncated_output_persists_and_prune_removes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = BackgroundTaskManager::new();
    // 60KB of output > DEFAULT_MAX_BYTES (50KB).
    manager
        .spawn(
            "yes 0123456789 | head -c 60000".to_string(),
            &services(tmp.path()),
        )
        .unwrap();
    assert!(wait_idle(&manager, Duration::from_secs(20)).await);
    let output = manager.output("bg1").unwrap();
    assert!(output.truncated, "output should be truncated");
    let path = output.full_output_path.expect("full output path");
    assert!(std::path::Path::new(&path).exists(), "{path} missing");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.len() >= 60_000,
        "full output must be complete: {}",
        body.len()
    );
    manager.prune_finished();
    assert!(
        !std::path::Path::new(&path).exists(),
        "pruned task leaked its temp file: {path}"
    );
    assert!(manager.output("bg1").is_none());
}

/// Finished tasks are evicted oldest-first past the 100 row cap, and ids are
/// never reused (a stale notification must not address a new task).
#[tokio::test]
async fn finished_task_cap_evicts_oldest_without_reusing_ids() {
    let manager = BackgroundTaskManager::new();
    for i in 0..105 {
        let handle = manager.register(format!("task {i}"));
        handle.finish(TaskStatus::Exited(0), None);
    }
    let tasks = manager.tasks();
    assert_eq!(tasks.len(), 100);
    assert_eq!(tasks[0].id, "bg6", "oldest rows must go first");
    assert_eq!(tasks[99].id, "bg105");
    let next = manager.register("fresh".to_string());
    assert_eq!(next.id, "bg106", "ids must not be recycled");
}

#[tokio::test]
async fn kill_reports_terminal_states() {
    let manager = BackgroundTaskManager::new();
    assert!(manager.kill("nope").unknown());
    let done = manager.register("done".to_string());
    done.finish(TaskStatus::Exited(0), None);
    assert!(manager.kill("bg1").already_finished());

    let tmp = tempfile::tempdir().unwrap();
    let running = manager
        .spawn("sleep 30".to_string(), &services(tmp.path()))
        .unwrap();
    assert!(manager.kill(&running).signalled());
    assert!(wait_idle(&manager, Duration::from_secs(10)).await);
    assert_eq!(manager.tasks()[1].status, TaskStatus::Killed);
}

#[tokio::test]
async fn output_len_tracks_growth_without_snapshotting() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = BackgroundTaskManager::new();
    manager
        .spawn(
            "echo one; sleep 0.3; echo two".to_string(),
            &services(tmp.path()),
        )
        .unwrap();
    assert_eq!(manager.output_len("bg1"), Some(0));
    assert!(wait_idle(&manager, Duration::from_secs(10)).await);
    let len = manager.output_len("bg1").unwrap();
    assert!(len >= "one\ntwo\n".len(), "len: {len}");
    assert_eq!(manager.output_len("nope"), None);
}

/// Status lines are rendered inside a one-line panel row: CR, newlines and
/// tabs must all be folded. (Row labels are folded by the panel, not here:
/// the raw label is also matched against worktree records.)
#[test]
fn status_text_folds_control_characters() {
    let manager = BackgroundTaskManager::new();
    let handle = manager.register("tab\there".to_string());
    handle.finish(
        TaskStatus::Failed,
        Some("err\tline\rsecond\nthird".to_string()),
    );
    let text = manager.tasks()[0].status_text();
    assert!(!text.contains(['\t', '\r', '\n']), "{text:?}");
    assert!(text.contains("err"), "{text}");
}

#[test]
fn kill_outcome_helpers() {
    assert!(KillOutcome::Unknown.unknown());
    assert!(KillOutcome::Signalled.signalled());
    assert!(KillOutcome::AlreadyFinished(TaskStatus::Killed).already_finished());
}
