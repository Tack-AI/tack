//! OAuth support: credential types, PKCE, RFC 8628 device-code polling, and
//! per-provider token exchange/refresh. Port of `packages/ai/src/auth/`.
//!
//! Login orchestration (loopback callback server, browser open, manual paste)
//! lives in tack-app (`oauth_login.rs`); this module is headless and testable.

use std::collections::BTreeMap;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod device;
pub mod pkce;
pub mod providers;

pub use device::{DeviceAuthorization, poll_device_code};
pub use pkce::{challenge_s256, generate_verifier};

/// Stored OAuth credential (TS `OAuthCredential`). `expires` is epoch millis.
/// Provider-specific extras (`accountId`, `enterpriseUrl`, `scope`, …) are
/// preserved verbatim via flatten.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    /// Epoch milliseconds.
    pub expires: i64,
    #[serde(flatten)]
    pub extras: BTreeMap<String, Value>,
}

impl std::fmt::Debug for OAuthCredential {
    /// Tokens are secrets: Debug (which lands in logs via `tracing`'s `?`
    /// formatting) must never show them in plaintext.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("access", &"[REDACTED]")
            .field("refresh", &"[REDACTED]")
            .field("expires", &self.expires)
            .field("extras", &self.extras)
            .finish()
    }
}

impl OAuthCredential {
    pub fn new(access: String, refresh: String, expires: i64) -> Self {
        OAuthCredential {
            access,
            refresh,
            expires,
            extras: BTreeMap::new(),
        }
    }

    /// True when the credential needs a refresh: `now + skew >= expires`
    /// (TS `resolveStoredOAuth`'s proactive rule).
    pub fn expires_within(&self, skew: std::time::Duration) -> bool {
        let now = crate::types::now_millis() as i64;
        now + skew.as_millis() as i64 >= self.expires
    }
}

/// What a provider adapter needs per request (TS `toAuth` result).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedAuth {
    pub api_key: Option<String>,
    /// Extra headers merged into `StreamOptions.headers`.
    pub headers: BTreeMap<String, String>,
    /// Per-credential base URL override (copilot `proxy-ep`, radius gateway).
    pub base_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("OAuth HTTP error: {0}")]
    Http(String),
    #[error("OAuth error from server: {0}")]
    Server(String),
    #[error("device authorization expired")]
    Expired,
    #[error("authorization was denied")]
    Denied,
    #[error("{0}")]
    Other(String),
}

pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Request-time auth source (TS pi re-resolves through AuthStorage on every
/// LLM call so OAuth tokens get refreshed proactively). Long-running modes
/// (rpc/serve/acp) hold an `Arc<dyn AuthResolver>` and resolve per call.
pub trait AuthResolver: Send + Sync + std::fmt::Debug {
    fn resolve(&self) -> BoxFuture<'static, Result<ResolvedAuth, String>>;
}

/// Fixed credentials (API key from flag/env/store).
#[derive(Clone, Debug, Default)]
pub struct StaticAuth(pub ResolvedAuth);

impl AuthResolver for StaticAuth {
    fn resolve(&self) -> BoxFuture<'static, Result<ResolvedAuth, String>> {
        let auth = self.0.clone();
        Box::pin(async move { Ok(auth) })
    }
}

impl From<Option<String>> for StaticAuth {
    fn from(api_key: Option<String>) -> Self {
        StaticAuth(ResolvedAuth {
            api_key,
            ..Default::default()
        })
    }
}

/// Material needed to run a browser (authorization-code + PKCE) login.
#[derive(Clone, Debug)]
pub struct BrowserFlow {
    pub authorize_url: String,
    /// Sent in the token exchange as `redirect_uri`.
    pub redirect_uri: String,
    pub state: String,
    pub verifier: String,
    /// Loopback port to bind (0 = ephemeral).
    pub callback_port: u16,
    pub callback_path: String,
}

/// A provider's OAuth flow (TS `OAuthAuth`). Login-side methods build URLs
/// and parse provider-specific responses; `refresh`/`to_auth` are used at
/// request time.
pub trait OAuthFlow: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &'static str;
    /// Skew subtracted when computing `expires` / deciding to refresh.
    fn refresh_skew(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    /// Browser login, if the provider supports it. `options["callback_port"]`
    /// carries the already-bound loopback port for ephemeral-port flows.
    fn start_browser<'a>(
        &'a self,
        client: &'a reqwest::Client,
        options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Option<BrowserFlow>, OAuthError>> {
        let _ = (client, options);
        Box::pin(async { Ok(None) })
    }
    /// Device-code login start, if supported.
    fn start_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DeviceAuthorization, OAuthError>> {
        let _ = (client, options);
        let id = self.id();
        Box::pin(async move { Err(OAuthError::Other(format!("{id} has no device flow"))) })
    }
    /// Poll a started device flow to completion.
    fn poll_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        auth: &'a DeviceAuthorization,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        let _ = (client, auth);
        let id = self.id();
        Box::pin(async move { Err(OAuthError::Other(format!("{id} has no device flow"))) })
    }
    /// Exchange an authorization code from a browser/manual flow.
    fn exchange_code<'a>(
        &'a self,
        client: &'a reqwest::Client,
        flow: &'a BrowserFlow,
        code: &'a str,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        let _ = (client, flow, code);
        let id = self.id();
        Box::pin(async move { Err(OAuthError::Other(format!("{id} has no browser flow"))) })
    }
    /// Refresh an existing credential (may be identity, e.g. openrouter).
    fn refresh<'a>(
        &'a self,
        client: &'a reqwest::Client,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>>;
    /// Map a (fresh) credential to request-time auth.
    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth;
}

/// Resolve the OAuth flow for a provider id.
pub fn oauth_flow(provider: &str) -> Option<&'static dyn OAuthFlow> {
    use providers::*;
    match provider {
        "anthropic" => Some(&anthropic::AnthropicOAuth),
        "openai-codex" => Some(&openai_codex::OpenAiCodexOAuth),
        "github-copilot" => Some(&github_copilot::GitHubCopilotOAuth),
        "openrouter" => Some(&openrouter::OpenRouterOAuth),
        "kimi-coding" => Some(&kimi_coding::KimiCodingOAuth),
        "xai" => Some(&xai::XaiOAuth),
        "radius" => Some(&radius::RadiusOAuth),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared HTTP helpers
// ---------------------------------------------------------------------------

pub(crate) async fn post_form(
    client: &reqwest::Client,
    url: &str,
    params: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
) -> Result<Value, OAuthError> {
    let mut request = client.post(url).form(params);
    for (k, v) in headers {
        request = request.header(k, v);
    }
    send_json(request).await
}

pub(crate) async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
) -> Result<Value, OAuthError> {
    send_json(client.post(url).json(body)).await
}

async fn send_json(request: reqwest::RequestBuilder) -> Result<Value, OAuthError> {
    let response = request
        .send()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    if !status.is_success() {
        // Surface the server's own error description when present.
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            let message = value
                .get("error_description")
                .or_else(|| value.get("error"))
                .and_then(|e| e.as_str().or_else(|| e.get("message")?.as_str()));
            if let Some(message) = message {
                return Err(OAuthError::Server(message.to_string()));
            }
        }
        return Err(OAuthError::Http(format!("{status}: {body}")));
    }
    serde_json::from_str(&body).map_err(|e| OAuthError::Http(format!("invalid JSON: {e}")))
}

/// Parse a standard token response `{access_token, refresh_token?, expires_in?}`
/// into a credential, applying the provider's skew. `fallback_refresh` keeps
/// the previous refresh token when the server omits one (xAI).
pub(crate) fn token_credential(
    body: &Value,
    skew: std::time::Duration,
    fallback_refresh: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::Server("token response has no access_token".into()))?;
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| fallback_refresh.map(str::to_string))
        .unwrap_or_default();
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let expires = crate::types::now_millis() as i64 + expires_in.saturating_mul(1000)
        - skew.as_millis() as i64;
    Ok(OAuthCredential::new(access.to_string(), refresh, expires))
}

/// Lowercase hex of N random bytes (OAuth `state` parameters).
pub(crate) fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract an unverified JWT claim (we trust the token endpoint; no
/// signature validation needed).
pub fn jwt_claim(token: &str, claim: &str) -> Option<Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get(claim).cloned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn credential_debug_redacts_tokens() {
        let credential = OAuthCredential::new(
            "access-secret-123".to_string(),
            "refresh-secret-456".to_string(),
            1_700_000_000_000,
        );
        let debug = format!("{credential:?}");
        assert!(!debug.contains("access-secret-123"), "{debug}");
        assert!(!debug.contains("refresh-secret-456"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(debug.contains("1700000000000"), "{debug}");
    }
}
