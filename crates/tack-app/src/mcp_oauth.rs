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
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use oauth2::TokenResponse as _;
use rmcp::transport::auth::{AuthorizationManager, OAuthClientConfig};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

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
    pub auth_server_metadata_url: Option<String>,
}

const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(20);
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);

#[derive(Clone, Debug)]
struct McpOAuthChallenge {
    header: String,
    params: rmcp::transport::auth::WWWAuthenticateParams,
}

fn parse_oauth_challenge(error: &str, base_url: &str) -> Option<McpOAuthChallenge> {
    let start = error.find("Bearer ").or_else(|| error.find("bearer "))?;
    let header = error[start..].to_string();
    let base_url = reqwest::Url::parse(base_url).ok()?;
    let params = rmcp::transport::auth::WWWAuthenticateParams::parse(&header, &base_url);
    Some(McpOAuthChallenge { header, params })
}

/// Cached token set for one server name + normalized URL.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Granted scope, retained across refresh and step-up authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
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

/// In-process serializer for token-store read/modify/write cycles. Network
/// refreshes use separate per-server locks and take this lock only to commit.
static TOKENS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn server_namespace(server: &str) -> String {
    // Storage identity is NOT the tool-name namespace: `foo-bar` and
    // `foo_bar` are distinct configured servers and may use different accounts.
    format!("mcp__{server}")
}

fn token_key(server: &str, base_url: &str) -> std::io::Result<String> {
    let url = reqwest::Url::parse(base_url).map_err(std::io::Error::other)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "MCP OAuth server URL must use HTTP(S)",
        ));
    }
    Ok(format!("{}|{url}", server_namespace(server)))
}

fn update_tokens<T>(
    agent_dir: &Path,
    update: impl FnOnce(&mut serde_json::Map<String, Value>) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let _local = TOKENS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = tokens_path(agent_dir);
    let _guard = crate::file_lock::strict_lock(
        &path,
        REFRESH_LOCK_STALE,
        std::time::Duration::from_secs(30),
    )?;
    let mut tokens = load_tokens(agent_dir);
    let result = update(&mut tokens)?;
    let content = serde_json::to_string_pretty(&tokens)?;
    crate::atomic_write::atomic_write_private(&path, &content, 0o600)?;
    Ok(result)
}

/// Load the composite entry, atomically claiming Tack's legacy name-only
/// entry when necessary. A URL change does not silently invalidate the grant.
fn load_token(agent_dir: &Path, server: &str, base_url: &str) -> std::io::Result<Option<McpToken>> {
    let key = token_key(server, base_url)?;
    if let Some(value) = load_tokens(agent_dir).get(&key).cloned() {
        return Ok(serde_json::from_value(value).ok());
    }
    update_tokens(agent_dir, |tokens| {
        if let Some(value) = tokens.get(&key).cloned() {
            return Ok(serde_json::from_value(value).ok());
        }
        let Some(legacy) = tokens.remove(server) else {
            return Ok(None);
        };
        tokens.insert(key.clone(), legacy.clone());
        Ok(serde_json::from_value(legacy).ok())
    })
}

fn save_token(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    token: &McpToken,
) -> std::io::Result<()> {
    let key = token_key(server, base_url)?;
    update_tokens(agent_dir, |tokens| {
        tokens.remove(server);
        tokens.insert(key.clone(), serde_json::to_value(token)?);
        Ok(())
    })
}

/// Commit a refresh only over the exact grant that was refreshed. Another
/// process's sign-in/refresh wins; logout cannot be undone by a late response.
fn save_refreshed_token(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    expected: &McpToken,
    fresh: &McpToken,
) -> std::io::Result<Option<McpToken>> {
    let key = token_key(server, base_url)?;
    update_tokens(agent_dir, |tokens| {
        let current = tokens
            .get(&key)
            .cloned()
            .and_then(|value| serde_json::from_value::<McpToken>(value).ok());
        let Some(current) = current else {
            return Ok(None);
        };
        if current.access != expected.access || current.refresh != expected.refresh {
            return Ok(Some(current));
        }
        tokens.remove(server);
        tokens.insert(key.clone(), serde_json::to_value(fresh)?);
        Ok(Some(fresh.clone()))
    })
}

/// Remove every cached grant for this configured name (all URLs plus the
/// legacy name-only entry). Returns true when an entry was removed.
pub fn delete_token(agent_dir: &Path, server: &str) -> std::io::Result<bool> {
    let prefix = format!("{}|", server_namespace(server));
    update_tokens(agent_dir, |tokens| {
        let before = tokens.len();
        tokens.retain(|key, _| key != server && !key.starts_with(&prefix));
        Ok(tokens.len() != before)
    })
}

/// Fresh cached token, if any.
fn cached_token(agent_dir: &Path, server: &str, base_url: &str) -> std::io::Result<Option<String>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Some(token) = load_token(agent_dir, server, base_url)? else {
        return Ok(None);
    };
    if token.expires_at != 0 && token.expires_at <= now + 30 {
        return Ok(None);
    }
    Ok(Some(token.access))
}

fn refresh_locks() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

/// Refresh an expired/rejected token. The reload → request → commit cycle is
/// serialized across processes; once the request starts it is detached and
/// tracked so caller cancellation cannot lose a rotated refresh token.
pub async fn refresh_token(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
) -> Result<Option<String>> {
    refresh_token_with_challenge(agent_dir, server, base_url, spec, None).await
}

async fn refresh_token_with_challenge(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
    challenge: Option<&McpOAuthChallenge>,
) -> Result<Option<String>> {
    let Some(stale) = load_token(agent_dir, server, base_url)? else {
        return Ok(None);
    };
    let key = token_key(server, base_url)?;
    let lock = {
        refresh_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone()
    };
    let _local = lock.lock().await;

    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let lock_target = agent_dir.join(format!(".mcp-oauth-refresh-{:x}", hasher.finish()));
    let refresh_guard = tokio::task::spawn_blocking(move || {
        crate::file_lock::strict_lock(&lock_target, REFRESH_LOCK_STALE, REFRESH_LOCK_WAIT)
    })
    .await
    .context("MCP OAuth refresh lock task")??;

    // A different process may already have replaced the rejected grant.
    let Some(expected) = load_token(agent_dir, server, base_url)? else {
        return Ok(None);
    };
    if expected.access != stale.access {
        return Ok(Some(expected.access));
    }
    if expected.refresh.is_none() {
        return Ok(None);
    }

    let task_agent_dir = agent_dir.to_path_buf();
    let task_server = server.to_string();
    let task_url = base_url.to_string();
    let task_spec = spec.clone();
    let task_challenge = challenge.cloned();
    let refresh = crate::oauth_lifecycle::spawn_tracked(async move {
        let _refresh_guard = refresh_guard;
        let result = async {
            let (_manager, metadata) =
                authorization_manager(&task_url, &task_spec, task_challenge.as_ref()).await?;
            let token_endpoint = metadata.token_endpoint;
            if token_endpoint.is_empty() {
                return Ok(None);
            }
            let refresh = expected.refresh.clone().expect("checked above");
            let client_id = task_spec.client_id.clone().or(expected.client_id.clone());
            let mut form: Vec<(&str, String)> = vec![
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", refresh.clone()),
            ];
            if let Some(client_id) = &client_id {
                form.push(("client_id", client_id.clone()));
            }
            if let Some(secret) = &task_spec.client_secret {
                form.push(("client_secret", secret.clone()));
            }
            let response = reqwest::Client::builder()
                .timeout(OAUTH_REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()?
                .post(&token_endpoint)
                .form(&form)
                .send()
                .await
                .context("refresh request failed")?;
            if !response.status().is_success() {
                return Ok(None); // caller may fall through to interactive sign-in
            }
            let body: Value = response.json().await.context("refresh response parse")?;
            let Some(access) = body["access_token"].as_str().map(str::to_string) else {
                return Ok(None);
            };
            let fresh = McpToken {
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
                // RFC 6749 §6: omitted scope retains the original grant.
                scope: body["scope"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| expected.scope.clone()),
            };
            let committed = tokio::task::spawn_blocking({
                let dir = task_agent_dir.clone();
                let server = task_server.clone();
                let url = task_url.clone();
                let expected = expected.clone();
                let fresh = fresh.clone();
                move || save_refreshed_token(&dir, &server, &url, &expected, &fresh)
            })
            .await
            .context("MCP OAuth credential write task")??;
            Ok(committed.map(|token| token.access))
        }
        .await;
        if let Err(error) = &result {
            tracing::debug!(server = %task_server, %error, "MCP OAuth refresh failed");
        }
        result
    });
    refresh.await.context("MCP OAuth refresh task")?
}

fn is_loopback(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]"
    })
}

fn require_http_url(value: &str, label: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).with_context(|| format!("{label} parse"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("{label} must use HTTP(S): {value}");
    }
    Ok(url)
}

fn require_secure_token_url(value: &str) -> Result<reqwest::Url> {
    let url = require_http_url(value, "OAuth token endpoint")?;
    if url.scheme() != "https" && !is_loopback(&url) {
        bail!("OAuth token endpoint must use HTTPS except on loopback: {value}");
    }
    Ok(url)
}

/// Resolve discovery, or replace it with the explicitly configured metadata
/// document. The configured URL is trusted as configured but must be HTTPS
/// except on loopback; every endpoint opened or posted to is HTTP(S)-only.
async fn authorization_manager(
    base_url: &str,
    spec: &McpOAuthSpec,
    challenge: Option<&McpOAuthChallenge>,
) -> Result<(
    AuthorizationManager,
    rmcp::transport::auth::AuthorizationMetadata,
)> {
    let mut manager = AuthorizationManager::new(base_url)
        .await
        .context("auth manager")?;
    let metadata = if let Some(raw_url) = &spec.auth_server_metadata_url {
        let url = require_http_url(raw_url, "oauth.authServerMetadataUrl")?;
        if url.scheme() != "https" && !is_loopback(&url) {
            bail!("oauth.authServerMetadataUrl must use HTTPS except on loopback");
        }
        let response = reqwest::Client::builder()
            .timeout(OAUTH_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("loading authorization metadata from {url}"))?;
        response
            .error_for_status()
            .with_context(|| format!("loading authorization metadata from {url}"))?
            .json::<rmcp::transport::auth::AuthorizationMetadata>()
            .await
            .context("authorization metadata parse")?
    } else {
        let resolution = match challenge {
            Some(challenge) => tokio::time::timeout(
                OAUTH_REQUEST_TIMEOUT,
                manager.resolve_metadata_from_challenge(Some(&challenge.header)),
            )
            .await
            .context("authorization metadata discovery timed out")?,
            None => tokio::time::timeout(OAUTH_REQUEST_TIMEOUT, manager.resolve_metadata())
                .await
                .context("authorization metadata discovery timed out")?,
        };
        resolution.context("metadata resolution")?.metadata
    };
    require_http_url(
        &metadata.authorization_endpoint,
        "OAuth authorization endpoint",
    )?;
    require_secure_token_url(&metadata.token_endpoint)?;
    manager.set_metadata(metadata.clone());
    Ok((manager, metadata))
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

struct CancelInputOnDrop(CancellationToken);

impl Drop for CancelInputOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Read a manually pasted redirect URL / `?code=…`. Unix uses bounded poll
/// intervals rather than Tokio's uncancellable blocking stdin adapter, so a
/// winning callback or cancelled flow cannot leave a competing reader behind.
async fn read_pasted_code(cancel: CancellationToken) -> Result<String> {
    let cancel = cancel.child_token();
    let _cancel_on_drop = CancelInputOnDrop(cancel.clone());
    #[cfg(unix)]
    {
        tokio::task::spawn_blocking(move || read_pasted_code_unix(&cancel))
            .await
            .context("stdin reader task")?
    }
    #[cfg(not(unix))]
    {
        use tokio::io::AsyncBufReadExt as _;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        tokio::select! {
            _ = cancel.cancelled() => bail!("MCP OAuth manual input cancelled"),
            line = lines.next_line() => {
                line.context("stdin reader")?
                    .context("stdin closed while waiting for the OAuth redirect")
            }
        }
    }
}

#[cfg(unix)]
fn read_pasted_code_unix(cancel: &CancellationToken) -> Result<String> {
    use std::io::Read as _;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = Vec::new();
    loop {
        if cancel.is_cancelled() {
            bail!("MCP OAuth manual input cancelled");
        }
        let mut pollfd = rustix::event::PollFd::new(&stdin, rustix::event::PollFlags::IN);
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        let ready = rustix::event::poll(std::slice::from_mut(&mut pollfd), Some(&timeout))
            .context("polling stdin")?;
        if ready == 0 {
            continue;
        }
        // One byte at a time avoids consuming a second pasted line when a
        // pipe already contains more than the current authorization needs.
        let mut chunk = [0u8; 1];
        let count = input.read(&mut chunk).context("reading stdin")?;
        if count == 0 {
            if output.is_empty() {
                bail!("stdin closed while waiting for the OAuth redirect");
            }
            break;
        }
        output.push(chunk[0]);
        if chunk[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn merged_scopes(
    configured: &[String],
    granted: Option<&str>,
    challenged: Option<&str>,
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    configured
        .iter()
        .map(String::as_str)
        .chain(granted.into_iter().flat_map(str::split_whitespace))
        .chain(challenged.into_iter().flat_map(str::split_whitespace))
        .filter(|scope| seen.insert((*scope).to_string()))
        .map(str::to_string)
        .collect()
}

/// Full interactive flow with an independent cancellation token.
pub async fn authorize(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
) -> Result<String> {
    authorize_cancellable(agent_dir, server, base_url, spec, CancellationToken::new()).await
}

/// Cancellable interactive authorization. Dropping this future is also safe:
/// requests, callback acceptance, and async stdin reads are all cancelled.
pub async fn authorize_cancellable(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
    cancel: CancellationToken,
) -> Result<String> {
    authorize_with_challenge(agent_dir, server, base_url, spec, cancel, None).await
}

async fn authorize_with_challenge(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
    cancel: CancellationToken,
    challenge: Option<&McpOAuthChallenge>,
) -> Result<String> {
    tokio::select! {
        _ = cancel.cancelled() => bail!("MCP OAuth sign-in cancelled for {server}"),
        result = authorize_inner(agent_dir, server, base_url, spec, challenge, cancel.clone()) => result,
    }
}

async fn authorize_inner(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
    challenge: Option<&McpOAuthChallenge>,
    cancel: CancellationToken,
) -> Result<String> {
    let (mut manager, _metadata) = authorization_manager(base_url, spec, challenge).await?;

    // Bind the loopback listener first: the redirect URI must carry the
    // port. A configured fixed port may still fall back to manual paste.
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
        let registered = tokio::time::timeout(
            OAUTH_REQUEST_TIMEOUT,
            manager.register_client("tack", &redirect_uri, &[]),
        )
        .await
        .context("dynamic client registration timed out")?
        .context(
            "dynamic client registration failed (set oauth.clientId in mcp.json for this server)",
        )?;
        client_id = Some(registered.client_id);
    }

    // A step-up challenge can name only missing scopes. Retain everything
    // already granted as well as the configured defaults (SEP-2350).
    let granted = load_token(agent_dir, server, base_url)?.and_then(|token| token.scope);
    let scopes = merged_scopes(
        &spec.scopes,
        granted.as_deref(),
        challenge.and_then(|challenge| challenge.params.scope.as_deref()),
    );
    let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let auth_url = tokio::time::timeout(
        OAUTH_REQUEST_TIMEOUT,
        manager.get_authorization_url(&scope_refs),
    )
    .await
    .context("authorization URL creation timed out")?
    .context("authorization url")?;
    // This URL is passed to the OS handler; file:/custom schemes could launch
    // local programs rather than a browser.
    require_http_url(&auth_url, "OAuth authorization URL")?;

    eprintln!("\nMCP server {server:?} requires OAuth authorization.");
    eprintln!("  Open: {auth_url}");
    eprintln!("  (or paste the redirected URL / ?code=… here)");
    crate::oauth_login::open_browser(&auth_url);

    let response = match listener {
        Some(listener) => {
            let path = target.path.clone();
            tokio::select! {
                callback = crate::oauth_login::await_callback_response(listener, &path) => {
                    callback.context("callback")?
                }
                pasted = read_pasted_code(cancel.clone()) => {
                    crate::oauth_login::parse_manual_response(&pasted?)
                        .context("no code in pasted input")?
                }
            }
        }
        None => {
            let pasted = read_pasted_code(cancel.clone()).await?;
            crate::oauth_login::parse_manual_response(&pasted).context("no code in pasted input")?
        }
    };

    // rmcp binds the code to the discovered/configured issuer according to
    // RFC 9207 before consuming the PKCE state.
    let token_response = tokio::time::timeout(
        OAUTH_REQUEST_TIMEOUT,
        manager.exchange_code_for_token_with_issuer(
            &response.code,
            response.state.as_deref().unwrap_or(""),
            response.issuer.as_deref(),
        ),
    )
    .await
    .context("code exchange timed out")?
    .context("code exchange failed")?;
    // RFC 6749 §5.1: omitted scope means the request was granted in full.
    let granted_scope = token_response
        .scopes()
        .map(|values| {
            values
                .iter()
                .map(|scope| scope.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .or_else(|| (!scopes.is_empty()).then(|| scopes.join(" ")));
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
        scope: granted_scope,
    };
    save_token(agent_dir, server, base_url, &token)?;
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
    access_token_with_cancel(
        agent_dir,
        server,
        base_url,
        spec,
        interactive,
        CancellationToken::new(),
    )
    .await
}

pub async fn access_token_with_cancel(
    agent_dir: &Path,
    server: &str,
    base_url: &str,
    spec: &McpOAuthSpec,
    interactive: bool,
    cancel: CancellationToken,
) -> Result<Option<String>> {
    if let Some(token) = cached_token(agent_dir, server, base_url)? {
        return Ok(Some(token));
    }
    let refreshed = tokio::select! {
        _ = cancel.cancelled() => bail!("MCP OAuth sign-in cancelled for {server}"),
        result = refresh_token(agent_dir, server, base_url, spec) => result,
    };
    if let Ok(Some(token)) = refreshed {
        return Ok(Some(token));
    }
    if cancel.is_cancelled() {
        bail!("MCP OAuth sign-in cancelled for {server}");
    }
    if !interactive {
        return Ok(None);
    }
    authorize_cancellable(agent_dir, server, base_url, spec, cancel)
        .await
        .map(Some)
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
            auth_server_metadata_url: o.auth_server_metadata_url.clone(),
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
    connect_all_oauth_cancellable(
        specs,
        agent_dir,
        interactive,
        callbacks,
        CancellationToken::new(),
    )
    .await
}

pub async fn connect_all_oauth_cancellable(
    specs: Vec<tack_tools::mcp::McpServerSpec>,
    agent_dir: &Path,
    interactive: bool,
    callbacks: tack_tools::mcp::McpClientCallbacks,
    cancel: CancellationToken,
) -> Vec<std::sync::Arc<tack_tools::mcp::McpConnection>> {
    connect_all_oauth_reporting_cancellable(specs, agent_dir, interactive, callbacks, cancel)
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
    connect_all_oauth_reporting_cancellable(
        specs,
        agent_dir,
        interactive,
        callbacks,
        CancellationToken::new(),
    )
    .await
}

pub async fn connect_all_oauth_reporting_cancellable(
    specs: Vec<tack_tools::mcp::McpServerSpec>,
    agent_dir: &Path,
    interactive: bool,
    callbacks: tack_tools::mcp::McpClientCallbacks,
    cancel: CancellationToken,
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
        let result = connect_one_oauth(&spec, agent_dir, interactive, &callbacks, &cancel).await;
        out.push(McpConnectOutcome { name, result });
    }
    out
}

async fn connect_cancellable(
    spec: &tack_tools::mcp::McpServerSpec,
    callbacks: tack_tools::mcp::McpClientCallbacks,
    cancel: &CancellationToken,
) -> Result<tack_tools::mcp::McpConnection, String> {
    if cancel.is_cancelled() {
        return Err("MCP OAuth sign-in cancelled".into());
    }
    let result = tokio::select! {
        _ = cancel.cancelled() => return Err("MCP OAuth sign-in cancelled".into()),
        result = tack_tools::mcp::connect_with(spec, callbacks) => result,
    };
    match result {
        Ok(conn) if cancel.is_cancelled() => {
            conn.cancel();
            Err("MCP OAuth sign-in cancelled".into())
        }
        other => other,
    }
}

pub(crate) async fn connect_one_oauth(
    spec: &tack_tools::mcp::McpServerSpec,
    agent_dir: &Path,
    interactive: bool,
    callbacks: &tack_tools::mcp::McpClientCallbacks,
    cancel: &CancellationToken,
) -> Result<std::sync::Arc<tack_tools::mcp::McpConnection>, String> {
    let url = spec.url().map(str::to_string);
    let wants_oauth = spec.oauth.is_some();
    match (wants_oauth, url) {
        (true, Some(url)) => {
            let oauth = spec_oauth(spec);
            // Cached/refreshed token first.
            let token = access_token_with_cancel(
                agent_dir,
                &spec.name,
                &url,
                &oauth,
                false,
                cancel.clone(),
            )
            .await
            .map_err(|error| format!("OAuth token resolution failed: {error}"))?;
            let with_token = |token: &str| {
                spec.clone().with_extra_headers(vec![(
                    "authorization".to_string(),
                    format!("Bearer {token}"),
                )])
            };
            let first = match &token {
                Some(t) => connect_cancellable(&with_token(t), callbacks.clone(), cancel).await,
                None => connect_cancellable(spec, callbacks.clone(), cancel).await,
            };
            match first {
                Ok(conn) => Ok(std::sync::Arc::new(conn)),
                Err(e) => {
                    let mut challenge = parse_oauth_challenge(&e, &url);
                    let step_up = challenge
                        .as_ref()
                        .is_some_and(|challenge| challenge.params.is_insufficient_scope());
                    let authish = e.contains("401")
                        || e.to_lowercase().contains("unauthor")
                        || e.contains("insufficient scope")
                        || e.contains("insufficient_scope")
                        || challenge.is_some();
                    if !authish {
                        tracing::warn!("MCP {}: {e}", spec.name);
                        return Err(e);
                    }

                    if !step_up {
                        // A rejected but unexpired token must still refresh. If
                        // another process already rotated it, the lock recheck
                        // returns that grant without another token request.
                        let refreshed = tokio::select! {
                            _ = cancel.cancelled() => return Err("MCP OAuth sign-in cancelled".into()),
                            result = refresh_token_with_challenge(
                                agent_dir,
                                &spec.name,
                                &url,
                                &oauth,
                                challenge.as_ref(),
                            ) => result,
                        };
                        if let Ok(Some(token)) = refreshed {
                            match connect_cancellable(
                                &with_token(&token),
                                callbacks.clone(),
                                cancel,
                            )
                            .await
                            {
                                Ok(conn) => return Ok(std::sync::Arc::new(conn)),
                                Err(retry_error) => {
                                    if let Some(next_challenge) =
                                        parse_oauth_challenge(&retry_error, &url)
                                    {
                                        challenge = Some(next_challenge);
                                    }
                                    tracing::warn!(
                                        "MCP {}: connect after token refresh failed: {retry_error}",
                                        spec.name
                                    );
                                }
                            }
                        }
                    }
                    if cancel.is_cancelled() {
                        return Err("MCP OAuth sign-in cancelled".into());
                    }
                    if !interactive {
                        tracing::warn!("MCP {}: {e}", spec.name);
                        return Err(e);
                    }

                    match authorize_with_challenge(
                        agent_dir,
                        &spec.name,
                        &url,
                        &oauth,
                        cancel.clone(),
                        challenge.as_ref(),
                    )
                    .await
                    {
                        Ok(token) => {
                            match connect_cancellable(
                                &with_token(&token),
                                callbacks.clone(),
                                cancel,
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
                        Err(e2) => {
                            tracing::warn!("MCP {}: OAuth flow failed: {e2}", spec.name);
                            Err(format!("OAuth flow failed: {e2}"))
                        }
                    }
                }
            }
        }
        _ => {
            // Stdio or OAuth-free remote: plain connect.
            match connect_cancellable(spec, callbacks.clone(), cancel).await {
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
            scope: None,
        };
        save_token(tmp.path(), "one", "https://example.com/mcp", &token).unwrap();
        save_token(tmp.path(), "two", "https://example.com/mcp", &token).unwrap();
        assert!(delete_token(tmp.path(), "one").unwrap());
        assert!(
            !delete_token(tmp.path(), "one").unwrap(),
            "second delete is a no-op"
        );
        assert!(
            cached_token(tmp.path(), "two", "https://example.com/mcp")
                .unwrap()
                .is_some(),
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
            scope: None,
        };
        save_token(tmp.path(), "github", "https://example.com/mcp", &token).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mcp-tokens.json kept mode {mode:o}");
    }

    #[test]
    fn token_cache_roundtrip_and_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let url = "https://example.com/mcp";
        let fresh = McpToken {
            access: "tok-fresh".into(),
            refresh: Some("rt".into()),
            expires_at: u64::MAX,
            client_id: None,
            scope: None,
        };
        save_token(tmp.path(), "github", url, &fresh).unwrap();
        assert_eq!(
            cached_token(tmp.path(), "github", url).unwrap().as_deref(),
            Some("tok-fresh")
        );
        let expired = McpToken {
            expires_at: 1,
            ..fresh
        };
        save_token(tmp.path(), "notion", url, &expired).unwrap();
        assert!(cached_token(tmp.path(), "notion", url).unwrap().is_none());
        assert!(cached_token(tmp.path(), "unknown", url).unwrap().is_none());
    }

    #[test]
    fn same_url_keeps_accounts_separate_by_server_name() {
        let tmp = tempfile::tempdir().unwrap();
        let url = "https://example.com/mcp";
        let mut work = McpToken {
            access: "work-token".into(),
            refresh: None,
            expires_at: u64::MAX,
            client_id: None,
            scope: None,
        };
        let mut personal = work.clone();
        personal.access = "personal-token".into();
        save_token(tmp.path(), "work", url, &work).unwrap();
        save_token(tmp.path(), "personal", url, &personal).unwrap();
        assert_eq!(
            load_token(tmp.path(), "work", url).unwrap().unwrap().access,
            "work-token"
        );
        assert_eq!(
            load_token(tmp.path(), "personal", url)
                .unwrap()
                .unwrap()
                .access,
            "personal-token"
        );
        let mut hyphen = work.clone();
        hyphen.access = "hyphen-token".into();
        let mut underscore = work.clone();
        underscore.access = "underscore-token".into();
        save_token(tmp.path(), "foo-bar", url, &hyphen).unwrap();
        save_token(tmp.path(), "foo_bar", url, &underscore).unwrap();
        assert_eq!(
            load_token(tmp.path(), "foo-bar", url)
                .unwrap()
                .unwrap()
                .access,
            "hyphen-token"
        );
        assert_eq!(
            load_token(tmp.path(), "foo_bar", url)
                .unwrap()
                .unwrap()
                .access,
            "underscore-token"
        );
        work.access = "other-url-token".into();
        save_token(tmp.path(), "work", "https://other.example.com/mcp", &work).unwrap();
        assert!(delete_token(tmp.path(), "work").unwrap());
        assert!(load_token(tmp.path(), "work", url).unwrap().is_none());
        assert!(
            load_token(tmp.path(), "work", "https://other.example.com/mcp")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            load_token(tmp.path(), "personal", url)
                .unwrap()
                .unwrap()
                .access,
            "personal-token"
        );
    }

    #[test]
    fn legacy_name_entry_is_migrated_without_losing_the_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let token = McpToken {
            access: "legacy-token".into(),
            refresh: Some("legacy-refresh".into()),
            expires_at: u64::MAX,
            client_id: Some("client".into()),
            scope: Some("read write".into()),
        };
        std::fs::write(
            tokens_path(tmp.path()),
            serde_json::json!({ "work": token }).to_string(),
        )
        .unwrap();
        let url = "https://example.com/mcp";
        assert_eq!(
            load_token(tmp.path(), "work", url).unwrap().unwrap(),
            McpToken {
                access: "legacy-token".into(),
                refresh: Some("legacy-refresh".into()),
                expires_at: u64::MAX,
                client_id: Some("client".into()),
                scope: Some("read write".into()),
            }
        );
        let stored = load_tokens(tmp.path());
        assert!(!stored.contains_key("work"));
        assert!(stored.contains_key(&token_key("work", url).unwrap()));
        assert!(load_token(tmp.path(), "personal", url).unwrap().is_none());
    }

    #[test]
    fn late_refresh_cannot_overwrite_login_or_logout() {
        let tmp = tempfile::tempdir().unwrap();
        let url = "https://example.com/mcp";
        let expected = McpToken {
            access: "old".into(),
            refresh: Some("old-refresh".into()),
            expires_at: 1,
            client_id: None,
            scope: Some("read".into()),
        };
        let fresh = McpToken {
            access: "fresh".into(),
            refresh: Some("fresh-refresh".into()),
            expires_at: u64::MAX,
            client_id: None,
            scope: Some("read".into()),
        };
        save_token(tmp.path(), "work", url, &expected).unwrap();
        let mut signed_in = fresh.clone();
        signed_in.access = "signed-in".into();
        save_token(tmp.path(), "work", url, &signed_in).unwrap();
        assert_eq!(
            save_refreshed_token(tmp.path(), "work", url, &expected, &fresh)
                .unwrap()
                .unwrap()
                .access,
            "signed-in"
        );
        assert!(delete_token(tmp.path(), "work").unwrap());
        assert!(
            save_refreshed_token(tmp.path(), "work", url, &expected, &fresh)
                .unwrap()
                .is_none()
        );
        assert!(load_token(tmp.path(), "work", url).unwrap().is_none());
    }

    #[test]
    fn parses_step_up_challenge_from_connection_error() {
        let challenge = parse_oauth_challenge(
            "MCP initialize failed for work (HTTP): insufficient scope: Bearer error=\"insufficient_scope\", scope=\"issues:write\"",
            "https://example.com/mcp",
        )
        .unwrap();
        assert!(challenge.params.is_insufficient_scope());
        assert_eq!(challenge.params.scope.as_deref(), Some("issues:write"));
    }

    #[test]
    fn scope_union_and_url_scheme_guards() {
        let scopes = merged_scopes(
            &["read".to_string(), "write".to_string()],
            Some("read admin"),
            Some("issues:write admin"),
        );
        assert_eq!(scopes, ["read", "write", "admin", "issues:write"]);
        for invalid in [
            "file:///tmp/authorize",
            "custom:authorize",
            "javascript:alert(1)",
        ] {
            assert!(require_http_url(invalid, "authorization").is_err());
        }
        assert!(require_http_url("https://example.com/authorize", "authorization").is_ok());
        assert!(require_secure_token_url("http://auth.example.com/token").is_err());
        assert!(require_secure_token_url("https://auth.example.com/token").is_ok());
        assert!(require_secure_token_url("http://127.0.0.1:8080/token").is_ok());
    }

    #[tokio::test]
    async fn configured_metadata_url_replaces_discovery() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = serde_json::json!({
                "issuer": format!("http://{address}"),
                "authorization_endpoint": format!("http://{address}/authorize"),
                "token_endpoint": format!("http://{address}/token"),
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let spec = McpOAuthSpec {
            auth_server_metadata_url: Some(format!("http://{address}/metadata")),
            ..Default::default()
        };
        // Nothing listens on the MCP URL: success proves discovery was replaced.
        let (_manager, metadata) = authorization_manager("http://127.0.0.1:1/mcp", &spec, None)
            .await
            .unwrap();
        assert_eq!(metadata.token_endpoint, format!("http://{address}/token"));
    }

    #[tokio::test]
    async fn cancelled_mcp_refresh_still_commits_rotation_and_scope() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut started_tx = Some(started_tx);
        let mut release_rx = Some(release_rx);
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 8192];
                let n = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                if request.starts_with("GET /metadata ") {
                    let body = serde_json::json!({
                        "issuer": format!("http://{address}"),
                        "authorization_endpoint": format!("http://{address}/authorize"),
                        "token_endpoint": format!("http://{address}/token"),
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                } else {
                    if let Some(tx) = started_tx.take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_rx.take() {
                        let _ = rx.await;
                    }
                    let body = "{\"access_token\":\"rotated\",\"refresh_token\":\"rotated-refresh\",\"expires_in\":3600}";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let base_url = format!("http://{address}/mcp");
        let old = McpToken {
            access: "old".into(),
            refresh: Some("old-refresh".into()),
            expires_at: 1,
            client_id: Some("client".into()),
            scope: Some("read write".into()),
        };
        save_token(tmp.path(), "work", &base_url, &old).unwrap();
        let spec = McpOAuthSpec {
            auth_server_metadata_url: Some(format!("http://{address}/metadata")),
            ..Default::default()
        };
        let dir = tmp.path().to_path_buf();
        let caller = tokio::spawn({
            let dir = dir.clone();
            let base_url = base_url.clone();
            let spec = spec.clone();
            async move { refresh_token(&dir, "work", &base_url, &spec).await }
        });
        started_rx.await.unwrap();
        caller.abort();
        let _ = caller.await;
        release_tx.send(()).unwrap();
        crate::oauth_lifecycle::settled().await;
        let stored = load_token(&dir, "work", &base_url).unwrap().unwrap();
        assert_eq!(stored.access, "rotated");
        assert_eq!(stored.refresh.as_deref(), Some("rotated-refresh"));
        assert_eq!(stored.scope.as_deref(), Some("read write"));
    }
}
