//! Subagent worktree registry and lifecycle.
//!
//! `subagent` with `isolation: "worktree"` leaves its checkout behind for the
//! parent to inspect or merge. Without a registry that is a dead-end: the
//! model gets a branch name in prose and the human has to reconstruct the
//! `git worktree`/`git merge` commands by hand. This module records every
//! worktree the subagent tool creates and exposes list/apply/discard/prune
//! operations for `/worktree`.
//!
//! Safety rules:
//! - only branches with the `tack-subagent-` prefix are ever removed;
//! - apply stages changes in the main repo (`merge --squash`), never
//!   commits or pushes on the user's behalf; it refuses a repo with
//!   uncommitted tracked changes and rolls a failed merge back, so the
//!   operation is retryable and never strands the child's commit;
//! - registry writes are serialized (in-process mutex + sibling `.guard`),
//!   so parallel subagents cannot lose each other's rows;
//! - worktree paths are passed as argv to git (never through a shell).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Every subagent worktree branch starts with this prefix. `discard`
/// refuses anything else, so a typo can never delete a user branch.
pub const BRANCH_PREFIX: &str = "tack-subagent-";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub label: String,
    /// Unix seconds (display only).
    pub created: i64,
    pub applied: bool,
    /// Background subagent task that owns this worktree (`bg3`). `None` for
    /// foreground runs and records written before this field existed.
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct WorktreeSummary {
    /// `git diff --shortstat HEAD` (tracked changes), already trimmed.
    pub shortstat: Option<String>,
    pub untracked: usize,
}

#[derive(Clone, Debug)]
pub struct WorktreeInfo {
    pub branch: String,
    pub path: PathBuf,
    pub repo: PathBuf,
    pub label: String,
    pub applied: bool,
    pub present: bool,
    pub summary: Option<WorktreeSummary>,
    pub task_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ApplyOutcome {
    /// Changes were committed in the worktree branch and squash-merged into
    /// the main repo's index (staged, not committed).
    Applied { summary: Option<String> },
    /// The worktree has no uncommitted changes.
    NoChanges,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    worktrees: Vec<WorktreeRecord>,
}

fn registry_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("worktrees.json")
}

/// Serializes registry read-modify-write inside this process. Parallel
/// subagents are the normal registration pattern (`register` runs on the
/// tokio workers), and an unlocked read-modify-write loses rows: eight
/// simultaneous registers reproducibly left one. The sibling `.guard` file
/// (see [`crate::file_lock`]) covers other tack processes (TUI + RPC/serve);
/// it is best-effort, which is exactly why the in-process lock must not be.
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

const REGISTRY_GUARD_STALE: Duration = Duration::from_secs(120);
const REGISTRY_GUARD_WAIT: Duration = Duration::from_secs(5);

struct RegistryGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
    _file: crate::file_lock::Guard,
}

/// Hold both locks for one read-modify-write of `worktrees.json`. Callers
/// must drop the guard before awaiting: `MutexGuard` is not `Send`, and
/// holding it across an await would stall every other registry writer.
fn lock_registry(agent_dir: &Path) -> RegistryGuard {
    let serial = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = crate::file_lock::lock(
        &registry_path(agent_dir),
        REGISTRY_GUARD_STALE,
        REGISTRY_GUARD_WAIT,
    );
    RegistryGuard {
        _serial: serial,
        _file: file,
    }
}

fn read_registry(agent_dir: &Path) -> Registry {
    std::fs::read_to_string(registry_path(agent_dir))
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn write_registry(agent_dir: &Path, registry: &Registry) -> Result<(), String> {
    let json = serde_json::to_string_pretty(registry)
        .map_err(|e| format!("failed to serialize worktree registry: {e}"))?;
    crate::atomic_write::atomic_write(&registry_path(agent_dir), &json)
        .map_err(|e| format!("failed to write worktree registry: {e}"))
}

fn canonical(path: &Path) -> PathBuf {
    // dunce strips Windows' \\?\ prefix; git-for-Windows accepts plain
    // drive paths more reliably than extended-length ones.
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_repo(record: &WorktreeRecord, repo: &Path) -> bool {
    canonical(Path::new(&record.repo)) == canonical(repo)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// True when `branch` is safe for this module to manage.
pub fn is_managed_branch(branch: &str) -> bool {
    branch.len() > BRANCH_PREFIX.len()
        && branch.starts_with(BRANCH_PREFIX)
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn validate_branch(branch: &str) -> Result<(), String> {
    if is_managed_branch(branch) {
        Ok(())
    } else {
        Err(format!(
            "refusing to manage non-{} branch: {branch}",
            BRANCH_PREFIX
        ))
    }
}

/// Record a freshly created subagent worktree. Best-effort at the call site:
/// a registry write failure must never fail the subagent run.
pub fn register(
    agent_dir: &Path,
    repo: &Path,
    path: &Path,
    branch: &str,
    label: &str,
    task_id: Option<&str>,
) -> Result<(), String> {
    validate_branch(branch)?;
    let _guard = lock_registry(agent_dir);
    let mut registry = read_registry(agent_dir);
    let repo = canonical(repo);
    registry
        .worktrees
        .retain(|r| !(same_repo(r, &repo) && r.branch == branch));
    registry.worktrees.push(WorktreeRecord {
        repo: repo.display().to_string(),
        path: canonical(path).display().to_string(),
        branch: branch.to_string(),
        label: label.to_string(),
        created: now_secs(),
        applied: false,
        task_id: task_id.map(str::to_string),
    });
    write_registry(agent_dir, &registry)
}

fn mark_applied(agent_dir: &Path, repo: &Path, branch: &str, label: &str) {
    let _guard = lock_registry(agent_dir);
    let mut registry = read_registry(agent_dir);
    let repo_path = canonical(repo);
    if let Some(record) = registry
        .worktrees
        .iter_mut()
        .find(|r| same_repo(r, &repo_path) && r.branch == branch)
    {
        record.applied = true;
    } else {
        // Not registered (worktree from an older run): add the record so
        // the applied state survives.
        registry.worktrees.push(WorktreeRecord {
            repo: repo_path.display().to_string(),
            path: String::new(),
            branch: branch.to_string(),
            label: label.to_string(),
            created: now_secs(),
            applied: true,
            task_id: None,
        });
    }
    let _ = write_registry(agent_dir, &registry);
}

fn remove_record(agent_dir: &Path, repo: &Path, branch: &str) -> Result<(), String> {
    let _guard = lock_registry(agent_dir);
    let mut registry = read_registry(agent_dir);
    let repo = canonical(repo);
    let before = registry.worktrees.len();
    registry
        .worktrees
        .retain(|r| !(same_repo(r, &repo) && r.branch == branch));
    if registry.worktrees.len() == before {
        return Ok(());
    }
    write_registry(agent_dir, &registry)
}

/// `git rev-parse --show-toplevel` for `cwd`.
pub async fn repo_root(cwd: &Path) -> Result<PathBuf, String> {
    let top = git_output(cwd, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(top.trim()))
}

/// Every subagent worktree of `repo`: registered ones plus worktrees
/// discovered from git (e.g. created by an older version). Sorted by branch.
pub async fn list(agent_dir: &Path, repo: &Path, with_stats: bool) -> Vec<WorktreeInfo> {
    let repo = canonical(repo);
    let records: Vec<WorktreeRecord> = read_registry(agent_dir)
        .worktrees
        .into_iter()
        .filter(|r| same_repo(r, &repo))
        .collect();
    let discovered = discover(&repo).await.unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (path, branch) in discovered {
        seen.insert(branch.clone());
        let record = records.iter().find(|r| r.branch == branch);
        // Prefer the registered path (canonicalized) when it still exists.
        let path = record
            .map(|r| PathBuf::from(&r.path))
            .filter(|p| p.exists())
            .unwrap_or(path);
        out.push(make_info(record, &branch, path, &repo, with_stats).await);
    }
    // Records whose worktree is gone stay visible (marked missing) so the
    // user can see what happened and prune them.
    for record in records.iter().filter(|r| !seen.contains(&r.branch)) {
        let path = PathBuf::from(&record.path);
        out.push(make_info(Some(record), &record.branch, path, &repo, with_stats).await);
    }
    out.sort_by(|a, b| a.branch.cmp(&b.branch));
    out
}

async fn make_info(
    record: Option<&WorktreeRecord>,
    branch: &str,
    path: PathBuf,
    repo: &Path,
    with_stats: bool,
) -> WorktreeInfo {
    let present = path.exists();
    let summary = if with_stats && present {
        Some(summarize(&path).await)
    } else {
        None
    };
    WorktreeInfo {
        branch: branch.to_string(),
        path,
        repo: repo.to_path_buf(),
        label: record.map(|r| r.label.clone()).unwrap_or_default(),
        applied: record.is_some_and(|r| r.applied),
        present,
        summary,
        task_id: record.and_then(|r| r.task_id.clone()),
    }
}

async fn summarize(path: &Path) -> WorktreeSummary {
    let shortstat = git_output(path, &["diff", "--shortstat", "HEAD"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let untracked = git_output(path, &["ls-files", "--others", "--exclude-standard"])
        .await
        .map(|s| s.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);
    WorktreeSummary {
        shortstat,
        untracked,
    }
}

/// Worktrees known to git whose branch carries the subagent prefix.
async fn discover(repo: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let out = git_output(repo, &["worktree", "list", "--porcelain"]).await?;
    Ok(parse_porcelain(&out))
}

fn parse_porcelain(text: &str) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && is_managed_branch(branch)
            && let Some(path) = path.take()
        {
            out.push((path, branch.to_string()));
        } else if line.is_empty() {
            path = None;
        }
    }
    out
}

async fn locate(agent_dir: &Path, repo: &Path, branch: &str) -> Result<WorktreeInfo, String> {
    list(agent_dir, repo, false)
        .await
        .into_iter()
        .find(|info| info.branch == branch)
        .ok_or_else(|| format!("unknown worktree branch: {branch}"))
}

/// Commit the worktree's changes on its branch, then squash-merge that
/// branch into the main repo's index. Stops at "staged": committing is the
/// user's decision.
///
/// Idempotent across failures: the parent's index is only touched when it
/// is clean (checked first), and a failed/conflicting merge is rolled back,
/// so a retry after the user resolves the conflict really does finish the
/// merge instead of reporting "no changes" on a commit that never landed.
pub async fn apply(agent_dir: &Path, repo: &Path, branch: &str) -> Result<ApplyOutcome, String> {
    validate_branch(branch)?;
    let info = locate(agent_dir, repo, branch).await?;
    if !info.present {
        return Err(format!("worktree path is gone: {}", info.path.display()));
    }
    // Applying the active checkout to itself would commit whatever the
    // user has in flight there; refuse, like discard does.
    if canonical(&info.path) == canonical(&info.repo) {
        return Err("refusing to apply the active worktree".to_string());
    }
    // The checkout may have been switched to another branch since the
    // subagent ran. Committing there would land the child's changes on a
    // user branch; require the recorded branch to still be checked out.
    let current = git_output(&info.path, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if current.trim() != branch {
        return Err(format!(
            "worktree is on `{}`, expected `{branch}` — refusing to commit there",
            current.trim()
        ));
    }
    let worktree_clean = worktree_is_clean(&info.path).await?;
    // Repeat apply after a success: the branch's work is already staged in
    // the parent. A second `merge --squash` would collide with that staged
    // result, so report the recorded state instead. (With new edits in the
    // worktree, or after the user reset the staged result, we fall through
    // and merge again.)
    if info.applied
        && worktree_clean
        && !git_status_ok(&info.repo, &["diff", "--cached", "--quiet"]).await?
    {
        return Ok(ApplyOutcome::NoChanges);
    }
    // The parent must be clean before we touch its index: a squash merge
    // into unrelated work either fails halfway or stages the merge next to
    // the user's changes — and the rollback below (reset --hard) would then
    // discard those changes too. Refusing early also keeps `apply`
    // retryable: nothing happened, so nothing needs undoing.
    let dirty = git_output(
        &info.repo,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .await?;
    if !dirty.trim().is_empty() {
        return Err(
            "the current repository has uncommitted changes — commit or stash them before applying"
                .to_string(),
        );
    }
    // Commit the child's changes on its branch (the squash merge needs a
    // commit). A clean worktree is fine: a previous apply may have committed
    // but failed to merge, and the retry must still merge that commit.
    if !worktree_clean {
        git_output(&info.path, &["add", "-A"]).await?;
        // Untracked files staged above; if nothing is staged (e.g. only
        // ignored files changed) there is nothing to commit.
        if !git_status_ok(&info.path, &["diff", "--cached", "--quiet"]).await? {
            let message = commit_message(&info.label, branch);
            git_output(
                &info.path,
                &[
                    "-c",
                    "user.name=tack-subagent",
                    "-c",
                    "user.email=subagent@tack.local",
                    // Never let a configured commit.gpgsign turn apply into a
                    // pinentry prompt the TUI cannot show.
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    &message,
                ],
            )
            .await?;
        }
    }

    let head = git_output(&info.repo, &["rev-parse", "HEAD"]).await?;
    if let Err(error) = git_output(&info.repo, &["merge", "--squash", branch]).await {
        // A conflicted squash merge writes markers and unmerged index
        // entries but no MERGE_HEAD, so `git merge --abort` cannot undo it.
        // The parent was clean, so restoring HEAD loses nothing of the
        // user's; the child's work is committed on its branch either way.
        let _ = git_output(&info.repo, &["reset", "--hard", head.trim()]).await;
        return Err(format!(
            "{error} — the repository was restored; resolve the conflict and retry"
        ));
    }
    // Merged, but the result equals HEAD: the branch is already contained
    // (or has a net-zero diff). Nothing to stage.
    if git_status_ok(&info.repo, &["diff", "--cached", "--quiet"]).await? {
        return Ok(ApplyOutcome::NoChanges);
    }
    mark_applied(agent_dir, &info.repo, branch, &info.label);
    let summary = git_output(&info.repo, &["diff", "--cached", "--shortstat"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(ApplyOutcome::Applied { summary })
}

async fn worktree_is_clean(path: &Path) -> Result<bool, String> {
    Ok(git_output(path, &["status", "--porcelain"])
        .await?
        .trim()
        .is_empty())
}

fn commit_message(label: &str, branch: &str) -> String {
    let first = label.lines().next().unwrap_or("").trim();
    let subject = if first.is_empty() { branch } else { first };
    let subject: String = subject.chars().take(72).collect();
    format!("tack subagent: {subject}")
}

/// Remove the worktree and its branch. Never touches non-subagent branches.
pub async fn discard(agent_dir: &Path, repo: &Path, branch: &str) -> Result<(), String> {
    validate_branch(branch)?;
    let info = locate(agent_dir, repo, branch).await?;
    // Never delete the checkout tack itself is running in.
    if canonical(&info.path) == canonical(&info.repo) {
        return Err("refusing to remove the active worktree".to_string());
    }
    if info.path.exists() {
        git_output(
            &info.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &info.path.display().to_string(),
            ],
        )
        .await?;
    }
    let ref_name = format!("refs/heads/{branch}");
    if git_status_ok(&info.repo, &["rev-parse", "--verify", "--quiet", &ref_name]).await? {
        git_output(&info.repo, &["branch", "-D", branch]).await?;
    }
    remove_record(agent_dir, &info.repo, branch)
}

/// Drop registry rows whose worktree directory no longer exists, and ask
/// git to prune its own metadata. Returns how many rows were dropped.
pub async fn prune(agent_dir: &Path, repo: &Path) -> usize {
    let repo = canonical(repo);
    let removed = {
        // Scope the guard: holding a non-Send MutexGuard across the git
        // await below would stall every other registry writer.
        let _guard = lock_registry(agent_dir);
        let mut registry = read_registry(agent_dir);
        let before = registry.worktrees.len();
        registry
            .worktrees
            .retain(|r| !same_repo(r, &repo) || Path::new(&r.path).exists());
        let removed = before - registry.worktrees.len();
        if removed > 0 {
            let _ = write_registry(agent_dir, &registry);
        }
        removed
    };
    let _ = git_cmd(&repo, &["worktree", "prune"]).await;
    removed
}

async fn git_cmd(repo: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("failed to run git: {e}"))
}

async fn git_output(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_cmd(repo, args).await?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn git_status_ok(repo: &Path, args: &[&str]) -> Result<bool, String> {
    Ok(git_cmd(repo, args).await?.status.success())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn porcelain_parser_keeps_only_subagent_branches() {
        let text = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /tmp/wt-a
HEAD 2222222222222222222222222222222222222222
branch refs/heads/tack-subagent-ab12

worktree /tmp/wt-b
HEAD 3333333333333333333333333333333333333333
branch refs/heads/feature/other
";
        let out = parse_porcelain(text);
        assert_eq!(
            out,
            vec![(PathBuf::from("/tmp/wt-a"), "tack-subagent-ab12".to_string())]
        );
    }

    #[test]
    fn branch_guard_rejects_user_branches() {
        assert!(is_managed_branch("tack-subagent-ab12"));
        assert!(!is_managed_branch("tack-subagent-"));
        assert!(!is_managed_branch("main"));
        assert!(!is_managed_branch("tack-subagent-a;rm -rf /"));
        assert!(validate_branch("main").is_err());
        assert!(validate_branch("tack-subagent-ab12").is_ok());
    }

    #[tokio::test]
    async fn register_list_apply_and_discard() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agent_dir = tmp.path().join("agent");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        init_repo(&repo);

        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "tack-subagent-test",
                "HEAD",
            ],
        );
        std::fs::write(wt.join("new.txt"), "fresh\n").unwrap();
        std::fs::write(wt.join("a.txt"), "one\ntwo\n").unwrap();
        register(
            &agent_dir,
            &repo,
            &wt,
            "tack-subagent-test",
            "refactor auth",
            Some("bg7"),
        )
        .unwrap();

        let infos = list(&agent_dir, &repo, true).await;
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert!(infos[0].present);
        assert_eq!(infos[0].label, "refactor auth");
        assert_eq!(infos[0].task_id.as_deref(), Some("bg7"));
        assert!(!infos[0].applied);
        let summary = infos[0].summary.as_ref().unwrap();
        assert!(summary.shortstat.is_some(), "{summary:?}");
        assert_eq!(summary.untracked, 1);

        let outcome = apply(&agent_dir, &repo, "tack-subagent-test")
            .await
            .unwrap();
        let ApplyOutcome::Applied { summary } = outcome else {
            panic!("expected Applied, got {outcome:?}");
        };
        assert!(summary.is_some());
        // Changes are staged in the main repo, not committed.
        assert_eq!(
            std::fs::read_to_string(repo.join("new.txt")).unwrap(),
            "fresh\n"
        );
        let staged = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap();
        let staged = String::from_utf8_lossy(&staged.stdout);
        assert!(staged.contains("new.txt"), "{staged}");
        assert!(staged.contains("a.txt"), "{staged}");
        let infos = list(&agent_dir, &repo, false).await;
        assert!(infos[0].applied);

        discard(&agent_dir, &repo, "tack-subagent-test")
            .await
            .unwrap();
        assert!(!wt.exists(), "worktree directory must be removed");
        let branch = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/tack-subagent-test",
            ])
            .output()
            .unwrap();
        assert!(!branch.status.success(), "branch must be deleted");
        assert!(list(&agent_dir, &repo, false).await.is_empty());
        // The applied change stays staged in the main repo.
        assert!(repo.join("new.txt").exists());
    }

    #[tokio::test]
    async fn apply_refuses_a_worktree_switched_to_another_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agent_dir = tmp.path().join("agent");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        init_repo(&repo);
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "tack-subagent-switched",
                "HEAD",
            ],
        );
        // Simulate a user checking out their own branch in the worktree.
        run_git(&wt, &["checkout", "-q", "-b", "user-branch"]);
        std::fs::write(wt.join("a.txt"), "user work\n").unwrap();
        register(
            &agent_dir,
            &repo,
            &wt,
            "tack-subagent-switched",
            "switched",
            None,
        )
        .unwrap();

        let err = apply(&agent_dir, &repo, "tack-subagent-switched")
            .await
            .unwrap_err();
        assert!(err.contains("expected `tack-subagent-switched`"), "{err}");
        // Nothing was committed on the user's branch.
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).lines().count(), 1);
    }

    #[tokio::test]
    async fn apply_reports_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agent_dir = tmp.path().join("agent");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        init_repo(&repo);
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "tack-subagent-empty",
                "HEAD",
            ],
        );
        register(
            &agent_dir,
            &repo,
            &wt,
            "tack-subagent-empty",
            "nothing to do",
            None,
        )
        .unwrap();
        assert!(matches!(
            apply(&agent_dir, &repo, "tack-subagent-empty")
                .await
                .unwrap(),
            ApplyOutcome::NoChanges
        ));
    }

    #[tokio::test]
    async fn discard_refuses_non_subagent_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        init_repo(&repo);
        let err = discard(&agent_dir, &repo, "main").await.unwrap_err();
        assert!(err.contains("refusing"), "{err}");
    }

    #[tokio::test]
    async fn apply_and_discard_refuse_the_active_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agent_dir = tmp.path().join("agent");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        init_repo(&repo);
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "tack-subagent-self",
                "HEAD",
            ],
        );
        // Treat the linked worktree as the current repo: tack running inside
        // it must not delete (or commit into) its own checkout.
        let err = apply(&agent_dir, &wt, "tack-subagent-self")
            .await
            .unwrap_err();
        assert!(err.contains("active"), "{err}");
        let err = discard(&agent_dir, &wt, "tack-subagent-self")
            .await
            .unwrap_err();
        assert!(err.contains("active"), "{err}");
        assert!(wt.exists());
    }

    #[tokio::test]
    async fn prune_drops_missing_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agent_dir = tmp.path().join("agent");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        init_repo(&repo);
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "tack-subagent-gone",
                "HEAD",
            ],
        );
        register(&agent_dir, &repo, &wt, "tack-subagent-gone", "gone", None).unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(prune(&agent_dir, &repo).await, 1);
        assert!(list(&agent_dir, &repo, false).await.is_empty());
    }
}
