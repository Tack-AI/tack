//! Adversarial integration tests for the `/worktree` registry.
//!
//! These exercise the lifecycle the slash command and the subagent tool rely
//! on, including failure modes the shipped unit tests do not cover.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use tack_app::worktrees::{self, ApplyOutcome};

fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

fn run_git_ok(dir: &Path, args: &[&str]) {
    let out = run_git(dir, args);
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

fn add_worktree(repo: &Path, wt: &Path, branch: &str) {
    run_git_ok(
        repo,
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
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = run_git(dir, args);
    String::from_utf8_lossy(&out.stdout).to_string()
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

/// `apply` refuses a dirty parent *before* committing the child's work, so
/// a refusal is retryable: clean the repo and the retry really merges.
#[tokio::test]
async fn dirty_parent_is_refused_before_committing_and_retry_merges() {
    let f = fixture();
    let wt = f._tmp.path().join("wt");
    add_worktree(&f.repo, &wt, "tack-subagent-partial");
    std::fs::write(wt.join("a.txt"), "child\n").unwrap();
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &wt,
        "tack-subagent-partial",
        "child edits a.txt",
        None,
    )
    .unwrap();
    // Parent side has an uncommitted edit: apply must not touch the index.
    std::fs::write(f.repo.join("a.txt"), "parent wip\n").unwrap();

    let err = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-partial")
        .await
        .unwrap_err();
    assert!(
        err.contains("uncommitted changes"),
        "unexpected error: {err}"
    );
    // Nothing was committed on the branch and nothing was staged.
    let log = git_stdout(&wt, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 1, "apply must not commit: {log}");
    assert!(
        git_stdout(&f.repo, &["status", "--porcelain"]).contains("a.txt"),
        "the user's own edit must be untouched"
    );

    // Clean the parent and retry: the child's work is committed and
    // squash-merged (staged, not committed).
    run_git_ok(&f.repo, &["checkout", "--", "a.txt"]);
    let outcome = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-partial")
        .await
        .unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{outcome:?}"
    );
    let staged = git_stdout(&f.repo, &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("a.txt"), "{staged}");
}

/// A failed (conflicting) merge is rolled back in the parent, the child's
/// commit stays on the branch, and once the parent moves back a retry
/// finishes the merge — no stranded work, no discard needed.
#[tokio::test]
async fn conflicting_apply_rolls_back_and_a_retry_merges() {
    let f = fixture();
    let wt = f._tmp.path().join("wt");
    add_worktree(&f.repo, &wt, "tack-subagent-conflict");
    std::fs::write(wt.join("a.txt"), "child\n").unwrap();
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &wt,
        "tack-subagent-conflict",
        "conflicting edit",
        None,
    )
    .unwrap();
    std::fs::write(f.repo.join("a.txt"), "parent\n").unwrap();
    run_git_ok(&f.repo, &["commit", "-q", "-am", "parent edit"]);
    let parent_head = git_stdout(&f.repo, &["rev-parse", "HEAD"]);

    let err = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-conflict")
        .await
        .unwrap_err();
    assert!(err.contains("restored"), "{err}");
    // The parent is exactly where it was: clean tree, clean index, same HEAD.
    assert_eq!(
        git_stdout(&f.repo, &["status", "--porcelain"]),
        "",
        "conflict rollback must leave a clean repo"
    );
    assert_eq!(git_stdout(&f.repo, &["rev-parse", "HEAD"]), parent_head);
    // The child's work is safe on its branch.
    let log = git_stdout(&wt, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 2, "{log}");

    // Move the parent back so the child's change applies, then retry.
    run_git_ok(&f.repo, &["reset", "--hard", "HEAD~1"]);
    let outcome = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-conflict")
        .await
        .unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "child\n"
    );
}

/// Regression for the old failure mode: a conflicted squash merge used to
/// leave unmerged index entries behind with no MERGE_HEAD, so `git merge
/// --abort` could not undo it. `apply` now restores the repo itself.
#[tokio::test]
async fn conflicting_apply_does_not_leave_a_mid_merge_repo() {
    let f = fixture();
    let wt = f._tmp.path().join("wt");
    add_worktree(&f.repo, &wt, "tack-subagent-conflict2");
    std::fs::write(wt.join("a.txt"), "child\n").unwrap();
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &wt,
        "tack-subagent-conflict2",
        "conflicting edit",
        None,
    )
    .unwrap();
    std::fs::write(f.repo.join("a.txt"), "parent\n").unwrap();
    run_git_ok(&f.repo, &["commit", "-q", "-am", "parent edit"]);

    assert!(
        worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-conflict2")
            .await
            .is_err()
    );
    // No unmerged paths, no MERGE_HEAD, no leftover files from the branch.
    assert_eq!(git_stdout(&f.repo, &["status", "--porcelain"]), "");
    let merge_head = run_git(&f.repo, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]);
    assert!(!merge_head.status.success());
    assert!(!f.repo.join("new.txt").exists());
}

/// Registry rows for *other* repos must survive prune/registration.
#[tokio::test]
async fn registry_rows_are_per_repo() {
    let f = fixture();
    let other = f._tmp.path().join("other");
    init_repo(&other);
    // Both recorded checkouts are gone; prune is scoped to one repo.
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &f._tmp.path().join("gone-a"),
        "tack-subagent-a",
        "repo a",
        None,
    )
    .unwrap();
    worktrees::register(
        &f.agent_dir,
        &other,
        &f._tmp.path().join("gone-b"),
        "tack-subagent-b",
        "repo b",
        None,
    )
    .unwrap();
    // prune for repo a must not touch repo b's record.
    assert_eq!(worktrees::prune(&f.agent_dir, &f.repo).await, 1);
    let content = std::fs::read_to_string(f.agent_dir.join("worktrees.json")).unwrap();
    assert!(content.contains("tack-subagent-b"), "{content}");
    assert!(!content.contains("tack-subagent-a"), "{content}");
}

/// A second round of child work after a successful apply: the parent still
/// holds the first squash merge staged, so apply refuses instead of merging
/// on top of it; once the user commits the staged result, the new work
/// applies cleanly.
#[tokio::test]
async fn new_worktree_work_after_an_apply_waits_for_the_user_to_commit() {
    let f = fixture();
    let wt = f._tmp.path().join("wt");
    add_worktree(&f.repo, &wt, "tack-subagent-rounds");
    std::fs::write(wt.join("a.txt"), "child round 1\n").unwrap();
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &wt,
        "tack-subagent-rounds",
        "two rounds",
        None,
    )
    .unwrap();
    let first = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-rounds")
        .await
        .unwrap();
    assert!(matches!(first, ApplyOutcome::Applied { .. }), "{first:?}");
    // Identical repeat: reported as no changes (already staged).
    assert!(matches!(
        worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-rounds")
            .await
            .unwrap(),
        ApplyOutcome::NoChanges
    ));

    // The child writes again while the parent still has the merge staged.
    std::fs::write(wt.join("b.txt"), "child round 2\n").unwrap();
    let err = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-rounds")
        .await
        .unwrap_err();
    assert!(err.contains("uncommitted changes"), "{err}");

    // The human keeps the final say: commit the staged merge, then apply.
    run_git_ok(&f.repo, &["commit", "-q", "-m", "merge round 1"]);
    let second = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-rounds")
        .await
        .unwrap();
    assert!(matches!(second, ApplyOutcome::Applied { .. }), "{second:?}");
    let staged = git_stdout(&f.repo, &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("b.txt"), "{staged}");
}

/// If the user throws away a successful apply's staged result
/// (`git reset --hard`), applying again restores it instead of reporting
/// "no changes" from the stale `applied` flag.
#[tokio::test]
async fn reapply_after_a_reset_restages_the_branch() {
    let f = fixture();
    let wt = f._tmp.path().join("wt");
    add_worktree(&f.repo, &wt, "tack-subagent-restage");
    std::fs::write(wt.join("a.txt"), "child\n").unwrap();
    worktrees::register(
        &f.agent_dir,
        &f.repo,
        &wt,
        "tack-subagent-restage",
        "restage me",
        None,
    )
    .unwrap();
    let first = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-restage")
        .await
        .unwrap();
    assert!(matches!(first, ApplyOutcome::Applied { .. }), "{first:?}");

    // The human drops the staged merge (keeps the child's branch).
    run_git_ok(&f.repo, &["reset", "--hard", "HEAD"]);
    let second = worktrees::apply(&f.agent_dir, &f.repo, "tack-subagent-restage")
        .await
        .unwrap();
    assert!(matches!(second, ApplyOutcome::Applied { .. }), "{second:?}");
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "child\n"
    );
}
