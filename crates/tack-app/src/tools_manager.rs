//! Managed external tools (fd, ripgrep). Port of
//! `packages/coding-agent/src/utils/tools-manager.ts`.
//!
//! tack's grep/find tools are native (ignore/grep crates), so fd/rg are not
//! required by the app itself — they are ensured for the agent's *bash*
//! usage (the system prompt tells the model to prefer rg/fd). The managed
//! bin dir is prepended to PATH for shell spawns (tack-tools executor).
//!
//! Resolution order (TS parity):
//! 1. `~/.tack/agent/bin/<tool>` (previously downloaded)
//! 2. system PATH (`fd` also tries `fdfind`)
//! 3. download the latest GitHub release asset for this platform
//!
//! Offline mode (TACK_OFFLINE or --offline) skips downloads; Android/Termux
//! advises `pkg install` instead (Bionic libc can't run the gnu builds).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Managed binaries directory: `<agent dir>/bin` (TS getBinDir).
pub fn bin_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("bin")
}

/// Status messages surfaced to the UI while ensuring tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Info(String),
    Warning(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedTool {
    Fd,
    Rg,
}

struct ToolConfig {
    name: &'static str,
    repo: &'static str,
    binary_name: &'static str,
    /// Alternative system command names tried before downloading.
    system_names: &'static [&'static str],
    tag_prefix: &'static str,
    termux_package: &'static str,
}

fn config(tool: ManagedTool) -> ToolConfig {
    match tool {
        ManagedTool::Fd => ToolConfig {
            name: "fd",
            repo: "sharkdp/fd",
            binary_name: "fd",
            system_names: &["fd", "fdfind"],
            tag_prefix: "v",
            termux_package: "fd",
        },
        ManagedTool::Rg => ToolConfig {
            name: "ripgrep",
            repo: "BurntSushi/ripgrep",
            binary_name: "rg",
            system_names: &["rg"],
            tag_prefix: "",
            termux_package: "ripgrep",
        },
    }
}

/// Release asset file name for this platform (TS getAssetName).
fn asset_name(tool: ManagedTool, version: &str, os: &str, arch: &str) -> Option<String> {
    let arch_str = if arch == "aarch64" {
        "aarch64"
    } else {
        "x86_64"
    };
    match (tool, os) {
        (ManagedTool::Fd, "macos") => Some(format!("fd-v{version}-{arch_str}-apple-darwin.tar.gz")),
        (ManagedTool::Fd, "linux") => {
            Some(format!("fd-v{version}-{arch_str}-unknown-linux-gnu.tar.gz"))
        }
        (ManagedTool::Fd, "windows") => {
            Some(format!("fd-v{version}-{arch_str}-pc-windows-msvc.zip"))
        }
        (ManagedTool::Rg, "macos") => {
            Some(format!("ripgrep-{version}-{arch_str}-apple-darwin.tar.gz"))
        }
        (ManagedTool::Rg, "linux") => Some(if arch == "aarch64" {
            format!("ripgrep-{version}-aarch64-unknown-linux-gnu.tar.gz")
        } else {
            format!("ripgrep-{version}-x86_64-unknown-linux-musl.tar.gz")
        }),
        (ManagedTool::Rg, "windows") => {
            Some(format!("ripgrep-{version}-{arch_str}-pc-windows-msvc.zip"))
        }
        _ => None,
    }
}

/// True when a command runs (`--version` exits without spawn error),
/// with an optional PATH override applied to the child process only
/// (never mutates this process's environment, so tests can simulate a
/// missing tool without racing other threads).
fn command_exists_in(cmd: &str, search_path: Option<&OsStr>) -> bool {
    let mut command = std::process::Command::new(cmd);
    command.arg("--version");
    if let Some(path) = search_path {
        command.env("PATH", path);
    }
    // A PATH shim that hangs on --version must not block the (async)
    // caller forever: bounded probe, killed on timeout.
    crate::sync_process::output_with_timeout(&mut command, std::time::Duration::from_secs(5))
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Path to the tool: managed bin dir first, then system PATH.
/// Returns a full path for managed binaries, the bare command name otherwise.
pub fn get_tool_path(tool: ManagedTool, agent_dir: &Path) -> Option<String> {
    get_tool_path_in(tool, agent_dir, None)
}

fn get_tool_path_in(
    tool: ManagedTool,
    agent_dir: &Path,
    search_path: Option<&OsStr>,
) -> Option<String> {
    let cfg = config(tool);
    let exe = if cfg!(windows) {
        format!("{}.exe", cfg.binary_name)
    } else {
        cfg.binary_name.to_string()
    };
    let local = bin_dir(agent_dir).join(&exe);
    if local.exists() {
        return Some(local.to_string_lossy().to_string());
    }
    for name in cfg.system_names {
        if command_exists_in(name, search_path) {
            return Some((*name).to_string());
        }
    }
    None
}

fn offline_mode(cli_offline: bool) -> bool {
    if cli_offline {
        return true;
    }
    std::env::var("TACK_OFFLINE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

/// Running on Android/Termux directly (not in a proot Linux userland, where
/// glibc builds work fine). TS checks platform() === "android".
fn is_termux_host() -> bool {
    cfg!(target_os = "android")
        || std::env::var("TERMUX_VERSION").is_ok()
        || std::env::var("PREFIX").is_ok_and(|p| p.contains("com.termux"))
}

async fn latest_version(client: &reqwest::Client, repo: &str) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let resp = send_with_retry(|| {
        client
            .get(format!(
                "https://api.github.com/repos/{repo}/releases/latest"
            ))
            .header("User-Agent", "tack-coding-agent")
    })
    .await?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API error: {}", resp.status()));
    }
    let release: Release = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API parse: {e}"))?;
    Ok(release.tag_name.trim_start_matches('v').to_string())
}

/// TS fetchWithRetry: 3 attempts, 500ms/1.5s backoff, only on transport
/// errors (HTTP error statuses are returned to the caller immediately).
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, String> {
    let mut delay = std::time::Duration::from_millis(500);
    let mut last_err = String::new();
    for attempt in 0..3 {
        match build().send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last_err = e.to_string();
                if attempt < 2 {
                    tokio::time::sleep(delay).await;
                    delay *= 3;
                }
            }
        }
    }
    Err(format!("download: {last_err}"))
}

// ---------------------------------------------------------------------
// Download integrity (shared by self-update): archives are verified
// against a published SHA-256 checksum BEFORE extraction/execution.
// ---------------------------------------------------------------------

/// Env opt-out for managed-tool checksum verification (fail-closed
/// default; see download_tool).
pub(crate) const TOOLS_SKIP_CHECKSUM_ENV: &str = "TACK_TOOLS_SKIP_CHECKSUM";

/// SHA-256 hex digest (lowercase) of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Look up `asset`'s digest in a `sha256sum`-format checksum file
/// ("<hex>  <name>" per line; a `*` binary-mode marker and directory
/// prefixes on the name are tolerated).
pub(crate) fn checksum_for_asset(sums: &str, asset: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(hex), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*');
        let name = name.rsplit('/').next().unwrap_or(name);
        if name == asset && hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
}

/// Verify `bytes` against the expected hex digest for `asset`.
pub(crate) fn verify_sha256(bytes: &[u8], asset: &str, expected: &str) -> Result<(), String> {
    let actual = sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch for {asset}: expected {expected}, got {actual} — the archive \
             may be corrupted or tampered with; refusing to install"
        ))
    }
}

/// Explicit user opt-out of checksum verification ("1"/"true").
pub(crate) fn checksum_bypassed(env_var: &str) -> bool {
    std::env::var(env_var)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Fetch `<url>.sha256` (the convention ripgrep's releases follow) and
/// verify `bytes` against it. A missing or unusable checksum asset is an
/// ERROR, not a skip: being unable to verify must not be silent. Takes and
/// returns the archive so the (blocking) digest can run on the blocking
/// pool without copying the bytes.
async fn verify_download_checksum(
    client: &reqwest::Client,
    url: &str,
    asset: &str,
    bytes: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let resp = send_with_retry(|| client.get(format!("{url}.sha256"))).await?;
    if !resp.status().is_success() {
        return Err(format!(
            "no checksum published for {asset} ({} fetching {url}.sha256) — refusing to \
             install an unverified binary; set {TOOLS_SKIP_CHECKSUM_ENV}=1 to bypass explicitly",
            resp.status()
        ));
    }
    let sums = resp
        .text()
        .await
        .map_err(|e| format!("checksum body: {e}"))?;
    let Some(expected) = checksum_for_asset(&sums, asset) else {
        return Err(format!(
            "checksum file for {asset} has no usable digest — refusing to install an \
             unverified binary; set {TOOLS_SKIP_CHECKSUM_ENV}=1 to bypass explicitly"
        ));
    };
    let asset = asset.to_string();
    tokio::task::spawn_blocking(move || verify_sha256(&bytes, &asset, &expected).map(|_| bytes))
        .await
        .map_err(|e| format!("checksum worker panicked: {e}"))?
}

/// Extract the tool binary from an archive into `dest` (as `binary_name`).
pub(crate) fn extract_binary(
    archive: &Path,
    asset: &str,
    binary_name: &str,
    dest_dir: &Path,
) -> Result<PathBuf, String> {
    let exe = if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    };
    let extract_dir = dest_dir.join(format!(
        "extract_tmp_{binary_name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&extract_dir).map_err(|e| format!("mkdir: {e}"))?;

    let result = (|| -> Result<PathBuf, String> {
        let file = std::fs::File::open(archive).map_err(|e| format!("open archive: {e}"))?;
        if asset.ends_with(".tar.gz") {
            let gz = flate2::read::GzDecoder::new(file);
            let mut tar = tar::Archive::new(gz);
            tar.unpack(&extract_dir)
                .map_err(|e| format!("extract {asset}: {e}"))?;
        } else if asset.ends_with(".zip") {
            let mut zip =
                zip::ZipArchive::new(file).map_err(|e| format!("open zip {asset}: {e}"))?;
            zip.extract(&extract_dir)
                .map_err(|e| format!("extract {asset}: {e}"))?;
        } else {
            return Err(format!("unsupported archive format: {asset}"));
        }
        // Find the binary: versioned subdirectory, archive root, or deeper.
        find_file_recursive(&extract_dir, &exe)
            .ok_or_else(|| format!("binary not found in archive: expected {exe}"))
    })();

    let out = result.and_then(|found| {
        let dest = dest_dir.join(&exe);
        std::fs::rename(&found, &dest)
            .or_else(|_| {
                std::fs::copy(&found, &dest).map(|_| ())?;
                std::fs::remove_file(&found)
            })
            .map_err(|e| format!("install {}: {e}", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
        }
        Ok(dest)
    });
    let _ = std::fs::remove_dir_all(&extract_dir);
    out
}

pub(crate) fn find_file_recursive(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if entry.file_type().ok()?.is_file() && entry.file_name() == name {
                return Some(path);
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(path);
            }
        }
    }
    None
}

/// Hard cap on a downloaded tool/release archive (largest real artifacts
/// are tens of MB); protects against a hostile or broken server streaming
/// an unbounded body.
pub(crate) const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Blocking part of the install (download + extract); the network half is
/// async, the sha256/archive-write/extract half runs via spawn_blocking.
async fn download_tool(tool: ManagedTool, agent_dir: &Path) -> Result<PathBuf, String> {
    let cfg = config(tool);
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return Err(format!("unsupported platform: {}", std::env::consts::OS));
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut version = latest_version(&client, cfg.repo).await?;
    // TS pin: fd 10.3.0 is the last release with a working darwin/x64 asset.
    if matches!(tool, ManagedTool::Fd) && os == "macos" && arch == "x86_64" {
        version = "10.3.0".to_string();
    }
    let asset = asset_name(tool, &version, os, arch)
        .ok_or_else(|| format!("unsupported platform: {os}/{arch}"))?;

    let dir = bin_dir(agent_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let url = format!(
        "https://github.com/{}/releases/download/{}{version}/{asset}",
        cfg.repo, cfg.tag_prefix
    );
    let archive = dir.join(&asset);

    let bytes = async {
        let mut resp = send_with_retry(|| client.get(&url)).await?;
        if !resp.status().is_success() {
            return Err(format!("download failed: {}", resp.status()));
        }
        crate::catalog_refresh::read_body_capped(&mut resp, MAX_DOWNLOAD_BYTES, "tool archive")
            .await
            .map_err(|e| format!("download body: {e}"))
    }
    .await;
    let mut bytes = match bytes {
        Ok(b) => b,
        Err(e) => return Err(e),
    };
    // Integrity: verify the archive against the checksum published next
    // to it (`<asset>.sha256`, sha256sum format — ripgrep's releases ship
    // these; fd's do not) BEFORE extraction. When no checksum exists the
    // download is unverifiable and fails closed; TACK_TOOLS_SKIP_CHECKSUM=1
    // is the explicit opt-out.
    if checksum_bypassed(TOOLS_SKIP_CHECKSUM_ENV) {
        tracing::warn!(
            "{TOOLS_SKIP_CHECKSUM_ENV} is set: installing {asset} WITHOUT checksum verification"
        );
    } else {
        bytes = verify_download_checksum(&client, &url, &asset, bytes).await?;
    }
    // sha256 is done; writing the archive and decompressing/extracting it
    // are blocking IO+CPU — keep them off the async executor.
    let extract_dir = dir.clone();
    let binary_name = cfg.binary_name;
    tokio::task::spawn_blocking(move || {
        std::fs::write(&archive, &bytes).map_err(|e| format!("write archive: {e}"))?;
        let out = extract_binary(&archive, &asset, binary_name, &extract_dir);
        let _ = std::fs::remove_file(&archive);
        out
    })
    .await
    .map_err(|e| format!("extract worker panicked: {e}"))?
}

/// Ensure a tool is available, downloading if necessary (TS ensureTool).
/// Reports progress through the returned statuses; the caller surfaces them.
pub async fn ensure_tool(
    tool: ManagedTool,
    agent_dir: &Path,
    cli_offline: bool,
) -> (Option<String>, Vec<ToolStatus>) {
    ensure_tool_in(tool, agent_dir, cli_offline, None).await
}

async fn ensure_tool_in(
    tool: ManagedTool,
    agent_dir: &Path,
    cli_offline: bool,
    search_path: Option<&OsStr>,
) -> (Option<String>, Vec<ToolStatus>) {
    if let Some(path) = get_tool_path_in(tool, agent_dir, search_path) {
        return (Some(path), Vec::new());
    }
    let cfg = config(tool);
    if offline_mode(cli_offline) {
        return (
            None,
            vec![ToolStatus::Warning(format!(
                "{} not found. Offline mode enabled, skipping download.",
                cfg.name
            ))],
        );
    }
    if is_termux_host() {
        return (
            None,
            vec![ToolStatus::Warning(format!(
                "{} not found. Install with: pkg install {}",
                cfg.name, cfg.termux_package
            ))],
        );
    }
    let mut statuses = vec![ToolStatus::Info(format!(
        "{} not found. Downloading...",
        cfg.name
    ))];
    match download_tool(tool, agent_dir).await {
        Ok(path) => {
            statuses.push(ToolStatus::Info(format!(
                "{} installed to {}",
                cfg.name,
                path.display()
            )));
            (Some(path.to_string_lossy().to_string()), statuses)
        }
        Err(e) => {
            statuses.push(ToolStatus::Warning(format!(
                "failed to download {}: {e}",
                cfg.name
            )));
            (None, statuses)
        }
    }
}

/// Startup check for both tools (TS interactive-mode ensureTool fd+rg).
pub async fn ensure_startup_tools(agent_dir: &Path, cli_offline: bool) -> Vec<ToolStatus> {
    let (fd, rg) = tokio::join!(
        ensure_tool(ManagedTool::Fd, agent_dir, cli_offline),
        ensure_tool(ManagedTool::Rg, agent_dir, cli_offline)
    );
    let mut statuses = fd.1;
    statuses.extend(rg.1);
    statuses
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(unsafe_code)] // std::env::set_var in tests
    use super::*;

    #[test]
    fn asset_names_match_ts() {
        assert_eq!(
            asset_name(ManagedTool::Fd, "10.2.0", "linux", "x86_64").unwrap(),
            "fd-v10.2.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name(ManagedTool::Fd, "10.2.0", "macos", "aarch64").unwrap(),
            "fd-v10.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name(ManagedTool::Fd, "10.2.0", "windows", "x86_64").unwrap(),
            "fd-v10.2.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            asset_name(ManagedTool::Rg, "14.1.1", "linux", "x86_64").unwrap(),
            "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            asset_name(ManagedTool::Rg, "14.1.1", "linux", "aarch64").unwrap(),
            "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name(ManagedTool::Rg, "14.1.1", "windows", "aarch64").unwrap(),
            "ripgrep-14.1.1-aarch64-pc-windows-msvc.zip"
        );
    }

    #[tokio::test]
    async fn offline_mode_skips_download() {
        let dir = tempfile::tempdir().unwrap();
        // rg/fd may exist on the dev machine, so force the missing case by
        // looking the tool up with the child PATH pointed at an empty dir.
        // (Mutating the process-global PATH here raced other tests in this
        // binary that spawn git, causing intermittent failures.)
        let (path, statuses) = ensure_tool_in(
            ManagedTool::Fd,
            dir.path(),
            true,
            Some(dir.path().as_os_str()),
        )
        .await;
        assert!(path.is_none());
        assert_eq!(
            statuses,
            vec![ToolStatus::Warning(
                "fd not found. Offline mode enabled, skipping download.".to_string()
            )]
        );
    }

    #[test]
    fn bin_dir_is_under_agent_dir() {
        assert_eq!(bin_dir(Path::new("/x/agent")), Path::new("/x/agent/bin"));
    }

    // ---- download checksum verification (shared by self-update) ----

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn checksum_for_asset_parses_sha256sum_format() {
        let sums = "\
ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  tool-x86_64.tar.gz\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 *tool-aarch64.tar.gz\n# comment line\n\nnot-a-digest  tool-other.tar.gz\n";
        assert_eq!(
            checksum_for_asset(sums, "tool-x86_64.tar.gz").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        // Binary-mode `*` marker is tolerated.
        assert!(checksum_for_asset(sums, "tool-aarch64.tar.gz").is_some());
        // Malformed lines and unknown assets yield None.
        assert!(checksum_for_asset(sums, "tool-other.tar.gz").is_none());
        assert!(checksum_for_asset(sums, "missing.tar.gz").is_none());
    }

    #[test]
    fn verify_sha256_accepts_matching_and_rejects_tampered_archive() {
        // Local fixture: an archive whose digest is listed in a
        // SHA256SUMS-style file, verified through the full chain.
        let archive = gzipped_tar(|b| append_file(b, "bin/toolbin", b"#!/bin/sh\n"));
        let sums = format!("{}  tool.tar.gz\n", sha256_hex(&archive));
        let expected = checksum_for_asset(&sums, "tool.tar.gz").unwrap();
        verify_sha256(&archive, "tool.tar.gz", &expected).unwrap();

        let mut tampered = archive.clone();
        tampered[10] ^= 0xff;
        let err = verify_sha256(&tampered, "tool.tar.gz", &expected).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
    }

    // ---- archive extraction safety (shared by self-update) ----

    use std::io::Read as _;
    use std::io::Write as _;

    fn gzipped_tar(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        build(&mut builder);
        let tar_bytes = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn append_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    }

    #[test]
    fn extract_finds_binary_in_versioned_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("tool.tar.gz");
        // Real Windows tool archives ship the binary with its .exe suffix;
        // the extractor looks it up by the platform exe name.
        let member = if cfg!(windows) {
            "pkg-v1/bin/toolbin.exe"
        } else {
            "pkg-v1/bin/toolbin"
        };
        let bytes = gzipped_tar(|b| append_file(b, member, b"#!/bin/sh\n"));
        std::fs::write(&archive, bytes).unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let out = extract_binary(&archive, "tool.tar.gz", "toolbin", &dest).unwrap();
        assert_eq!(
            out,
            dest.join(if cfg!(windows) {
                "toolbin.exe"
            } else {
                "toolbin"
            })
        );
        assert_eq!(std::fs::read(&out).unwrap(), b"#!/bin/sh\n");
    }

    #[test]
    fn tar_dotdot_entries_cannot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("evil.tar.gz");
        // The tar builder refuses `..` names, so patch the raw header name
        // field afterwards (and fix the checksum) to simulate a hostile
        // archive produced by another tool.
        let mut bytes = gzipped_tar(|b| append_file(b, "zzescaped.txt", b"pwned"));
        patch_first_tar_entry_name(&mut bytes, b"../escaped.txt");
        std::fs::write(&archive, bytes).unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let _ = extract_binary(&archive, "evil.tar.gz", "toolbin", &dest);
        assert!(
            !tmp.path().join("escaped.txt").exists(),
            "tar entry escaped the extraction dir"
        );
    }

    /// Overwrite the first entry's name in a gzipped tar and re-checksum.
    fn patch_first_tar_entry_name(gz: &mut Vec<u8>, name: &[u8]) {
        let mut tar_bytes = Vec::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_end(&mut tar_bytes)
            .unwrap();
        assert!(name.len() <= 100);
        tar_bytes[..100].fill(0);
        tar_bytes[..name.len()].copy_from_slice(name);
        // Checksum: octal sum of the header with the checksum field as spaces.
        tar_bytes[148..156].fill(b' ');
        let sum: u64 = tar_bytes[..512].iter().map(|b| u64::from(*b)).sum();
        let encoded = format!("{sum:06o}\0 ");
        tar_bytes[148..156].copy_from_slice(encoded.as_bytes());
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        *gz = encoder.finish().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tar_symlink_attack_cannot_escape() {
        let outside = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("evil.tar.gz");
        let bytes = gzipped_tar(|b| {
            // symlink "link" -> outside dir, then a file written through it.
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            b.append_link(&mut header, "link", outside.path()).unwrap();
            append_file(b, "link/pwned.txt", b"pwned");
        });
        std::fs::write(&archive, bytes).unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let _ = extract_binary(&archive, "evil.tar.gz", "toolbin", &dest);
        assert!(
            !outside.path().join("pwned.txt").exists(),
            "tar symlink attack escaped the extraction dir"
        );
    }

    #[test]
    fn zip_dotdot_entries_cannot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("evil.zip");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            writer.start_file("../escaped.txt", opts).unwrap();
            writer.write_all(b"pwned").unwrap();
            writer.finish().unwrap();
        }
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let _ = extract_binary(&archive, "evil.zip", "toolbin", &dest);
        assert!(
            !tmp.path().join("escaped.txt").exists(),
            "zip entry escaped the extraction dir"
        );
    }
}
