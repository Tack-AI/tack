//! Plugin bundle archives (roadmap §9): the air-gapped distribution
//! unit. `tack ext bundle pack` produces a deterministic tar.gz of an
//! extension directory; `tack ext install <file.tgz>` extracts it under
//! hostile-input rules (shared `archive_extract`: no links, no
//! traversal, cumulative size cap) and lands in the normal store.

use std::path::{Path, PathBuf};

/// Is this install source a bundle archive path?
pub fn is_bundle_path(source: &str) -> bool {
    source.ends_with(".tgz") || source.ends_with(".tar.gz")
}

/// Compressed-bytes ceiling for reading a bundle off disk (the
/// decompressed caps live in `archive_extract::ExtractCaps::PLUGIN_BUNDLE`).
const MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;

/// Stage a bundle archive into `staging`: extract defensively, then
/// unwrap the single top-level directory when the archive has one (both
/// `pack`-produced `<name>-<version>/...` and hand-rolled flat archives
/// install cleanly).
pub(crate) fn stage_bundle_archive(source: &Path, staging: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(source)
        .map_err(|e| anyhow::anyhow!("cannot read bundle {}: {e}", source.display()))?;
    let declared = file.metadata().map(|m| m.len()).unwrap_or(0);
    if declared > MAX_BUNDLE_BYTES {
        anyhow::bail!(
            "bundle {} is {declared} bytes, over the {MAX_BUNDLE_BYTES}-byte cap",
            source.display()
        );
    }
    let mut bytes = Vec::new();
    use std::io::Read as _;
    file.take(MAX_BUNDLE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        anyhow::bail!("bundle {} exceeds the size cap", source.display());
    }
    let scratch = staging.with_file_name(format!(".bundle-extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    crate::archive_extract::extract_tgz(
        &bytes,
        &scratch,
        &crate::archive_extract::ExtractCaps::PLUGIN_BUNDLE,
    )?;
    let inner =
        crate::archive_extract::single_top_level_dir(&scratch).unwrap_or_else(|| scratch.clone());
    if let Err(e) = std::fs::rename(&inner, staging) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(())
}

/// Minimal manifest view for packing (the full manifest type is the
/// loader's; a bundle only needs name + version for its filename).
#[derive(serde::Deserialize)]
struct BundleManifestStub {
    name: String,
    #[serde(default)]
    version: Option<String>,
}

/// One file to pack, relative path + source.
struct PackEntry {
    rel: PathBuf,
    abs: PathBuf,
    exec: bool,
    is_dir: bool,
}

/// Collect the pack set: everything under `dir` except `.git` and the
/// output file itself. File symlinks that resolve INSIDE the plugin
/// root are dereferenced; dir symlinks and escaping/broken links are
/// skipped with a warning (bundles contain regular files only, by
/// construction — the extractor rejects anything else).
fn collect_pack_entries(dir: &Path, out: &Path, warnings: &mut Vec<String>) -> Vec<PackEntry> {
    let canonical_root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let canonical_out = out.canonicalize().unwrap_or_else(|_| out.to_path_buf());
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());
            if name == ".git" || canonical_path == canonical_out {
                continue;
            }
            let Ok(rel) = path.strip_prefix(dir) else {
                continue;
            };
            let rel = rel.to_path_buf();
            let Ok(meta) = entry.metadata() else {
                warnings.push(format!("skipping unreadable entry {}", path.display()));
                continue;
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                let Ok(target) = path.canonicalize() else {
                    warnings.push(format!("skipping broken symlink {}", path.display()));
                    continue;
                };
                if !target.starts_with(&canonical_root) {
                    warnings.push(format!(
                        "skipping symlink {} escaping the plugin directory",
                        path.display()
                    ));
                    continue;
                }
                let Ok(target_meta) = std::fs::metadata(&target) else {
                    warnings.push(format!("skipping broken symlink {}", path.display()));
                    continue;
                };
                if target_meta.is_dir() {
                    warnings.push(format!(
                        "skipping directory symlink {} (pack the real directory)",
                        path.display()
                    ));
                    continue;
                }
                entries.push(PackEntry {
                    rel,
                    abs: target,
                    exec: is_exec(&target_meta),
                    is_dir: false,
                });
                continue;
            }
            if meta.is_dir() {
                entries.push(PackEntry {
                    rel: rel.clone(),
                    abs: path.clone(),
                    exec: true,
                    is_dir: true,
                });
                stack.push(path);
            } else if meta.is_file() {
                entries.push(PackEntry {
                    rel,
                    abs: path,
                    exec: is_exec(&meta),
                    is_dir: false,
                });
            }
        }
    }
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    entries
}

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Pack an extension directory into a deterministic
/// `<name>-<version>.tgz` (sorted entries, zeroed mtimes/owners,
/// normalized modes — the same input bytes always produce the same
/// bundle, so fingerprints are stable).
pub fn pack_bundle(dir: &Path, output: Option<&Path>) -> anyhow::Result<PathBuf> {
    let manifest_path = dir.join("extension.json");
    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest: BundleManifestStub =
        serde_json::from_str(&content).map_err(|e| anyhow::anyhow!("bad extension.json: {e}"))?;
    // The name feeds the archive prefix and later the store path.
    tack_ext::plugin_id::PluginId::new(&manifest.name, "user")?;
    let version = manifest.version.as_deref().unwrap_or("0.0.0");
    let prefix = format!("{}-{version}", manifest.name);
    let out = output
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{prefix}.tgz")));
    let out = out.canonicalize().unwrap_or(out);

    let mut warnings = Vec::new();
    let entries = collect_pack_entries(dir, &out, &mut warnings);
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let mut builder = tar::Builder::new(Vec::new());
    for entry in &entries {
        let archive_path = PathBuf::from(&prefix).join(&entry.rel);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(if entry.is_dir {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        header.set_mode(if entry.is_dir || entry.exec {
            0o755
        } else {
            0o644
        });
        // Determinism: no mtimes, owners, or names from the build host.
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_username("").ok();
        header.set_groupname("").ok();
        if entry.is_dir {
            header.set_size(0);
            header.set_cksum();
            builder.append_data(&mut header, &archive_path, std::io::empty())?;
        } else {
            let body = std::fs::read(&entry.abs)?;
            header.set_size(body.len() as u64);
            header.set_cksum();
            // append_data rejects nothing here (paths are built from a
            // validated name + relative walk), and writes the path for us.
            builder.append_data(&mut header, &archive_path, body.as_slice())?;
        }
    }
    let tar = builder.into_inner()?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    use std::io::Write as _;
    gz.write_all(&tar)?;
    let tgz = gz.finish()?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, tgz)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn plugin_dir(tmp: &Path) -> PathBuf {
        let dir = tmp.join("demo");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join("extension.json"),
            r#"{"name": "demo", "version": "1.2.3", "command": "run.sh"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(dir.join("bin/data.txt"), "payload").unwrap();
        dir
    }

    /// pack → stage roundtrip: the bundle extracts into an identical
    /// plugin directory, and packing is byte-deterministic.
    #[test]
    fn pack_then_stage_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(tmp.path());
        let out = pack_bundle(&dir, Some(&tmp.path().join("demo-1.2.3.tgz"))).unwrap();
        assert_eq!(out.file_name().unwrap(), "demo-1.2.3.tgz");

        let staging = tmp.path().join("staging");
        stage_bundle_archive(&out, &staging).unwrap();
        assert_eq!(
            std::fs::read_to_string(staging.join("extension.json")).unwrap(),
            std::fs::read_to_string(dir.join("extension.json")).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(staging.join("bin/data.txt")).unwrap(),
            "payload"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(staging.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "exec bit survives the roundtrip");
        }

        // Deterministic: same input → same bytes.
        let out2 = pack_bundle(&dir, Some(&tmp.path().join("second.tgz"))).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(out2).unwrap());
    }

    /// `.git` and the output file itself never enter the bundle.
    #[test]
    fn pack_excludes_git_and_the_output() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(tmp.path());
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref").unwrap();
        let out = pack_bundle(&dir, Some(&dir.join("demo-1.2.3.tgz"))).unwrap();
        let staging = tmp.path().join("staging");
        stage_bundle_archive(&out, &staging).unwrap();
        assert!(!staging.join(".git").exists());
        assert!(!staging.join("demo-1.2.3.tgz").exists());
    }
}
