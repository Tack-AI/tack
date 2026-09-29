//! Model catalog refresh (TS model-catalog-refresh.ts / models-store.ts).
//!
//! The embedded `catalog.json` snapshots the model list at build time; new
//! upstream models only arrive with a tack release. This module fetches a
//! fresh catalog from the published `@earendil-works/pi-ai` npm package —
//! its tarball ships `dist/providers/data/<provider>.json`, the exact data
//! the embedded catalog was converted from — and installs it as a runtime
//! override (cached in `<agentDir>/catalog.json`).
//!
//! Behavior:
//! - `load_cached_override` installs a previously fetched catalog at startup
//!   (no network; runs whenever the cache file exists).
//! - `refresh_from_npm` does the network fetch (gated by the
//!   `modelCatalogRefresh` setting, `TACK_OFFLINE`, and the `--offline` flag
//!   at the call sites).
//! - The upstream registry URL can be overridden with
//!   `TACK_MODEL_CATALOG_REGISTRY` (an npm-style registry base URL).
//!
//! Supply-chain controls (a fetched catalog is code-adjacent: it decides
//! where requests — carrying API keys — are sent):
//! - HTTPS only for the registry and tarball URLs (loopback http is allowed
//!   for local mirrors/tests).
//! - The tarball is verified against the registry document's
//!   `dist.integrity` sha512 (SRI) before parsing; missing integrity fails
//!   closed.
//! - Size caps on the metadata document, the compressed tarball, the
//!   decompressed stream, and each provider data file (zip-bomb guard).
//! - Per-model `baseUrl` values must be https and — when the provider has a
//!   built-in endpoint — must keep the built-in host, so a poisoned catalog
//!   cannot redirect a provider's traffic (and credentials) to a new host.
//! - Auth/hop-by-hop header names (`authorization`, `x-api-key`, …) in
//!   catalog data are stripped.
//! - A truncated catalog (<20 providers / <50 models) is refused.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, bail};
use serde_json::Value;

use tack_ai::providers::{BUILTIN_PROVIDERS, install_catalog_override, parse_catalog_json};
use tack_ai::types::{InputKind, Model, ModelCost};

/// npm registry document for the latest tack-ai package.
const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/@earendil-works%2fpi-ai";

/// Path of the provider data files inside the published tarball.
const TARBALL_DATA_PREFIX: &str = "package/dist/providers/data/";

/// Cap on the registry metadata document (`/<pkg>/latest`).
const MAX_METADATA_BYTES: u64 = 8 * 1024 * 1024;
/// Cap on the compressed tarball download.
const MAX_TARBALL_BYTES: u64 = 64 * 1024 * 1024;
/// Cap on the decompressed tarball stream (gzip bombs expand ~1000:1).
const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
/// Cap on a single provider data file inside the tarball.
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;
/// Cap on the cached catalog read at startup (a bigger file is corrupt or
/// hostile — the real cache is a few hundred KB).
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Sanity floor: fewer providers/models than this means a truncated or
/// layout-changed tarball, not a real catalog.
const MIN_CATALOG_PROVIDERS: usize = 20;
const MIN_CATALOG_MODELS: usize = 50;

/// Where the converted catalog is cached between runs.
pub fn catalog_cache_path(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join("catalog.json")
}

/// Sidecar recording provenance of the cached catalog.
pub fn catalog_meta_path(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join("catalog.meta.json")
}

#[derive(Clone, Debug)]
pub struct RefreshSummary {
    /// Version of the npm package the catalog came from.
    pub version: String,
    /// Providers with model lists in the refreshed catalog.
    pub providers: usize,
    /// Total models across providers.
    pub models: usize,
}

/// Install a previously refreshed catalog from `<agentDir>/catalog.json`.
/// Returns the provider/model counts, or None when no cache exists. A
/// corrupt cache is deleted (the embedded catalog remains authoritative).
pub fn load_cached_override(agent_dir: &Path) -> Option<(usize, usize)> {
    let path = catalog_cache_path(agent_dir);
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > MAX_CACHE_BYTES
    {
        tracing::warn!(
            "cached catalog implausibly large ({} bytes), removing",
            meta.len()
        );
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    match parse_catalog_json(&raw) {
        Ok(mut catalog) => {
            // Re-apply the untrusted-data rules on load: a cache written by
            // an older, unsanitizing build (or hand-edited) must not smuggle
            // in off-host baseUrls or auth headers.
            sanitize_loaded_catalog(&mut catalog);
            let providers = catalog.len();
            let models = catalog.values().map(Vec::len).sum();
            install_catalog_override(catalog);
            Some((providers, models))
        }
        Err(e) => {
            tracing::warn!("cached catalog corrupt, removing: {e}");
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Remove the cached override and restart with the embedded catalog.
/// Note: the in-process override of this run stays until restart.
pub fn clear_override(agent_dir: &Path) {
    let _ = std::fs::remove_file(catalog_cache_path(agent_dir));
    let _ = std::fs::remove_file(catalog_meta_path(agent_dir));
}

/// Fetch the latest catalog from the published npm package and install it.
pub async fn refresh_from_npm(
    agent_dir: &Path,
    timeout: std::time::Duration,
) -> anyhow::Result<RefreshSummary> {
    let registry = std::env::var("TACK_MODEL_CATALOG_REGISTRY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
    let registry = registry.trim_end_matches('/');
    validate_url_scheme(registry, "registry")?;
    let latest_url = format!("{registry}/latest");

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("tack/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("http client")?;

    let mut resp = client
        .get(&latest_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("fetching registry metadata")?;
    let body = read_body_capped(&mut resp, MAX_METADATA_BYTES, "registry metadata").await?;
    let doc: Value = serde_json::from_slice(&body).context("parsing registry metadata")?;
    let version = doc
        .get("version")
        .and_then(|v| v.as_str())
        .context("registry metadata has no version")?
        .to_string();
    let tarball = doc
        .pointer("/dist/tarball")
        .and_then(|v| v.as_str())
        .context("registry metadata has no dist.tarball")?
        .to_string();
    validate_url_scheme(&tarball, "tarball")?;
    // Fail closed: without an SRI hash the tarball's contents cannot be tied
    // to what the registry vended, so any mirror/CDN could substitute it.
    let integrity = doc
        .pointer("/dist/integrity")
        .and_then(|v| v.as_str())
        .context("registry metadata has no dist.integrity; refusing unverified tarball")?
        .to_string();

    let mut resp = client
        .get(&tarball)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("downloading package tarball")?;
    let bytes = read_body_capped(&mut resp, MAX_TARBALL_BYTES, "package tarball").await?;

    // sha512 over up-to-64MB + gzip/tar extraction + the atomic persist
    // are blocking CPU/IO: run them on the blocking pool so an executor
    // thread isn't stalled for the duration.
    let agent_dir = agent_dir.to_path_buf();
    let (catalog, summary) = tokio::task::spawn_blocking(
        move || -> anyhow::Result<(BTreeMap<String, Vec<Model>>, RefreshSummary)> {
            verify_tarball_integrity(&bytes, &integrity)?;
            let catalog = extract_catalog(&bytes)?;
            // Sanity gate: a truncated or layout-changed tarball must not
            // wipe the catalog down to a handful of providers.
            let (providers, models) = validate_fetched_catalog(&catalog)?;
            let summary = RefreshSummary {
                version,
                providers,
                models,
            };
            persist(&agent_dir, &catalog, &summary)?;
            Ok((catalog, summary))
        },
    )
    .await
    .context("catalog refresh worker panicked")??;
    install_catalog_override(catalog);
    Ok(summary)
}

/// Require https (loopback http is allowed for local mirrors and tests).
pub(crate) fn validate_url_scheme(url: &str, what: &str) -> anyhow::Result<()> {
    if is_https_or_loopback(url) {
        return Ok(());
    }
    bail!("{what} URL must be https (got {url:?}); refusing to fetch catalog data insecurely");
}

pub(crate) fn is_https_or_loopback(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    if let Some(rest) = url.strip_prefix("http://") {
        return is_loopback_host(rest.split('/').next().unwrap_or(""));
    }
    false
}

fn is_loopback_host(hostport: &str) -> bool {
    let host = if let Some(h) = hostport.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// Host portion of a URL (without port), "" when unparseable.
fn url_host(url: &str) -> &str {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let hostport = rest.split('/').next().unwrap_or("");
    if let Some(h) = hostport.strip_prefix('[') {
        return h.split(']').next().unwrap_or("");
    }
    hostport.split(':').next().unwrap_or("")
}

/// Read a response body with a hard size cap (Content-Length fast path plus
/// a per-chunk guard for chunked/lying servers). Shared by the managed-tool
/// and self-update downloads (tools_manager / self_update).
pub(crate) async fn read_body_capped(
    resp: &mut reqwest::Response,
    cap: u64,
    what: &str,
) -> anyhow::Result<Vec<u8>> {
    if let Some(len) = resp.content_length()
        && len > cap
    {
        bail!("{what} is {len} bytes, over the {cap}-byte cap");
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("reading {what}"))?
    {
        push_capped(&mut buf, &chunk, cap, what)?;
    }
    Ok(buf)
}

fn push_capped(buf: &mut Vec<u8>, chunk: &[u8], cap: u64, what: &str) -> anyhow::Result<()> {
    if buf.len() as u64 + chunk.len() as u64 > cap {
        bail!("{what} exceeds the {cap}-byte cap");
    }
    buf.extend_from_slice(chunk);
    Ok(())
}

/// Verify the tarball against the registry document's SRI `dist.integrity`
/// (`sha512-<base64>`; multiple space-separated entries allowed).
fn verify_tarball_integrity(bytes: &[u8], integrity: &str) -> anyhow::Result<()> {
    use base64::Engine as _;
    use sha2::Digest as _;
    let mut saw_sha512 = false;
    for token in integrity.split_whitespace() {
        let Some((algo, b64)) = token.split_once('-') else {
            continue;
        };
        if !algo.eq_ignore_ascii_case("sha512") {
            continue;
        }
        saw_sha512 = true;
        let expected = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("malformed dist.integrity sha512")?;
        let actual = sha2::Sha512::digest(bytes);
        if actual[..] == expected[..] {
            return Ok(());
        }
    }
    if saw_sha512 {
        bail!("tarball sha512 does not match dist.integrity");
    }
    bail!("dist.integrity has no sha512 entry; refusing unverified tarball");
}

/// Sanity gate on a freshly fetched catalog (corruption protection, not a
/// security boundary — it only catches truncation/layout changes).
fn validate_fetched_catalog(
    catalog: &BTreeMap<String, Vec<Model>>,
) -> anyhow::Result<(usize, usize)> {
    let providers = catalog.len();
    let models: usize = catalog.values().map(Vec::len).sum();
    if providers < MIN_CATALOG_PROVIDERS || models < MIN_CATALOG_MODELS {
        bail!(
            "fetched catalog looks truncated ({providers} providers, {models} models); refusing to install"
        );
    }
    Ok((providers, models))
}

/// Write the converted catalog (+ provenance sidecar) atomically.
fn persist(
    agent_dir: &Path,
    catalog: &BTreeMap<String, Vec<Model>>,
    summary: &RefreshSummary,
) -> anyhow::Result<()> {
    let mut out = serde_json::Map::new();
    for (provider, models) in catalog {
        let api = models.first().map(|m| m.api.clone()).unwrap_or_default();
        out.insert(
            provider.clone(),
            serde_json::json!({ "api": api, "models": models }),
        );
    }
    write_atomic(
        &catalog_cache_path(agent_dir),
        &serde_json::to_string_pretty(&Value::Object(out))?,
    )
    .context("writing catalog cache")?;

    let meta = serde_json::json!({
        "source": "@earendil-works/pi-ai",
        "version": summary.version,
        "providers": summary.providers,
        "models": summary.models,
        "fetchedAt": tack_ai::types::now_millis(),
    });
    write_atomic(
        &catalog_meta_path(agent_dir),
        &serde_json::to_string_pretty(&meta)?,
    )?;
    Ok(())
}

/// Atomic file write: unique temp file in the same directory, fsync, then
/// rename (a crash mid-write never leaves a torn target; concurrent
/// processes don't clobber each other's temp file).
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("catalog.json");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A reader that errors (rather than silently truncating) once `remaining`
/// bytes have been produced — zip-bomb guard for the gzip stream.
struct CapReader<R> {
    inner: R,
    remaining: u64,
    what: &'static str,
}

impl<R: Read> Read for CapReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            // Distinguish a clean EOF exactly at the cap from real overflow.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe) {
                Ok(0) => Ok(0),
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} exceeds size cap", self.what),
                )),
                Err(e) => Err(e),
            };
        }
        let max = (self.remaining as usize).min(buf.len());
        let n = self.inner.read(&mut buf[..max])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Size limits for tarball extraction (tests use small values).
#[derive(Clone, Copy, Debug)]
struct ExtractLimits {
    decompressed: u64,
    entry: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            decompressed: MAX_DECOMPRESSED_BYTES,
            entry: MAX_ENTRY_BYTES,
        }
    }
}

/// Decompress the npm tarball and convert every
/// `package/dist/providers/data/<provider>.json` into catalog models.
fn extract_catalog(tarball: &[u8]) -> anyhow::Result<BTreeMap<String, Vec<Model>>> {
    extract_catalog_with_limits(tarball, ExtractLimits::default())
}

fn extract_catalog_with_limits(
    tarball: &[u8],
    limits: ExtractLimits,
) -> anyhow::Result<BTreeMap<String, Vec<Model>>> {
    let gz = flate2::read::GzDecoder::new(tarball);
    let capped = CapReader {
        inner: gz,
        remaining: limits.decompressed,
        what: "decompressed tarball",
    };
    let mut archive = tar::Archive::new(capped);
    let mut catalog: BTreeMap<String, Vec<Model>> = BTreeMap::new();
    for entry in archive.entries().context("reading tarball entries")? {
        let entry = entry.context("tarball entry")?;
        let path = entry.path().context("tarball entry path")?.to_path_buf();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let Some(provider_id) = name.strip_suffix(".json") else {
            continue;
        };
        if !path.starts_with(TARBALL_DATA_PREFIX) {
            continue;
        }
        // Only providers tack knows about (same id space as the TS repo).
        let Some(def) = BUILTIN_PROVIDERS.iter().find(|p| p.id == provider_id) else {
            continue;
        };
        // Read one byte past the cap so an oversized file is an error, not a
        // silently truncated JSON document.
        let mut raw = String::new();
        entry
            .take(limits.entry + 1)
            .read_to_string(&mut raw)
            .context("reading provider data")?;
        if raw.len() as u64 > limits.entry {
            bail!(
                "provider data file {name} exceeds the {}-byte cap",
                limits.entry
            );
        }
        if let Some(models) = convert_provider_data(provider_id, def.base_url, &raw)
            && !models.is_empty()
        {
            catalog.insert(provider_id.to_string(), models);
        }
    }
    Ok(catalog)
}

/// Keep a catalog-supplied baseUrl only when it cannot redirect the
/// provider's traffic: https (loopback http tolerated), and — when the
/// provider has a built-in endpoint — the same host as that endpoint. A
/// poisoned catalog may add/drop models but must not move a provider (and
/// its API keys) to an attacker host.
fn sanitize_catalog_base_url(candidate: Option<String>, default_base_url: &str) -> String {
    let Some(url) = candidate else {
        return default_base_url.to_string();
    };
    if !is_https_or_loopback(&url) {
        tracing::warn!("catalog baseUrl {url:?} is not https; using provider default");
        return default_base_url.to_string();
    }
    let default_host = url_host(default_base_url);
    if !default_host.is_empty() && url_host(&url) != default_host {
        tracing::warn!(
            "catalog baseUrl {url:?} points off the provider's built-in host {default_host}; using provider default"
        );
        return default_base_url.to_string();
    }
    url
}

/// Re-apply the refresh-path URL/header policy to a catalog loaded from
/// disk (the cache is only ever written post-sanitization, but older builds
/// and manual edits are not covered by that).
fn sanitize_loaded_catalog(catalog: &mut BTreeMap<String, Vec<Model>>) {
    for (provider, models) in catalog.iter_mut() {
        let default = BUILTIN_PROVIDERS
            .iter()
            .find(|p| p.id == provider)
            .map(|d| d.base_url)
            .unwrap_or("");
        for model in models.iter_mut() {
            if model.base_url.is_empty() {
                model.base_url = default.to_string();
            } else {
                model.base_url = sanitize_catalog_base_url(Some(model.base_url.clone()), default);
            }
            if let Some(headers) = model.headers.take() {
                model.headers = sanitize_catalog_headers(headers);
            }
        }
    }
}

/// Header names a fetched catalog may not set: credential carriers and
/// hop-by-hop fields that could replace or redirect request auth.
fn is_forbidden_catalog_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "apikey"
            | "x-goog-api-key"
            | "x-amz-security-token"
            | "cookie"
            | "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
    )
}

fn sanitize_catalog_headers(headers: BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
    let clean: BTreeMap<String, String> = headers
        .into_iter()
        .filter(|(k, _)| !is_forbidden_catalog_header(k))
        .collect();
    if clean.is_empty() { None } else { Some(clean) }
}

/// Convert one provider data file — `{ "<api>": { "<modelId>": {…} } }`
/// (TS providers/data JSON + flattenModelCatalog) — into catalog models.
fn convert_provider_data(
    provider_id: &str,
    default_base_url: &str,
    raw: &str,
) -> Option<Vec<Model>> {
    let parsed: BTreeMap<String, BTreeMap<String, Value>> = serde_json::from_str(raw).ok()?;
    let mut models = Vec::new();
    for (api, group) in parsed {
        for (id, fields) in group {
            let obj = fields.as_object()?;
            let get_str = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(str::to_string);
            let base_url = get_str("baseUrl").filter(|u| !u.is_empty());
            let mut model = Model {
                id: id.clone(),
                name: get_str("name")
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| id.clone()),
                api: api.clone(),
                provider: provider_id.to_string(),
                base_url: sanitize_catalog_base_url(base_url, default_base_url),
                reasoning: obj
                    .get("reasoning")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                thinking_level_map: obj
                    .get("thinkingLevelMap")
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
                input: obj
                    .get("input")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .filter_map(|s| match s {
                                "text" => Some(InputKind::Text),
                                "image" => Some(InputKind::Image),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    })
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| vec![InputKind::Text]),
                cost: obj
                    .get("cost")
                    .and_then(|v| serde_json::from_value::<ModelCost>(v.clone()).ok())
                    .unwrap_or_default(),
                context_window: obj
                    .get("contextWindow")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                max_tokens: obj.get("maxTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                sampling_params: obj
                    .get("samplingParams")
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
                headers: obj
                    .get("headers")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .and_then(sanitize_catalog_headers),
                compat: obj.get("compat").cloned().filter(|v| !v.is_null()),
            };
            // The api on a group key must be one tack can speak; a provider
            // file may group models under apis with no Rust adapter yet —
            // those models stay in the catalog (id parity) but are flagged
            // through the api string, same as the embedded catalog.
            if model.api.is_empty() {
                model.api = "openai-completions".to_string();
            }
            models.push(model);
        }
    }
    Some(models)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_tgz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        let plain = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&plain).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn converts_provider_groups_to_catalog_models() {
        let raw = r#"{
            "anthropic-messages": {
                "claude-test": { "name": "Claude Test", "reasoning": true, "input": ["text","image"], "cost": {"input":1,"output":2,"cacheRead":0.5,"cacheWrite":1.25}, "contextWindow": 200000, "maxTokens": 8192 }
            }
        }"#;
        let models = convert_provider_data("anthropic", "https://api.anthropic.com", raw).unwrap();
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "claude-test");
        assert_eq!(m.api, "anthropic-messages");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.base_url, "https://api.anthropic.com");
        assert!(m.reasoning);
        assert!(m.supports_images());
        assert_eq!(m.context_window, 200000);
        assert_eq!(m.cost.input, 1.0);
    }

    #[test]
    fn defaults_missing_fields() {
        let raw = r#"{ "openai-completions": { "m1": {} } }"#;
        let models = convert_provider_data("deepseek", "https://api.deepseek.com", raw).unwrap();
        assert_eq!(models[0].name, "m1");
        assert!(!models[0].reasoning);
        assert_eq!(models[0].input, vec![InputKind::Text]);
        assert_eq!(models[0].context_window, 0);
    }

    // --- supply chain: baseUrl / headers sanitization ---

    #[test]
    fn http_base_url_is_dropped_to_provider_default() {
        let raw = r#"{ "anthropic-messages": { "m1": { "baseUrl": "http://evil.example.com" } } }"#;
        let models = convert_provider_data("anthropic", "https://api.anthropic.com", raw).unwrap();
        assert_eq!(models[0].base_url, "https://api.anthropic.com");
    }

    #[test]
    fn off_host_https_base_url_is_dropped_to_provider_default() {
        // Even over https, a fetched catalog must not move a provider with a
        // built-in endpoint to a new host (API-key exfiltration vector).
        let raw =
            r#"{ "anthropic-messages": { "m1": { "baseUrl": "https://evil.example.com/v1" } } }"#;
        let models = convert_provider_data("anthropic", "https://api.anthropic.com", raw).unwrap();
        assert_eq!(models[0].base_url, "https://api.anthropic.com");
    }

    #[test]
    fn same_host_https_base_url_is_kept() {
        let raw =
            r#"{ "openai-completions": { "m1": { "baseUrl": "https://api.deepseek.com/v2" } } }"#;
        let models = convert_provider_data("deepseek", "https://api.deepseek.com", raw).unwrap();
        assert_eq!(models[0].base_url, "https://api.deepseek.com/v2");
    }

    #[test]
    fn https_base_url_kept_when_provider_has_no_builtin_endpoint() {
        let raw = r#"{ "bedrock-converse-stream": { "m1": { "baseUrl": "https://bedrock-runtime.us-east-1.amazonaws.com" } } }"#;
        let models = convert_provider_data("amazon-bedrock", "", raw).unwrap();
        assert_eq!(
            models[0].base_url,
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn auth_headers_are_stripped_benign_headers_kept() {
        let raw = r#"{ "openai-completions": { "m1": { "headers": {
            "authorization": "Bearer attacker",
            "X-Api-Key": "attacker",
            "x-goog-api-key": "attacker",
            "User-Agent": "GitHubCopilotChat/0.35.0"
        } } } }"#;
        let models = convert_provider_data(
            "github-copilot",
            "https://api.individual.githubcopilot.com",
            raw,
        )
        .unwrap();
        let headers = models[0].headers.as_ref().unwrap();
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("GitHubCopilotChat/0.35.0")
        );
        assert!(!headers.keys().any(|k| is_forbidden_catalog_header(k)));
    }

    #[test]
    fn catalog_with_only_auth_headers_ends_up_headerless() {
        let raw =
            r#"{ "openai-completions": { "m1": { "headers": { "Authorization": "Bearer x" } } } }"#;
        let models = convert_provider_data("deepseek", "https://api.deepseek.com", raw).unwrap();
        assert!(models[0].headers.is_none());
    }

    // --- supply chain: integrity / URL scheme / size caps ---

    #[test]
    fn integrity_accepts_matching_sha512() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let payload = b"fake tarball bytes";
        let b64 = base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(payload));
        verify_tarball_integrity(payload, &format!("sha512-{b64}")).unwrap();
    }

    #[test]
    fn integrity_rejects_mismatched_sha512() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let b64 =
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(b"other bytes"));
        let err =
            verify_tarball_integrity(b"fake tarball bytes", &format!("sha512-{b64}")).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn integrity_rejects_missing_sha512_and_malformed() {
        use base64::Engine as _;
        let sha1 = base64::engine::general_purpose::STANDARD.encode(b"01234567890123456789");
        let err = verify_tarball_integrity(b"x", &format!("sha1-{sha1}")).unwrap_err();
        assert!(err.to_string().contains("no sha512"), "{err}");
        assert!(verify_tarball_integrity(b"x", "sha512-!!!not-base64!!!").is_err());
    }

    #[test]
    fn url_scheme_validation() {
        assert!(validate_url_scheme("https://registry.npmjs.org/x", "registry").is_ok());
        assert!(validate_url_scheme("http://127.0.0.1:4873/x", "registry").is_ok());
        assert!(validate_url_scheme("http://localhost/x", "registry").is_ok());
        assert!(validate_url_scheme("http://[::1]:8080/x", "registry").is_ok());
        assert!(validate_url_scheme("http://evil.example.com/x", "registry").is_err());
        assert!(validate_url_scheme("ftp://registry.npmjs.org/x", "registry").is_err());
        assert!(validate_url_scheme("registry.npmjs.org/x", "registry").is_err());
    }

    #[test]
    fn push_capped_enforces_limit() {
        let mut buf = Vec::new();
        push_capped(&mut buf, b"12345", 10, "body").unwrap();
        push_capped(&mut buf, b"12345", 10, "body").unwrap();
        assert_eq!(buf.len(), 10);
        assert!(push_capped(&mut buf, b"1", 10, "body").is_err());
    }

    #[test]
    fn cap_reader_errors_beyond_limit_allows_exact_fit() {
        let data = [b'x'; 100];
        let mut r = CapReader {
            inner: &data[..],
            remaining: 10,
            what: "test",
        };
        let mut out = String::new();
        assert!(r.read_to_string(&mut out).is_err());

        let mut r = CapReader {
            inner: &data[..10],
            remaining: 10,
            what: "test",
        };
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn oversized_provider_file_is_rejected_not_truncated() {
        // A provider data file past the entry cap must fail extraction; a
        // silent take() truncation could parse as valid-but-partial JSON.
        let big = format!(
            r#"{{ "openai-completions": {{ "m1": {{ "name": "{}" }} }} }}"#,
            "x".repeat(256)
        );
        let tgz = make_tgz(&[("package/dist/providers/data/deepseek.json", big.as_bytes())]);
        let limits = ExtractLimits {
            decompressed: 1 << 20,
            entry: 64,
        };
        assert!(extract_catalog_with_limits(&tgz, limits).is_err());
        // And with a sufficient cap the same file parses fine.
        let limits = ExtractLimits {
            decompressed: 1 << 20,
            entry: 1 << 20,
        };
        let catalog = extract_catalog_with_limits(&tgz, limits).unwrap();
        assert_eq!(catalog["deepseek"].len(), 1);
    }

    #[test]
    fn decompression_cap_stops_zip_bombs() {
        // Highly compressible large payload: tiny on the wire, huge inflated.
        let big = vec![b' '; 64 * 1024];
        let tgz = make_tgz(&[("package/dist/providers/data/deepseek.json", &big)]);
        assert!(tgz.len() < 4 * 1024, "gz should compress well");
        let limits = ExtractLimits {
            decompressed: 1024,
            entry: 1 << 20,
        };
        assert!(extract_catalog_with_limits(&tgz, limits).is_err());
    }

    // --- truncation guard ---

    fn stub_catalog(providers: usize, models_per_provider: usize) -> BTreeMap<String, Vec<Model>> {
        (0..providers)
            .map(|i| {
                (
                    format!("p{i}"),
                    (0..models_per_provider)
                        .map(|j| Model {
                            id: format!("m{j}"),
                            name: String::new(),
                            api: "openai-completions".into(),
                            provider: format!("p{i}"),
                            base_url: String::new(),
                            reasoning: false,
                            thinking_level_map: None,
                            input: vec![InputKind::Text],
                            cost: Default::default(),
                            context_window: 0,
                            max_tokens: 0,
                            sampling_params: None,
                            headers: None,
                            compat: None,
                        })
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn truncation_guard_refuses_tiny_catalogs() {
        assert!(validate_fetched_catalog(&stub_catalog(19, 10)).is_err());
        assert!(validate_fetched_catalog(&stub_catalog(21, 2)).is_err()); // <50 models
        assert!(validate_fetched_catalog(&stub_catalog(0, 0)).is_err());
        let (p, m) = validate_fetched_catalog(&stub_catalog(21, 3)).unwrap();
        assert_eq!((p, m), (21, 63));
    }

    // --- extraction / cache roundtrip (pre-existing behavior) ---

    #[test]
    fn extract_catalog_reads_tarball_data_files() {
        // Build an in-memory tgz with one provider data file.
        let data = r#"{ "openai-completions": { "m1": { "name": "M1" } } }"#;
        let tarball = make_tgz(&[
            ("package/dist/providers/data/deepseek.json", data.as_bytes()),
            ("package/dist/index.js", &b"ignored"[..]),
        ]);

        let catalog = extract_catalog(&tarball).unwrap();
        let models = catalog.get("deepseek").unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");
    }

    #[test]
    fn oversized_cache_is_removed_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = catalog_cache_path(dir.path());
        std::fs::write(&path, vec![b'x'; (MAX_CACHE_BYTES + 1) as usize]).unwrap();
        assert!(load_cached_override(dir.path()).is_none());
        assert!(!path.exists(), "oversized cache must be removed");
    }

    #[test]
    fn cached_override_is_resanitized_on_load() {
        // Simulates a cache written by a build without the refresh-path
        // sanitization: off-host baseUrl and an auth header must not survive
        // the load.
        let dir = tempfile::tempdir().unwrap();
        let cache = serde_json::json!({
            "deepseek": {
                "api": "openai-completions",
                "models": [{
                    "id": "m1", "name": "M1", "api": "openai-completions",
                    "provider": "deepseek", "baseUrl": "http://evil.example.com/v1",
                    "reasoning": false, "input": ["text"],
                    "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
                    "contextWindow": 100, "maxTokens": 10,
                    "headers": {"authorization": "Bearer evil", "User-Agent": "ok"}
                }]
            }
        });
        std::fs::write(
            catalog_cache_path(dir.path()),
            serde_json::to_string(&cache).unwrap(),
        )
        .unwrap();
        assert_eq!(load_cached_override(dir.path()), Some((1, 1)));
        let models = tack_ai::providers::builtin_models("deepseek");
        let m = models.iter().find(|m| m.id == "m1").unwrap();
        assert_eq!(m.base_url, "https://api.deepseek.com");
        let headers = m.headers.as_ref().unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("User-Agent").map(String::as_str), Some("ok"));
    }

    #[test]
    #[ignore = "network: run with --ignored when online"]
    fn refresh_from_real_npm_registry() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let s = rt
            .block_on(refresh_from_npm(
                dir.path(),
                std::time::Duration::from_secs(60),
            ))
            .expect("refresh");
        assert!(s.providers >= 20, "providers: {}", s.providers);
        assert!(s.models >= 50, "models: {}", s.models);
        let anthropic = tack_ai::providers::builtin_models("anthropic");
        assert!(!anthropic.is_empty());
        assert!(anthropic.iter().all(|m| m.provider == "anthropic"));
    }

    #[test]
    fn cached_override_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let catalog: BTreeMap<String, Vec<Model>> = BTreeMap::from([(
            "anthropic".to_string(),
            vec![Model {
                id: "x".into(),
                name: "X".into(),
                api: "anthropic-messages".into(),
                provider: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                reasoning: false,
                thinking_level_map: None,
                input: vec![InputKind::Text],
                cost: Default::default(),
                context_window: 100,
                max_tokens: 10,
                sampling_params: None,
                headers: None,
                compat: None,
            }],
        )]);
        persist(
            dir.path(),
            &catalog,
            &RefreshSummary {
                version: "0.0.0-test".into(),
                providers: 1,
                models: 1,
            },
        )
        .unwrap();
        assert_eq!(load_cached_override(dir.path()), Some((1, 1)));
        assert_eq!(tack_ai::providers::builtin_models("anthropic").len(), 1);
        clear_override(dir.path());
        assert!(load_cached_override(dir.path()).is_none());
        // No temp files left behind by the atomic write.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stale temp files: {leftovers:?}");
    }
}
