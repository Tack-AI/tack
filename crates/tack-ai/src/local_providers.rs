//! Local inference providers (Ollama, llama.cpp): zero-config use of models
//! running on localhost. No API key is needed; the servers are probed with a
//! short timeout at startup and their model catalogs are discovered at
//! runtime (`GET {host}/api/tags` for Ollama, `GET {host}/v1/models` for
//! llama.cpp), then injected into the built-in catalog via
//! [`crate::providers::install_local_models`].
//!
//! Discovery never blocks startup meaningfully: probes run concurrently with
//! a ~500ms timeout and every failure is silent (no local server is the
//! common case). Results are cached for the process lifetime. Offline mode
//! (`TACK_OFFLINE`; the app's `--offline` flag skips calling [`refresh`])
//! disables probing entirely.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use serde::Deserialize;

use crate::types::{InputKind, Model};

/// Provider id for Ollama (built-in registry entry).
pub const OLLAMA: &str = "ollama";
/// Provider id for llama.cpp (matches TS pi's `llama.cpp` provider id).
pub const LLAMA_CPP: &str = "llama.cpp";

const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";
const DEFAULT_LLAMA_CPP_HOST: &str = "http://localhost:8080";

/// Context window assumed when the server does not report one. Per-model
/// `contextWindow` in models.json overrides this (sparse merge).
pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;
/// Max output tokens assumed for discovered local models.
pub const DEFAULT_MAX_TOKENS: u32 = 8_192;

/// Probe timeout: localhost servers answer instantly or not at all.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Whether the provider id is a zero-config local provider.
pub fn is_local_provider(id: &str) -> bool {
    matches!(id, OLLAMA | LLAMA_CPP)
}

/// `TACK_OFFLINE` truthiness (same convention as tack-app's tools manager).
pub fn offline_mode() -> bool {
    std::env::var("TACK_OFFLINE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

/// Normalize a host override: add a default scheme, drop trailing `/` and a
/// trailing `/v1` (the base URL helpers re-add it).
fn normalize_host(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// Server host (`scheme://host:port`, no path) honoring env overrides:
/// `OLLAMA_HOST` for Ollama, `LLAMA_CPP_HOST` for llama.cpp.
pub fn host(provider_id: &str) -> String {
    let (var, default) = match provider_id {
        OLLAMA => ("OLLAMA_HOST", DEFAULT_OLLAMA_HOST),
        LLAMA_CPP => ("LLAMA_CPP_HOST", DEFAULT_LLAMA_CPP_HOST),
        _ => return String::new(),
    };
    std::env::var(var)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| normalize_host(&v))
        .unwrap_or_else(|| default.to_string())
}

/// OpenAI-compatible base URL (`{host}/v1`) for a local provider.
pub fn base_url(provider_id: &str) -> String {
    format!("{}/v1", host(provider_id))
}

/// Shell hint shown when the server is not running.
pub fn start_hint(provider_id: &str) -> &'static str {
    match provider_id {
        OLLAMA => "ollama serve",
        LLAMA_CPP => "llama-server --port 8080",
        _ => "",
    }
}

/// Compat quirks for local OpenAI-completions servers (same shape TS pi uses
/// for llama.cpp): no developer role, no store, `max_tokens` field.
pub fn local_compat() -> serde_json::Value {
    serde_json::json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "supportsUsageInStreaming": true,
        "supportsStrictMode": false,
        "maxTokensField": "max_tokens",
    })
}

// ---------------------------------------------------------------------------
// Discovery status (process-wide cache)
// ---------------------------------------------------------------------------

/// Outcome of the last probe of a local provider.
#[derive(Clone, Debug)]
pub struct LocalProviderStatus {
    pub id: String,
    /// Resolved host (after env overrides) that was probed.
    pub host: String,
    /// The server answered; `model_count` models were discovered.
    pub running: bool,
    pub model_count: usize,
}

static STATUSES: RwLock<BTreeMap<String, LocalProviderStatus>> = RwLock::new(BTreeMap::new());
static PROBED: AtomicBool = AtomicBool::new(false);

fn read_statuses() -> RwLockReadGuard<'static, BTreeMap<String, LocalProviderStatus>> {
    STATUSES.read().unwrap_or_else(|e| e.into_inner())
}

fn write_statuses() -> RwLockWriteGuard<'static, BTreeMap<String, LocalProviderStatus>> {
    STATUSES.write().unwrap_or_else(|e| e.into_inner())
}

/// Last probe outcome for a provider (`None` = never probed, e.g. offline).
pub fn status(provider_id: &str) -> Option<LocalProviderStatus> {
    read_statuses().get(provider_id).cloned()
}

/// Record a probe outcome and inject discovered models into the catalog.
fn record(id: &str, host: &str, models: Option<Vec<Model>>) {
    let (running, count) = match &models {
        Some(models) => (true, models.len()),
        None => (false, 0),
    };
    if let Some(models) = models {
        crate::providers::install_local_models(id, models);
    }
    write_statuses().insert(
        id.to_string(),
        LocalProviderStatus {
            id: id.to_string(),
            host: host.to_string(),
            running,
            model_count: count,
        },
    );
    if running {
        tracing::debug!("local provider {id}: {count} model(s) at {host}");
    }
}

/// Probe both local providers once (concurrently, short timeout) and inject
/// discovered models into the built-in catalog. Silent on failure; a no-op
/// on subsequent calls and in offline mode (`TACK_OFFLINE`).
pub async fn refresh() {
    if PROBED.swap(true, Ordering::SeqCst) {
        return;
    }
    if offline_mode() {
        return;
    }
    let ollama_host = host(OLLAMA);
    let llama_host = host(LLAMA_CPP);
    let (ollama, llama) = tokio::join!(probe_ollama(&ollama_host), probe_llama_cpp(&llama_host));
    record(OLLAMA, &ollama_host, ollama);
    record(LLAMA_CPP, &llama_host, llama);
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// GET a URL with the probe timeout; `None` on any failure (silent).
async fn get_json(url: &str) -> Option<String> {
    crate::tls::ensure_ring_provider();
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.text().await.ok()
}

/// Probe an Ollama server: `GET {host}/api/tags`. `None` when the server is
/// unreachable, times out, or answers with an unexpected body — all silent.
pub async fn probe_ollama(host: &str) -> Option<Vec<Model>> {
    let url = format!("{}/api/tags", host.trim_end_matches('/'));
    let body = get_json(&url).await?;
    parse_ollama_tags(&body)
}

/// Probe a llama.cpp server: `GET {host}/v1/models`. `None` on any failure.
pub async fn probe_llama_cpp(host: &str) -> Option<Vec<Model>> {
    let url = format!("{}/v1/models", host.trim_end_matches('/'));
    let body = get_json(&url).await?;
    parse_openai_models(&body, LLAMA_CPP)
}

#[derive(Debug, Deserialize)]
struct OllamaTags {
    #[serde(default)]
    models: Vec<OllamaTag>,
}

#[derive(Debug, Deserialize)]
struct OllamaTag {
    name: String,
    #[serde(default)]
    details: Option<OllamaDetails>,
}

#[derive(Debug, Deserialize)]
struct OllamaDetails {
    #[serde(default)]
    parameter_size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelList {
    #[serde(default)]
    data: Vec<OpenAiModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelEntry {
    id: String,
}

fn local_model(provider: &str, id: String, name: String) -> Model {
    Model {
        id,
        name,
        api: "openai-completions".to_string(),
        provider: provider.to_string(),
        base_url: base_url(provider),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: Default::default(),
        context_window: DEFAULT_CONTEXT_WINDOW,
        max_tokens: DEFAULT_MAX_TOKENS,
        sampling_params: None,
        headers: None,
        compat: Some(local_compat()),
    }
}

/// Parse an Ollama `/api/tags` body into catalog models (sorted by id).
/// Parameter sizes from `details` are folded into the display name.
fn parse_ollama_tags(body: &str) -> Option<Vec<Model>> {
    let tags: OllamaTags = serde_json::from_str(body).ok()?;
    let mut models: Vec<Model> = tags
        .models
        .into_iter()
        .map(|tag| {
            let size = tag.details.and_then(|d| d.parameter_size);
            let name = match size {
                Some(size) if !size.is_empty() => format!("{} ({size})", tag.name),
                _ => tag.name.clone(),
            };
            local_model(OLLAMA, tag.name, name)
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Some(models)
}

/// Parse an OpenAI `/v1/models` body into catalog models (sorted by id).
fn parse_openai_models(body: &str, provider: &str) -> Option<Vec<Model>> {
    let list: OpenAiModelList = serde_json::from_str(body).ok()?;
    let mut models: Vec<Model> = list
        .data
        .into_iter()
        .map(|entry| local_model(provider, entry.id.clone(), entry.id))
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Some(models)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::await_holding_lock)]
    #![allow(unsafe_code)] // std::env::set_var in tests

    use super::*;

    // Env vars are process-global; serialize the tests that touch them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn local_provider_ids() {
        assert!(is_local_provider("ollama"));
        assert!(is_local_provider("llama.cpp"));
        assert!(!is_local_provider("openai"));
    }

    #[test]
    fn host_defaults_and_normalization() {
        let _guard = ENV_LOCK.lock().unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("OLLAMA_HOST");
            std::env::remove_var("LLAMA_CPP_HOST");
        }
        assert_eq!(host(OLLAMA), "http://localhost:11434");
        assert_eq!(base_url(OLLAMA), "http://localhost:11434/v1");
        assert_eq!(host(LLAMA_CPP), "http://localhost:8080");
        assert_eq!(base_url(LLAMA_CPP), "http://localhost:8080/v1");

        // OLLAMA_HOST is conventionally bare host:port; a trailing /v1 or /
        // must not double up.
        for (raw, want) in [
            ("localhost:1234", "http://localhost:1234"),
            ("http://localhost:1234/", "http://localhost:1234"),
            ("http://localhost:1234/v1", "http://localhost:1234"),
            ("https://gpu-box:11434", "https://gpu-box:11434"),
        ] {
            assert_eq!(normalize_host(raw), want, "input {raw}");
        }
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OLLAMA_HOST", "gpu-box:11434");
        }
        assert_eq!(host(OLLAMA), "http://gpu-box:11434");
        assert_eq!(base_url(OLLAMA), "http://gpu-box:11434/v1");
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("OLLAMA_HOST");
        }
    }

    #[test]
    fn offline_mode_truthiness() {
        let _guard = ENV_LOCK.lock().unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TACK_OFFLINE");
        }
        assert!(!offline_mode());
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TACK_OFFLINE", "1");
        }
        assert!(offline_mode());
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TACK_OFFLINE", "0");
        }
        assert!(!offline_mode());
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TACK_OFFLINE");
        }
    }

    /// Offline mode skips probing entirely: refresh is a no-op that records
    /// no statuses and installs no models.
    #[tokio::test]
    async fn refresh_skips_probing_when_offline() {
        let _guard = ENV_LOCK.lock().unwrap();
        PROBED.store(false, Ordering::SeqCst);
        write_statuses().clear();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TACK_OFFLINE", "1");
        }
        refresh().await;
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TACK_OFFLINE");
        }
        assert!(status(OLLAMA).is_none());
        assert!(status(LLAMA_CPP).is_none());
        assert!(crate::providers::builtin_models(OLLAMA).is_empty());
    }

    #[test]
    fn ollama_display_name_includes_parameter_size() {
        let body = r#"{"models":[
            {"name":"qwen3:32b","details":{"parameter_size":"32.8B"}},
            {"name":"llama3.1:8b","details":{"parameter_size":"8.0B"}},
            {"name":"custom-nosize","details":{}}
        ]}"#;
        let models = parse_ollama_tags(body).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "custom-nosize");
        assert_eq!(models[0].name, "custom-nosize");
        assert_eq!(models[1].name, "llama3.1:8b (8.0B)");
        assert_eq!(models[2].name, "qwen3:32b (32.8B)");
        for m in &models {
            assert_eq!(m.provider, "ollama");
            assert_eq!(m.api, "openai-completions");
            assert_eq!(m.context_window, DEFAULT_CONTEXT_WINDOW);
            assert!(m.compat.is_some());
        }
        assert!(parse_ollama_tags("not json").is_none());
    }

    #[test]
    fn openai_models_parse() {
        let body = r#"{"object":"list","data":[
            {"id":"models/ggml-model.gguf","object":"model"},
            {"id":"b-model","object":"model"}
        ]}"#;
        let models = parse_openai_models(body, LLAMA_CPP).unwrap();
        assert_eq!(models[0].id, "b-model");
        assert_eq!(models[1].id, "models/ggml-model.gguf");
        assert_eq!(models[1].provider, "llama.cpp");
        assert!(parse_openai_models("{} garbage", LLAMA_CPP).is_none());
    }
}
