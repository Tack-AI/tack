//! Cross-process guards for read-modify-write JSON stores.
//!
//! [`crate::atomic_write`] prevents *torn* files, but two writers that
//! read-modify-write the same store can still lose each other's entries.
//! These guards serialize the whole cycle with an OS-backed lock on a sibling
//! `<file>.guard`. The handle releases automatically on drop or process death;
//! no stale reclaim, heartbeat, or unlink race is involved. Lock files remain
//! in place deliberately: unlinking one can split waiters across inodes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Held guard file. A best-effort acquisition that timed out carries no file
/// and reports `held() == false`; a strict acquisition returns an error instead.
#[derive(Debug)]
pub struct Guard {
    path: PathBuf,
    _file: Option<std::fs::File>,
}

impl Guard {
    /// True when the OS lock was actually acquired. False is the best-effort
    /// fallback (another holder did not release in time).
    pub fn held(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }
}

fn empty_guard() -> Guard {
    Guard {
        path: PathBuf::new(),
        _file: None,
    }
}

/// Acquire a best-effort cross-process lock for `target`.
///
/// On timeout or I/O failure this preserves the historical fallback: log and
/// return an unheld guard. Callers needing a hard guarantee must use
/// [`strict_lock`]. `stale_after` is retained for API compatibility; OS locks
/// do not need stale reclamation.
pub fn lock(target: &Path, stale_after: Duration, wait: Duration) -> Guard {
    match strict_lock(target, stale_after, wait) {
        Ok(guard) => guard,
        Err(error) => {
            tracing::warn!(
                guard = %target.with_extension("guard").display(),
                %error,
                "cross-process file guard unavailable; proceeding without it"
            );
            empty_guard()
        }
    }
}

/// Acquire a mandatory OS-backed cross-process lock for `target`.
///
/// Credential refreshes must never continue unguarded, because rotating
/// refresh tokens can make an overlapping refresh invalidate the grant. The
/// parent is canonicalized so symlink aliases of one agent directory share the
/// same lock identity. `stale_after` is retained only for call-site API
/// compatibility.
pub fn strict_lock(target: &Path, stale_after: Duration, wait: Duration) -> std::io::Result<Guard> {
    let _ = stale_after;
    let original = target.with_extension("guard");
    if let Some(parent) = original.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = match original.parent() {
        Some(parent) => {
            let parent = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            parent.join(original.file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "guard target has no name")
            })?)
        }
        None => original,
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    let deadline = Instant::now()
        .checked_add(wait)
        .unwrap_or_else(Instant::now);
    loop {
        use fs2::FileExt as _;
        match file.try_lock_exclusive() {
            Ok(()) => {
                return Ok(Guard {
                    path,
                    _file: Some(file),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("timed out acquiring credential lock {}", path.display()),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
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

        let second = lock(&target, Duration::from_secs(60), Duration::ZERO);
        assert!(!second.held(), "guard must not be shared");
        drop(second);
        drop(first);

        // The file remains, but the OS lock is released and reacquirable.
        assert!(target.with_extension("guard").exists());
        let third = strict_lock(&target, Duration::from_secs(60), Duration::from_secs(1))
            .expect("guard must be acquirable again after release");
        assert!(third.held());
    }

    #[test]
    fn unheld_guard_drop_leaves_the_live_holder_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store.json");
        let holder = strict_lock(&target, Duration::from_secs(60), Duration::ZERO).unwrap();
        let fallback = lock(&target, Duration::from_secs(60), Duration::ZERO);
        assert!(!fallback.held());
        drop(fallback);
        drop(holder);
        assert!(
            strict_lock(&target, Duration::from_secs(60), Duration::from_secs(1)).is_ok(),
            "guard must be acquirable after the live holder drops"
        );
    }

    #[test]
    fn strict_lock_times_out_instead_of_proceeding_unguarded() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("auth.json");
        let first = strict_lock(&target, Duration::from_secs(60), Duration::ZERO).unwrap();
        let error = strict_lock(&target, Duration::from_secs(60), Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(first);
        assert!(strict_lock(&target, Duration::from_secs(60), Duration::ZERO).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn strict_lock_resolves_symlinked_parent_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let alias = tmp.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let _first = strict_lock(
            &real.join("auth.json"),
            Duration::from_secs(60),
            Duration::ZERO,
        )
        .unwrap();
        assert!(
            strict_lock(
                &alias.join("auth.json"),
                Duration::from_secs(60),
                Duration::ZERO
            )
            .is_err()
        );
    }
}
