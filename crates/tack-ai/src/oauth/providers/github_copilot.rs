//! GitHub Copilot OAuth (device-code only). Port of
//! `auth/oauth/github-copilot.ts`: the device flow yields a long-lived
//! GitHub token stored as `refresh`; "refresh" exchanges it for a
//! short-lived Copilot token. The credential carries `enterpriseUrl` and
//! `availableModelIds` extras; `to_auth` derives the per-credential baseUrl
//! from the token's `proxy-ep` field.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::oauth::{
    BoxFuture, DeviceAuthorization, OAuthCredential, OAuthError, OAuthFlow, ResolvedAuth,
    device::{PollStyle, poll_device_code},
};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";

#[derive(Debug)]
pub struct GitHubCopilotOAuth;

fn domain(options: &BTreeMap<String, String>) -> String {
    options
        .get("domain")
        .filter(|d| !d.is_empty())
        .cloned()
        .unwrap_or_else(|| "github.com".to_string())
}

fn credential_domain(credential: &OAuthCredential) -> String {
    credential
        .extras
        .get("enterpriseUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "github.com".to_string())
}

fn copilot_headers() -> BTreeMap<String, String> {
    [
        ("user-agent", USER_AGENT),
        ("editor-version", "vscode/1.107.0"),
        ("editor-plugin-version", "copilot-chat/0.35.0"),
        ("copilot-integration-id", "vscode-chat"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

impl GitHubCopilotOAuth {
    async fn exchange_for_copilot_token(
        client: &reqwest::Client,
        credential: &OAuthCredential,
    ) -> Result<OAuthCredential, OAuthError> {
        let domain = credential_domain(credential);
        let mut headers = copilot_headers();
        headers.insert(
            "authorization".to_string(),
            format!("Bearer {}", credential.refresh),
        );
        let mut request = client.get(format!("https://api.{domain}/copilot_internal/v2/token"));
        for (k, v) in &headers {
            request = request.header(k, v);
        }
        let response = request
            .send()
            .await
            .map_err(|e| OAuthError::Http(e.to_string()))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|e| OAuthError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(OAuthError::Http(format!(
                "{status}: copilot token exchange failed"
            )));
        }
        let token = body
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| OAuthError::Server("copilot token response has no token".into()))?;
        let expires_at = body.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
        let mut fresh = OAuthCredential::new(
            token.to_string(),
            credential.refresh.clone(),
            expires_at * 1000 - 5 * 60 * 1000,
        );
        fresh.extras.clone_from(&credential.extras);
        Ok(fresh)
    }

    /// Exchange + catalog fetch + policy auto-enable (TS login flow).
    /// Errors in the catalog/policy steps degrade to an empty model list —
    /// the login itself must not fail over them.
    async fn exchange_and_discover(
        client: &reqwest::Client,
        credential: &OAuthCredential,
    ) -> Result<OAuthCredential, OAuthError> {
        let mut fresh = Self::exchange_for_copilot_token(client, credential).await?;
        let domain = credential_domain(&fresh);
        let base = copilot_api_base(&fresh.access, &domain);
        if let Some((available, policy_ids)) =
            fetch_model_catalog(client, &fresh.access, &base).await
        {
            let enabled = enable_models(client, &fresh.access, &base, &policy_ids).await;
            let mut ids = available;
            for id in enabled {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            fresh.extras.insert(
                "availableModelIds".to_string(),
                Value::Array(ids.into_iter().map(Value::String).collect()),
            );
        }
        Ok(fresh)
    }
}

/// Copilot API base: token's proxy-ep → api host; enterprise domain; else
/// the Individual endpoint (TS getGitHubCopilotBaseUrl).
fn copilot_api_base(token: &str, enterprise_domain: &str) -> String {
    if let Some(host) = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))
    {
        return format!("https://{}", host.replacen("proxy.", "api.", 1));
    }
    if enterprise_domain != "github.com" {
        return format!("https://copilot-api.{enterprise_domain}");
    }
    "https://api.individual.githubcopilot.com".to_string()
}

/// GET {base}/models → (availableModelIds, policyModelIds), TS
/// parseGitHubCopilotModelCatalog. Best-effort: None on any failure.
async fn fetch_model_catalog(
    client: &reqwest::Client,
    access: &str,
    base: &str,
) -> Option<(Vec<String>, Vec<String>)> {
    let response = client
        .get(format!("{base}/models"))
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access}"))
        .header("X-GitHub-Api-Version", "2025-04-01")
        .header("user-agent", USER_AGENT)
        .header("editor-version", "vscode/1.107.0")
        .header("editor-plugin-version", "copilot-chat/0.35.0")
        .header("copilot-integration-id", "vscode-chat")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let data = body.get("data")?.as_array()?;

    let mut models: Vec<(String, bool, Option<String>)> = Vec::new();
    for item in data {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        // Models without tool-call support can't drive the agent loop.
        if item.pointer("/capabilities/supports/tool_calls") == Some(&Value::Bool(false)) {
            continue;
        }
        let picker = item.get("model_picker_enabled") == Some(&Value::Bool(true));
        let policy = item
            .get("policy")
            .and_then(|p| p.get("state"))
            .and_then(Value::as_str)
            .map(str::to_string);
        models.push((id.to_string(), picker, policy));
    }

    let picker_ids: Vec<String> = models
        .iter()
        .filter(|(_, picker, policy)| *picker && policy.as_deref() != Some("disabled"))
        .map(|(id, _, _)| id.clone())
        .collect();
    // Some Individual accounts return false for every picker flag despite
    // explicit enabled policies (TS allowPolicyFallback).
    let allow_fallback = base == "https://api.individual.githubcopilot.com";
    let available = if !picker_ids.is_empty() || !allow_fallback {
        picker_ids
    } else {
        models
            .iter()
            .filter(|(_, _, policy)| policy.as_deref() == Some("enabled"))
            .map(|(id, _, _)| id.clone())
            .collect()
    };
    let use_fallback = allow_fallback && available.is_empty();
    let policy_ids: Vec<String> = models
        .iter()
        .filter(|(id, picker, policy)| {
            policy.as_deref() == Some("unconfigured")
                && (*picker || use_fallback)
                && crate::providers::builtin_models("github-copilot")
                    .iter()
                    .any(|m| m.id == *id)
        })
        .map(|(id, _, _)| id.clone())
        .collect();
    Some((available, policy_ids))
}

/// POST {base}/models/{id}/policy {state:"enabled"} for each id (required
/// before some models can be used). Returns the ids that enabled cleanly.
async fn enable_models(
    client: &reqwest::Client,
    access: &str,
    base: &str,
    ids: &[String],
) -> Vec<String> {
    let mut enabled = Vec::new();
    for id in ids {
        let result = client
            .post(format!("{base}/models/{id}/policy"))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {access}"))
            .header("openai-intent", "chat-policy")
            .header("x-interaction-type", "chat-policy")
            .header("user-agent", USER_AGENT)
            .header("editor-version", "vscode/1.107.0")
            .header("editor-plugin-version", "copilot-chat/0.35.0")
            .header("copilot-integration-id", "vscode-chat")
            .body(r#"{"state":"enabled"}"#)
            .send()
            .await;
        if result.is_ok_and(|r| r.status().is_success()) {
            enabled.push(id.clone());
        }
    }
    enabled
}

impl OAuthFlow for GitHubCopilotOAuth {
    fn id(&self) -> &'static str {
        "github-copilot"
    }

    fn refresh_skew(&self) -> Duration {
        Duration::from_secs(5 * 60)
    }

    fn start_device<'a>(
        &'a self,
        client: &'a reqwest::Client,
        options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DeviceAuthorization, OAuthError>> {
        Box::pin(async move {
            let domain = domain(options);
            let mut params = BTreeMap::new();
            params.insert("client_id".to_string(), CLIENT_ID.to_string());
            params.insert("scope".to_string(), "read:user".to_string());
            let body = crate::oauth::post_form(
                client,
                &format!("https://{domain}/login/device/code"),
                &params,
                &copilot_headers(),
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
            let mut extras = BTreeMap::new();
            extras.insert("domain".to_string(), domain);
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
            let domain = auth
                .extras
                .get("domain")
                .cloned()
                .unwrap_or_else(|| "github.com".to_string());
            let mut params = BTreeMap::new();
            params.insert("client_id".to_string(), CLIENT_ID.to_string());
            params.insert("device_code".to_string(), auth.device_code.clone());
            params.insert(
                "grant_type".to_string(),
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            );
            let body = poll_device_code(
                client,
                &format!("https://{domain}/login/oauth/access_token"),
                &params,
                auth.interval,
                auth.deadline,
                PollStyle::default(),
            )
            .await?;
            // The GitHub access token becomes the stored `refresh`; mint the
            // first Copilot token immediately.
            let github_token = body
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or_else(|| OAuthError::Server("device poll returned no access_token".into()))?;
            let mut credential =
                OAuthCredential::new(github_token.to_string(), github_token.to_string(), 0);
            if domain != "github.com" {
                credential
                    .extras
                    .insert("enterpriseUrl".to_string(), Value::String(domain));
            }
            Self::exchange_and_discover(client, &credential).await
        })
    }

    fn refresh<'a>(
        &'a self,
        client: &'a reqwest::Client,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(async move { Self::exchange_and_discover(client, credential).await })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth {
        // The Copilot token embeds `proxy-ep=proxy.<host>`; the API base URL
        // is the same host with `proxy.` → `api.`.
        let base_url = credential
            .access
            .split(';')
            .find_map(|part| part.strip_prefix("proxy-ep="))
            .map(|host| format!("https://{}", host.replacen("proxy.", "api.", 1)))
            .or_else(|| {
                let domain = credential_domain(credential);
                if domain == "github.com" {
                    Some("https://api.individual.githubcopilot.com".to_string())
                } else {
                    Some(format!("https://copilot-api.{domain}"))
                }
            });
        ResolvedAuth {
            api_key: Some(credential.access.clone()),
            headers: BTreeMap::new(),
            base_url,
        }
    }
}
