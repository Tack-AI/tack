//! xAI OAuth (device flow). Port of `auth/oauth/xai.ts`. The previous
//! refresh token is kept when the server omits one (no rotation).

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::oauth::{
    BoxFuture, DeviceAuthorization, OAuthCredential, OAuthError, OAuthFlow, ResolvedAuth,
    device::{PollStyle, poll_device_code},
};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

#[derive(Debug)]
pub struct XaiOAuth;

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl OAuthFlow for XaiOAuth {
    fn id(&self) -> &'static str {
        "xai"
    }

    fn refresh_skew(&self) -> Duration {
        Duration::from_secs(5 * 60)
    }

    fn start_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DeviceAuthorization, OAuthError>> {
        Box::pin(async move {
            let body = crate::oauth::post_form(
                client,
                DEVICE_URL,
                &params(&[
                    ("client_id", CLIENT_ID),
                    ("scope", SCOPE),
                    ("referrer", "pi"),
                ]),
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
                TOKEN_URL,
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
            crate::oauth::token_credential(&body, self.refresh_skew(), None)
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
                TOKEN_URL,
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
            Ok(fresh)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth {
        ResolvedAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        }
    }
}
