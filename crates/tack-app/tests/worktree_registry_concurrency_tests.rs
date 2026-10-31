//! Regression test for concurrent worktree-registry writes.
//!
//! Parallel subagents call `register()` from separate tokio workers; the
//! registry is a read-modify-write of `<agentDir>/worktrees.json`.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use tack_app::worktrees;

fn run_git_ok(dir: &Path, args: &[&str]) {
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
    run_git_ok(dir, &["init", "-q", "-b", "main"]);
    run_git_ok(dir, &["config", "user.email", "test@example.com"]);
    run_git_ok(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("a.txt"), "base\n").unwrap();
    run_git_ok(dir, &["add", "-A"]);
    run_git_ok(dir, &["commit", "-q", "-m", "init"]);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    agent_dir: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    init_repo(&repo);
    Fixture {
        _tmp: tmp,
        repo,
        agent_dir,
    }
}

/// Parallel subagents register their worktrees at (nearly) the same time.
/// `register` is a read-modify-write of `<agentDir>/worktrees.json`; atomic
/// rename prevents corruption but not lost updates.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_registrations_do_not_lose_rows() {
    let f = fixture();
    let mut join = Vec::new();
    for i in 0..8 {
        let repo = f.repo.clone();
        let agent_dir = f.agent_dir.clone();
        join.push(tokio::spawn(async move {
            let branch = format!("tack-subagent-par{i}");
            worktrees::register(
                &agent_dir,
                &repo,
                &repo, // path is irrelevant for this test
                &branch,
                &format!("parallel child {i}"),
                Some(&format!("bg{i}")),
            )
            .unwrap();
        }));
    }
    for j in join {
        j.await.unwrap();
    }
    let listed = worktrees::list(&f.agent_dir, &f.repo, false).await;
    let branches: Vec<&str> = listed.iter().map(|w| w.branch.as_str()).collect();
    assert_eq!(
        listed.len(),
        8,
        "concurrent registers lost rows: {branches:?}"
    );
}
