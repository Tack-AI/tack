//! Kimi Code OAuth (RFC 8628 device flow). Port of
//! `auth/oauth/kimi-coding.ts`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::oauth::{
    BoxFuture, DeviceAuthorization, OAuthCredential, OAuthError, OAuthFlow, ResolvedAuth,
    device::{PollStyle, poll_device_code},
};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

#[derive(Debug)]
pub struct KimiCodingOAuth;

fn host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "https://auth.kimi.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl OAuthFlow for KimiCodingOAuth {
    fn id(&self) -> &'static str {
        "kimi-coding"
    }

    fn start_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DeviceAuthorization, OAuthError>> {
        Box::pin(async move {
            let host = host();
            let body = crate::oauth::post_form(
                client,
                &format!("{host}/api/oauth/device_authorization"),
                &params(&[("client_id", CLIENT_ID)]),
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
                &format!("{}/api/oauth/token", host()),
                &params(&[
                    ("client_id", CLIENT_ID),
                    ("device_code", &auth.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]),
                auth.interval,
                auth.deadline,
                PollStyle {
                    transient_retries: 3,
                },
            )
            .await?;
            crate::oauth::token_credential(&body, Duration::ZERO, None)
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
                &format!("{}/api/oauth/token", host()),
                &params(&[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &credential.refresh),
                ]),
                &BTreeMap::new(),
            )
            .await?;
            let mut fresh =
                crate::oauth::token_credential(&body, Duration::ZERO, Some(&credential.refresh))?;
            fresh.extras.clone_from(&credential.extras);
            Ok(fresh)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth {
        // TS maps to a Bearer *header* (not apiKey); the anthropic-messages
        // adapter treats an authorization header as sufficient auth.
        let mut headers = BTreeMap::new();
        headers.insert(
            "authorization".to_string(),
            format!("Bearer {}", credential.access),
        );
        ResolvedAuth {
            api_key: None,
            headers,
            base_url: None,
        }
    }
}
