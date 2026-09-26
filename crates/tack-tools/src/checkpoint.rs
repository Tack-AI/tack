//! File-level checkpoints: per-turn snapshots of every file the edit/write
//! tools touch, so a turn's changes can be rolled back with one command.
//! This complements (not replaces) conversation-level rewind: `/rewind`
//! forks the session tree, `/checkpoints restore <turn>` restores files.
//!
//! Layout under `<agent dir>/checkpoints/<session id>/`:
//!   `turn-<n>/meta.json`   { "time": ..., "files": [{path, existed, blob}] }
//!   `turn-<n>/blob-<k>`    pre-edit content of the k-th file
//!
//! Only the FIRST touch of a file within a turn is snapshotted (that's the
//! state to roll back to). Files that did not exist are recorded with
//! `existed: false` and deleted on restore.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct CheckpointFile {
    pub path: PathBuf,
    /// false = file was created this turn (deleted on restore).
    pub existed: bool,
}

#[derive(Clone, Debug)]
pub struct CheckpointTurn {
    pub turn: u64,
    pub time: String,
    pub files: Vec<CheckpointFile>,
}

struct Inner {
    root: PathBuf,
    turn: u64,
    /// Paths already snapshotted in the current turn.
    snapshotted: std::collections::HashSet<PathBuf>,
    /// Working directory (for the git baseline that covers bash-made edits).
    workdir: Option<PathBuf>,
    /// Git baseline capture running on a worker thread (spawned by
    /// begin_turn). Joined before anything touches the turn's meta.json.
    pending_baseline: Option<std::thread::JoinHandle<()>>,
}

/// Upper bound for joining the background git-baseline capture. The
/// baseline thread runs `git rev-parse` / `git status` via
/// std::process::Command with no timeout of its own; a wedged git
/// (fsmonitor daemon, network filesystem, pathological repo) must not
/// wedge every later settle/begin_turn/record/list/restore — an
/// unbounded join blocks the caller synchronously, and under the manager
/// mutex every other checkpoint entry point piles up behind it.
const BASELINE_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Join the background git-baseline capture (if any) so subsequent
/// meta.json reads/writes are ordered after it — WITHOUT holding the
/// manager mutex, and bounded by BASELINE_JOIN_TIMEOUT. Instant in
/// practice: the baseline has a whole model round-trip to finish before
/// the first tool call lands here. On timeout the thread is detached
/// (never joined): that turn's baseline may be incomplete — degraded
/// restore fidelity, but the agent stays responsive.
fn settle_baseline(manager: &Mutex<Option<Inner>>) {
    let handle = manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|inner| inner.pending_baseline.take());
    let Some(handle) = handle else { return };
    let deadline = std::time::Instant::now() + BASELINE_JOIN_TIMEOUT;
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                "checkpoint: git baseline thread wedged past {BASELINE_JOIN_TIMEOUT:?}; detaching"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let _ = handle.join();
}

/// Run blocking IO without stalling the async runtime: on a multi-threaded
/// tokio runtime the current worker is marked as blocked so the runtime
/// moves other tasks (the UI) to a fresh worker; outside a runtime (sync
/// tests, `#[tokio::test(flavor = "current_thread")]`) the work just runs
/// inline. Do NOT nest: callers wrap once at the public-fn boundary.
fn unblock_runtime<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Shared checkpoint registry. Disabled until `enable` is called with a
/// storage root (the host enables it once the session id is known).
#[derive(Clone, Default)]
pub struct CheckpointManager {
    inner: Arc<Mutex<Option<Inner>>>,
}

impl std::fmt::Debug for CheckpointManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointManager")
            .field(
                "enabled",
                &self.inner.lock().map(|i| i.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl CheckpointManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Activate checkpointing rooted at `root` (usually
    /// `<agent dir>/checkpoints/<session id>`). Re-enabling with a different
    /// root (session switch) resets the turn counter.
    pub fn enable(&self, root: PathBuf) {
        // The counter persists in state.json: turn dirs only exist for turns
        // that actually touched files, so directory scanning alone would
        // undercount after quiet turns.
        let persisted = std::fs::read_to_string(root.join("state.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .and_then(|v| v["turn"].as_u64())
            .unwrap_or(0);
        let turn = persisted.max(latest_turn(&root));
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = Some(Inner {
            root,
            turn,
            snapshotted: std::collections::HashSet::new(),
            workdir: None,
            pending_baseline: None,
        });
    }

    /// Set the working directory used for the git baseline (call before
    /// begin_turn). The baseline covers file changes made through bash,
    /// which never pass through the edit/write tools.
    pub fn set_workdir(&self, workdir: PathBuf) {
        if let Some(inner) = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            inner.workdir = Some(workdir);
        }
    }

    pub fn disable(&self) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Wait for the pending git baseline (if any) to finish. begin_turn
    /// captures the baseline on a worker thread so prompt submission never
    /// stalls; every file-mutation entry point that bypasses record() —
    /// bash/powershell run arbitrary commands — MUST call this before
    /// executing so the baseline still reflects the pre-turn state.
    /// Cheap in practice: a full model round-trip passes between
    /// begin_turn and the first tool call, so the join rarely waits.
    pub fn settle(&self) {
        unblock_runtime(|| settle_baseline(&self.inner));
    }

    pub fn is_enabled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Start a new turn (host calls this when the user submits a prompt).
    /// Returns the new turn number; 0 when disabled.
    ///
    /// The git baseline (`git status` + reading every dirty/untracked file)
    /// runs on a worker thread: it is by far the slowest part of prompt
    /// submission and the host calls this synchronously on the UI task.
    /// Ordering is preserved because record/restore/list join the pending
    /// baseline before touching the turn's meta.json — and a full model
    /// round-trip passes before any tool can mutate files.
    pub fn begin_turn(&self) -> u64 {
        // The previous turn's baseline is long finished; join defensively
        // (off the async worker, and without holding the manager lock) so
        // threads can't pile up.
        unblock_runtime(|| settle_baseline(&self.inner));
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(inner) = guard.as_mut() else {
            return 0;
        };
        inner.turn += 1;
        inner.snapshotted.clear();
        let root = inner.root.clone();
        let turn = inner.turn;
        // Small metadata write: cheap, and enable() re-reads it on session
        // switch, so it stays synchronous (unlike the baseline below).
        if std::fs::create_dir_all(&root).is_ok() {
            let _ = std::fs::write(root.join("state.json"), json!({ "turn": turn }).to_string());
        }
        // Git baseline: snapshot everything already dirty/untracked so that
        // bash-made changes during the turn are also restorable (clean files
        // are restored via `git checkout`, dirty ones from these blobs).
        if let Some(workdir) = inner.workdir.clone() {
            let thread_root = root.clone();
            inner.pending_baseline = Some(std::thread::spawn(move || {
                capture_git_baseline(&thread_root, turn, &workdir);
            }));
        }
        turn
    }

    pub fn current_turn(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|i| i.turn)
            .unwrap_or(0)
    }

    /// Snapshot `path` before modification (first touch per turn only).
    /// Call BEFORE writing the new content. No-op when disabled or when the
    /// turn hasn't started (turn 0 — snapshot anyway under turn-0? No: the
    /// host must begin_turn first; until then changes are not tracked).
    pub fn record(&self, path: &Path) {
        // Whole-file read + blob write: edit/write tools call this from
        // async contexts, so keep the executor responsive while it runs.
        unblock_runtime(|| self.record_blocking(path));
    }

    fn record_blocking(&self, path: &Path) {
        // meta.json writes must be ordered after the background baseline —
        // joined without holding the manager lock.
        settle_baseline(&self.inner);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(inner) = guard.as_mut() else { return };
        let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if inner.snapshotted.contains(&canonical) {
            return;
        }
        inner.snapshotted.insert(canonical.clone());

        let dir = inner.root.join(format!("turn-{}", inner.turn.max(1)));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("checkpoint: cannot create {}: {e}", dir.display());
            return;
        }

        // Read once and derive existence from the result: a separate
        // exists() → read() sequence races with a concurrent delete
        // (TOCTOU) and could record a stale `existed: true`.
        let mut meta = read_meta(&dir);
        let blob = format!("blob-{}", meta["files"].as_array().map_or(0, |f| f.len()));
        let existed = match std::fs::read(path) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(dir.join(&blob), bytes) {
                    tracing::warn!("checkpoint: cannot snapshot {}: {e}", path.display());
                    return;
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                tracing::warn!("checkpoint: cannot read {}: {e}", path.display());
                return;
            }
        };
        if meta["time"].is_null() {
            meta["time"] = Value::String(chrono_now());
        }
        // read_meta() normalizes `files` to an array, but stay panic-free even
        // if the on-disk shape changes underneath us.
        let Some(files) = meta["files"].as_array_mut() else {
            tracing::warn!(
                "checkpoint: meta files is not an array in {}",
                dir.display()
            );
            return;
        };
        files.push(json!({
            "path": canonical,
            "existed": existed,
            "blob": blob,
        }));
        write_meta(&dir, &meta);
    }

    /// List recorded turns, newest first.
    pub fn list(&self) -> Vec<CheckpointTurn> {
        unblock_runtime(|| {
            settle_baseline(&self.inner);
            let root = {
                let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_ref() {
                    Some(inner) => inner.root.clone(),
                    None => return Vec::new(),
                }
            };
            list_turns(&root)
        })
    }

    /// Restore the files of `turn` to their pre-turn state. Returns the
    /// affected paths (restored or deleted) or an error message.
    pub fn restore(&self, turn: u64) -> Result<Vec<(PathBuf, bool)>, String> {
        // File IO + `git` subprocesses: keep the executor responsive (the
        // /checkpoints command runs on the UI task).
        unblock_runtime(|| self.restore_blocking(turn))
    }

    fn restore_blocking(&self, turn: u64) -> Result<Vec<(PathBuf, bool)>, String> {
        settle_baseline(&self.inner);
        let root = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(inner) => inner.root.clone(),
                None => return Err("checkpoints are disabled".to_string()),
            }
        };
        let dir = root.join(format!("turn-{turn}"));
        let meta = read_meta(&dir);
        let files = meta["files"].as_array().cloned().unwrap_or_default();
        let baseline = meta["baseline"].as_array().cloned().unwrap_or_default();
        let git_root = meta["gitRoot"].as_str().map(PathBuf::from);
        // A turn whose ONLY changes came through bash has no blobs and an
        // empty baseline (everything was clean at turn start) — the git
        // catch-up is still a valid restore.
        if files.is_empty() && baseline.is_empty() && git_root.is_none() {
            return Err(format!("turn {turn} has no recorded file changes"));
        }

        // Paths covered by blob restores (dedupe the git catch-up against
        // them). Canonicalize for comparison.
        let mut covered: std::collections::HashSet<PathBuf> = files
            .iter()
            .filter_map(|f| f["path"].as_str().map(PathBuf::from))
            .collect();
        if let Some(repo) = &git_root {
            for entry in &baseline {
                if let Some(rel) = entry["path"].as_str() {
                    covered.insert(repo.join(rel));
                }
            }
        }

        // Git catch-up: files that were clean at baseline but bash changed
        // during the turn (not covered by any blob).
        let mut catchup: Vec<(PathBuf, bool)> = Vec::new(); // (abs, is_untracked_now)
        if let Some(repo) = &git_root {
            for (rel, status) in git_dirty_paths(repo) {
                let abs = repo.join(&rel);
                if covered.contains(&abs) {
                    continue;
                }
                catchup.push((abs, status == "??"));
            }
        }

        let mut restored = Vec::new();

        // 1. record() blobs (edit/write tool snapshots).
        for file in &files {
            let path = PathBuf::from(file["path"].as_str().unwrap_or_default());
            let existed = file["existed"].as_bool().unwrap_or(true);
            let blob = file["blob"].as_str().unwrap_or_default();
            if existed {
                let bytes = std::fs::read(dir.join(blob))
                    .map_err(|e| format!("cannot read snapshot for {}: {e}", path.display()))?;
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&path, bytes)
                    .map_err(|e| format!("cannot restore {}: {e}", path.display()))?;
                restored.push((path, true));
            } else if path.exists() {
                remove_path(&path).map_err(|e| format!("cannot delete {}: {e}", path.display()))?;
                restored.push((path, false));
            }
        }

        // 2. Baseline blobs (dirty/untracked at turn start).
        if let Some(repo) = &git_root {
            for entry in &baseline {
                if entry["kind"].as_str() == Some("dir") {
                    // Pre-existing untracked directory: leave it (and its
                    // contents) alone — it was never snapshotted.
                    continue;
                }
                let rel = entry["path"].as_str().unwrap_or_default();
                let abs = repo.join(rel);
                match entry["blob"].as_str() {
                    Some(blob) => {
                        if files.iter().any(|f| {
                            f["path"].as_str().map(PathBuf::from)
                                == Some(dunce::canonicalize(&abs).unwrap_or_else(|_| abs.clone()))
                        }) {
                            continue; // already restored in step 1
                        }
                        let bytes = std::fs::read(dir.join(blob)).map_err(|e| {
                            format!("cannot read snapshot for {}: {e}", abs.display())
                        })?;
                        if let Some(parent) = abs.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        std::fs::write(&abs, bytes)
                            .map_err(|e| format!("cannot restore {}: {e}", abs.display()))?;
                        restored.push((abs, true));
                    }
                    None => {
                        // Missing at baseline: ensure it doesn't exist now.
                        if abs.exists() {
                            remove_path(&abs)
                                .map_err(|e| format!("cannot delete {}: {e}", abs.display()))?;
                            restored.push((abs, false));
                        }
                    }
                }
            }
        }

        // 3. Catch-up: bash-made changes — checkout from git, delete new files.
        for (abs, is_untracked) in catchup {
            if is_untracked {
                if abs.exists() {
                    remove_path(&abs)
                        .map_err(|e| format!("cannot delete {}: {e}", abs.display()))?;
                    restored.push((abs, false));
                }
            } else if let Some(repo) = &git_root {
                let rel = abs.strip_prefix(repo).unwrap_or(&abs).to_path_buf();
                let checked_out = git(repo, &["checkout", "HEAD", "--", &rel.to_string_lossy()]);
                if checked_out.is_some() {
                    restored.push((abs, true));
                } else if abs.exists() {
                    // Not in HEAD (staged-new): remove it.
                    remove_path(&abs)
                        .map_err(|e| format!("cannot delete {}: {e}", abs.display()))?;
                    restored.push((abs, false));
                }
            }
        }

        Ok(restored)
    }
}

/// Delete a file or (recursively) a directory.
fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn chrono_now() -> String {
    // Avoid a chrono dep in tack-tools: RFC3339-ish from SystemTime.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

// ---------------------------------------------------------------------
// Git baseline (covers bash-made file changes)
// ---------------------------------------------------------------------

fn git(workdir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_repo_root(workdir: &Path) -> Option<PathBuf> {
    let out = git(workdir, &["rev-parse", "--show-toplevel"])?;
    let path = PathBuf::from(out.trim());
    path.exists().then_some(path)
}

/// `git status --porcelain=v1 -z --no-renames` → (rel_path, status_code).
fn git_dirty_paths(repo: &Path) -> Vec<(String, String)> {
    let Some(out) = git(
        repo,
        ["status", "--porcelain=v1", "-z", "--no-renames"].as_slice(),
    ) else {
        return Vec::new();
    };
    out.split('\0')
        .filter_map(|entry| {
            if entry.len() < 4 {
                return None;
            }
            let status = entry[..2].trim().to_string();
            let path = entry[3..].to_string();
            Some((path, status))
        })
        .collect()
}

/// Snapshot everything dirty/untracked at turn start. Clean-at-baseline
/// files need no blob — `git checkout` restores them; untracked-at-baseline
/// and modified-at-baseline files need blobs.
fn capture_git_baseline(root: &Path, turn: u64, workdir: &Path) {
    let Some(repo) = git_repo_root(workdir) else {
        return;
    };
    let dirty = git_dirty_paths(&repo);
    let dir = root.join(format!("turn-{turn}"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("checkpoint: cannot create {}: {e}", dir.display());
        return;
    }
    let mut meta = read_meta(&dir);
    let mut baseline = Vec::new();
    for (rel, status) in &dirty {
        let abs = repo.join(rel);
        if abs.is_dir() {
            // Untracked DIRECTORY at baseline (git collapses it to `dir/`):
            // it cannot be snapshotted as a blob and must NOT be treated as
            // "missing" — restore would delete a pre-existing directory.
            baseline.push(json!({ "path": rel, "kind": "dir" }));
            continue;
        }
        if status == "??" {
            // Untracked at baseline: snapshot, restore from blob.
            match std::fs::read(&abs) {
                Ok(bytes) => {
                    let blob = format!("base-{}", baseline.len());
                    if std::fs::write(dir.join(&blob), bytes).is_ok() {
                        baseline.push(json!({ "path": rel, "kind": "untracked", "blob": blob }));
                    }
                }
                Err(_) => {
                    baseline.push(json!({ "path": rel, "kind": "missing" }));
                }
            }
        } else if status.contains('D') || !abs.exists() {
            baseline.push(json!({ "path": rel, "kind": "missing" }));
        } else {
            // Tracked-modified at baseline: snapshot current content.
            match std::fs::read(&abs) {
                Ok(bytes) => {
                    let blob = format!("base-{}", baseline.len());
                    if std::fs::write(dir.join(&blob), bytes).is_ok() {
                        baseline.push(json!({ "path": rel, "kind": "modified", "blob": blob }));
                    }
                }
                Err(_) => {
                    baseline.push(json!({ "path": rel, "kind": "missing" }));
                }
            }
        }
    }
    // Always persist gitRoot (even with an empty baseline): a turn whose
    // only changes come through bash restores entirely via the catch-up.
    meta["gitRoot"] = Value::String(repo.display().to_string());
    meta["baseline"] = Value::Array(baseline);
    write_meta(&dir, &meta);
}

fn read_meta(dir: &Path) -> Value {
    let mut meta = std::fs::read_to_string(dir.join("meta.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .unwrap_or_else(|| json!({ "time": Value::Null, "files": [] }));
    // Normalize instead of trusting the on-disk shape: a truncated/foreign
    // meta.json may still be valid JSON without an array `files` field, and
    // callers index into it unconditionally.
    if !meta.is_object() {
        meta = json!({ "time": Value::Null, "files": [] });
    } else if !meta["files"].is_array() {
        meta["files"] = json!([]);
    }
    meta
}

fn write_meta(dir: &Path, meta: &Value) {
    match serde_json::to_string_pretty(meta) {
        Ok(json) => {
            if let Err(e) = std::fs::write(dir.join("meta.json"), json) {
                tracing::warn!("checkpoint: cannot write meta in {}: {e}", dir.display());
            }
        }
        Err(e) => tracing::warn!("checkpoint: cannot serialize meta: {e}"),
    }
}

fn latest_turn(root: &Path) -> u64 {
    list_turns(root).first().map(|t| t.turn).unwrap_or(0)
}

fn list_turns(root: &Path) -> Vec<CheckpointTurn> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut turns: Vec<CheckpointTurn> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let turn: u64 = name.strip_prefix("turn-")?.parse().ok()?;
            let meta = read_meta(&e.path());
            let time = meta["time"].as_str().unwrap_or_default().to_string();
            let files = meta["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .map(|f| CheckpointFile {
                            path: PathBuf::from(f["path"].as_str().unwrap_or_default()),
                            existed: f["existed"].as_bool().unwrap_or(true),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(CheckpointTurn { turn, time, files })
        })
        .collect();
    turns.sort_by_key(|t| std::cmp::Reverse(t.turn));
    turns
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn init_repo(repo: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {:?}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir_all(repo).unwrap();
        run(&["init"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("tracked.txt"), "original").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
    }

    /// Regression: an untracked DIRECTORY created during the turn used to
    /// fail the whole restore (remove_file on a directory errors).
    #[test]
    fn restore_removes_untracked_directory_created_during_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        let manager = CheckpointManager::new();
        manager.enable(tmp.path().join("ckpt"));
        manager.set_workdir(repo.clone());
        manager.begin_turn();
        // bash/powershell settle the baseline before running; direct writes
        // in tests must do the same (begin_turn captures it in background).
        manager.settle();

        // "bash" creates a directory tree mid-turn.
        let newdir = repo.join("gen").join("nested");
        std::fs::create_dir_all(&newdir).unwrap();
        std::fs::write(newdir.join("out.txt"), "generated").unwrap();
        std::fs::write(repo.join("tracked.txt"), "changed").unwrap();

        let restored = manager.restore(1).unwrap();
        assert!(restored.len() >= 2, "{restored:?}");
        assert!(!repo.join("gen").exists(), "untracked dir removed");
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "original"
        );
    }

    /// Regression: a pre-existing untracked directory must survive restore
    /// (it used to abort the restore with a remove_file error).
    #[test]
    fn restore_preserves_pre_existing_untracked_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        // Untracked dir exists BEFORE the turn starts.
        std::fs::create_dir_all(repo.join("keepme")).unwrap();
        std::fs::write(repo.join("keepme").join("data.txt"), "precious").unwrap();

        let manager = CheckpointManager::new();
        manager.enable(tmp.path().join("ckpt"));
        manager.set_workdir(repo.clone());
        manager.begin_turn();
        manager.settle(); // baseline in background; mutation paths settle first

        std::fs::write(repo.join("tracked.txt"), "changed").unwrap();
        manager.restore(1).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("keepme").join("data.txt")).unwrap(),
            "precious"
        );
    }

    #[test]
    fn record_and_restore_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let existing = work.join("a.txt");
        std::fs::write(&existing, "before").unwrap();

        let manager = CheckpointManager::new();
        manager.enable(tmp.path().join("ckpt"));
        assert_eq!(manager.begin_turn(), 1);

        // Modify existing + create new file.
        manager.record(&existing);
        std::fs::write(&existing, "after").unwrap();
        let created = work.join("b.txt");
        manager.record(&created);
        std::fs::write(&created, "new file").unwrap();

        // Second modification of the same file in the same turn: no re-snapshot.
        std::fs::write(&existing, "after2").unwrap();
        manager.record(&existing);

        let turns = manager.list();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].files.len(), 2);

        let restored = manager.restore(1).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "before");
        assert!(!created.exists());
    }

    #[test]
    fn disabled_manager_is_noop() {
        let manager = CheckpointManager::new();
        assert_eq!(manager.begin_turn(), 0);
        manager.record(Path::new("whatever"));
        manager.settle();
        assert!(manager.list().is_empty());
        assert!(manager.restore(1).is_err());
    }

    /// begin_turn returns immediately (baseline on a worker thread); settle
    /// waits for it, after which the turn's meta carries the git baseline.
    #[test]
    fn baseline_completes_in_background_and_settle_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        // Pre-dirty file must be captured by the baseline blob.
        std::fs::write(repo.join("tracked.txt"), "dirty").unwrap();

        let manager = CheckpointManager::new();
        manager.enable(tmp.path().join("ckpt"));
        manager.set_workdir(repo.clone());
        assert_eq!(manager.begin_turn(), 1);
        manager.settle();
        manager.settle(); // second settle is a no-op

        let meta = read_meta(&tmp.path().join("ckpt").join("turn-1"));
        assert!(meta["gitRoot"].is_string(), "{meta}");
        let baseline = meta["baseline"].as_array().unwrap();
        assert_eq!(baseline.len(), 1, "{meta}");
        assert_eq!(baseline[0]["path"].as_str().unwrap(), "tracked.txt");
    }

    #[test]
    fn turn_counter_survives_reenable() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = CheckpointManager::new();
        manager.enable(tmp.path().join("ckpt"));
        manager.begin_turn();
        let work = tmp.path().join("x.txt");
        std::fs::write(&work, "v1").unwrap();
        manager.record(&work);
        manager.begin_turn();
        // Simulate app restart: new manager on the same root.
        let manager2 = CheckpointManager::new();
        manager2.enable(tmp.path().join("ckpt"));
        assert_eq!(manager2.current_turn(), 2);
        assert_eq!(manager2.begin_turn(), 3);
    }

    #[test]
    fn git_baseline_covers_bash_style_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {:?}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("tracked.txt"), "original").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);

        let manager = CheckpointManager::new();
        manager.enable(tmp.path().join("ckpt"));
        manager.set_workdir(repo.clone());
        assert_eq!(manager.begin_turn(), 1);
        manager.settle(); // baseline in background; mutation paths settle first

        // "bash" changes: modify tracked, create untracked — no record() calls.
        std::fs::write(repo.join("tracked.txt"), "changed-by-bash").unwrap();
        std::fs::write(repo.join("created-by-script.txt"), "new").unwrap();

        let restored = manager.restore(1).unwrap();
        assert_eq!(restored.len(), 2, "{restored:?}");
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "original"
        );
        assert!(!repo.join("created-by-script.txt").exists());
    }

    #[test]
    fn git_baseline_preserves_pre_dirty_state() {
        // A file already dirty at turn start must restore to the dirty
        // content, NOT to git HEAD.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["init"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "committed").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        // Dirty BEFORE the turn starts.
        std::fs::write(repo.join("f.txt"), "dirty-before-turn").unwrap();

        let manager = CheckpointManager::new();
        manager.enable(tmp.path().join("ckpt"));
        manager.set_workdir(repo.clone());
        manager.begin_turn();
        manager.settle(); // baseline in background; mutation paths settle first

        std::fs::write(repo.join("f.txt"), "changed-during-turn").unwrap();
        manager.restore(1).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("f.txt")).unwrap(),
            "dirty-before-turn"
        );
    }
}
