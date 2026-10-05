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
/// "clientSecret": "…", "scopes": ["…"], "callbackPort": 8765,
/// "callbackUrl": "http://127.0.0.1:8765/callback"}` (all optional —
/// dynamic registration and an ephemeral loopback callback fill the rest).
#[derive(Clone, Debug, Default)]
pub struct McpOAuthSpec {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub callback_port: Option<u16>,
    pub callback_url: Option<String>,
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

/// Remove a server's cached token (`tack mcp logout`). Returns true when
/// an entry was actually removed.
pub fn delete_token(agent_dir: &Path, server: &str) -> std::io::Result<bool> {
    let _guard = TOKENS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut tokens = load_tokens(agent_dir);
    if tokens.remove(server).is_none() {
        return Ok(false);
    }
    let content = serde_json::to_string_pretty(&tokens)?;
    crate::atomic_write::atomic_write_private(&tokens_path(agent_dir), &content, 0o600)?;
    Ok(true)
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
    if let Some(secret) = &spec.client_secret {
        form.push(("client_secret", secret.clone()));
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

/// Loopback callback target derived from the OAuth config. `{port}` in
/// `uri` is substituted with the bound port after the listener is up.
struct CallbackTarget {
    bind: std::net::SocketAddr,
    path: String,
    uri: String,
}

/// Resolve the redirect target: an explicit `callbackUrl` (must be plain
/// HTTP on a loopback host — the redirect carries the authorization code,
/// anything else would be a code-exfiltration footgun), else
/// `http://127.0.0.1:<callbackPort>/callback` with an ephemeral port when
/// no port is configured either.
fn callback_target(spec: &McpOAuthSpec) -> Result<CallbackTarget> {
    if let Some(raw) = &spec.callback_url {
        let url = reqwest::Url::parse(raw).context("oauth.callbackUrl parse")?;
        anyhow::ensure!(
            url.scheme() == "http",
            "oauth.callbackUrl must use http (loopback only), got {raw:?}"
        );
        let host = url.host_str().unwrap_or("");
        anyhow::ensure!(
            host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]",
            "oauth.callbackUrl must be on a loopback host (localhost, 127.0.0.1, [::1]), got {raw:?}"
        );
        anyhow::ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "oauth.callbackUrl must not carry a query or fragment, got {raw:?}"
        );
        let port = url.port().or(spec.callback_port).unwrap_or(0);
        let path = match url.path() {
            "" | "/" => "/callback".to_string(),
            path => path.to_string(),
        };
        let bind_ip: std::net::IpAddr = if host == "[::1]" {
            std::net::Ipv6Addr::LOCALHOST.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        };
        return Ok(CallbackTarget {
            bind: std::net::SocketAddr::new(bind_ip, port),
            path: path.clone(),
            uri: format!("http://{host}:{{port}}{path}"),
        });
    }
    Ok(CallbackTarget {
        bind: std::net::SocketAddr::new(
            std::net::Ipv4Addr::LOCALHOST.into(),
            spec.callback_port.unwrap_or(0),
        ),
        path: "/callback".to_string(),
        uri: "http://127.0.0.1:{port}/callback".to_string(),
    })
}

/// Read a manually pasted redirect URL / `?code=…` from stdin (the
/// fallback when the browser runs on another machine, e.g. over SSH).
async fn read_pasted_code() -> Result<String> {
    tokio::task::spawn_blocking(|| {
        use std::io::BufRead as _;
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        line
    })
    .await
    .map_err(|e| anyhow::anyhow!("stdin reader: {e}"))
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

    // Bind the loopback listener first: the redirect URI must carry the
    // port. A configured (fixed) port/URL that fails to bind degrades to
    // manual-paste-only — the registered redirect URI cannot move to a
    // free port.
    let target = callback_target(spec)?;
    let fixed = target.bind.port() != 0;
    let listener = match tokio::net::TcpListener::bind(target.bind).await {
        Ok(listener) => Some(listener),
        Err(e) if fixed => {
            eprintln!(
                "warning: cannot bind {} ({e}); paste the redirected URL when prompted",
                target.bind
            );
            None
        }
        Err(e) => return Err(e).context("loopback bind"),
    };
    let port = match &listener {
        Some(listener) => listener.local_addr()?.port(),
        None => target.bind.port(),
    };
    let redirect_uri = target.uri.replace("{port}", &port.to_string());

    // Client: configured id (+ optional secret), else dynamic registration.
    let mut client_id = spec.client_id.clone();
    let mut config = OAuthClientConfig::new(
        client_id.clone().unwrap_or_else(|| "tack".to_string()),
        &redirect_uri,
    );
    if let Some(secret) = &spec.client_secret {
        config = config.with_client_secret(secret.clone());
    }
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
    let (code, state) = match listener {
        Some(listener) => {
            let path = target.path.clone();
            tokio::select! {
                callback = crate::oauth_login::await_callback_pair(listener, &path) => {
                    callback.context("callback")?
                }
                pasted = read_pasted_code() => {
                    crate::oauth_login::parse_manual_input(&pasted?)
                        .context("no code in pasted input")?
                }
            }
        }
        None => {
            let pasted = read_pasted_code().await?;
            crate::oauth_login::parse_manual_input(&pasted).context("no code in pasted input")?
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
            client_secret: o.client_secret.clone(),
            scopes: o.scopes.clone(),
            callback_port: o.callback_port,
            callback_url: o.callback_url.clone(),
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
        if !spec.enabled {
            // `enabled: false`: listed in config/status surfaces but never
            // connected (defense in depth — the plain connect paths filter
            // the same way in tack-tools).
            continue;
        }
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

    #[test]
    fn callback_target_validates_loopback() {
        // Default: ephemeral 127.0.0.1 port, /callback path.
        let target = callback_target(&McpOAuthSpec::default()).unwrap();
        assert_eq!(target.bind.port(), 0, "ephemeral by default");
        assert_eq!(target.path, "/callback");

        // Fixed port.
        let target = callback_target(&McpOAuthSpec {
            callback_port: Some(8765),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(target.bind.port(), 8765);
        assert!(target.uri.contains("{port}"), "port substituted post-bind");

        // Full callback URL: loopback hosts accepted, path honored.
        for url in [
            "http://localhost:9000/auth/done",
            "http://127.0.0.1/cb",
            "http://[::1]:8080/callback",
        ] {
            let target = callback_target(&McpOAuthSpec {
                callback_url: Some(url.to_string()),
                ..Default::default()
            })
            .unwrap_or_else(|e| panic!("{url} must be accepted: {e}"));
            assert!(!target.path.is_empty());
        }

        // Rejected: https, non-loopback host, query string.
        for url in [
            "https://127.0.0.1:9000/callback",
            "http://example.com/callback",
            "http://127.0.0.1:9000/callback?x=1",
        ] {
            assert!(
                callback_target(&McpOAuthSpec {
                    callback_url: Some(url.to_string()),
                    ..Default::default()
                })
                .is_err(),
                "{url} must be rejected"
            );
        }

        // A URL port wins over callbackPort; callbackPort fills a missing
        // URL port.
        let target = callback_target(&McpOAuthSpec {
            callback_url: Some("http://127.0.0.1:1111/callback".to_string()),
            callback_port: Some(2222),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(target.bind.port(), 1111);
        let target = callback_target(&McpOAuthSpec {
            callback_url: Some("http://127.0.0.1/callback".to_string()),
            callback_port: Some(2222),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(target.bind.port(), 2222);
    }

    #[test]
    fn delete_token_removes_only_the_named_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let token = McpToken {
            access: "a".into(),
            refresh: None,
            expires_at: 0,
            client_id: None,
        };
        save_token(tmp.path(), "one", &token).unwrap();
        save_token(tmp.path(), "two", &token).unwrap();
        assert!(delete_token(tmp.path(), "one").unwrap());
        assert!(
            !delete_token(tmp.path(), "one").unwrap(),
            "second delete is a no-op"
        );
        assert!(
            cached_token(tmp.path(), "two").is_some(),
            "other entry kept"
        );
    }

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
