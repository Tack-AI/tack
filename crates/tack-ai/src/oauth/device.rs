//! RFC 8628 device-code polling (port of `oauth/device-code.ts`).

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use super::OAuthError;

/// Provider response to a device-authorization request, normalized.
#[derive(Clone, Debug)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub interval: Duration,
    pub deadline: std::time::Instant,
    /// Provider-specific extras needed at poll time (e.g. codex
    /// `device_auth_id`).
    pub extras: BTreeMap<String, String>,
}

/// Poll behavior knobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct PollStyle {
    /// Retry the poll HTTP call itself on 429/5xx/connection errors up to N
    /// times with backoff before giving up (kimi-coding uses 3).
    pub transient_retries: u32,
}

/// Poll a device-code token endpoint until the user authorizes, the deadline
/// passes, or the server reports a fatal error. Returns the raw token JSON.
///
/// Handles `authorization_pending` (keep polling), `slow_down` (+5s),
/// `expired_token`, and `access_denied` per RFC 8628 §3.5.
pub async fn poll_device_code(
    client: &reqwest::Client,
    token_url: &str,
    params: &BTreeMap<String, String>,
    interval: Duration,
    deadline: std::time::Instant,
    style: PollStyle,
) -> Result<Value, OAuthError> {
    let mut interval = interval.max(Duration::from_secs(1));
    let mut transient_attempts = 0u32;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(OAuthError::Expired);
        }
        let response = client.post(token_url).form(params).send().await;
        let (status, body) = match response {
            Ok(r) => {
                let status = r.status();
                let body: Value = r
                    .json()
                    .await
                    .map_err(|e| OAuthError::Http(format!("invalid poll response: {e}")))?;
                (status, body)
            }
            Err(e) => {
                if transient_attempts < style.transient_retries {
                    transient_attempts += 1;
                    tokio::time::sleep(Duration::from_millis(500 * (1 << transient_attempts)))
                        .await;
                    continue;
                }
                return Err(OAuthError::Http(e.to_string()));
            }
        };
        if status.is_success() {
            return Ok(body);
        }
        if status.as_u16() == 429 || status.is_server_error() {
            if transient_attempts < style.transient_retries {
                transient_attempts += 1;
                tokio::time::sleep(Duration::from_millis(500 * (1 << transient_attempts))).await;
                continue;
            }
            return Err(OAuthError::Http(format!("{status}: poll failed")));
        }
        // A definitive 4xx: inspect the RFC 8628 error code.
        transient_attempts = 0;
        match body.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("expired_token") => return Err(OAuthError::Expired),
            Some("access_denied") => return Err(OAuthError::Denied),
            Some(other) => return Err(OAuthError::Server(other.to_string())),
            None => {
                // Some providers (codex deviceauth) signal pending via bare
                // 403/404 without an error body.
                if status.as_u16() == 403 || status.as_u16() == 404 {
                    // treat as pending
                } else {
                    return Err(OAuthError::Http(format!("{status}: poll failed")));
                }
            }
        }
        let now = std::time::Instant::now();
        if now + interval >= deadline {
            return Err(OAuthError::Expired);
        }
        tokio::time::sleep(interval).await;
    }
}
