//! Adversarial tests for the `/tasks` control panel driven through its public
//! dialog surface with *real* background tasks (no mocks of the registry).
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use tack_app::tui::dialogs::TaskDialog;
use tack_app::tui::theme::Theme;
use tack_tools::background::{BackgroundTaskManager, TaskStatus};
use tack_tui::{InputEvent, Key, KeyEvent};

fn key(k: Key) -> InputEvent {
    InputEvent::Key(KeyEvent::plain(k))
}

fn services(cwd: &std::path::Path) -> tack_tools::ToolServices {
    tack_tools::default_services(cwd.to_path_buf())
}

fn frame_text(dialog: &mut TaskDialog, width: u16) -> String {
    dialog
        .render(width)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait (bounded) for the manager to report no running tasks.
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

/// `x` must kill the whole process tree, not just the shell wrapper: a
/// cancelled background task whose grandchild keeps running leaks a process
/// (and the output pipe stays open).
#[tokio::test]
async fn x_kills_the_whole_process_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("child.pid");
    let manager = BackgroundTaskManager::new();
    let id = manager
        .spawn(
            format!("sh -c 'sleep 300 & echo $! > {}; wait'", pid_file.display()),
            &services(tmp.path()),
        )
        .unwrap();

    // Wait for the grandchild pid to land.
    let mut pid = String::new();
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && !text.trim().is_empty()
        {
            pid = text.trim().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!pid.is_empty(), "grandchild never started");

    let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
    dialog.handle_input(&key(Key::Char('x')));
    assert!(
        wait_idle(&manager, Duration::from_secs(10)).await,
        "task stuck running after x"
    );

    let status = manager.tasks()[0].status;
    assert_eq!(status, TaskStatus::Killed, "status: {status:?}");

    // Give the kernel a moment to reap, then the pid must be gone.
    let mut alive = true;
    for _ in 0..100 {
        let probe = std::process::Command::new("kill")
            .arg("-0")
            .arg(&pid)
            .output()
            .unwrap();
        alive = probe.status.success();
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!alive, "grandchild {pid} survived x on task {id}");

    // The panel now shows the terminal state.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    dialog.tick();
    let text = frame_text(&mut dialog, 100);
    assert!(text.contains("killed"), "{text}");
}

/// The viewer follows real growth: open on a task that is still writing,
/// and the panel must pick up new lines without user input.
#[tokio::test]
async fn viewer_follows_live_output() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = BackgroundTaskManager::new();
    manager
        .spawn(
            "for i in $(seq 1 40); do echo line-$i; sleep 0.05; done".to_string(),
            &services(tmp.path()),
        )
        .unwrap();
    let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
    dialog.handle_input(&key(Key::Enter));
    let first = frame_text(&mut dialog, 100);
    assert!(first.contains("following"), "{first}");

    // Drive ticks like the main loop (~100ms) until the task finishes.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut text = String::new();
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(120)).await;
        dialog.tick();
        text = frame_text(&mut dialog, 100);
        if text.contains("line-40") {
            break;
        }
    }
    assert!(text.contains("line-40"), "viewer never caught up:\n{text}");
    // Following the tail keeps the last line visible.
    assert!(frame_text(&mut dialog, 100).contains("line-40"));
}

/// Progress bars (`\r`), multi-line bash commands and tabs must never leak
/// control characters into a rendered panel row.
#[tokio::test]
async fn control_characters_do_not_break_the_panel() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = BackgroundTaskManager::new();
    // printf emits CR/TAB/NL; octal escapes keep the *command* (the row
    // label) free of control characters. A multi-line command gives the row
    // label real newlines to fold.
    let progress = manager
        .spawn(
            "printf '10%%\\01550%%\\015100%%\\012'; printf 'tab\\011here\\012'".to_string(),
            &services(tmp.path()),
        )
        .unwrap();
    let multiline = manager
        .spawn("echo first\necho second".to_string(), &services(tmp.path()))
        .unwrap();
    assert!(wait_idle(&manager, Duration::from_secs(10)).await);
    // Bash commands can contain tabs; a row must fold them like newlines.
    let tabbed = manager.register("echo\tone".to_string());
    let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
    // The multi-line command label is folded onto one row.
    let rows = frame_text(&mut dialog, 120);
    assert!(rows.contains("echo first echo second"), "{rows}");
    for width in [40u16, 80, 200] {
        let text = frame_text(&mut dialog, width);
        assert!(!text.contains('\r'), "CR leaked at width {width}: {text:?}");
        assert!(
            !text.contains('\t'),
            "TAB leaked at width {width}: {text:?}"
        );
        // Each rendered line is one physical row.
        assert!(
            dialog
                .render(width)
                .iter()
                .all(|l| !l.spans.iter().any(|s| s.text.contains('\n'))),
            "newline inside a Line at width {width}"
        );
    }
    // Open the viewer on the first row (progress output).
    dialog.handle_input(&key(Key::Enter));
    let viewer = frame_text(&mut dialog, 80);
    assert!(viewer.contains("10%"), "{viewer}");
    assert!(viewer.contains("100%"), "{viewer}");
    assert!(viewer.contains("tab    here"), "{viewer}");
    let _ = (progress, multiline, tabbed);
}

/// A pasted steering message reaches the task's message sink and is logged
/// in the task output (the audit trail 06b71e4 added).
#[tokio::test]
async fn pasted_steering_message_is_delivered_and_logged() {
    let manager = BackgroundTaskManager::new();
    let task = manager.register("subagent: refactor auth".to_string());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    task.set_message_sink(std::sync::Arc::new(move |text| {
        // Mirror the subagent tool's sink: log + queue.
        sink.lock().unwrap().push(text.to_string());
    }));
    let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
    dialog.handle_input(&key(Key::Char('m')));
    // Paste arrives as one event (bracketed paste).
    dialog.handle_input(&InputEvent::Paste("use the index\nsecond line".to_string()));
    dialog.handle_input(&key(Key::Enter));
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["use the index\nsecond line".to_string()]
    );
    let note = frame_text(&mut dialog, 100);
    assert!(note.contains("bg1"), "{note}");
}

/// `m` on a finished task says so instead of queuing into the void.
#[tokio::test]
async fn steering_a_finished_task_is_refused() {
    let manager = BackgroundTaskManager::new();
    let task = manager.register("subagent: done".to_string());
    task.set_message_sink(std::sync::Arc::new(|_| {}));
    task.finish(TaskStatus::Exited(0), None);
    let mut dialog = TaskDialog::new(manager, Theme::dark());
    dialog.handle_input(&key(Key::Char('m')));
    let text = frame_text(&mut dialog, 100);
    assert!(text.contains("already finished"), "{text}");
}

/// Clearing finished rows must not drop a running task (or its viewer).
#[tokio::test]
async fn clear_finished_keeps_running_rows_and_viewer() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = BackgroundTaskManager::new();
    let finished = manager.register("old".to_string());
    finished.finish(TaskStatus::Exited(0), None);
    let running = manager
        .spawn("sleep 30".to_string(), &services(tmp.path()))
        .unwrap();
    let mut dialog = TaskDialog::new(manager.clone(), Theme::dark());
    dialog.handle_input(&key(Key::Char('c')));
    dialog.tick();
    let text = frame_text(&mut dialog, 100);
    assert!(!text.contains("old"), "{text}");
    assert!(text.contains("sleep 30"), "{text}");
    // Viewer for the running task still opens.
    dialog.handle_input(&key(Key::Enter));
    assert!(frame_text(&mut dialog, 100).contains("following"));
    manager.kill(&running);
    assert!(wait_idle(&manager, Duration::from_secs(10)).await);
}
