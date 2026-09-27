//! Self-update from GitHub releases (`tack update`).
//!
//! Release convention (built by `.github/workflows/tack-release.yml`):
//! tag `tack-vX.Y.Z`, assets `tack-<target-triple>.tar.gz` (`.zip` on
//! Windows) containing a single `tack` binary, plus `SHA256SUMS.txt`
//! (sha256sum format) covering every archive.
//!
//! The downloaded archive is verified against `SHA256SUMS.txt` BEFORE
//! extraction/execution. A release without a usable checksum asset fails
//! closed — being unable to verify must not silently install a binary;
//! `TACK_UPDATE_SKIP_CHECKSUM=1` is the explicit opt-out.
//!
//! The repo is resolved from `TACK_UPDATE_REPO` → settings `updateRepo`
//! (global/managed layers only — the project layer is ignored, see
//! settings.rs) → [`DEFAULT_UPDATE_REPO`]. Windows cannot overwrite a
//! running exe, so the current binary is renamed to `tack.old.exe`
//! first and cleaned up on the next launch ([`cleanup_quarantine`], TS
//! windows-self-update equivalent).

use std::path::{Path, PathBuf};

/// Fallback release repo (overridable; this repo must publish tack assets).
pub const DEFAULT_UPDATE_REPO: &str = "Tack-AI/tack";

/// Checksum asset every tack release must carry (see the module docs).
const CHECKSUMS_ASSET: &str = "SHA256SUMS.txt";

/// Env opt-out for self-update checksum verification (fail-closed default).
const UPDATE_SKIP_CHECKSUM_ENV: &str = "TACK_UPDATE_SKIP_CHECKSUM";

/// Resolved update source: env → settings → default.
pub fn update_repo(settings_repo: Option<&str>) -> String {
    std::env::var("TACK_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| settings_repo.map(str::to_string))
        .unwrap_or_else(|| DEFAULT_UPDATE_REPO.to_string())
}

/// Rust target triple of this build (matches the release asset names).
pub fn target_triple() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "aarch64-pc-windows-msvc"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-gnu"
    }
}

/// Release asset file name for a target triple.
pub fn asset_name(triple: &str) -> String {
    if triple.contains("windows") {
        format!("tack-{triple}.zip")
    } else {
        format!("tack-{triple}.tar.gz")
    }
}

/// Is `latest` strictly newer than `current`? Unparseable versions never
/// count as newer (the startup hint fails closed, like the updater).
pub fn is_newer_version(latest: &str, current: &str) -> bool {
    match (version_tuple(latest), version_tuple(current)) {
        (Some(new), Some(cur)) => new > cur,
        _ => false,
    }
}

/// Parse "1.2.3" into a comparable tuple; None when unparseable.
fn version_tuple(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let maj = parts.next()?.parse().ok()?;
    let min = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((maj, min, patch))
}

#[derive(Debug, serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Strip the tag prefix: "tack-v1.2.3" or "v1.2.3" → "1.2.3".
fn tag_version(tag: &str) -> &str {
    tag.trim()
        .strip_prefix("tack-v")
        .or_else(|| tag.trim().strip_prefix('v'))
        .unwrap_or(tag)
}

/// Outcome of an update run.
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Already on the latest (or newer) version.
    UpToDate(String),
    /// --check: a newer version exists.
    Available(String),
    /// Installed a new version.
    Updated(String),
}

/// Fetch the latest tack release for `repo`: the newest release whose tag
/// follows the `tack-vX.Y.Z` convention (`/releases/latest` could be a
/// different product line in the same repo).
async fn latest_release(client: &reqwest::Client, repo: &str) -> Result<Release, String> {
    let resp = crate::tools_manager::send_with_retry(|| {
        client
            .get(format!(
                "https://api.github.com/repos/{repo}/releases?per_page=30"
            ))
            .header("User-Agent", "tack-coding-agent")
    })
    .await?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API error: {}", resp.status()));
    }
    let releases: Vec<Release> = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API parse: {e}"))?;
    releases
        .into_iter()
        .find(|r| r.tag_name.starts_with("tack-v") && !r.assets.is_empty())
        .ok_or_else(|| format!("no tack-v* release with assets found in {repo}"))
}

/// Latest tack release info from a pure query (no download/install).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestRelease {
    /// Semver without the tag prefix ("1.2.3").
    pub version: String,
    /// Release display name (falls back to the tag).
    pub label: String,
}

/// Non-interactive latest-release query (TUI startup update check): a
/// single GitHub API call with a short timeout — no download, no
/// checksum verification, no prompts, no installation.
pub async fn check_latest_release(repo: &str) -> Result<LatestRelease, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let release = latest_release(&client, repo).await?;
    Ok(LatestRelease {
        version: tag_version(&release.tag_name).to_string(),
        label: release.name.unwrap_or(release.tag_name),
    })
}

/// `tack update [--check] [--force]`. Returns a human-readable summary.
pub async fn run_update(
    repo: &str,
    check_only: bool,
    force: bool,
) -> Result<(UpdateOutcome, String), String> {
    let current = env!("CARGO_PKG_VERSION");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let release = latest_release(&client, repo).await?;
    let latest = tag_version(&release.tag_name);
    let (cur, new) = (version_tuple(current), version_tuple(latest));
    if !force
        && let (Some(cur), Some(new)) = (cur, new)
        && cur >= new
    {
        return Ok((
            UpdateOutcome::UpToDate(latest.to_string()),
            format!("tack v{current} is up to date (latest: {latest})"),
        ));
    }

    let triple = target_triple();
    let wanted = asset_name(triple);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| format!("release {} has no asset {wanted}", release.tag_name))?;
    let sums_asset = release.assets.iter().find(|a| a.name == CHECKSUMS_ASSET);
    let mut log = format!(
        "tack v{current} → v{latest} ({})",
        release.name.as_deref().unwrap_or(&release.tag_name)
    );
    if check_only {
        return Ok((
            UpdateOutcome::Available(latest.to_string()),
            format!("{log}\nasset: {wanted}"),
        ));
    }

    // Download + extract.
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let tmp = std::env::temp_dir().join(format!("tack-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("mkdir {}: {e}", tmp.display()))?;
    let result = download_and_install(&client, asset, sums_asset, &tmp, &exe).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
        .map(|_| {
            log.push_str(&format!("\ninstalled to {}", exe.display()));
            (UpdateOutcome::Updated(latest.to_string()), log)
        })
        .map_err(|e| format!("update failed (binary unchanged): {e}"))
}

async fn download_and_install(
    client: &reqwest::Client,
    asset: &Asset,
    sums_asset: Option<&Asset>,
    tmp: &Path,
    exe: &Path,
) -> Result<(), String> {
    let archive = tmp.join(&asset.name);
    let mut resp =
        crate::tools_manager::send_with_retry(|| client.get(&asset.browser_download_url)).await?;
    if !resp.status().is_success() {
        return Err(format!("download failed: {}", resp.status()));
    }
    let mut bytes = crate::catalog_refresh::read_body_capped(
        &mut resp,
        crate::tools_manager::MAX_DOWNLOAD_BYTES,
        "release archive",
    )
    .await
    .map_err(|e| format!("download body: {e}"))?;

    // Integrity: verify the archive against SHA256SUMS.txt BEFORE it is
    // extracted or executed (the checksum file itself is attested by the
    // release workflow). Missing/unusable checksums fail closed.
    if crate::tools_manager::checksum_bypassed(UPDATE_SKIP_CHECKSUM_ENV) {
        eprintln!(
            "warning: {UPDATE_SKIP_CHECKSUM_ENV} is set: installing {} WITHOUT checksum verification",
            asset.name
        );
    } else {
        bytes = verify_release_checksum(client, sums_asset, &asset.name, bytes).await?;
    }

    // Archive write + extract + the `--version` sanity run + the binary
    // swap are all blocking IO/subprocess work: keep them off the async
    // executor.
    let asset_name = asset.name.clone();
    let tmp = tmp.to_path_buf();
    let exe = exe.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::write(&archive, &bytes).map_err(|e| format!("write archive: {e}"))?;
        let binary = crate::tools_manager::extract_binary(&archive, &asset_name, "tack", &tmp)?;
        // Sanity: the new binary must run.
        let ok = std::process::Command::new(&binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Err(format!(
                "downloaded binary failed --version: {}",
                binary.display()
            ));
        }
        install_binary(&binary, &exe)
    })
    .await
    .map_err(|e| format!("install worker panicked: {e}"))?
}

/// Verify `bytes` against the release's SHA256SUMS.txt entry for
/// `asset_name`. Every failure mode (no checksum asset, fetch error, no
/// entry for the asset, digest mismatch) is an error that names the
/// explicit bypass — an unverifiable binary is never installed silently.
/// Takes and returns the archive so the (blocking) digest can run on the
/// blocking pool without copying the bytes.
async fn verify_release_checksum(
    client: &reqwest::Client,
    sums_asset: Option<&Asset>,
    asset_name: &str,
    bytes: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let Some(sums_asset) = sums_asset else {
        return Err(format!(
            "release has no {CHECKSUMS_ASSET} checksum asset — refusing to install an \
             unverified binary; set {UPDATE_SKIP_CHECKSUM_ENV}=1 to bypass explicitly"
        ));
    };
    let resp =
        crate::tools_manager::send_with_retry(|| client.get(&sums_asset.browser_download_url))
            .await?;
    if !resp.status().is_success() {
        return Err(format!(
            "checksum download failed: {} — refusing to install an unverified binary; set \
             {UPDATE_SKIP_CHECKSUM_ENV}=1 to bypass explicitly",
            resp.status()
        ));
    }
    let sums = resp
        .text()
        .await
        .map_err(|e| format!("checksum body: {e}"))?;
    let Some(expected) = crate::tools_manager::checksum_for_asset(&sums, asset_name) else {
        return Err(format!(
            "{CHECKSUMS_ASSET} has no entry for {asset_name} — refusing to install an \
             unverified binary; set {UPDATE_SKIP_CHECKSUM_ENV}=1 to bypass explicitly"
        ));
    };
    let asset_name = asset_name.to_string();
    tokio::task::spawn_blocking(move || {
        crate::tools_manager::verify_sha256(&bytes, &asset_name, &expected).map(|_| bytes)
    })
    .await
    .map_err(|e| format!("checksum worker panicked: {e}"))?
}

/// Replace the running executable (TS windows-self-update strategy):
/// Unix overwrites via rename; Windows renames the locked exe aside first.
fn install_binary(new_binary: &Path, exe: &Path) -> Result<(), String> {
    let dir = exe.parent().ok_or("exe has no parent")?;
    let staged = dir.join(if cfg!(windows) {
        "tack.new.exe"
    } else {
        "tack.new"
    });
    std::fs::copy(new_binary, &staged).map_err(|e| format!("stage into {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }

    #[cfg(windows)]
    {
        // A running exe can be renamed but not overwritten.
        let quarantine = quarantine_path(exe);
        let _ = std::fs::remove_file(&quarantine);
        std::fs::rename(exe, &quarantine).map_err(|e| format!("rename current exe aside: {e}"))?;
    }
    std::fs::rename(&staged, exe).map_err(|e| format!("move new binary into place: {e}"))?;
    Ok(())
}

/// The renamed-aside previous binary (Windows self-update remnant).
fn quarantine_path(exe: &Path) -> PathBuf {
    exe.with_extension("old.exe")
}

/// Delete a leftover `tack.old.exe` from a previous self-update. Call early
/// at startup (TS cleanupWindowsSelfUpdateQuarantine).
pub fn cleanup_quarantine() {
    if !cfg!(windows) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(quarantine_path(&exe));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn asset_naming() {
        assert_eq!(
            asset_name("x86_64-pc-windows-msvc"),
            "tack-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            asset_name("aarch64-unknown-linux-gnu"),
            "tack-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("aarch64-apple-darwin"),
            "tack-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn tag_and_version_parsing() {
        assert_eq!(tag_version("tack-v0.2.0"), "0.2.0");
        assert_eq!(tag_version("v1.0.0"), "1.0.0");
        assert_eq!(version_tuple("0.1.0"), Some((0, 1, 0)));
        assert_eq!(version_tuple("v1.2"), Some((1, 2, 0)));
        assert!(version_tuple("0.2.0").unwrap() > version_tuple("0.1.9").unwrap());
        assert_eq!(version_tuple("garbage"), None);
    }

    #[test]
    fn newer_version_comparison() {
        assert!(is_newer_version("0.3.0", "0.2.9"));
        assert!(is_newer_version("v1.0.0", "0.9.9"));
        assert!(is_newer_version("1.2", "1.1.9"));
        assert!(!is_newer_version("0.2.0", "0.2.0"));
        assert!(!is_newer_version("0.1.9", "0.2.0"));
        // Unparseable input fails closed (no hint).
        assert!(!is_newer_version("garbage", "0.2.0"));
        assert!(!is_newer_version("0.3.0", "garbage"));
    }

    #[test]
    fn repo_override_precedence() {
        assert_eq!(update_repo(Some("owner/repo")), "owner/repo");
        assert_eq!(update_repo(None), DEFAULT_UPDATE_REPO);
    }
}
