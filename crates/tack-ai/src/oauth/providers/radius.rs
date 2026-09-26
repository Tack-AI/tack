//! Radius gateway OAuth (browser PKCE + device flow). Port of
//! `auth/oauth/radius.ts`: endpoints discovered via `GET {gateway}/v1/oauth`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::oauth::{
    BoxFuture, BrowserFlow, DeviceAuthorization, OAuthCredential, OAuthError, OAuthFlow,
    ResolvedAuth,
    device::{PollStyle, poll_device_code},
};

const CLIENT_ID: &str = "pi-gateway";
const SCOPE: &str = "gateway offline_access";
const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";

#[derive(Debug)]
pub struct RadiusOAuth;

/// Gateway base URL: `RADIUS_GATEWAY_URL` env, else the default.
pub fn gateway() -> String {
    std::env::var("RADIUS_GATEWAY_URL")
        .ok()
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| "https://radius.pi.dev".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Discovery: `GET {gateway}/v1/oauth` → `{ authorizationEndpoint }`.
async fn authorization_endpoint(client: &reqwest::Client) -> Result<String, OAuthError> {
    let response = client
        .get(format!("{}/v1/oauth", gateway()))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    let body: Value = response
        .json()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    body.get("authorizationEndpoint")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| OAuthError::Server("gateway /v1/oauth has no authorizationEndpoint".into()))
}

fn with_scope(body: &Value, mut credential: OAuthCredential) -> OAuthCredential {
    if let Some(scope) = body.get("scope").and_then(Value::as_str) {
        credential
            .extras
            .insert("scope".to_string(), Value::String(scope.to_string()));
    }
    credential
}

impl OAuthFlow for RadiusOAuth {
    fn id(&self) -> &'static str {
        "radius"
    }

    fn refresh_skew(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn start_browser<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Option<BrowserFlow>, OAuthError>> {
        Box::pin(async move {
            let endpoint = authorization_endpoint(client).await?;
            let verifier = crate::oauth::generate_verifier();
            let challenge = crate::oauth::challenge_s256(&verifier);
            let state = crate::oauth::random_hex(16);
            let mut url =
                reqwest::Url::parse(&endpoint).map_err(|e| OAuthError::Other(e.to_string()))?;
            url.query_pairs_mut()
                .append_pair("response_type", "code")
                .append_pair("client_id", CLIENT_ID)
                .append_pair("redirect_uri", REDIRECT_URI)
                .append_pair("scope", SCOPE)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("handoff", "url");
            Ok(Some(BrowserFlow {
                authorize_url: url.to_string(),
                redirect_uri: REDIRECT_URI.to_string(),
                state,
                verifier,
                callback_port: 1456,
                callback_path: "/oauth/callback".to_string(),
            }))
        })
    }

    fn exchange_code<'a>(
        &'a self,
        client: &'a reqwest::Client,
        flow: &'a BrowserFlow,
        code: &'a str,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(async move {
            let body = crate::oauth::post_form(
                client,
                &format!("{}/v1/oauth/token", gateway()),
                &params(&[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &flow.redirect_uri),
                    ("code_verifier", &flow.verifier),
                    ("client_id", CLIENT_ID),
                ]),
                &BTreeMap::new(),
            )
            .await?;
            let credential = crate::oauth::token_credential(&body, self.refresh_skew(), None)?;
            Ok(with_scope(&body, credential))
        })
    }

    fn start_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DeviceAuthorization, OAuthError>> {
        Box::pin(async move {
            let body = crate::oauth::post_form(
                client,
                &format!("{}/v1/oauth/device", gateway()),
                &params(&[("client_id", CLIENT_ID), ("scope", SCOPE)]),
                &BTreeMap::new(),
            )
            .await?;
            let get = |key: &str| {
                body.get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| OAuthError::Server(format!("device response missing {key}")))
            };
            let interval = body
                .get("interval")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .max(1);
            let expires_in = body
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(15 * 60);
            Ok(DeviceAuthorization {
                device_code: get("device_code")?,
                user_code: get("user_code")?,
                verification_uri: get("verification_uri")?,
                verification_uri_complete: body
                    .get("verification_uri_complete")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                interval: Duration::from_secs(interval),
                deadline: std::time::Instant::now() + Duration::from_secs(expires_in),
                extras: BTreeMap::new(),
            })
        })
    }

    fn poll_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        auth: &'a DeviceAuthorization,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(async move {
            let body = poll_device_code(
                client,
                &format!("{}/v1/oauth/token", gateway()),
                &params(&[
                    ("client_id", CLIENT_ID),
                    ("device_code", &auth.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]),
                auth.interval,
                auth.deadline,
                PollStyle::default(),
            )
            .await?;
            let credential = crate::oauth::token_credential(&body, self.refresh_skew(), None)?;
            Ok(with_scope(&body, credential))
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
                &format!("{}/v1/oauth/token", gateway()),
                &params(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &credential.refresh),
                    ("client_id", CLIENT_ID),
                ]),
                &BTreeMap::new(),
            )
            .await?;
            let mut fresh = crate::oauth::token_credential(
                &body,
                self.refresh_skew(),
                Some(&credential.refresh),
            )?;
            fresh.extras.clone_from(&credential.extras);
            Ok(with_scope(&body, fresh))
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth {
        ResolvedAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        }
    }
}
