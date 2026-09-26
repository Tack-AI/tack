//! OpenAI Codex (ChatGPT Plus/Pro subscription) OAuth. Port of
//! `auth/oauth/openai-codex.ts`: browser PKCE flow (port 1455) plus a
//! headless device-authorization variant whose server supplies the
//! `code_verifier`. The credential carries an `accountId` extra extracted
//! from the access-token JWT.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::oauth::{
    BoxFuture, BrowserFlow, DeviceAuthorization, OAuthCredential, OAuthError, OAuthFlow,
    ResolvedAuth,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
/// Consecutive transient poll failures (transport error, 429, 5xx)
/// tolerated before the device login is aborted — mirrors
/// `oauth::device::PollStyle::transient_retries` (kimi-coding uses 3).
const POLL_TRANSIENT_RETRIES: u32 = 3;

#[derive(Debug)]
pub struct OpenAiCodexOAuth;

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Extract `chatgpt_account_id` from the access-token JWT; login fails
/// without it (per TS pi).
fn with_account_id(
    body: &Value,
    mut credential: OAuthCredential,
) -> Result<OAuthCredential, OAuthError> {
    let account_id = crate::oauth::jwt_claim(&credential.access, "https://api.openai.com/auth")
        .and_then(|claim| claim.get("chatgpt_account_id").cloned())
        .and_then(|v| v.as_str().map(str::to_string));
    let _ = body;
    match account_id {
        Some(id) => {
            credential
                .extras
                .insert("accountId".to_string(), Value::String(id));
            Ok(credential)
        }
        None => Err(OAuthError::Server(
            "access token has no chatgpt_account_id claim".to_string(),
        )),
    }
}

impl OpenAiCodexOAuth {
    async fn exchange(
        client: &reqwest::Client,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<OAuthCredential, OAuthError> {
        let body = crate::oauth::post_form(
            client,
            &format!("{AUTH_BASE}/oauth/token"),
            &params(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier),
            ]),
            &BTreeMap::new(),
        )
        .await?;
        let credential = crate::oauth::token_credential(&body, Duration::ZERO, None)?;
        with_account_id(&body, credential)
    }
}

impl OAuthFlow for OpenAiCodexOAuth {
    fn id(&self) -> &'static str {
        "openai-codex"
    }

    fn start_browser<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Option<BrowserFlow>, OAuthError>> {
        Box::pin(async {
            let verifier = crate::oauth::generate_verifier();
            let challenge = crate::oauth::challenge_s256(&verifier);
            let state = crate::oauth::random_hex(16);
            let mut url = reqwest::Url::parse(&format!("{AUTH_BASE}/oauth/authorize"))
                .map_err(|e| OAuthError::Other(e.to_string()))?;
            url.query_pairs_mut()
                .append_pair("response_type", "code")
                .append_pair("client_id", CLIENT_ID)
                .append_pair("redirect_uri", REDIRECT_URI)
                .append_pair("scope", "openid profile email offline_access")
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state)
                .append_pair("id_token_add_organizations", "true")
                .append_pair("codex_cli_simplified_flow", "true")
                .append_pair("originator", "pi");
            Ok(Some(BrowserFlow {
                authorize_url: url.to_string(),
                redirect_uri: REDIRECT_URI.to_string(),
                state,
                verifier,
                callback_port: 1455,
                callback_path: "/auth/callback".to_string(),
            }))
        })
    }

    fn exchange_code<'a>(
        &'a self,
        client: &'a reqwest::Client,
        flow: &'a BrowserFlow,
        code: &'a str,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(
            async move { Self::exchange(client, code, &flow.redirect_uri, &flow.verifier).await },
        )
    }

    fn start_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DeviceAuthorization, OAuthError>> {
        Box::pin(async move {
            let body = crate::oauth::post_json(
                client,
                &format!("{AUTH_BASE}/api/accounts/deviceauth/usercode"),
                &json!({ "client_id": CLIENT_ID }),
            )
            .await?;
            let device_auth_id = body
                .get("device_auth_id")
                .and_then(Value::as_str)
                .ok_or_else(|| OAuthError::Server("no device_auth_id".into()))?
                .to_string();
            let user_code = body
                .get("user_code")
                .and_then(Value::as_str)
                .ok_or_else(|| OAuthError::Server("no user_code".into()))?
                .to_string();
            let interval = body
                .get("interval")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .max(1);
            let mut extras = BTreeMap::new();
            extras.insert("device_auth_id".to_string(), device_auth_id);
            Ok(DeviceAuthorization {
                device_code: user_code.clone(),
                user_code,
                verification_uri: DEVICE_VERIFICATION_URI.to_string(),
                verification_uri_complete: None,
                interval: Duration::from_secs(interval),
                deadline: std::time::Instant::now() + Duration::from_secs(15 * 60),
                extras,
            })
        })
    }

    fn poll_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        auth: &'a DeviceAuthorization,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(async move {
            let device_auth_id = auth
                .extras
                .get("device_auth_id")
                .ok_or_else(|| OAuthError::Other("missing device_auth_id".into()))?;
            // The deviceauth poll speaks JSON, not form, and reports pending
            // as 403/404 or `deviceauth_authorization_pending`.
            // Transient transport/5xx failures get a few backoff retries
            // (same policy as oauth::device::poll_device_code) — a single
            // dropped connection must not abort the whole login.
            let mut transient_attempts = 0u32;
            loop {
                if std::time::Instant::now() >= auth.deadline {
                    return Err(OAuthError::Expired);
                }
                let response = client
                    .post(format!("{AUTH_BASE}/api/accounts/deviceauth/token"))
                    .json(&json!({
                        "device_auth_id": device_auth_id,
                        "user_code": auth.user_code,
                    }))
                    .send()
                    .await;
                let response = match response {
                    Ok(r) => r,
                    Err(e) => {
                        if transient_attempts < POLL_TRANSIENT_RETRIES {
                            transient_attempts += 1;
                            tokio::time::sleep(Duration::from_millis(
                                500 * (1 << transient_attempts),
                            ))
                            .await;
                            continue;
                        }
                        return Err(OAuthError::Http(e.to_string()));
                    }
                };
                let status = response.status();
                if status.as_u16() == 429 || status.is_server_error() {
                    if transient_attempts < POLL_TRANSIENT_RETRIES {
                        transient_attempts += 1;
                        tokio::time::sleep(Duration::from_millis(500 * (1 << transient_attempts)))
                            .await;
                        continue;
                    }
                    return Err(OAuthError::Http(format!(
                        "{status}: deviceauth poll failed"
                    )));
                }
                if status.is_success() {
                    let body: Value = response
                        .json()
                        .await
                        .map_err(|e| OAuthError::Http(e.to_string()))?;
                    let code = body
                        .get("authorization_code")
                        .and_then(Value::as_str)
                        .ok_or_else(|| OAuthError::Server("no authorization_code".into()))?;
                    let verifier = body
                        .get("code_verifier")
                        .and_then(Value::as_str)
                        .ok_or_else(|| OAuthError::Server("no code_verifier".into()))?;
                    return Self::exchange(client, code, DEVICE_REDIRECT_URI, verifier).await;
                }
                let body: Value = response.json().await.unwrap_or_default();
                let pending = status.as_u16() == 403
                    || status.as_u16() == 404
                    || body.get("error").and_then(Value::as_str)
                        == Some("deviceauth_authorization_pending");
                if !pending {
                    return Err(OAuthError::Http(format!(
                        "{status}: deviceauth poll failed"
                    )));
                }
                // A definitive pending answer resets the transient budget.
                transient_attempts = 0;
                tokio::time::sleep(auth.interval).await;
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        client: &'a reqwest::Client,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(async move {
            let body = crate::oauth::post_form(
                client,
                &format!("{AUTH_BASE}/oauth/token"),
                &params(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &credential.refresh),
                    ("client_id", CLIENT_ID),
                ]),
                &BTreeMap::new(),
            )
            .await?;
            let fresh =
                crate::oauth::token_credential(&body, Duration::ZERO, Some(&credential.refresh))?;
            with_account_id(&body, fresh)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth {
        let mut headers = BTreeMap::new();
        if let Some(account_id) = credential.extras.get("accountId").and_then(Value::as_str) {
            headers.insert("chatgpt-account-id".to_string(), account_id.to_string());
        }
        headers.insert("originator".to_string(), "pi".to_string());
        ResolvedAuth {
            api_key: Some(credential.access.clone()),
            headers,
            base_url: None,
        }
    }
}
