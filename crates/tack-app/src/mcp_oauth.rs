//! OAuth 2.1 for remote MCP servers (the MCP spec's standard auth for HTTP
//! transports — GitHub/Notion/Linear-style servers). Flow: 401 →
//! authorization-server metadata discovery → dynamic client registration
//! (or a `clientId` from mcp.json) → PKCE browser flow with loopback
//! callback and manual-paste fallback → token cache with refresh.
//!
//! Tokens cache in `<agent dir>/mcp-tokens.json` (0600 on unix). The cached
//! access token is injected as a static `Authorization: Bearer` header on
//! reconnect; refresh happens transparently on the next connect after
//! expiry. (Long-lived connections don't refresh in place — reconnecting
//! per run, which is the existing model.)

use std::path::Path;

use anyhow::{Context as _, Result};
use oauth2::TokenResponse as _;
use rmcp::transport::auth::{AuthorizationManager, OAuthClientConfig};
use serde_json::Value;

/// Per-server OAuth config from mcp.json: `"oauth": {"clientId": "…",
/// "scopes": ["…"]}` (both optional — dynamic registration fills the rest).
#[derive(Clone, Debug, Default)]
pub struct McpOAuthSpec {
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
}

/// Cached token set for one server.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct McpToken {
    pub access: String,
    #[serde(default)]
    pub refresh: Option<String>,
    /// Unix seconds; 0 = no expiry known.
    #[serde(default)]
    pub expires_at: u64,
    /// Client id from dynamic registration (needed for refresh).
    #[serde(default)]
    pub client_id: Option<String>,
}

fn tokens_path(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join("mcp-tokens.json")
}

fn load_tokens(agent_dir: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(tokens_path(agent_dir))
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// In-process serializer for the load-modify-save cycle on
/// mcp-tokens.json: concurrent token refreshes in one process must not
/// lose each other's entry. NOTE: process-local only — two tack
/// PROCESSES writing concurrently still race (last writer wins); the
/// atomic write keeps the file intact either way.
static TOKENS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn save_token(agent_dir: &Path, server: &str, token: &McpToken) -> std::io::Result<()> {
    let _guard = TOKENS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut tokens = load_tokens(agent_dir);
    tokens.insert(server.to_string(), serde_json::to_value(token)?);
    let content = serde_json::to_string_pretty(&tokens)?;
    // 0600 on unix from the first byte (temp file created with the mode);
    // Windows relies on the profile ACL. Atomic: a crash mid-write keeps
    // the previous token cache instead of a truncated file.
    crate::atomic_write::atomic_write_private(&tokens_path(agent_dir), &content, 0o600)
}

/// Fresh cached token, if any.
pub fn cached_token(agent_dir: &Path, server: &str) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = load_tokens(agent_dir).remove(server)?;
    let token: McpToken = serde_json::from_value(entry).ok()?;
    if token.expires_at != 0 && token.expires_at <= now + 30 {
        return None;
    }
    Some(token.access)
}

/// Try to refresh an expired cached token (plain RFC 6749 refresh grant
/// against the discovered token endpoint — no rmcp store plumbing needed).
pub async fn refresh_token(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
) -> Result<Option<String>> {
    let Some(entry) = load_tokens(agent_dir).remove(server) else {
        return Ok(None);
    };
    let token: McpToken = serde_json::from_value(entry).context("cached token parse")?;
    let Some(refresh) = token.refresh.clone() else {
        return Ok(None);
    };
    let client_id = spec.client_id.clone().or(token.client_id.clone());

    // Discover the token endpoint.
    let manager = AuthorizationManager::new(base_url)
        .await
        .context("auth manager")?;
    let resolution = manager
        .resolve_metadata()
        .await
        .context("metadata resolution")?;
    let token_endpoint = resolution.metadata.token_endpoint;
    if token_endpoint.is_empty() {
        return Ok(None);
    }

    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh.clone()),
    ];
    if let Some(client_id) = &client_id {
        form.push(("client_id", client_id.clone()));
    }
    let response = reqwest::Client::new()
        .post(&token_endpoint)
        .form(&form)
        .send()
        .await
        .context("refresh request failed")?;
    if !response.status().is_success() {
        return Ok(None); // caller falls through to the interactive flow
    }
    let body: Value = response.json().await.context("refresh response parse")?;
    let Some(access) = body["access_token"].as_str().map(str::to_string) else {
        return Ok(None);
    };
    let new_token = McpToken {
        access,
        refresh: body["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or(Some(refresh)),
        expires_at: body["expires_in"]
            .as_u64()
            .map(|secs| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() + secs)
                    .unwrap_or(0)
            })
            .unwrap_or(0),
        client_id,
    };
    save_token(agent_dir, server, &new_token)?;
    Ok(Some(new_token.access))
}

/// Full interactive flow. Returns the fresh access token.
pub async fn authorize(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
) -> Result<String> {
    let mut manager = AuthorizationManager::new(base_url)
        .await
        .context("auth manager")?;
    manager
        .resolve_metadata()
        .await
        .context("authorization server metadata discovery")?;

    // Bind the loopback listener first: the redirect URI must carry the port.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("loopback bind")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // Client: configured id, else dynamic registration.
    let mut client_id = spec.client_id.clone();
    let config = OAuthClientConfig::new(
        client_id.clone().unwrap_or_else(|| "tack".to_string()),
        &redirect_uri,
    );
    manager
        .configure_client(config)
        .context("oauth client config")?;
    if client_id.is_none() {
        let registered = manager
            .register_client("tack", &redirect_uri, &[])
            .await
            .context("dynamic client registration failed (set oauth.clientId in mcp.json for this server)")?;
        client_id = Some(registered.client_id);
    }

    let scopes: Vec<&str> = spec.scopes.iter().map(String::as_str).collect();
    let auth_url = manager
        .get_authorization_url(&scopes)
        .await
        .context("authorization url")?;

    eprintln!("\nMCP server {server:?} requires OAuth authorization.");
    eprintln!("  Open: {auth_url}");
    eprintln!("  (or paste the redirected URL / ?code=… here)");
    crate::oauth_login::open_browser(&auth_url);

    // Loopback callback raced against manual paste. The state value is the
    // csrf key into rmcp's state store — the code exchange requires it.
    let (code, state) = tokio::select! {
        callback = crate::oauth_login::await_callback_pair(listener, "/callback") => {
            callback.context("callback")?
        }
        pasted = async move {
            let mut line = String::new();
            tokio::task::spawn_blocking(move || {
                use std::io::BufRead as _;
                let _ = std::io::stdin().lock().read_line(&mut line);
                line
            })
            .await
            .map_err(|e| anyhow::anyhow!("stdin reader: {e}"))
        } => {
            let pasted = pasted?;
            crate::oauth_login::parse_manual_input(&pasted)
                .context("no code in pasted input")?
        }
    };

    let token_response = manager
        .exchange_code_for_token(&code, state.as_deref().unwrap_or(""))
        .await
        .context("code exchange failed")?;
    let token = McpToken {
        access: token_response.access_token().secret().to_string(),
        refresh: token_response
            .refresh_token()
            .map(|t| t.secret().to_string()),
        expires_at: token_response
            .expires_in()
            .map(|secs| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() + secs.as_secs())
                    .unwrap_or(0)
            })
            .unwrap_or(0),
        client_id,
    };
    save_token(agent_dir, server, &token)?;
    eprintln!("MCP server {server:?} authorized (token cached).");
    Ok(token.access)
}

/// Resolve an access token for a server: cache → refresh → (interactive)
/// browser flow. `interactive=false` (print/rpc/serve) never prompts.
pub async fn access_token(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
    interactive: bool,
) -> Result<Option<String>> {
    if let Some(token) = cached_token(agent_dir, server) {
        return Ok(Some(token));
    }
    if let Ok(Some(token)) = refresh_token(agent_dir, server, base_url, spec).await {
        return Ok(Some(token));
    }
    if !interactive {
        return Ok(None);
    }
    authorize(agent_dir, server, base_url, spec).await.map(Some)
}

pub(crate) fn spec_oauth(spec: &tack_tools::mcp::McpServerSpec) -> McpOAuthSpec {
    spec.oauth
        .as_ref()
        .map(|o| McpOAuthSpec {
            client_id: o.client_id.clone(),
            scopes: o.scopes.clone(),
        })
        .unwrap_or_default()
}

/// Connect all configured MCP servers, with OAuth for remote ones: cached
/// token → refresh → (interactive) browser flow on 401. `interactive=false`
/// (print/rpc/serve) only uses cached/refreshed tokens, never prompts.
/// `callbacks` carries the client-side handlers for server-initiated
/// requests (sampling / elicitation; see tack_tools::mcp::McpClientCallbacks).
pub async fn connect_all_oauth(
    specs: Vec<tack_tools::mcp::McpServerSpec>,
    agent_dir: &Path,
    interactive: bool,
    callbacks: tack_tools::mcp::McpClientCallbacks,
) -> Vec<std::sync::Arc<tack_tools::mcp::McpConnection>> {
    connect_all_oauth_reporting(specs, agent_dir, interactive, callbacks)
        .await
        .into_iter()
        .filter_map(|outcome| outcome.result.ok())
        .collect()
}

/// Per-server outcome of [`connect_all_oauth_reporting`]: the spec name plus
/// the live connection or the connect error (for status surfaces that must
/// show WHICH server failed and why, not just drop it from the tool set).
#[derive(Debug)]
pub struct McpConnectOutcome {
    pub name: String,
    pub result: Result<std::sync::Arc<tack_tools::mcp::McpConnection>, String>,
}

/// [`connect_all_oauth`] with per-server error reporting (same connect
/// logic, same warn logs; failures come back as `Err` instead of being
/// silently absent from the result).
pub async fn connect_all_oauth_reporting(
    specs: Vec<tack_tools::mcp::McpServerSpec>,
    agent_dir: &Path,
    interactive: bool,
    callbacks: tack_tools::mcp::McpClientCallbacks,
) -> Vec<McpConnectOutcome> {
    let mut out = Vec::new();
    for spec in specs {
        let name = spec.name.clone();
        let result = connect_one_oauth(&spec, agent_dir, interactive, &callbacks).await;
        out.push(McpConnectOutcome { name, result });
    }
    out
}

async fn connect_one_oauth(
    spec: &tack_tools::mcp::McpServerSpec,
    agent_dir: &Path,
    interactive: bool,
    callbacks: &tack_tools::mcp::McpClientCallbacks,
) -> Result<std::sync::Arc<tack_tools::mcp::McpConnection>, String> {
    let url = spec.url().map(str::to_string);
    let wants_oauth = spec.oauth.is_some();
    match (wants_oauth, url) {
        (true, Some(url)) => {
            let oauth = spec_oauth(spec);
            // Cached/refreshed token first.
            let token = access_token(agent_dir, &spec.name, &url, &oauth, false)
                .await
                .ok()
                .flatten();
            let with_token = |token: &str| {
                spec.clone().with_extra_headers(vec![(
                    "authorization".to_string(),
                    format!("Bearer {token}"),
                )])
            };
            let first = match &token {
                Some(t) => tack_tools::mcp::connect_with(&with_token(t), callbacks.clone()).await,
                None => tack_tools::mcp::connect_with(spec, callbacks.clone()).await,
            };
            match first {
                Ok(conn) => Ok(std::sync::Arc::new(conn)),
                Err(e) => {
                    let authish = e.contains("401") || e.to_lowercase().contains("unauthor");
                    if authish && interactive {
                        match access_token(agent_dir, &spec.name, &url, &oauth, true).await {
                            Ok(Some(token)) => {
                                match tack_tools::mcp::connect_with(
                                    &with_token(&token),
                                    callbacks.clone(),
                                )
                                .await
                                {
                                    Ok(conn) => Ok(std::sync::Arc::new(conn)),
                                    Err(e2) => {
                                        tracing::warn!(
                                            "MCP {}: connect after OAuth failed: {e2}",
                                            spec.name
                                        );
                                        Err(format!("connect after OAuth failed: {e2}"))
                                    }
                                }
                            }
                            Ok(None) => {
                                tracing::warn!("MCP {}: OAuth produced no token", spec.name);
                                Err("OAuth produced no token".to_string())
                            }
                            Err(e2) => {
                                tracing::warn!("MCP {}: OAuth flow failed: {e2}", spec.name);
                                Err(format!("OAuth flow failed: {e2}"))
                            }
                        }
                    } else {
                        tracing::warn!("MCP {}: {e}", spec.name);
                        Err(e)
                    }
                }
            }
        }
        _ => {
            // Stdio or OAuth-free remote: plain connect.
            match tack_tools::mcp::connect_with(spec, callbacks.clone()).await {
                Ok(conn) => Ok(std::sync::Arc::new(conn)),
                Err(e) => {
                    tracing::warn!("MCP {}: {e}", spec.name);
                    Err(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn preexisting_token_file_permissions_are_tightened() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp-tokens.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let token = McpToken {
            access: "tok".into(),
            refresh: None,
            expires_at: 0,
            client_id: None,
        };
        save_token(tmp.path(), "github", &token).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mcp-tokens.json kept mode {mode:o}");
    }

    #[test]
    fn token_cache_roundtrip_and_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = McpToken {
            access: "tok-fresh".into(),
            refresh: Some("rt".into()),
            expires_at: u64::MAX,
            client_id: None,
        };
        save_token(tmp.path(), "github", &fresh).unwrap();
        assert_eq!(
            cached_token(tmp.path(), "github").as_deref(),
            Some("tok-fresh")
        );

        let expired = McpToken {
            access: "tok-old".into(),
            refresh: None,
            expires_at: 1,
            client_id: None,
        };
        save_token(tmp.path(), "notion", &expired).unwrap();
        assert!(
            cached_token(tmp.path(), "notion").is_none(),
            "expired tokens don't serve"
        );
        assert!(cached_token(tmp.path(), "unknown").is_none());
    }
}
