//! Defensive tar.gz extraction for hostile archives: marketplace sync
//! tarballs (curated catalog repos) and plugin bundle installs
//! (`tack ext install <file.tgz>`). One strict extractor for both — the
//! input is attacker-controlled in each case, so anything outside a
//! regular file tree fails the whole extraction.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

/// Extraction limits. The cumulative cap is the anti-zip-bomb control:
/// decompressed bytes across ALL entries, enforced while streaming.
#[derive(Clone, Copy, Debug)]
pub struct ExtractCaps {
    /// Total uncompressed bytes across every entry.
    pub max_total_bytes: u64,
    /// Uncompressed bytes per single file.
    pub max_file_bytes: u64,
    /// Entry count (files + directories).
    pub max_entries: usize,
}

impl ExtractCaps {
    /// Curated marketplace repo snapshots (a catalog plus maybe docs).
    pub const REPO_SNAPSHOT: ExtractCaps = ExtractCaps {
        max_total_bytes: 64 * 1024 * 1024,
        max_file_bytes: 8 * 1024 * 1024,
        max_entries: 4_096,
    };

    /// Plugin bundle archives (a whole extension directory, WASM blobs
    /// included).
    pub const PLUGIN_BUNDLE: ExtractCaps = ExtractCaps {
        max_total_bytes: 256 * 1024 * 1024,
        max_file_bytes: 64 * 1024 * 1024,
        max_entries: 16_384,
    };
}

/// A path inside the archive, validated component by component.
fn validate_entry_path(path: &Path) -> anyhow::Result<PathBuf> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            // `.` segments are harmless; everything else escapes or
            // anchors the extraction root and is rejected.
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "archive entry escapes the extraction root: {}",
                    path.display()
                );
            }
        }
    }
    if clean.as_os_str().is_empty() {
        anyhow::bail!("archive entry has an empty path");
    }
    Ok(clean)
}

/// Extract `tgz_bytes` into `dest` (created). Every entry must be a
/// regular file or a directory: symbolic/hard links, fifos, and device
/// nodes are rejected outright — a plugin that needs one can create it
/// at runtime, an archive never can. Unix modes are applied masked to
/// 0o777 (no setuid/sgid/sticky). Returns the number of files written.
///
/// The cumulative cap is enforced on ENTRY DATA as it streams out (a zip
/// bomb dies at the ceiling, not the disk); tar container overhead is
/// bounded separately by the entry-count cap.
pub fn extract_tgz(tgz_bytes: &[u8], dest: &Path, caps: &ExtractCaps) -> anyhow::Result<usize> {
    let gz = flate2::read::GzDecoder::new(tgz_bytes);
    let mut decompressed: u64 = 0;
    let mut entries_seen = 0usize;
    let mut files_written = 0usize;

    let mut archive = tar::Archive::new(gz);
    std::fs::create_dir_all(dest)?;
    for entry in archive
        .entries()
        .map_err(|e| anyhow::anyhow!("bad bundle archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| anyhow::anyhow!("bad bundle archive entry: {e}"))?;
        entries_seen += 1;
        if entries_seen > caps.max_entries {
            anyhow::bail!("archive has more than {} entries", caps.max_entries);
        }
        let header = entry.header();
        let entry_type = header.entry_type();
        let is_dir = entry_type == tar::EntryType::Directory;
        if !is_dir && entry_type != tar::EntryType::Regular {
            anyhow::bail!(
                "archive entry {:?} is a {:?}, not a regular file",
                entry
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "?".to_string()),
                entry_type
            );
        }
        let declared_size = header.size().unwrap_or(u64::MAX);
        let mode_bits = header.mode().unwrap_or(0o644);
        if !is_dir && declared_size > caps.max_file_bytes {
            anyhow::bail!("archive entry is {declared_size} bytes, over the per-file cap");
        }
        let clean = validate_entry_path(&entry.path()?)?;
        let target = dest.join(&clean);
        // Defense in depth: the component walk above already rejects
        // escapes; this catches platform quirks (UNC, alternate streams).
        if !target.starts_with(dest) {
            anyhow::bail!(
                "archive entry escapes the extraction root: {}",
                clean.display()
            );
        }
        if is_dir {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Read the entry body through a per-file cap, then write it out.
        // `declared_size` is attacker-controlled; the stream is the truth.
        let mut body = Vec::with_capacity(declared_size.min(caps.max_file_bytes) as usize);
        let read = entry
            .by_ref()
            .take(caps.max_file_bytes.saturating_add(1))
            .read_to_end(&mut body)?;
        decompressed = decompressed.saturating_add(read as u64);
        if read as u64 > caps.max_file_bytes || decompressed > caps.max_total_bytes {
            anyhow::bail!("archive exceeds the extraction size caps");
        }
        std::fs::write(&target, &body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = mode_bits & 0o777;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
        }
        files_written += 1;
    }
    Ok(files_written)
}

/// The single top-level directory of an extracted archive, when every
/// entry shares one (forge snapshots extract as `<repo>-<ref>/...`).
pub fn single_top_level_dir(dest: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(dest).ok()?.flatten();
    let first = entries.next()?;
    if entries.next().is_some() || !first.file_type().ok()?.is_dir() {
        return None;
    }
    Some(first.path())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Write as _;

    /// Build a tar.gz in memory: (path, body, mode, entry_type). Uses the
    /// raw header API — `append_data` (rightly) refuses the malicious
    /// paths these tests exercise.
    fn make_tgz(entries: &[(&str, &[u8], u32, tar::EntryType)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, body, mode, entry_type) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*entry_type);
            header.set_mode(*mode);
            let body: &[u8] = if *entry_type == tar::EntryType::Directory {
                &[]
            } else {
                body
            };
            header.set_size(body.len() as u64);
            // Write the name bytes directly: set_path validates away the
            // traversal paths we deliberately construct.
            let name = path.as_bytes();
            assert!(
                name.len() <= 100,
                "test path too long for the gnu name field"
            );
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_cksum();
            builder.append(&header, body).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    fn caps() -> ExtractCaps {
        ExtractCaps {
            max_total_bytes: 1024,
            max_file_bytes: 128,
            max_entries: 16,
        }
    }

    #[test]
    fn extracts_a_regular_tree() {
        let tgz = make_tgz(&[
            ("pkg/", &[], 0o755, tar::EntryType::Directory),
            (
                "pkg/extension.json",
                br#"{"name":"demo"}"#,
                0o644,
                tar::EntryType::Regular,
            ),
            (
                "pkg/bin/run.sh",
                b"#!/bin/sh\n",
                0o755,
                tar::EntryType::Regular,
            ),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let files = extract_tgz(&tgz, tmp.path(), &caps()).unwrap();
        assert_eq!(files, 2);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("pkg/extension.json")).unwrap(),
            r#"{"name":"demo"}"#
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(tmp.path().join("pkg/bin/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "exec bits survive: {mode:o}");
        }
        assert_eq!(
            single_top_level_dir(tmp.path()),
            Some(tmp.path().join("pkg"))
        );
    }

    #[test]
    fn rejects_traversal_links_and_special_files() {
        for (path, entry_type) in [
            ("../evil", tar::EntryType::Regular),
            ("/abs/evil", tar::EntryType::Regular),
            ("pkg/link", tar::EntryType::Symlink),
            ("pkg/fifo", tar::EntryType::Fifo),
        ] {
            let tgz = make_tgz(&[(path, b"x", 0o644, entry_type)]);
            let tmp = tempfile::tempdir().unwrap();
            let err = extract_tgz(&tgz, tmp.path(), &caps()).unwrap_err();
            assert!(
                !tmp.path().join("evil").exists(),
                "{path}/{entry_type:?} must fail: {err}"
            );
        }
    }

    #[test]
    fn enforces_the_size_caps() {
        // Per-file cap.
        let big = vec![b'x'; 256];
        let tgz = make_tgz(&[("big.bin", &big, 0o644, tar::EntryType::Regular)]);
        let tmp = tempfile::tempdir().unwrap();
        assert!(extract_tgz(&tgz, tmp.path(), &caps()).is_err());
        // Cumulative cap across many small files (16 × 96B > 1024B total
        // would trip entries first; use 13 × 96B = 1248 > 1024).
        let chunk = vec![b'y'; 96];
        let entries: Vec<(String, &[u8], u32, tar::EntryType)> = (0..13)
            .map(|i| {
                (
                    format!("f{i}"),
                    chunk.as_slice(),
                    0o644,
                    tar::EntryType::Regular,
                )
            })
            .collect();
        let refs: Vec<(&str, &[u8], u32, tar::EntryType)> = entries
            .iter()
            .map(|(p, b, m, t)| (p.as_str(), *b, *m, *t))
            .collect();
        let tgz = make_tgz(&refs);
        let tmp = tempfile::tempdir().unwrap();
        assert!(extract_tgz(&tgz, tmp.path(), &caps()).is_err());
    }
}
