//! Atomic file writes for the security-relevant JSON stores (cron.json,
//! trust.json, permissions.json, settings.json). Plain `std::fs::write`
//! truncates in place: a crash mid-write corrupts the file and every
//! loader silently falls back to empty defaults — dropping all cron jobs,
//! trust decisions, and allow-always rules. Writing a sibling temp file
//! and renaming it over the target leaves either the old or the new
//! contents intact (same pattern as tack-session's atomic_rewrite).

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Replace `path`'s contents with `contents` atomically (temp sibling +
/// rename + parent-dir fsync). Creates the parent directory if needed.
pub fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    atomic_write_impl(path, contents, None)
}

/// atomic_write with restrictive permissions on the temp file (unix mode,
/// e.g. 0o600 for credential stores). The mode survives the rename, so the
/// secret is never on disk with broader permissions — not even between
/// write and a post-hoc chmod. Ignored off unix.
pub fn atomic_write_private(path: &Path, contents: &str, unix_mode: u32) -> std::io::Result<()> {
    atomic_write_impl(path, contents, Some(unix_mode))
}

fn atomic_write_impl(
    path: &Path,
    contents: &str,
    #[allow(unused_variables)] unix_mode: Option<u32>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Random suffix: the pid alone collides when two writes to the same
    // path run concurrently in one process (both create the same tmp file
    // and clobber each other's contents mid-write).
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(
        ".tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let tmp = PathBuf::from(tmp_name);
    let write_result = (|| {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            if let Some(mode) = unix_mode {
                options.mode(mode);
            }
            options.open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // std rename does not replace an existing target on Windows.
    if let Err(e) = std::fs::rename(&tmp, path) {
        std::fs::remove_file(path)?;
        std::fs::rename(&tmp, path).map_err(|_| e)?;
    }
    // Without a directory fsync the rename itself can be lost on a crash,
    // resurrecting the pre-write file (and leaving the tmp file behind).
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn atomic_write_replaces_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("store.json");
        atomic_write(&file, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":1}");
        atomic_write(&file, "{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":2}");
        // No temp/backup litter left behind.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "leftover files: {entries:?}");
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("nested").join("dir").join("store.json");
        atomic_write(&file, "{}").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
    }
}
