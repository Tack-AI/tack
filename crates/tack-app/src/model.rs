//! Model resolution: custom providers from models.json first, then the
//! built-in registry + embedded catalog (aligned with TS pi's provider set).

use std::path::Path;

use tack_ai::providers::{
    builtin_model, builtin_models, builtin_provider, default_model_for, load_custom_providers,
};
use tack_ai::{InputKind, Model, ModelCost};

/// Enforce managed provider/model locks. Call after resolve_model at every
/// entry point; returns the (unchanged) model or an error naming the lock.
pub fn enforce_locked(settings: &crate::settings::Settings, model: &Model) -> Result<(), String> {
    enforce_locked_values(
        settings.locked_provider.as_deref(),
        settings.locked_model.as_deref(),
        model,
    )
}

/// Lock check against raw managed lock values, for call sites that don't
/// hold a full Settings (SubagentTool child-model resolution, fallback
/// chains). Same error wording as [`enforce_locked`].
pub fn enforce_locked_values(
    locked_provider: Option<&str>,
    locked_model: Option<&str>,
    model: &Model,
) -> Result<(), String> {
    if let Some(locked) = locked_provider
        && model.provider != *locked
    {
        return Err(format!(
            "provider {} is not allowed — locked to {locked} by managed settings",
            model.provider
        ));
    }
    if let Some(locked) = locked_model
        && model.id != *locked
    {
        return Err(format!(
            "model {} is not allowed — locked to {locked} by managed settings",
            model.id
        ));
    }
    Ok(())
}

/// Managed lockedProvider/lockedModel read straight from the managed
/// settings file. The managed path is cwd-independent (settings.rs), so
/// call sites without a loaded Settings (fallback resolution) still
/// enforce the lock.
fn managed_locks() -> (Option<String>, Option<String>) {
    let raw = std::fs::read_to_string(crate::settings::managed_settings_path())
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok());
    let get = |key: &str| {
        raw.as_ref()
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    (get("lockedProvider"), get("lockedModel"))
}

/// Resolve the fallback chain (settings `fallbackModels`, "provider/id"
/// entries) into concrete models, skipping the active model, unresolvable
/// entries, and entries that violate managed provider/model locks (warned,
/// not fatal).
pub fn resolve_fallback_models(
    entries: &[String],
    current: &Model,
    agent_dir: &Path,
) -> Vec<Model> {
    let (locked_provider, locked_model) = managed_locks();
    let mut out = Vec::new();
    for entry in entries {
        let Some((provider, id)) = entry.split_once('/') else {
            tracing::warn!("fallbackModels entry {entry:?} is not provider/id; skipped");
            continue;
        };
        match resolve_model(provider, Some(id), agent_dir) {
            Ok(model) if model.provider == current.provider && model.id == current.id => {}
            Ok(model) => {
                if let Err(e) = enforce_locked_values(
                    locked_provider.as_deref(),
                    locked_model.as_deref(),
                    &model,
                ) {
                    tracing::warn!("fallback model {entry:?} skipped: {e}");
                    continue;
                }
                out.push(model);
            }
            Err(e) => tracing::warn!("fallback model {entry:?} unavailable: {e}"),
        }
    }
    out
}

/// Resolve a model from a provider name and optional model id.
///
/// Order: custom providers (`models.json`) → built-in catalog → error.
/// The Anthropic default path honors `ANTHROPIC_BASE_URL` / `ANTHROPIC_MODEL`
/// for proxy setups (TS pi does not; this is a deliberate tack extension).
pub fn resolve_model(
    provider_name: &str,
    model_id: Option<&str>,
    agent_dir: &Path,
) -> Result<Model, String> {
    // 1. Custom providers from models.json. A custom provider whose id
    // matches a built-in merges: catalog models are patched by sparse
    // overrides (same id) and extended by new entries; provider-level fields
    // (baseUrl/api/compat) from models.json win when the provider id matches.
    let customs = load_custom_providers(agent_dir);
    let runtime = tack_ai::providers::runtime_providers();
    // CodeBuddy's auth lives entirely in the CLI session (`codebuddy
    // login`), so a same-id custom entry — a models.json / extension shim
    // from the pre-native-provider era, api openai-completions, usually
    // without apiKey — must NOT shadow the builtin CLI provider: every
    // request would be stranded on an HTTP adapter failing with "No API key
    // for provider: codebuddy". The builtin always wins for this id; the
    // custom entry is only consulted for per-model metadata overrides (see
    // the builtin path below).
    if provider_name == tack_ai::codebuddy::PROVIDER_ID
        && customs
            .iter()
            .chain(runtime.iter())
            .any(|c| c.id == provider_name)
    {
        tracing::warn!(
            "custom provider 'codebuddy' (models.json / extension): provider-level fields \
             (api/baseUrl/apiKey) are ignored — the built-in CLI provider owns this id; \
             per-model metadata is applied as overrides"
        );
    }
    let custom = customs
        .iter()
        .chain(runtime.iter())
        .find(|c| c.id == provider_name && c.id != tack_ai::codebuddy::PROVIDER_ID);
    if let Some(custom) = custom {
        let mut merged: Vec<Model> = custom.models.clone();
        if let Some(def) = builtin_provider(provider_name) {
            for mut base in builtin_models(def.id).to_vec() {
                if let Some(pos) = merged.iter().position(|m| m.id == base.id) {
                    // Patch the custom entry's unset fields from the catalog,
                    // but keep custom/api base fields.
                    let sparse = &custom.sparse[pos];
                    let mut patched = base.clone();
                    sparse.apply_to(&mut patched);
                    // Custom provider-level api/baseUrl win over the catalog.
                    patched.api = merged[pos].api.clone();
                    patched.base_url = merged[pos].base_url.clone();
                    merged[pos] = patched;
                } else {
                    base.api = def.api.to_string();
                    merged.push(base);
                }
            }
        }
        return match model_id {
            Some(id) => merged
                .into_iter()
                .find(|m| m.id == id)
                .ok_or_else(|| format!("model {id} not found in provider {provider_name}")),
            None => merged
                .into_iter()
                .next()
                .ok_or_else(|| format!("custom provider {provider_name} has no models")),
        };
    }

    // 2. Built-in registry.
    let Some(def) = builtin_provider(provider_name) else {
        let available: Vec<&str> = tack_ai::providers::BUILTIN_PROVIDERS
            .iter()
            .map(|p| p.id)
            .collect();
        return Err(format!(
            "unknown provider {provider_name:?}. Built-in providers: {}",
            available.join(", ")
        ));
    };

    let mut model = match model_id {
        Some(id) => builtin_model(def.id, id).unwrap_or_else(|| bare_model(def.id, id)),
        None => default_model_for(def.id).unwrap_or_else(|| bare_model(def.id, "")),
    };

    // CodeBuddy: the CLI's model list carries no capabilities, so metadata
    // is estimated (and learned from result.modelUsage at runtime). A
    // same-id custom provider entry can't shadow the builtin (see above)
    // but its per-model fields ARE honored as overrides — the manual fix
    // when an estimate is wrong (contextWindow / maxTokens / reasoning /
    // input / thinkingLevelMap).
    if def.id == tack_ai::codebuddy::PROVIDER_ID
        && let Some(custom) = customs
            .iter()
            .chain(runtime.iter())
            .find(|c| c.id == def.id)
        && let Some(pos) = custom.models.iter().position(|m| m.id == model.id)
    {
        let api = model.api.clone();
        let base_url = model.base_url.clone();
        custom.sparse[pos].apply_to(&mut model);
        model.api = api;
        model.base_url = base_url;
    }

    // Local providers (Ollama, llama.cpp): honor OLLAMA_HOST / LLAMA_CPP_HOST
    // overrides even for catalog misses, and give bare fallbacks the local
    // compat quirks (no developer role / store).
    if tack_ai::local_providers::is_local_provider(def.id) {
        model.base_url = tack_ai::local_providers::base_url(def.id);
        if model.compat.is_none() {
            model.compat = Some(tack_ai::local_providers::local_compat());
        }
    }

    // 3. Anthropic proxy convenience: ANTHROPIC_BASE_URL / ANTHROPIC_MODEL.
    if def.id == "anthropic" {
        if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL")
            && !base.is_empty()
        {
            model.base_url = base.trim_end_matches('/').to_string();
        }
        if model_id.is_none()
            && let Ok(env_model) = std::env::var("ANTHROPIC_MODEL")
            && !env_model.is_empty()
        {
            // Claude Code aliases like `k3[1m]` are stripped.
            let stripped = env_model
                .split('[')
                .next()
                .unwrap_or(&env_model)
                .to_string();
            model.id = stripped.clone();
            model.name = stripped;
        }
    }

    Ok(model)
}

/// A model with just an id (catalog miss or empty catalog): fields come from
/// the provider def, windows/cost left at zero.
fn bare_model(provider_id: &str, model_id: &str) -> Model {
    let def = builtin_provider(provider_id);
    Model {
        id: model_id.to_string(),
        name: model_id.to_string(),
        api: def
            .map(|d| d.api)
            .unwrap_or("anthropic-messages")
            .to_string(),
        provider: provider_id.to_string(),
        base_url: def.map(|d| d.base_url).unwrap_or("").to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![InputKind::Text, InputKind::Image],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 64_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// Resolve an API key for a provider: explicit flag value > stored credential
/// (auth.json) > custom provider apiKey > the provider's env vars.
pub fn resolve_api_key(
    provider_name: &str,
    explicit: Option<String>,
    agent_dir: &Path,
) -> Option<String> {
    if explicit.is_some() {
        return explicit;
    }
    if let Some(key) = crate::auth::get_api_key(agent_dir, provider_name) {
        return Some(key);
    }
    for custom in load_custom_providers(agent_dir)
        .into_iter()
        .chain(tack_ai::providers::runtime_providers())
    {
        if custom.id == provider_name && custom.api_key.is_some() {
            return custom.api_key;
        }
    }
    // Local providers need no credential; the OpenAI-completions adapter
    // requires a non-empty bearer, so hand it a placeholder (the same
    // "ollama" convention as TS pi's models.json example).
    if tack_ai::local_providers::is_local_provider(provider_name) {
        return Some("ollama".to_string());
    }
    let def = builtin_provider(provider_name)?;
    for var in def.env_keys {
        if let Ok(value) = std::env::var(var)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    tack_ai::env_keys::get_env_api_key(def.id)
}

/// Cheap availability check for the model picker (TS Models.getAvailable):
/// true when the provider can produce credentials — a stored credential
/// (auth.json api key or OAuth), an inline custom-provider api key, an env
/// var, or ambient cloud auth (Bedrock AWS chain, Vertex ADC).
pub fn provider_has_auth(agent_dir: &std::path::Path, provider: &str) -> bool {
    if crate::auth::get_credential(agent_dir, provider).is_some() {
        return true;
    }
    if resolve_api_key(provider, None, agent_dir).is_some() {
        return true;
    }
    // CodeBuddy: auth is CLI-side (`codebuddy login`); the provider is
    // ready whenever the CLI binary is installed.
    if provider == "codebuddy" {
        return tack_ai::codebuddy::cli_available();
    }
    match provider {
        // Bedrock's chain also resolves from ambient AWS config (TS
        // resolveProviderAuth → provider.auth.apiKey.check equivalents).
        "amazon-bedrock" => {
            std::env::var_os("AWS_PROFILE").is_some()
                || std::env::var_os("AWS_ACCESS_KEY_ID").is_some()
                || std::env::var_os("AWS_BEARER_TOKEN_BEDROCK").is_some()
                || std::env::var_os("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
                || std::env::var_os("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
                || std::env::var_os("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
                || dirs::home_dir().is_some_and(|h| {
                    h.join(".aws").join("credentials").exists()
                        || h.join(".aws").join("config").exists()
                })
        }
        "google-vertex" => {
            std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").is_some()
                || dirs::home_dir().is_some_and(|h| {
                    h.join(".config")
                        .join("gcloud")
                        .join("application_default_credentials.json")
                        .exists()
                })
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Request-time auth (OAuth-aware)
// ---------------------------------------------------------------------------

/// Refresh when the token expires within this floor (TS
/// `DEFAULT_OAUTH_MINIMUM_VALIDITY_MS`).
const OAUTH_MINIMUM_VALIDITY_MS: i64 = 5 * 60 * 1000;
/// Timeout for a single refresh HTTP call (TS `DEFAULT_OAUTH_REFRESH_TIMEOUT_MS`).
const OAUTH_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Resolve request-time auth for a provider. Order: explicit flag → stored
/// OAuth credential (proactively refreshed per call) → stored API key →
/// custom provider apiKey → env vars. Never fails outright; an unresolvable
/// provider yields an empty `StaticAuth` and the adapter reports the missing
/// key in-band.
pub fn resolve_auth(
    provider_name: &str,
    explicit: Option<String>,
    agent_dir: &Path,
) -> std::sync::Arc<dyn tack_ai::oauth::AuthResolver> {
    use tack_ai::oauth::{AuthResolver, StaticAuth};
    if let Some(key) = explicit {
        return std::sync::Arc::new(StaticAuth::from(Some(key)));
    }
    if let Some(entry) = crate::auth::get_credential(agent_dir, provider_name)
        && entry.get("type").and_then(serde_json::Value::as_str) == Some("oauth")
        && tack_ai::oauth::oauth_flow(provider_name).is_some()
    {
        let resolver: std::sync::Arc<dyn AuthResolver> = std::sync::Arc::new(OAuthResolver {
            provider: provider_name.to_string(),
            agent_dir: agent_dir.to_path_buf(),
        });
        return resolver;
    }
    std::sync::Arc::new(StaticAuth::from(resolve_api_key(
        provider_name,
        None,
        agent_dir,
    )))
}

/// Stored-OAuth resolver: refreshes proactively under a per-provider
/// double-checked lock (another process may have refreshed first).
#[derive(Debug)]
struct OAuthResolver {
    provider: String,
    agent_dir: std::path::PathBuf,
}

impl tack_ai::oauth::AuthResolver for OAuthResolver {
    fn resolve(
        &self,
    ) -> tack_ai::oauth::BoxFuture<'static, Result<tack_ai::oauth::ResolvedAuth, String>> {
        let provider = self.provider.clone();
        let agent_dir = self.agent_dir.clone();
        Box::pin(async move { resolve_oauth(&provider, &agent_dir).await })
    }
}

fn oauth_locks() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

async fn resolve_oauth(
    provider: &str,
    agent_dir: &Path,
) -> Result<tack_ai::oauth::ResolvedAuth, String> {
    let flow = tack_ai::oauth::oauth_flow(provider)
        .ok_or_else(|| format!("no OAuth flow for provider {provider}"))?;
    // Credential reads/writes do synchronous keychain/libsecret IPC and
    // file I/O; a hung credential service (e.g. a macOS Keychain access
    // prompt nobody answers) must not park an async worker thread — that
    // wedges every timeout/cancellation scheduled on it. Run them on the
    // blocking pool instead.
    async fn read_credential(
        agent_dir: &Path,
        provider: &str,
    ) -> Result<tack_ai::oauth::OAuthCredential, String> {
        let agent_dir = agent_dir.to_path_buf();
        let provider = provider.to_string();
        tokio::task::spawn_blocking(move || {
            crate::auth::get_oauth(&agent_dir, &provider).ok_or_else(|| {
                format!(
                    "no stored OAuth credential for {provider}; run `tack login --provider {provider}`"
                )
            })
        })
        .await
        .map_err(|e| format!("credential read task failed: {e}"))?
    }
    let near_expiry = |c: &tack_ai::oauth::OAuthCredential| {
        c.expires_within(std::time::Duration::from_millis(
            OAUTH_MINIMUM_VALIDITY_MS as u64,
        ))
    };

    let mut credential = read_credential(agent_dir, provider).await?;
    if near_expiry(&credential) {
        let lock = {
            oauth_locks()
                .lock()
                .expect("oauth locks poisoned")
                .entry(provider.to_string())
                .or_default()
                .clone()
        };
        let _guard = lock.lock().await;
        // Re-read under the lock: another process/request may have refreshed.
        credential = read_credential(agent_dir, provider).await?;
        if near_expiry(&credential) {
            let client = reqwest::Client::new();
            let fresh = tokio::time::timeout(OAUTH_REFRESH_TIMEOUT, flow.refresh(&client, &credential))
                .await
                .map_err(|_| format!("OAuth refresh for {provider} timed out"))?
                .map_err(|e| format!("OAuth refresh for {provider} failed: {e}; run `tack login --provider {provider}`"))?;
            let agent_dir = agent_dir.to_path_buf();
            let provider_owned = provider.to_string();
            let fresh_for_write = fresh.clone();
            tokio::task::spawn_blocking(move || {
                crate::auth::set_oauth(&agent_dir, &provider_owned, &fresh_for_write)
            })
            .await
            .map_err(|e| format!("credential write task failed: {e}"))?
            .map_err(|e| format!("failed to persist refreshed credential: {e}"))?;
            credential = fresh;
        }
    }
    Ok(flow.to_auth(&credential))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(unsafe_code)] // std::env::set_var in tests

    use super::*;

    // Env vars are process-global; serialize the tests that touch them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Managed locks: enforced at model resolution entry points via raw
    /// lock values (SubagentTool child models) — cross-provider and
    /// cross-model picks are rejected naming the lock; the exact locked
    /// provider/model passes.
    #[test]
    fn enforce_locked_values_rejects_unlocked_models() {
        let dir = tempfile::tempdir().unwrap();
        let anthropic = resolve_model("anthropic", Some("claude-haiku-4-5"), dir.path()).unwrap();
        let openai = resolve_model("openai", Some("gpt-4.1"), dir.path()).unwrap();
        // No locks: everything passes.
        assert!(enforce_locked_values(None, None, &openai).is_ok());
        // Provider lock.
        let err = enforce_locked_values(Some("anthropic"), None, &openai).unwrap_err();
        assert!(err.contains("locked to anthropic"), "{err}");
        assert!(enforce_locked_values(Some("anthropic"), None, &anthropic).is_ok());
        // Model lock.
        let sonnet = resolve_model("anthropic", Some("claude-sonnet-4-5"), dir.path()).unwrap();
        let err = enforce_locked_values(None, Some("claude-haiku-4-5"), &sonnet).unwrap_err();
        assert!(err.contains("locked to claude-haiku-4-5"), "{err}");
        assert!(enforce_locked_values(None, Some("claude-haiku-4-5"), &anthropic).is_ok());
    }

    /// Regression: fallback models must not route around a managed
    /// lockedProvider — entries from other providers are skipped (warned),
    /// same-provider entries survive.
    #[test]
    fn fallback_models_respect_managed_locks() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed-settings.json");
        std::fs::write(&managed, r#"{"lockedProvider":"anthropic"}"#).unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TACK_MANAGED_SETTINGS", &managed);
        }
        let current = resolve_model("anthropic", Some("claude-haiku-4-5"), dir.path()).unwrap();
        let entries = vec![
            "openai/gpt-4.1".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        ];
        let out = resolve_fallback_models(&entries, &current, dir.path());
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TACK_MANAGED_SETTINGS");
        }
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].provider, "anthropic");
        assert_eq!(out[0].id, "claude-sonnet-4-5");
    }

    /// Without managed settings, fallback resolution is unchanged.
    #[test]
    fn fallback_models_without_locks_pass_through() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Point at a nonexistent managed file: no locks.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TACK_MANAGED_SETTINGS", dir.path().join("nope.json"));
        }
        let current = resolve_model("anthropic", Some("claude-haiku-4-5"), dir.path()).unwrap();
        let entries = vec!["openai/gpt-4.1".to_string()];
        let out = resolve_fallback_models(&entries, &current, dir.path());
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TACK_MANAGED_SETTINGS");
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provider, "openai");
    }

    /// CodeBuddy is a built-in provider whose auth lives entirely in the
    /// CLI (`codebuddy login`): registered without env keys, and its
    /// has-auth predicate mirrors CLI presence.
    #[test]
    fn codebuddy_provider_registered() {
        let def = tack_ai::providers::builtin_provider("codebuddy").expect("codebuddy def");
        assert_eq!(def.api, tack_ai::codebuddy::CODEBUDDY_API);
        assert!(def.env_keys.is_empty());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            provider_has_auth(dir.path(), "codebuddy"),
            tack_ai::codebuddy::cli_available()
        );
    }

    /// A same-id custom provider must not shadow the builtin CodeBuddy CLI
    /// provider: its auth is CLI-side, so an HTTP api override strands every
    /// request on an HTTP adapter failing with "No API key for provider:
    /// codebuddy". Per-model metadata, however, IS honored as an override
    /// (the CLI's model list carries no capabilities; estimates can be
    /// wrong).
    #[test]
    fn codebuddy_custom_provider_does_not_shadow_builtin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{"providers":{"codebuddy":{"baseUrl":"http://127.0.0.1:1/v1","api":"openai-completions","models":[{"id":"gpt-5","contextWindow":1048576,"maxTokens":64000}]}}}"#,
        )
        .unwrap();
        for model_id in [Some("gpt-5"), None] {
            let model = resolve_model("codebuddy", model_id, dir.path()).unwrap();
            assert_eq!(
                model.api,
                tack_ai::codebuddy::CODEBUDDY_API,
                "model_id {model_id:?}"
            );
            assert_eq!(model.provider, "codebuddy");
            assert!(
                model.base_url.is_empty(),
                "custom baseUrl must not leak into the builtin: {model:?}"
            );
        }
        // Metadata overrides apply (the None default is a different model,
        // so only the explicit gpt-5 resolution is patched).
        let model = resolve_model("codebuddy", Some("gpt-5"), dir.path()).unwrap();
        assert_eq!(model.context_window, 1_048_576);
        assert_eq!(model.max_tokens, 64_000);
    }

    /// Local providers need no API key: resolution always succeeds with a
    /// placeholder, and the provider counts as "has auth" for the picker.
    #[test]
    fn local_providers_need_no_api_key() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["ollama", "llama.cpp"] {
            assert_eq!(
                resolve_api_key(id, None, dir.path()).as_deref(),
                Some("ollama"),
                "provider {id}"
            );
            assert!(provider_has_auth(dir.path(), id), "provider {id}");
        }
    }

    /// models.json can still override a local provider (its apiKey wins over
    /// the placeholder; sparse contextWindow patches discovered/merged models).
    #[test]
    fn local_provider_models_json_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{"providers":{"ollama":{"baseUrl":"http://localhost:11434/v1","api":"openai-completions","apiKey":"custom","models":[{"id":"qwen3:32b","contextWindow":131072}]}}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_api_key("ollama", None, dir.path()).as_deref(),
            Some("custom")
        );
        let model = resolve_model("ollama", Some("qwen3:32b"), dir.path()).unwrap();
        assert_eq!(model.context_window, 131072);
    }

    /// OLLAMA_HOST / LLAMA_CPP_HOST override the request base URL even for
    /// models not in any catalog (bare fallback path).
    #[test]
    fn local_provider_host_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OLLAMA_HOST", "gpu-box:11434");
            std::env::set_var("LLAMA_CPP_HOST", "http://127.0.0.1:9999/");
        }
        let model = resolve_model("ollama", Some("llama3.1:8b"), dir.path()).unwrap();
        assert_eq!(model.base_url, "http://gpu-box:11434/v1");
        assert_eq!(model.api, "openai-completions");
        assert!(model.compat.is_some());
        let model = resolve_model("llama.cpp", None, dir.path()).unwrap();
        assert_eq!(model.base_url, "http://127.0.0.1:9999/v1");
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("OLLAMA_HOST");
            std::env::remove_var("LLAMA_CPP_HOST");
        }
    }
}
