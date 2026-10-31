//! Best-effort cross-process guard for read-modify-write JSON stores.
//!
//! [`crate::atomic_write`] prevents *torn* files, but two writers that
//! read-modify-write the same store can still lose each other's entries:
//! both read the old contents and whichever rename lands last wins. This
//! guard serializes the whole read-modify-write cycle with a sibling
//! `<file>.guard` opened with `O_EXCL`.
//!
//! Best-effort by design, matching the extensions lockfile guard it was
//! extracted from: a guard abandoned by a crashed holder is reclaimed once
//! its mtime is older than `stale_after`, and a process that cannot acquire
//! within `wait` proceeds without it (`Guard::held()` is false) instead of
//! blocking forever. Callers that need a hard guarantee within one process
//! (parallel subagents writing the worktree registry) hold their own
//! process-wide mutex as well — [`Guard`] can only *observe* the timeout.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Held guard file; removed on drop. A guard that could not be acquired
/// carries an empty path and drops without touching the filesystem.
#[derive(Debug)]
pub struct Guard {
    path: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Guard {
    /// True when the guard file was actually acquired. False is the
    /// best-effort fallback (another holder did not release in time).
    pub fn held(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }
}

/// Acquire the guard for `target` (a sibling `<target>.guard` file).
///
/// `stale_after` reclaims guards abandoned by crashed processes;
/// `wait` bounds how long this call spins before proceeding unguarded.
pub fn lock(target: &Path, stale_after: Duration, wait: Duration) -> Guard {
    let path = target.with_extension("guard");
    // A first write to a brand-new store (fresh agent dir) would otherwise
    // skip the cross-process guard entirely: create_new on a missing parent
    // fails with NotFound and falls back to "proceed unguarded".
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // `Instant + Duration` panics on overflow; the constants used by the
    // callers are seconds. Clamp the wait defensively.
    let deadline = Instant::now()
        .checked_add(wait)
        .unwrap_or_else(Instant::now);
    loop {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(_) => return Guard { path },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|mtime| mtime.elapsed().ok())
                    .is_some_and(|age| age > stale_after);
                if stale {
                    // Rename-claim the stale guard aside (never delete it in
                    // place — two concurrent reclaims could then both
                    // create_new successfully). Ownership is decided ONLY
                    // by the create_new above.
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    let claimed =
                        path.with_extension(format!("stale-{}-{nanos}", std::process::id()));
                    if std::fs::rename(&path, &claimed).is_ok() {
                        let _ = std::fs::remove_file(&claimed);
                        continue;
                    }
                }
                if Instant::now() >= deadline {
                    tracing::warn!(
                        guard = %path.display(),
                        "cross-process file guard held by another process; proceeding without it"
                    );
                    return Guard {
                        path: PathBuf::new(),
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                return Guard {
                    path: PathBuf::new(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store.json");
        let first = lock(&target, Duration::from_secs(60), Duration::ZERO);
        assert!(first.held());
        assert!(target.with_extension("guard").exists());

        // A second acquisition cannot succeed while the first is held and
        // the wait budget is exhausted.
        let second = lock(&target, Duration::from_secs(60), Duration::ZERO);
        assert!(!second.held(), "guard must not be shared");
        drop(second);

        drop(first);
        assert!(
            !target.with_extension("guard").exists(),
            "guard file must be removed on drop"
        );
        let third = lock(&target, Duration::from_secs(60), Duration::ZERO);
        assert!(third.held(), "guard must be acquirable again");
    }

    #[test]
    fn unheld_guard_drop_leaves_a_foreign_guard_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store.json");
        let guard_path = target.with_extension("guard");
        std::fs::write(&guard_path, b"").unwrap();
        let fallback = lock(&target, Duration::from_secs(60), Duration::ZERO);
        assert!(!fallback.held());
        drop(fallback);
        assert!(
            guard_path.exists(),
            "a fallback guard must not delete the live holder's file"
        );
    }
}
