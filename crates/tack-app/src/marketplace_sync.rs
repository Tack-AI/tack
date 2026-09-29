//! Curated marketplace startup sync (roadmap §9): settings-declared
//! catalogs kept fresh. Transport degrades git → https archive; a cheap
//! fingerprint short-circuits unchanged catalogs; activation is a
//! backup/rename/swap behind a cross-process lock; a failing sync NEVER
//! blocks startup (the background task warns and keeps the old catalog).
//!
//! Declarations come from the global and MANAGED settings layers only —
//! a catalog can push code via `installed-by-default`, so a project
//! layer must not redirect it (same rule as `updateRepo`):
//!
//! ```jsonc
//! {
//!   "pluginMarketplaces": {
//!     "acme": {
//!       "source": "https://git.acme.com/tack/plugins.git", // or an https
//!                                                          // .json catalog
//!                                                          // URL, or a path
//!       "ref": "main",                 // git ref (default: remote HEAD)
//!       "path": "marketplace.json",    // catalog file inside the repo
//!       "publicKey": "<ed25519 hex>"   // signed catalogs (TOFU-pinned)
//!     }
//!   }
//! }
//! ```

use std::path::{Path, PathBuf};

use serde_json::Value;

/// One settings-declared marketplace to keep synced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketplaceSyncDecl {
    pub name: String,
    /// Git URL, https URL to a JSON catalog, or a local file/dir path.
    pub source: String,
    /// Git ref (branch/tag/sha); the remote HEAD when unset.
    pub git_ref: Option<String>,
    /// Catalog file inside a git repo (default `marketplace.json`).
    pub catalog_path: Option<String>,
    /// ed25519 public key (hex) for signed catalogs.
    pub public_key: Option<String>,
}

/// Outcome of one marketplace sync pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Fingerprint matched; the registered catalog is already current.
    Unchanged,
    /// The registered catalog was activated from the source.
    Synced,
    /// Another tack process holds the sync lock.
    Busy,
    /// The sync failed; the previously registered catalog is untouched.
    Failed(String),
}

impl SyncOutcome {
    pub fn label(&self) -> String {
        match self {
            SyncOutcome::Unchanged => "unchanged".to_string(),
            SyncOutcome::Synced => "synced".to_string(),
            SyncOutcome::Busy => "busy (another process is syncing)".to_string(),
            SyncOutcome::Failed(e) => format!("failed: {e}"),
        }
    }
}

/// Read `pluginMarketplaces` from the global + managed layers (managed
/// wins per name). Malformed entries warn and skip, matching the
/// settings loader's rule-by-rule tolerance.
pub fn sync_declarations(agent_dir: &Path) -> Vec<MarketplaceSyncDecl> {
    let layers = [
        agent_dir.join("settings.json"),
        crate::settings::managed_settings_path(),
    ];
    let mut by_name: std::collections::BTreeMap<String, MarketplaceSyncDecl> =
        std::collections::BTreeMap::new();
    for layer in layers.iter().filter(|p| p.is_file()) {
        let Ok(content) = std::fs::read_to_string(layer) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<Value>(&content) else {
            continue; // the settings loader already reports malformed JSON
        };
        let Some(decls) = raw.get("pluginMarketplaces").and_then(Value::as_object) else {
            continue;
        };
        for (name, value) in decls {
            match parse_declaration(name, value) {
                Ok(decl) => {
                    by_name.insert(name.clone(), decl);
                }
                Err(e) => {
                    tracing::warn!(
                        "ignoring pluginMarketplaces.{name:?} in {}: {e}",
                        layer.display()
                    );
                }
            }
        }
    }
    by_name.into_values().collect()
}

fn parse_declaration(name: &str, value: &Value) -> anyhow::Result<MarketplaceSyncDecl> {
    tack_ext::plugin_id::validate_marketplace_name(name)?;
    // Shorthand: `"acme": "<source>"`.
    if let Some(source) = value.as_str() {
        if source.is_empty() {
            anyhow::bail!("empty source");
        }
        return Ok(MarketplaceSyncDecl {
            name: name.to_string(),
            source: source.to_string(),
            git_ref: None,
            catalog_path: None,
            public_key: None,
        });
    }
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected a source string or an object"))?;
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing `source`"))?
        .to_string();
    let string_field = |key: &str| -> anyhow::Result<Option<String>> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(_) => anyhow::bail!("`{key}` must be a string"),
        }
    };
    Ok(MarketplaceSyncDecl {
        name: name.to_string(),
        source,
        git_ref: string_field("ref")?,
        catalog_path: string_field("path")?,
        public_key: string_field("publicKey")?,
    })
}

/// What kind of transport the source implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    /// Git repo (clone; https repos degrade to an archive download).
    Git,
    /// https URL to a bare JSON catalog.
    JsonUrl,
    /// Local catalog file, or a directory containing one.
    LocalPath,
}

fn classify(source: &str) -> SourceKind {
    if source.starts_with("http://") || source.starts_with("https://") {
        // Classify on the URL path alone: a signed-catalog URL carries
        // a query (`?token=…`) or fragment that must not break the
        // `.json` check, and the suffix match is case-insensitive.
        let path = source.split(['?', '#']).next().unwrap_or(source);
        if path.to_ascii_lowercase().ends_with(".json") {
            SourceKind::JsonUrl
        } else {
            SourceKind::Git
        }
    } else if source.starts_with("git@") || source.starts_with("ssh://") || source.ends_with(".git")
    {
        SourceKind::Git
    } else {
        SourceKind::LocalPath
    }
}

// ---------------------------------------------------------------------------
// Sync state + cross-process lock
// ---------------------------------------------------------------------------

/// Hidden sync scratch dir inside the marketplaces root (the registered
/// catalog listing only reads top-level `*.json`).
fn sync_scratch_dir(agent_dir: &Path) -> PathBuf {
    crate::extension_host::marketplaces_root_path(agent_dir).join(".sync")
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct SyncState {
    fingerprint: String,
    synced_at: u64,
}

fn state_path(agent_dir: &Path, name: &str) -> PathBuf {
    sync_scratch_dir(agent_dir).join(format!("{name}.state.json"))
}

fn read_state(agent_dir: &Path, name: &str) -> Option<SyncState> {
    let content = std::fs::read_to_string(state_path(agent_dir, name)).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_state(agent_dir: &Path, name: &str, fingerprint: &str) {
    let state = SyncState {
        fingerprint: fingerprint.to_string(),
        synced_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = crate::atomic_write::atomic_write(&state_path(agent_dir, name), &json);
    }
}

/// A create_new lock file; stale locks (>10 min, a crashed process) are
/// reclaimed. The catalog swap itself is rename-atomic, so the lock only
/// serializes the expensive fetch.
struct SyncLock(PathBuf);

impl SyncLock {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

    fn acquire(agent_dir: &Path, name: &str) -> Option<SyncLock> {
        let dir = sync_scratch_dir(agent_dir);
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{name}.lock"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                let _ = writeln!(file, "pid {}", std::process::id());
                Some(SyncLock(path))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > Self::STALE_AFTER)
                    .unwrap_or(false);
                if !stale {
                    return None;
                }
                // Rename-claim the stale lock before recreating it: the
                // rename of one source path succeeds for exactly one
                // racer, so two processes reclaiming the same stale lock
                // can't both end up believing they hold it (plain
                // remove+create_new can).
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let claimed = dir.join(format!("{name}.lock.stale-{}-{nanos}", std::process::id()));
                if std::fs::rename(&path, &claimed).is_err() {
                    // Already reclaimed (or removed) by someone else.
                    return None;
                }
                match std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                {
                    Ok(mut file) => {
                        use std::io::Write as _;
                        let _ = writeln!(file, "pid {}", std::process::id());
                        // Best-effort cleanup of the claimed stale file.
                        let _ = std::fs::remove_file(&claimed);
                        Some(SyncLock(path))
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::AlreadyExists {
                            // Someone else won the reclaim race; back off.
                            // The claimed stale file is garbage either way.
                            let _ = std::fs::remove_file(&claimed);
                        } else if std::fs::rename(&claimed, &path).is_err() {
                            // Put the lock back for the next reclaimer.
                            let _ = std::fs::remove_file(&claimed);
                        }
                        None
                    }
                }
            }
            Err(_) => None,
        }
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The scrubbed-git environment shared by every git invocation (no
/// terminal prompt, no inherited GIT_* config — the install channel's
/// rules apply to sync too).
fn scrub_git(command: &mut std::process::Command) -> &mut std::process::Command {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_COUNT")
}

/// Is `reference` a full 40-char hex commit sha? A sha pin IS its own
/// fingerprint — no network round-trip needed.
fn is_commit_sha(reference: &str) -> bool {
    reference.len() == 40 && reference.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `git ls-remote` patterns for a ref, in preference order: the exact
/// branch, then the tag (the peeled `^{}` line for annotated tags plus
/// the plain tag line for lightweight ones). A bare pattern tailglobs
/// and can return several lines, so every pattern is exact.
fn ls_remote_patterns(git_ref: Option<&str>) -> Vec<String> {
    match git_ref {
        Some(reference) => vec![
            format!("refs/heads/{reference}"),
            format!("refs/tags/{reference}^{{}}"),
            format!("refs/tags/{reference}"),
        ],
        None => vec!["HEAD".to_string()],
    }
}

/// Pick the fingerprint sha out of `git ls-remote` output: prefer the
/// branch line, then the peeled annotated-tag line (`^{}`), then the
/// first sha present (a lightweight tag, or `HEAD`).
fn ls_remote_fingerprint(stdout: &str) -> Option<String> {
    let mut branch: Option<&str> = None;
    let mut peeled: Option<&str> = None;
    let mut first: Option<&str> = None;
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(sha) = parts.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let reference = parts.next().unwrap_or("");
        if first.is_none() {
            first = Some(sha);
        }
        if reference.ends_with("^{}") {
            peeled = Some(sha);
        } else if reference.starts_with("refs/heads/") {
            branch = Some(sha);
        }
    }
    branch.or(peeled).or(first).map(str::to_string)
}

/// `git ls-remote` fingerprint of the remote ref (None when git is
/// unavailable or the probe fails — the archive fallback then
/// fingerprints by content).
fn git_ls_remote(source: &str, git_ref: Option<&str>) -> Option<String> {
    // A sha pin is its own fingerprint: skip the network entirely.
    if let Some(reference) = git_ref
        && is_commit_sha(reference)
    {
        return Some(reference.to_string());
    }
    let mut command = std::process::Command::new("git");
    command.arg("ls-remote").arg(source);
    for pattern in ls_remote_patterns(git_ref) {
        command.arg(pattern);
    }
    let output = crate::sync_process::output_with_timeout(
        scrub_git(&mut command),
        std::time::Duration::from_secs(30),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    ls_remote_fingerprint(&stdout)
}

/// A git source's forge base for archive degradation
/// (`https://host/owner/repo.git` → `https://host/owner/repo`). Only
/// http(s) sources have a forge to degrade to; ssh-style sources don't.
fn forge_base(source: &str) -> Option<(&str, &str)> {
    if !source.starts_with("http://") && !source.starts_with("https://") {
        return None;
    }
    let base = source.strip_suffix(".git").unwrap_or(source);
    let base = base.trim_end_matches('/');
    let repo = base.rsplit('/').next().filter(|r| !r.is_empty())?;
    Some((base, repo))
}

/// Candidate archive URLs for a ref: GitHub/Gitea shape, then GitLab.
fn archive_urls(source: &str, reference: &str) -> Vec<String> {
    let Some((base, repo)) = forge_base(source) else {
        return Vec::new();
    };
    vec![
        format!("{base}/archive/{reference}.tar.gz"),
        format!("{base}/-/archive/{reference}/{repo}-{reference}.tar.gz"),
    ]
}

const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
/// Redirect hop cap for catalog/archive fetches — a reqwest custom
/// redirect policy must enforce its own limit (the default of 10 no
/// longer applies).
const MAX_REDIRECTS: usize = 10;

/// Whether a redirect hop to `target` (after `hops` prior redirects)
/// may proceed. Every hop re-runs the same scheme rule as the initial
/// URL: reqwest's default policy follows up to 10 redirects WITHOUT
/// re-validating, so an https catalog/archive URL could otherwise
/// downgrade to http:// or hop to a non-public host. Loopback http
/// targets stay allowed, matching `validate_url_scheme`'s exception
/// for local mirrors.
fn redirect_allowed(target: &str, hops: usize) -> bool {
    hops < MAX_REDIRECTS
        && crate::catalog_refresh::validate_url_scheme(target, "redirect target").is_ok()
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("tack/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if redirect_allowed(attempt.url().as_str(), attempt.previous().len()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn http_get(url: &str, cap: u64, what: &str) -> anyhow::Result<Vec<u8>> {
    crate::catalog_refresh::validate_url_scheme(url, what)?;
    let mut response = http_client()
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("fetching {url}: {e}"))?;
    if !response.status().is_success() {
        anyhow::bail!("fetching {url} failed: {}", response.status());
    }
    crate::catalog_refresh::read_body_capped(&mut response, cap, what).await
}

/// Fetch a git repo's catalog via an https archive (the git → https
/// degradation): try each candidate ref × forge URL shape, extract
/// defensively, read the catalog out of the snapshot.
async fn fetch_catalog_via_archive(
    source: &str,
    refs: &[String],
    catalog_path: &str,
) -> anyhow::Result<(Vec<u8>, String)> {
    let mut last_error: Option<anyhow::Error> = None;
    for reference in refs {
        for url in archive_urls(source, reference) {
            let bytes = match http_get(&url, MAX_ARCHIVE_BYTES, "repo archive").await {
                Ok(bytes) => bytes,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            let catalog_path = catalog_path.to_string();
            let extracted = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
                let tmp = tempfile::tempdir()?;
                crate::archive_extract::extract_tgz(
                    &bytes,
                    tmp.path(),
                    &crate::archive_extract::ExtractCaps::REPO_SNAPSHOT,
                )?;
                let root = crate::archive_extract::single_top_level_dir(tmp.path())
                    .unwrap_or_else(|| tmp.path().to_path_buf());
                Ok(std::fs::read(root.join(&catalog_path))?)
            })
            .await
            .map_err(|e| anyhow::anyhow!("archive extraction worker failed: {e}"))?;
            // A forge can answer 200 with a non-tarball body (a login
            // page, error HTML): treat extraction/catalog-read failures
            // like fetch failures — a later URL/ref candidate may work.
            match extracted {
                Ok(catalog) => return Ok((catalog, reference.clone())),
                Err(e) => {
                    last_error = Some(e.context(format!("extracting {url}")));
                    continue;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no archive URL candidates for {source}")))
}

// ---------------------------------------------------------------------------
// The sync engine
// ---------------------------------------------------------------------------

/// Kick the curated marketplace sync in the background. Never blocks
/// startup: every failure is a structured warning (target
/// `marketplace_sync`, shipped via the managed auditSink when set).
pub fn start_background_sync(agent_dir: PathBuf) {
    let decls = sync_declarations(&agent_dir);
    if decls.is_empty() {
        return;
    }
    crate::task::spawn_guarded("marketplace-sync", async move {
        for decl in decls {
            let name = decl.name.clone();
            let agent = agent_dir.clone();
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(180), sync_one(&agent, &decl))
                    .await;
            match outcome {
                Ok(SyncOutcome::Synced) => tracing::info!(
                    target: "marketplace_sync",
                    marketplace = %name,
                    outcome = "synced",
                    "marketplace {name} synced"
                ),
                Ok(SyncOutcome::Unchanged) => tracing::debug!(
                    target: "marketplace_sync",
                    marketplace = %name,
                    outcome = "unchanged",
                    "marketplace {name} already current"
                ),
                Ok(SyncOutcome::Busy) => tracing::debug!(
                    target: "marketplace_sync",
                    marketplace = %name,
                    outcome = "busy",
                    "marketplace {name} is syncing in another process"
                ),
                Ok(SyncOutcome::Failed(e)) => tracing::warn!(
                    target: "marketplace_sync",
                    marketplace = %name,
                    outcome = "failed",
                    detail = %e,
                    "marketplace {name} sync failed: {e} (keeping the previous catalog)"
                ),
                Err(_) => tracing::warn!(
                    target: "marketplace_sync",
                    marketplace = %name,
                    outcome = "failed",
                    detail = "timeout",
                    "marketplace {name} sync timed out (keeping the previous catalog)"
                ),
            }
        }
        Some(())
    });
}

/// Synchronous sync for `tack ext marketplace sync [name]`: every
/// declared marketplace (or one), with default-plugin installs applied.
pub async fn sync_all(agent_dir: &Path, only: Option<&str>) -> Vec<(String, SyncOutcome)> {
    let decls = sync_declarations(agent_dir);
    let mut outcomes = Vec::new();
    let mut matched = false;
    for decl in &decls {
        if let Some(only) = only
            && decl.name != only
        {
            continue;
        }
        matched = true;
        let outcome = match tokio::time::timeout(
            std::time::Duration::from_secs(180),
            sync_one(agent_dir, decl),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => SyncOutcome::Failed("timeout".to_string()),
        };
        outcomes.push((decl.name.clone(), outcome));
    }
    if !matched {
        let note = match only {
            Some(only) => {
                format!("no pluginMarketplaces declaration named {only:?} (see settings)")
            }
            None => "no pluginMarketplaces declared in settings".to_string(),
        };
        outcomes.push((only.unwrap_or("-").to_string(), SyncOutcome::Failed(note)));
    }
    outcomes
}

/// Sync one marketplace end to end; the registered catalog is only ever
/// replaced by a fully validated one.
async fn sync_one(agent_dir: &Path, decl: &MarketplaceSyncDecl) -> SyncOutcome {
    match sync_one_inner(agent_dir, decl).await {
        Ok(outcome) => outcome,
        Err(e) => SyncOutcome::Failed(format!("{e:#}")),
    }
}

async fn sync_one_inner(
    agent_dir: &Path,
    decl: &MarketplaceSyncDecl,
) -> anyhow::Result<SyncOutcome> {
    let name = decl.name.as_str();
    let Some(_lock) = SyncLock::acquire(agent_dir, name) else {
        return Ok(SyncOutcome::Busy);
    };
    let target =
        crate::extension_host::marketplaces_root_path(agent_dir).join(format!("{name}.json"));
    // Crash-window self-heal: a crash between the backup rename and the
    // staged rename leaves only `<name>.json.bak`. Put it back BEFORE
    // the fingerprint decision, so an unreachable source doesn't strand
    // a marketplace that has a perfectly good backup.
    let backup =
        crate::extension_host::marketplaces_root_path(agent_dir).join(format!("{name}.json.bak"));
    if !target.is_file() && backup.is_file() {
        match std::fs::rename(&backup, &target) {
            Ok(()) => tracing::warn!(
                target: "marketplace_sync",
                marketplace = %name,
                "restored {name}.json from its .bak (crash-window recovery)"
            ),
            Err(e) => tracing::warn!(
                target: "marketplace_sync",
                marketplace = %name,
                "cannot restore {name}.json.bak: {e}"
            ),
        }
    }
    let unchanged = |fingerprint: &str| {
        read_state(agent_dir, name)
            .map(|s| s.fingerprint == fingerprint)
            .unwrap_or(false)
            && target.is_file()
    };
    let catalog_rel = decl.catalog_path.as_deref().unwrap_or("marketplace.json");

    let (content, fingerprint) = match classify(&decl.source) {
        SourceKind::LocalPath => {
            let path = PathBuf::from(&decl.source);
            let file = if path.is_dir() {
                path.join(catalog_rel)
            } else {
                path
            };
            let content = std::fs::read(&file)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", file.display()))?;
            let fingerprint = sha256_hex(&content);
            (content, fingerprint)
        }
        SourceKind::JsonUrl => {
            let content = http_get(&decl.source, MAX_CATALOG_BYTES, "marketplace catalog").await?;
            let fingerprint = sha256_hex(&content);
            (content, fingerprint)
        }
        SourceKind::Git => {
            let source = decl.source.clone();
            let git_ref = decl.git_ref.clone();
            let sha = {
                let source = source.clone();
                let git_ref = git_ref.clone();
                tokio::task::spawn_blocking(move || git_ls_remote(&source, git_ref.as_deref()))
                    .await?
            };
            if let Some(sha) = &sha
                && unchanged(sha)
            {
                install_default_plugins(agent_dir, name).await;
                return Ok(SyncOutcome::Unchanged);
            }
            let clone_result = {
                let source = source.clone();
                let git_ref = git_ref.clone();
                tokio::task::spawn_blocking(move || {
                    let tmp = tempfile::tempdir()?;
                    let repo = tmp.path().join("repo");
                    crate::extension_host::git_clone(&source, git_ref.as_deref(), &repo)?;
                    // Keep the tempdir alive for the reader.
                    Ok::<_, anyhow::Error>((tmp, repo))
                })
                .await?
            };
            match clone_result {
                Ok((tmp, repo)) => {
                    let file = repo.join(catalog_rel);
                    let content = std::fs::read(&file).map_err(|e| {
                        anyhow::anyhow!("catalog {} not in repo: {e}", file.display())
                    })?;
                    let fingerprint = sha.unwrap_or_else(|| sha256_hex(&content));
                    drop(tmp);
                    (content, fingerprint)
                }
                Err(clone_error) => {
                    // git → https archive degradation.
                    if !crate::catalog_refresh::is_https_or_loopback(&decl.source) {
                        return Err(clone_error.context("git clone failed"));
                    }
                    tracing::warn!(
                        target: "marketplace_sync",
                        marketplace = %name,
                        "git clone failed ({clone_error:#}); trying the https archive"
                    );
                    let mut refs: Vec<String> = Vec::new();
                    for candidate in [
                        sha,
                        decl.git_ref.clone(),
                        Some("main".to_string()),
                        Some("master".to_string()),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if !refs.contains(&candidate) {
                            refs.push(candidate);
                        }
                    }
                    let (content, used_ref) =
                        fetch_catalog_via_archive(&decl.source, &refs, catalog_rel).await?;
                    let fingerprint = sha256_hex(&content);
                    tracing::info!(
                        target: "marketplace_sync",
                        marketplace = %name,
                        "catalog fetched via the https archive (ref {used_ref})"
                    );
                    (content, fingerprint)
                }
            }
        }
    };

    if unchanged(&fingerprint) {
        install_default_plugins(agent_dir, name).await;
        return Ok(SyncOutcome::Unchanged);
    }
    let changed = crate::extension_host::activate_synced_catalog(
        agent_dir,
        name,
        &content,
        decl.public_key.as_deref(),
    )?;
    write_state(agent_dir, name, &fingerprint);
    install_default_plugins(agent_dir, name).await;
    Ok(if changed {
        SyncOutcome::Synced
    } else {
        SyncOutcome::Unchanged
    })
}

/// Install catalog entries marked `installed-by-default` that are not
/// installed yet (policy-gated by the install channel like any other
/// install; failures are warnings, never startup blockers).
async fn install_default_plugins(agent_dir: &Path, marketplace: &str) {
    let agent_dir = agent_dir.to_path_buf();
    let marketplace_owned = marketplace.to_string();
    let marketplace_for_task = marketplace_owned.clone();
    let result = tokio::task::spawn_blocking(move || {
        install_default_plugins_blocking(&agent_dir, &marketplace_for_task)
    })
    .await;
    if let Err(e) = result {
        tracing::warn!(
            target: "marketplace_sync",
            marketplace = %marketplace_owned,
            "default-plugin installs panicked off: {e}"
        );
    }
}

/// Does the lock entry's installed directory still exist? A lock row
/// alone is not proof of an install — the store copy can be deleted or
/// corrupted out from under it, and then the default top-up must
/// re-materialize the plugin instead of skipping it.
///
/// Local mirror of `extension_host::lock_entry_dir` (private there; the
/// layout is `store/<source>/<name>/<version>` vs the legacy flat
/// `extensions/<name>`). Keep in sync with that function.
fn lock_entry_dir_exists(
    agent_dir: &Path,
    id: &str,
    entry: &crate::extension_host::LockEntry,
) -> bool {
    let (name, source) = match id.split_once('@') {
        Some((name, source)) => (name, source),
        None => (id, "user"),
    };
    let dir = if entry.store {
        agent_dir
            .join("extensions")
            .join("store")
            .join(source)
            .join(name)
            .join(entry.version.as_deref().unwrap_or("local"))
    } else {
        agent_dir.join("extensions").join(name)
    };
    dir.is_dir()
}

fn install_default_plugins_blocking(agent_dir: &Path, marketplace: &str) {
    let defaults = match crate::extension_host::marketplace_default_installs(agent_dir, marketplace)
    {
        Ok(defaults) => defaults,
        Err(e) => {
            tracing::warn!(
                target: "marketplace_sync",
                marketplace = %marketplace,
                "cannot enumerate default plugins: {e:#}"
            );
            return;
        }
    };
    if defaults.is_empty() {
        return;
    }
    let lock = crate::extension_host::read_lock(agent_dir).unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_else(|_| agent_dir.to_path_buf());
    for spec in defaults {
        let id = format!("{}@{}", spec.plugin, spec.marketplace);
        // Skip only a COMPLETE install: lock row AND on-disk store dir.
        if let Some(entry) = lock.plugins.get(&id)
            && lock_entry_dir_exists(agent_dir, &id, entry)
        {
            continue;
        }
        tracing::info!(
            target: "marketplace_sync",
            marketplace = %marketplace,
            plugin = %id,
            "installing default plugin {id}"
        );
        match crate::extension_host::install_extension_named(
            &spec.source,
            &cwd,
            agent_dir,
            false,
            Some(&spec.plugin),
            spec.rev.as_deref(),
            Some(&spec.marketplace),
        ) {
            Ok(_) => tracing::info!(
                target: "marketplace_sync",
                marketplace = %marketplace,
                plugin = %id,
                "default plugin {id} installed"
            ),
            // Policy denials land here too — the reason names the rule.
            Err(e) => tracing::warn!(
                target: "marketplace_sync",
                marketplace = %marketplace,
                plugin = %id,
                detail = %format!("{e:#}"),
                "default plugin {id} not installed: {e:#}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn declaration(settings: &str) -> Vec<MarketplaceSyncDecl> {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), settings).unwrap();
        sync_declarations(tmp.path())
    }

    #[test]
    fn declarations_parse_objects_and_shorthand() {
        let decls = declaration(
            r#"{
              "pluginMarketplaces": {
                "acme": {
                  "source": "https://git.acme.com/plugins.git",
                  "ref": "main",
                  "path": "catalog/acme.json",
                  "publicKey": "deadbeef"
                },
                "onprem": "/opt/tack/catalog.json"
              }
            }"#,
        );
        assert_eq!(decls.len(), 2);
        let acme = decls.iter().find(|d| d.name == "acme").unwrap();
        assert_eq!(acme.git_ref.as_deref(), Some("main"));
        assert_eq!(acme.catalog_path.as_deref(), Some("catalog/acme.json"));
        assert_eq!(acme.public_key.as_deref(), Some("deadbeef"));
        let local = decls.iter().find(|d| d.name == "onprem").unwrap();
        assert_eq!(local.source, "/opt/tack/catalog.json");
        assert!(local.git_ref.is_none());
    }

    #[test]
    fn declarations_skip_bad_entries() {
        let decls = declaration(
            r#"{
              "pluginMarketplaces": {
                "bad name!": "https://x",
                "nosrc": {"ref": "main"},
                "good": "https://example.com/catalog.json"
              }
            }"#,
        );
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].name, "good");
    }

    #[test]
    fn classify_picks_transports() {
        assert_eq!(classify("https://h/c.json"), SourceKind::JsonUrl);
        assert_eq!(classify("https://h/o/r.git"), SourceKind::Git);
        assert_eq!(classify("https://h/o/r"), SourceKind::Git);
        assert_eq!(classify("git@h:o/r.git"), SourceKind::Git);
        assert_eq!(classify("ssh://git@h/o/r.git"), SourceKind::Git);
        assert_eq!(classify("/opt/c.json"), SourceKind::LocalPath);
        assert_eq!(classify("./repo"), SourceKind::LocalPath);
    }

    #[test]
    fn classify_ignores_query_and_fragment_for_json_urls() {
        // A signed-catalog URL carries a token query — classify on the
        // path alone, case-insensitively.
        assert_eq!(classify("https://h/c.json?token=abc"), SourceKind::JsonUrl);
        assert_eq!(classify("https://h/c.json#frag"), SourceKind::JsonUrl);
        assert_eq!(classify("https://h/C.JSON?token=abc"), SourceKind::JsonUrl);
        assert_eq!(classify("https://h/dir/catalog.Json"), SourceKind::JsonUrl);
        // `.json` only in the query is NOT a JSON catalog.
        assert_eq!(classify("https://h/o/r?f=c.json"), SourceKind::Git);
        assert_eq!(classify("https://h/o/r.git?token=abc"), SourceKind::Git);
    }

    #[test]
    fn commit_sha_detection() {
        assert!(is_commit_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(is_commit_sha("0123456789ABCDEF0123456789ABCDEF01234567"));
        assert!(!is_commit_sha("0123456789abcdef0123456789abcdef0123456")); // 39
        assert!(!is_commit_sha("0123456789abcdef0123456789abcdef012345678")); // 41
        assert!(!is_commit_sha("g123456789abcdef0123456789abcdef01234567"));
        assert!(!is_commit_sha("main"));
    }

    #[test]
    fn ls_remote_patterns_cover_branch_then_tag() {
        assert_eq!(
            ls_remote_patterns(Some("v1.0")),
            vec![
                "refs/heads/v1.0".to_string(),
                "refs/tags/v1.0^{}".to_string(),
                "refs/tags/v1.0".to_string(),
            ]
        );
        assert_eq!(ls_remote_patterns(None), vec!["HEAD".to_string()]);
    }

    #[test]
    fn fingerprint_prefers_branch_then_peeled_tag() {
        // Annotated tag only: the peeled commit sha wins over the tag
        // object sha.
        let out = "aaaa refs/tags/v1.0\nbbbb refs/tags/v1.0^{}\n";
        assert_eq!(ls_remote_fingerprint(out).as_deref(), Some("bbbb"));
        // A branch of the same name beats the tag.
        let out = "cccc refs/heads/main\ndddd refs/tags/main\neeee refs/tags/main^{}\n";
        assert_eq!(ls_remote_fingerprint(out).as_deref(), Some("cccc"));
        // Lightweight tag / HEAD fall back to the first line.
        assert_eq!(
            ls_remote_fingerprint("dddd refs/tags/v2\n").as_deref(),
            Some("dddd")
        );
        assert_eq!(
            ls_remote_fingerprint("ffff HEAD\n").as_deref(),
            Some("ffff")
        );
        assert_eq!(ls_remote_fingerprint(""), None);
        assert_eq!(ls_remote_fingerprint("\n\n"), None);
    }

    #[test]
    fn sha_pin_needs_no_network() {
        // A sha pin is its own fingerprint: even an unreachable source
        // resolves instantly without a git invocation.
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            git_ls_remote("https://192.0.2.1.invalid/nope.git", Some(sha)).as_deref(),
            Some(sha)
        );
    }

    #[test]
    fn lock_blocks_reclaims_stale_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        // A held lock blocks a second acquisition.
        let held = SyncLock::acquire(agent_dir, "corp").unwrap();
        assert!(SyncLock::acquire(agent_dir, "corp").is_none());
        drop(held);
        // RAII: dropping releases the lock file.
        assert!(SyncLock::acquire(agent_dir, "corp").is_some());

        // A stale lock (a crashed process) is reclaimed via
        // rename-claim, and the claimed stale file is cleaned up.
        let stale_path = sync_scratch_dir(agent_dir).join("stale.lock");
        std::fs::write(&stale_path, "pid 1\n").unwrap();
        let file = std::fs::File::open(&stale_path).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(1200))
            .unwrap();
        drop(file);
        let lock = SyncLock::acquire(agent_dir, "stale").expect("stale lock is reclaimed");
        assert!(stale_path.is_file(), "a fresh lock replaces the stale one");
        let leftovers: Vec<_> = std::fs::read_dir(sync_scratch_dir(agent_dir))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".stale-"))
            .collect();
        assert!(leftovers.is_empty(), "rename-claimed file cleaned up");
        drop(lock);
    }

    #[test]
    fn lock_entry_dir_matches_the_store_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = crate::extension_host::LockEntry {
            source: "https://x/plugins.git".to_string(),
            rev: None,
            resolved_commit: None,
            installed_at: 0,
            version: Some("1.2.3".to_string()),
            store: true,
            marketplace: Some("corp".to_string()),
        };
        assert!(!lock_entry_dir_exists(tmp.path(), "review@corp", &entry));
        let dir = tmp.path().join("extensions/store/corp/review/1.2.3");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(lock_entry_dir_exists(tmp.path(), "review@corp", &entry));
        // Legacy flat layout (`store: false`): `extensions/<name>`.
        let legacy = crate::extension_host::LockEntry {
            store: false,
            version: None,
            ..entry
        };
        assert!(!lock_entry_dir_exists(tmp.path(), "demo@user", &legacy));
        std::fs::create_dir_all(tmp.path().join("extensions/demo")).unwrap();
        assert!(lock_entry_dir_exists(tmp.path(), "demo@user", &legacy));
    }

    #[test]
    fn forge_urls_cover_github_and_gitlab_shapes() {
        let urls = archive_urls("https://git.acme.com/tack/plugins.git", "main");
        assert_eq!(
            urls,
            vec![
                "https://git.acme.com/tack/plugins/archive/main.tar.gz",
                "https://git.acme.com/tack/plugins/-/archive/main/plugins-main.tar.gz"
            ]
        );
        assert!(archive_urls("git@git.acme.com:tack/plugins.git", "main").is_empty());
    }

    /// Redirect decisions re-validate every hop against the same rule
    /// as the initial URL (policy-level, no network).
    #[test]
    fn redirect_hops_are_revalidated() {
        assert!(redirect_allowed("https://cdn.example.com/x", 0));
        // The loopback exception matches validate_url_scheme.
        assert!(redirect_allowed("http://127.0.0.1:8080/x", 0));
        assert!(redirect_allowed("http://localhost/x", 0));
        assert!(redirect_allowed("http://[::1]:8080/x", 0));
        // An https → http downgrade to a public host is rejected.
        assert!(!redirect_allowed("http://evil.example.com/x", 0));
        assert!(!redirect_allowed("ftp://example.com/x", 0));
        assert!(!redirect_allowed("example.com/x", 0));
        // The hop cap stops redirect loops.
        assert!(!redirect_allowed("https://example.com/x", MAX_REDIRECTS));
        assert!(redirect_allowed("https://example.com/x", MAX_REDIRECTS - 1));
    }

    /// One queued fixture response: (status line, extra headers, body).
    type FixtureResponse = (String, Vec<(String, String)>, Vec<u8>);

    /// One-shot loopback HTTP responder: serves each queued response to
    /// one connection, in order, then exits. (Loopback http is the
    /// sanctioned test exception in validate_url_scheme — no real
    /// network is involved.)
    fn serve_http(responses: Vec<FixtureResponse>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for (status, headers, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                // Drain the request head (bounded — tests only).
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                let mut response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (key, value) in &headers {
                    response.push_str(&format!("{key}: {value}\r\n"));
                }
                response.push_str("\r\n");
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
            }
        });
        port
    }

    /// A 302 whose target fails the scheme rule is stopped by the
    /// redirect policy: the fetch fails without ever dialing the
    /// target (192.0.2.1 is TEST-NET-1 — unroutable by design).
    #[tokio::test(flavor = "multi_thread")]
    async fn redirect_to_insecure_target_is_rejected() {
        let port = serve_http(vec![(
            "302 Found".to_string(),
            vec![("Location".to_string(), "http://192.0.2.1/evil".to_string())],
            Vec::new(),
        )]);
        let url = format!("http://127.0.0.1:{port}/catalog.json");
        let err = http_get(&url, MAX_CATALOG_BYTES, "marketplace catalog")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("fetching"), "{err:#}");
    }

    /// Build a minimal repo-snapshot tgz: one top-level dir with one file.
    fn make_snapshot_tgz(top_dir: &str, file: &str, body: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        builder
            .append_data(&mut header, format!("{top_dir}/"), std::io::empty())
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(body.len() as u64);
        builder
            .append_data(&mut header, format!("{top_dir}/{file}"), body)
            .unwrap();
        let tar = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    /// A 200 with a non-tarball body on the first candidate must not
    /// kill the sync: extraction failures fall through to the next
    /// URL/ref candidate exactly like fetch failures.
    #[tokio::test(flavor = "multi_thread")]
    async fn archive_fallback_continues_past_a_bad_archive() {
        let catalog = br#"{"name": "corp", "plugins": {}}"#;
        let tgz = make_snapshot_tgz("plugins-main", "marketplace.json", catalog);
        let port = serve_http(vec![
            // GitHub shape: the forge answers 200 with an HTML page.
            ("200 OK".to_string(), vec![], b"<html>login</html>".to_vec()),
            // GitLab shape: the real snapshot.
            ("200 OK".to_string(), vec![], tgz),
        ]);
        let source = format!("http://127.0.0.1:{port}/tack/plugins.git");
        let (content, used_ref) =
            fetch_catalog_via_archive(&source, &["main".to_string()], "marketplace.json")
                .await
                .unwrap();
        assert_eq!(content, catalog);
        assert_eq!(used_ref, "main");
    }

    /// A local-path sync activates the catalog and short-circuits the
    /// second pass via the fingerprint.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_sync_activates_then_short_circuits() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(
            &catalog,
            r#"{"name": "corp", "plugins": {"demo": {"source": "/x/demo"}}}"#,
        )
        .unwrap();
        let decl = MarketplaceSyncDecl {
            name: "corp".to_string(),
            source: catalog.display().to_string(),
            git_ref: None,
            catalog_path: None,
            public_key: None,
        };
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Synced);
        let registered =
            crate::extension_host::marketplaces_root_path(&agent_dir).join("corp.json");
        assert!(registered.is_file());
        // Second pass: fingerprint hit → Unchanged, no backup churn.
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Unchanged);
        // A content change activates again and keeps a backup.
        std::fs::write(
            &catalog,
            r#"{"name": "corp", "plugins": {"demo": {"source": "/x/demo2"}}}"#,
        )
        .unwrap();
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Synced);
        assert!(
            crate::extension_host::marketplaces_root_path(&agent_dir)
                .join("corp.json.bak")
                .is_file(),
            "the previous catalog is backed up on swap"
        );
        let listed = crate::extension_host::marketplace_plugins(&agent_dir, "corp").unwrap();
        assert_eq!(listed[0].source, "/x/demo2");
        // The .sync scratch dir must not pollute the marketplace listing.
        let names: Vec<String> = crate::extension_host::list_marketplaces(&agent_dir)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["corp".to_string()]);
    }

    /// An invalid synced catalog never replaces the working one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_sync_keeps_the_previous_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(&catalog, r#"{"name": "corp", "plugins": {}}"#).unwrap();
        let decl = MarketplaceSyncDecl {
            name: "corp".to_string(),
            source: catalog.display().to_string(),
            git_ref: None,
            catalog_path: None,
            public_key: None,
        };
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Synced);
        std::fs::write(&catalog, "{not json").unwrap();
        let outcome = sync_one(&agent_dir, &decl).await;
        assert!(matches!(outcome, SyncOutcome::Failed(_)), "{outcome:?}");
        let registered =
            crate::extension_host::marketplaces_root_path(&agent_dir).join("corp.json");
        assert_eq!(
            std::fs::read_to_string(registered).unwrap(),
            r#"{"name": "corp", "plugins": {}}"#
        );
    }

    /// Default installs pull installed-by-default entries through the
    /// install channel (here: a local plugin dir source).
    #[tokio::test(flavor = "multi_thread")]
    async fn default_installs_materialize_missing_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let plugin = tmp.path().join("review");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("extension.json"),
            r#"{"name": "review", "version": "1.0.0"}"#,
        )
        .unwrap();
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(
            &catalog,
            serde_json::to_string(&serde_json::json!({
                "name": "corp",
                "plugins": {
                    "review": {
                        "source": plugin.display().to_string(),
                        "installation": "installed-by-default"
                    },
                    "ondemand": { "source": "/x/ondemand" }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let decl = MarketplaceSyncDecl {
            name: "corp".to_string(),
            source: catalog.display().to_string(),
            git_ref: None,
            catalog_path: None,
            public_key: None,
        };
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Synced);
        let lock = crate::extension_host::read_lock(&agent_dir).unwrap();
        assert!(
            lock.plugins.contains_key("review@corp"),
            "installed-by-default lands in the lockfile: {:?}",
            lock.plugins.keys().collect::<Vec<_>>()
        );
        assert!(!lock.plugins.contains_key("ondemand@corp"));
        // A second pass leaves the install alone (fingerprint-idempotent).
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Unchanged);
    }

    /// Crash-window self-heal: when the live `<name>.json` is missing
    /// but its `.bak` survives, the sync restores it BEFORE touching
    /// the source — even an unreachable source leaves the marketplace
    /// registered again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_live_catalog_is_restored_from_the_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(&catalog, r#"{"name": "corp", "plugins": {}}"#).unwrap();
        let decl = MarketplaceSyncDecl {
            name: "corp".to_string(),
            source: catalog.display().to_string(),
            git_ref: None,
            catalog_path: None,
            public_key: None,
        };
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Synced);
        // A changed second sync leaves the previous catalog as `.bak`.
        std::fs::write(
            &catalog,
            r#"{"name": "corp", "plugins": {"demo": {"source": "/x/d"}}}"#,
        )
        .unwrap();
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Synced);
        let root = crate::extension_host::marketplaces_root_path(&agent_dir);
        assert!(root.join("corp.json.bak").is_file());
        // Simulate the crash window: the live catalog is gone, only the
        // backup survived — and the source is now unreachable.
        std::fs::remove_file(root.join("corp.json")).unwrap();
        std::fs::remove_file(&catalog).unwrap();
        let outcome = sync_one(&agent_dir, &decl).await;
        assert!(matches!(outcome, SyncOutcome::Failed(_)), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("corp.json")).unwrap(),
            r#"{"name": "corp", "plugins": {}}"#,
            "the previous catalog went back live from the backup"
        );
        assert!(!root.join("corp.json.bak").exists());
    }

    /// The default-install top-up must not trust the lockfile alone: a
    /// deleted store copy is re-materialized on the next pass.
    #[tokio::test(flavor = "multi_thread")]
    async fn default_installs_rematerialize_a_deleted_store_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let plugin = tmp.path().join("review");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("extension.json"),
            r#"{"name": "review", "version": "1.0.0"}"#,
        )
        .unwrap();
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(
            &catalog,
            serde_json::to_string(&serde_json::json!({
                "name": "corp",
                "plugins": {
                    "review": {
                        "source": plugin.display().to_string(),
                        "installation": "installed-by-default"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let decl = MarketplaceSyncDecl {
            name: "corp".to_string(),
            source: catalog.display().to_string(),
            git_ref: None,
            catalog_path: None,
            public_key: None,
        };
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Synced);
        let store_dir = agent_dir
            .join("extensions")
            .join("store")
            .join("corp")
            .join("review")
            .join("1.0.0");
        assert!(store_dir.is_dir(), "store layout: {}", store_dir.display());
        // The store copy vanishes; the lock row remains.
        std::fs::remove_dir_all(&store_dir).unwrap();
        let lock = crate::extension_host::read_lock(&agent_dir).unwrap();
        assert!(lock.plugins.contains_key("review@corp"));
        // The unchanged-fingerprint pass still runs the top-up, which
        // now detects the missing store dir and re-installs.
        assert_eq!(sync_one(&agent_dir, &decl).await, SyncOutcome::Unchanged);
        assert!(
            store_dir.is_dir(),
            "the deleted store copy is re-materialized"
        );
    }
}
