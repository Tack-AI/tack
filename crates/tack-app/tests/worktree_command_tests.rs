//! End-to-end tests for the `/worktree` and `/tasks` TUI commands: command
//! dispatch, rendered notices, and the running-subagent guard from 06b71e4.
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tack_app::tui::{TuiApp, TuiOptions};
use tack_app::worktrees;

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("a.txt"), "base\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

async fn test_app(cwd: &Path, agent_dir: &Path) -> TuiApp {
    unsafe { std::env::set_var("TACK_AGENT_DIR", agent_dir) };
    unsafe { std::env::set_var("TACK_OFFLINE", "1") };
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"updateCheck": false, "notifications": false}"#,
    )
    .unwrap();
    std::fs::write(
        agent_dir.join("auth.json"),
        r#"{"anthropic": {"type": "api_key", "key": "test-key"}}"#,
    )
    .unwrap();
    let model =
        tack_app::model::resolve_model("anthropic", Some("k3"), &tack_session::default_agent_dir())
            .unwrap();
    let mut app = TuiApp::new(TuiOptions {
        model,
        auth: Arc::new(tack_ai::oauth::StaticAuth::from(Some(
            "test-key".to_string(),
        ))),
        thinking: None,
        cwd: cwd.to_path_buf(),
        continue_session: false,
        system_prompt: None,
        session_dir: None,
        flags: tack_app::cli_flags::CliFlags::default(),
    })
    .await
    .unwrap();
    app.test_close_dialog();
    app
}

fn frame(app: &mut TuiApp) -> String {
    let mut out = Vec::new();
    app.render(&mut out).unwrap();
    String::from_utf8_lossy(&out).to_string()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    agent_dir: PathBuf,
    wt: PathBuf,
}

fn fixture(branch: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    init_repo(&repo);
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            branch,
            "HEAD",
        ],
    );
    std::fs::write(wt.join("a.txt"), "child\n").unwrap();
    Fixture {
        _tmp: tmp,
        repo,
        agent_dir,
        wt,
    }
}

#[tokio::test]
async fn worktree_list_renders_branch_label_and_status() {
    let f = fixture("tack-subagent-e2e");
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &f.wt,
        "tack-subagent-e2e",
        "refactor the auth flow",
        Some("bg1"),
    )
    .unwrap();
    let mut app = test_app(&f.repo, &f.agent_dir).await;
    app.on_submit("/worktree".to_string()).await;
    let text = frame(&mut app);
    assert!(text.contains("Subagent worktrees"), "{text}");
    assert!(text.contains("tack-subagent-e2e"), "{text}");
    assert!(text.contains("refactor the auth flow"), "{text}");
    assert!(text.contains("pending"), "{text}");
    // Diff summary comes from git (one modified file).
    assert!(text.contains("1 file changed"), "{text}");
}

#[tokio::test]
async fn worktree_apply_discard_and_prune_through_the_tui() {
    let f = fixture("tack-subagent-apply");
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &f.wt,
        "tack-subagent-apply",
        "child edit",
        None,
    )
    .unwrap();
    let mut app = test_app(&f.repo, &f.agent_dir).await;

    app.on_submit("/worktree apply".to_string()).await;
    let text = frame(&mut app);
    assert!(text.contains("Applied tack-subagent-apply"), "{text}");
    // Staged in the parent, not committed.
    let staged = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.repo)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&staged.stdout).contains("a.txt"));

    // Listing after the apply reports the recorded state.
    app.on_submit("/worktree list".to_string()).await;
    let text = frame(&mut app);
    assert!(text.contains("[applied]"), "{text}");

    app.on_submit("/worktree apply tack-subagent-apply".to_string())
        .await;
    let text = frame(&mut app);
    assert!(text.contains("has no changes to apply"), "{text}");

    app.on_submit("/worktree discard tack-subagent-apply".to_string())
        .await;
    let text = frame(&mut app);
    assert!(text.contains("Discarded tack-subagent-apply"), "{text}");
    assert!(!f.wt.exists());

    // Prune with nothing stale reports zero.
    app.on_submit("/worktree prune".to_string()).await;
    assert!(frame(&mut app).contains("Pruned 0 stale worktree(s)"));
}

#[tokio::test]
async fn worktree_discard_refuses_a_user_branch() {
    let f = fixture("tack-subagent-guard");
    let mut app = test_app(&f.repo, &f.agent_dir).await;
    app.on_submit("/worktree discard main".to_string()).await;
    let text = frame(&mut app);
    assert!(
        text.contains("Worktree operation failed") && text.contains("refusing"),
        "{text}"
    );
    // The main branch is still there.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.repo)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/main"])
        .output()
        .unwrap();
    assert!(out.status.success());
}

/// 06b71e4: apply/discard refuse while the recorded background subagent is
/// still running (same process, matching label).
#[tokio::test]
async fn worktree_is_locked_while_its_subagent_task_runs() {
    let f = fixture("tack-subagent-locked");
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &f.wt,
        "tack-subagent-locked",
        "probe task",
        Some("bg1"),
    )
    .unwrap();
    let mut app = test_app(&f.repo, &f.agent_dir).await;
    // The subagent tool registers its task as "subagent: <prompt prefix>".
    let handle = app
        .test_background_tasks()
        .register("subagent: probe task".to_string());

    app.on_submit("/worktree apply tack-subagent-locked".to_string())
        .await;
    let text = frame(&mut app);
    assert!(
        text.contains("Subagent bg1 is still running"),
        "apply not guarded: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "base\n",
        "guarded apply must not stage anything"
    );

    // A *different* task reusing the id (next process, other label) must not
    // block: the label no longer matches.
    drop(handle);
    let other = app
        .test_background_tasks()
        .register("subagent: unrelated work".to_string());
    app.on_submit("/worktree discard tack-subagent-locked".to_string())
        .await;
    let text = frame(&mut app);
    assert!(
        text.contains("Discarded tack-subagent-locked"),
        "stale task id blocked discard: {text}"
    );
    drop(other);
}

#[tokio::test]
async fn worktree_commands_outside_a_repo_warn() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let cwd = tmp.path().join("plain");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut app = test_app(&cwd, &agent_dir).await;
    app.on_submit("/worktree list".to_string()).await;
    assert!(frame(&mut app).contains("Not a git repository"));
}

#[tokio::test]
async fn tasks_command_shows_and_cancels_a_real_background_task() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let cwd = tmp.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut app = test_app(&cwd, &agent_dir).await;
    let manager = app.test_background_tasks();
    let id = manager
        .spawn(
            "sleep 30".to_string(),
            &tack_tools::default_services(cwd.clone()),
        )
        .unwrap();

    app.on_submit("/tasks".to_string()).await;
    let text = frame(&mut app);
    assert!(text.contains("Background tasks & agents"), "{text}");
    assert!(text.contains("sleep 30"), "{text}");
    assert!(text.contains("1 running"), "{text}");

    app.handle_input(tack_tui::InputEvent::Key(tack_tui::KeyEvent::plain(
        tack_tui::Key::Char('x'),
    )))
    .await;
    assert!(
        frame(&mut app).contains(&format!("Cancelling {id}")),
        "x did not cancel"
    );
    // The registry reflects the kill.
    for _ in 0..200 {
        if !manager.has_running() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(!manager.has_running(), "task {id} survived x");
}
