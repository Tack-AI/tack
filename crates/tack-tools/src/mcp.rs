//! MCP (Model Context Protocol) client: connects to stdio, Streamable HTTP,
//! and legacy SSE MCP servers and exposes their tools, resources, and prompts
//! as `AgentTool`s. TS pi deliberately has no built-in MCP; tack adds it
//! because ACP clients (Zed) pass `mcpServers` in `session/new`.
//!
//! Scope: stdio + Streamable HTTP + legacy SSE (2024-11-05, via the hand-rolled
//! transport in `mcp_sse`) transports, tool proxying, resources (list/read
//! meta-tools), prompts (exposed as tools).
//!
//! Resilience: connections heal themselves — a tool call that finds its
//! connection closed reconnects (re-spawn/re-dial) before failing, per-server
//! request timeouts reset while the server reports progress, and
//! `tools|resources|prompts/list_changed` notifications refresh the cached
//! capability lists in place.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::RoleClient;
use rmcp::model::ErrorData as McpError;
use tack_agent_core::{AgentTool, AgentToolResult};
// Sampling types are deprecated upstream by SEP-2577 but remain the only
// wire mechanism for server-initiated LLM requests; servers in the wild
// still use them, so we implement the (deprecated) client side.
#[allow(deprecated)]
use rmcp::model::{
    CallToolRequestParams, CreateMessageRequestParams, CreateMessageResult, ElicitRequestParams,
    ElicitResult, Prompt, ReadResourceRequestParams, Resource, ResourceTemplate,
    Tool as McpToolInfo,
};
use rmcp::service::{NotificationContext, Peer, RequestContext, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// OAuth 2.1 config for a remote server (mcp.json `"oauth": {clientId,
/// clientSecret, scopes, callbackPort, callbackUrl}` — everything optional;
/// dynamic registration + an ephemeral loopback callback fill the rest).
#[derive(Clone, Debug, Default)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    /// Confidential-client secret (registered clients that do not support
    /// dynamic registration). Env-var expansion (`${VAR}`) happens at
    /// config-parse time, never here.
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Fixed loopback callback port (`http://127.0.0.1:<port>/callback`) for
    /// providers whose registered redirect URI is fixed. Default: ephemeral.
    pub callback_port: Option<u16>,
    /// Full redirect URI override; must be HTTP on a loopback host
    /// (`localhost`, `127.0.0.1`, `[::1]`) — validated at flow start.
    pub callback_url: Option<String>,
}

/// How to reach an MCP server.
#[derive(Clone, Debug)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransport,
    /// OAuth config (HTTP/SSE only). `Some(…)` = authorize on 401.
    pub oauth: Option<McpOAuthConfig>,
    /// Strip sensitive inherited env vars from the stdio child (see
    /// [`Self::with_credential_stripping`]). Off by default:
    /// user-configured servers keep the host's full environment.
    pub strip_credentials: bool,
    /// `enabled: false` keeps the entry in config/status surfaces but never
    /// connects (mcp.json `enabled`). On by default.
    pub enabled: bool,
    /// Per-request timeout for tool/resource/prompt calls. `None` → the
    /// default (60 s); `Some(Duration::ZERO)` disables the limit. Progress
    /// notifications reset the clock (see [`Self::effective_request_timeout`]).
    pub request_timeout: Option<Duration>,
    /// How the server's tools reach the model (mcp.json `exposure`).
    pub exposure: McpExposure,
    /// Per-tool exposure overrides (mcp.json `toolExposure`): exact server
    /// tool names or `*` patterns. Exact names win over patterns; among
    /// patterns the LONGEST (most specific) wins — serde_json drops file
    /// order, so pi's first-match rule is upgraded to a deterministic
    /// specificity rule.
    pub tool_exposure: Vec<(String, McpExposure)>,
}

/// Default per-request timeout when the spec does not set one (matches TS
/// pi's 60 s). `"timeout": 0` in mcp.json disables the limit.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How a server's tools reach the model (mcp.json `exposure`; TS pi calls
/// the middle tier `deferred`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpExposure {
    /// Declared to the model like a built-in tool (tack's behavior so far).
    #[default]
    Direct,
    /// Not declared until `tool_search` loads a match; the tool then stays
    /// declared on that session branch (transcript-recorded).
    Deferred,
    /// Registered nowhere — unreachable.
    Hidden,
}

impl McpExposure {
    /// Parse an mcp.json exposure value. pi's `codemode` /
    /// `codemode-deferred` map to `Deferred` (tack has no codemode tool;
    /// tool_search is the closest indirect reach) with a warning from the
    /// caller.
    pub fn from_config(value: &str) -> Option<(Self, bool)> {
        match value {
            "direct" => Some((McpExposure::Direct, false)),
            "deferred" => Some((McpExposure::Deferred, false)),
            "hidden" => Some((McpExposure::Hidden, false)),
            "codemode" | "codemode-deferred" => Some((McpExposure::Deferred, true)),
            _ => None,
        }
    }
}

/// `*` glob for `toolExposure` patterns: `*` matches any character run
/// (including empty); every other character matches literally.
pub fn star_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        // A pattern without `*` is an exact match (the first/last anchor
        // branches alone would accept it as a prefix).
        return pattern == name;
    }
    let segments: Vec<&str> = pattern.split('*').collect();
    let mut rest = name;
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            continue;
        }
        let first = index == 0;
        let last = index == segments.len() - 1;
        if first && !pattern.starts_with('*') {
            let Some(stripped) = rest.strip_prefix(segment) else {
                return false;
            };
            rest = stripped;
        } else if last && !pattern.ends_with('*') {
            let Some(pos) = rest.rfind(segment) else {
                return false;
            };
            if pos + segment.len() != rest.len() {
                return false;
            }
        } else {
            let Some(pos) = rest.find(segment) else {
                return false;
            };
            rest = &rest[pos + segment.len()..];
        }
    }
    true
}

#[derive(Clone, Debug)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<PathBuf>,
    },
    /// Streamable HTTP (the current MCP HTTP transport; SSE-compatible).
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
    /// Legacy SSE transport (spec 2024-11-05; pre-Streamable-HTTP servers).
    Sse {
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl McpServerSpec {
    pub fn stdio(
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<PathBuf>,
    ) -> Self {
        McpServerSpec {
            name,
            transport: McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            },
            oauth: None,
            strip_credentials: false,
            enabled: true,
            request_timeout: None,
            exposure: McpExposure::default(),
            tool_exposure: Vec::new(),
        }
    }

    pub fn http(name: String, url: String, headers: Vec<(String, String)>) -> Self {
        McpServerSpec {
            name,
            transport: McpTransport::Http { url, headers },
            oauth: None,
            strip_credentials: false,
            enabled: true,
            request_timeout: None,
            exposure: McpExposure::default(),
            tool_exposure: Vec::new(),
        }
    }

    pub fn sse(name: String, url: String, headers: Vec<(String, String)>) -> Self {
        McpServerSpec {
            name,
            transport: McpTransport::Sse { url, headers },
            oauth: None,
            strip_credentials: false,
            enabled: true,
            request_timeout: None,
            exposure: McpExposure::default(),
            tool_exposure: Vec::new(),
        }
    }

    /// Attach an OAuth config (HTTP/SSE only).
    pub fn with_oauth(mut self, oauth: McpOAuthConfig) -> Self {
        self.oauth = Some(oauth);
        self
    }

    /// Opt in to credential stripping for stdio spawns: sensitive
    /// inherited env vars (API keys, tokens — see
    /// `is_sensitive_env_key`) the spec does not explicitly declare
    /// are removed from the child process. For PLUGIN carriers only —
    /// a plugin is third-party code and the host's API keys are not
    /// its business. User-configured MCP servers deliberately keep the
    /// host's full environment (the existing public behavior).
    pub fn with_credential_stripping(mut self) -> Self {
        self.strip_credentials = true;
        self
    }

    /// Opt out of connecting (`mcp.json `"enabled": false`): the entry
    /// stays visible in config/status surfaces but is skipped at connect.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Override the per-request timeout (mcp.json `timeout`, seconds).
    /// `Some(Duration::ZERO)` disables the limit; `None` restores the
    /// default (60 s).
    pub fn with_request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Effective per-request timeout: the spec default (60 s) unless
    /// overridden; `Some(0)` in config means no limit.
    pub fn effective_request_timeout(&self) -> Option<Duration> {
        match self.request_timeout {
            None => Some(DEFAULT_REQUEST_TIMEOUT),
            Some(d) if d.is_zero() => None,
            Some(d) => Some(d),
        }
    }

    /// Set the server-level tool exposure (mcp.json `exposure`).
    pub fn with_exposure(mut self, exposure: McpExposure) -> Self {
        self.exposure = exposure;
        self
    }

    /// Set per-tool exposure overrides (mcp.json `toolExposure`).
    pub fn with_tool_exposure(mut self, entries: Vec<(String, McpExposure)>) -> Self {
        self.tool_exposure = entries;
        self
    }

    /// Effective exposure for one SERVER tool name: exact `toolExposure`
    /// keys win over `*` patterns; among patterns the longest (most
    /// specific) wins; otherwise the server-level `exposure`.
    pub fn effective_exposure(&self, server_tool_name: &str) -> McpExposure {
        let mut best: Option<(usize, McpExposure)> = None;
        for (pattern, exposure) in &self.tool_exposure {
            if pattern == server_tool_name {
                return *exposure;
            }
            if pattern.contains('*') && star_match(pattern, server_tool_name) {
                let specificity = pattern.len();
                if best.is_none_or(|(len, _)| specificity > len) {
                    best = Some((specificity, *exposure));
                }
            }
        }
        best.map(|(_, exposure)| exposure).unwrap_or(self.exposure)
    }

    /// Extra headers merged in at connect time (e.g. a cached OAuth token).
    pub fn with_extra_headers(mut self, extra: Vec<(String, String)>) -> Self {
        match &mut self.transport {
            McpTransport::Http { headers, .. } | McpTransport::Sse { headers, .. } => {
                headers.extend(extra);
            }
            _ => {}
        }
        self
    }

    /// Base URL for HTTP/SSE transports (OAuth metadata discovery root).
    pub fn url(&self) -> Option<&str> {
        match &self.transport {
            McpTransport::Http { url, .. } | McpTransport::Sse { url, .. } => Some(url),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Client handler: server-initiated requests (sampling + elicitation)
// ---------------------------------------------------------------------------

/// Handles `sampling/createMessage`: the MCP server asks the client to run
/// an LLM completion. Implementations must treat every server-provided
/// message as untrusted data and run the completion in an isolated context
/// (never the main session).
#[allow(deprecated)]
#[async_trait]
pub trait SamplingHandler: Send + Sync + std::fmt::Debug {
    async fn create_message(
        &self,
        server: &str,
        params: CreateMessageRequestParams,
    ) -> Result<CreateMessageResult, String>;
}

/// Handles `elicitation/create`: the MCP server asks the user for structured
/// input. Headless implementations should decline.
#[async_trait]
pub trait ElicitationHandler: Send + Sync + std::fmt::Debug {
    async fn elicit(
        &self,
        server: &str,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, String>;
}

/// Optional callbacks for server-initiated requests. A missing callback
/// means the capability is NOT advertised at initialize time (well-behaved
/// servers then never send the request; misbehaving ones get
/// method-not-found for sampling and an automatic decline for elicitation).
#[derive(Clone, Debug, Default)]
pub struct McpClientCallbacks {
    #[allow(deprecated)]
    pub sampling: Option<Arc<dyn SamplingHandler>>,
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
}

impl McpClientCallbacks {
    #[allow(deprecated)]
    pub fn with_sampling(mut self, handler: Arc<dyn SamplingHandler>) -> Self {
        self.sampling = Some(handler);
        self
    }

    pub fn with_elicitation(mut self, handler: Arc<dyn ElicitationHandler>) -> Self {
        self.elicitation = Some(handler);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.sampling.is_none() && self.elicitation.is_none()
    }
}

/// Discovered server capabilities, refreshed in place when the server
/// announces a `list_changed` notification.
#[derive(Debug, Default)]
struct DiscoveredLists {
    tools: Vec<McpToolInfo>,
    resources: Vec<Resource>,
    resource_templates: Vec<ResourceTemplate>,
    prompts: Vec<Prompt>,
}

/// State shared between the client handler (server-initiated requests AND
/// notifications) and the [`McpConnection`] that owns the session. The
/// handler is created before the transport handshake, so the peer arrives
/// late via [`HandlerShared::peer`].
#[derive(Debug)]
struct HandlerShared {
    server_name: String,
    /// Set once `initialize` completes; list-refresh tasks use it to
    /// re-query the server after a `list_changed` notification.
    peer: std::sync::OnceLock<Peer<RoleClient>>,
    lists: std::sync::RwLock<DiscoveredLists>,
    /// Bumped on every progress notification: request timeouts reset while
    /// progress flows (a long call that reports progress is not stuck).
    progress: AtomicU64,
    /// Bumped whenever a `list_changed` refresh lands, so status surfaces
    /// notice a changed tool set under a cached connection.
    generation: AtomicU64,
}

impl HandlerShared {
    fn new(server_name: String) -> Self {
        HandlerShared {
            server_name,
            peer: std::sync::OnceLock::new(),
            lists: std::sync::RwLock::new(DiscoveredLists::default()),
            progress: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    fn lists_read(&self) -> std::sync::RwLockReadGuard<'_, DiscoveredLists> {
        self.lists.read().unwrap_or_else(|e| e.into_inner())
    }

    fn lists_write(&self) -> std::sync::RwLockWriteGuard<'_, DiscoveredLists> {
        self.lists.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-query one capability list after a `list_changed` notification.
    /// Runs in a spawned task: the notification path must not block on a
    /// request round-trip with the same session.
    fn refresh(self: &Arc<Self>, kind: ListKind) {
        let Some(peer) = self.peer.get().cloned() else {
            return;
        };
        let shared = self.clone();
        tokio::spawn(async move {
            let result = match kind {
                ListKind::Tools => peer.list_all_tools().await.map(|tools| {
                    shared.lists_write().tools = tools;
                }),
                ListKind::Resources => {
                    let resources = peer.list_all_resources().await;
                    let templates = peer.list_all_resource_templates().await;
                    match (resources, templates) {
                        (Ok(resources), Ok(templates)) => {
                            let mut lists = shared.lists_write();
                            lists.resources = resources;
                            lists.resource_templates = templates;
                            Ok(())
                        }
                        (Err(e), _) | (_, Err(e)) => Err(e),
                    }
                }
                ListKind::Prompts => peer.list_all_prompts().await.map(|prompts| {
                    shared.lists_write().prompts = prompts;
                }),
            };
            match result {
                Ok(()) => {
                    shared.generation.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        "MCP {}: {:?} list changed; refreshed",
                        shared.server_name,
                        kind
                    );
                }
                Err(e) => tracing::warn!(
                    "MCP {}: {:?} refresh after list_changed failed: {e}",
                    shared.server_name,
                    kind
                ),
            }
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum ListKind {
    Tools,
    Resources,
    Prompts,
}

/// rmcp `ClientHandler` bridging server-initiated requests to the configured
/// callbacks and `list_changed`/progress notifications to the shared
/// connection state. Also used (with empty callbacks) for plain connections
/// so `McpConnection` has one concrete service type.
#[derive(Clone, Debug)]
pub struct TackClientHandler {
    server_name: String,
    callbacks: McpClientCallbacks,
    shared: Arc<HandlerShared>,
}

impl TackClientHandler {
    pub fn new(server_name: String, callbacks: McpClientCallbacks) -> Self {
        TackClientHandler {
            shared: Arc::new(HandlerShared::new(server_name.clone())),
            server_name,
            callbacks,
        }
    }

    fn with_shared(
        server_name: String,
        callbacks: McpClientCallbacks,
        shared: Arc<HandlerShared>,
    ) -> Self {
        TackClientHandler {
            server_name,
            callbacks,
            shared,
        }
    }
}

#[allow(deprecated)]
impl rmcp::ClientHandler for TackClientHandler {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, McpError> {
        let Some(handler) = &self.callbacks.sampling else {
            return Err(McpError::method_not_found::<
                rmcp::model::CreateMessageRequestMethod,
            >());
        };
        handler
            .create_message(&self.server_name, params)
            .await
            .map_err(|e| McpError::internal_error(e, None))
    }

    async fn create_elicitation(
        &self,
        params: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, McpError> {
        let Some(handler) = &self.callbacks.elicitation else {
            // No UI (or feature off): decline rather than erroring so the
            // server's tool call can continue gracefully.
            return Ok(ElicitResult::new(rmcp::model::ElicitationAction::Decline));
        };
        handler
            .elicit(&self.server_name, params)
            .await
            .map_err(|e| McpError::internal_error(e, None))
    }

    async fn on_progress(
        &self,
        _params: rmcp::model::ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.shared.progress.fetch_add(1, Ordering::Relaxed);
    }

    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        // Server log notifications go to the session trace (there is no
        // mcp.log file like TS pi's); the target makes them filterable.
        tracing::info!(
            target: "mcp_server_log",
            server = self.server_name.as_str(),
            level = ?params.level,
            logger = params.logger.as_deref().unwrap_or(""),
            "{}",
            params.data
        );
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.shared.refresh(ListKind::Tools);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.shared.refresh(ListKind::Resources);
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.shared.refresh(ListKind::Prompts);
    }

    fn get_info(&self) -> rmcp::model::ClientInfo {
        let mut info = rmcp::model::ClientInfo::default();
        if self.callbacks.sampling.is_some() {
            info.capabilities.sampling = Some(rmcp::model::SamplingCapability::default());
        }
        if self.callbacks.elicitation.is_some() {
            info.capabilities.elicitation = Some(
                rmcp::model::ElicitationCapability::new()
                    .with_form(rmcp::model::FormElicitationCapability::new()),
            );
        }
        info
    }
}

/// The swappable live parts of a connection: replaced wholesale on
/// reconnect (new process/session, new capability lists).
struct ConnParts {
    service: RunningService<RoleClient, TackClientHandler>,
    shared: Arc<HandlerShared>,
}

/// Rebuilds a connection from scratch (re-spawn the child / re-dial the
/// HTTP session) — the reconnect path. `None` for in-memory test
/// transports, which cannot reconnect.
#[doc(hidden)]
pub type McpConnector = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<McpConnection, String>> + Send>,
        > + Send
        + Sync,
>;

/// A live MCP server connection plus its discovered capabilities.
///
/// Self-healing: [`McpConnection::ensure_connected`] reconnects a closed
/// session before the next call (re-spawning stdio children), and
/// `list_changed` notifications refresh the capability lists in place —
/// both invisible to holders of the `Arc`.
pub struct McpConnection {
    pub name: String,
    /// The spec this connection was built from (with any connect-time
    /// headers, e.g. OAuth bearer tokens, already merged). Drives
    /// reconnects.
    spec: Option<McpServerSpec>,
    /// Resolved per-request timeout (from the spec, the 60 s default, or a
    /// test override); `None` = no limit.
    request_timeout: Option<Duration>,
    parts: std::sync::RwLock<ConnParts>,
    reconnect_lock: tokio::sync::Mutex<()>,
    connector: Option<McpConnector>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shared = self.shared();
        let lists = shared.lists_read();
        f.debug_struct("McpConnection")
            .field("name", &self.name)
            .field("tools", &lists.tools.len())
            .field(
                "resources",
                &(lists.resources.len() + lists.resource_templates.len()),
            )
            .field("prompts", &lists.prompts.len())
            .finish()
    }
}

impl McpConnection {
    fn new(
        name: String,
        spec: Option<McpServerSpec>,
        service: RunningService<RoleClient, TackClientHandler>,
        shared: Arc<HandlerShared>,
        connector: Option<McpConnector>,
    ) -> Self {
        let request_timeout = spec
            .as_ref()
            .map(McpServerSpec::effective_request_timeout)
            .unwrap_or(Some(DEFAULT_REQUEST_TIMEOUT));
        McpConnection {
            name,
            spec,
            request_timeout,
            parts: std::sync::RwLock::new(ConnParts { service, shared }),
            reconnect_lock: tokio::sync::Mutex::new(()),
            connector,
        }
    }

    fn parts_read(&self) -> std::sync::RwLockReadGuard<'_, ConnParts> {
        self.parts.read().unwrap_or_else(|e| e.into_inner())
    }

    fn parts_write(&self) -> std::sync::RwLockWriteGuard<'_, ConnParts> {
        self.parts.write().unwrap_or_else(|e| e.into_inner())
    }

    fn shared(&self) -> Arc<HandlerShared> {
        self.parts_read().shared.clone()
    }

    /// Current session peer. Cloned per call so a reconnect swap never
    /// leaves a tool holding a stale session.
    pub fn peer(&self) -> Peer<RoleClient> {
        self.parts_read().service.peer().clone()
    }

    /// The spec this connection was built from (connect-time headers
    /// included), when reconnectable.
    pub fn spec(&self) -> Option<&McpServerSpec> {
        self.spec.as_ref()
    }

    pub fn tools(&self) -> Vec<McpToolInfo> {
        self.shared().lists_read().tools.clone()
    }

    pub fn resources(&self) -> Vec<Resource> {
        self.shared().lists_read().resources.clone()
    }

    pub fn resource_templates(&self) -> Vec<ResourceTemplate> {
        self.shared().lists_read().resource_templates.clone()
    }

    pub fn prompts(&self) -> Vec<Prompt> {
        self.shared().lists_read().prompts.clone()
    }

    /// Generation counter bumped on every `list_changed` refresh — status
    /// surfaces can cheaply notice a tool set changing under a cached pool.
    pub fn generation(&self) -> u64 {
        self.shared().generation.load(Ordering::Relaxed)
    }

    /// Current progress-notification count: bumped by the client handler
    /// on every progress notification; request timeouts reset while it moves.
    fn progress_value(&self) -> u64 {
        self.shared().progress.load(Ordering::Relaxed)
    }

    /// Per-request timeout (spec default 60 s; `Some(0)` = no limit).
    pub fn request_timeout(&self) -> Option<Duration> {
        self.request_timeout
    }

    /// Override the request timeout (test seam; production timeouts come
    /// from the spec's mcp.json `timeout`).
    #[doc(hidden)]
    pub fn with_request_timeout_override(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// True when the underlying service/transport is gone (server child
    /// died, transport closed or cancelled). A call on a closed connection
    /// first tries [`McpConnection::ensure_connected`].
    pub fn is_closed(&self) -> bool {
        let parts = self.parts_read();
        parts.service.is_closed() || parts.service.peer().is_transport_closed()
    }

    pub fn has_resources(&self) -> bool {
        let shared = self.shared();
        let lists = shared.lists_read();
        !lists.resources.is_empty() || !lists.resource_templates.is_empty()
    }

    /// Cancel the service (server child killed / HTTP session closed).
    /// Idempotent; in-flight calls resolve with closed-channel errors.
    /// Dropping the last `Arc<McpConnection>` cancels too — this is the
    /// explicit, prompt version for plugin shutdown.
    pub fn cancel(&self) {
        self.parts_read().service.cancellation_token().cancel();
    }

    /// Reconnect a closed session before the next call: re-runs the
    /// original connect (re-spawn/re-dial + handshake + capability probe)
    /// and swaps the live parts. Concurrent callers serialize; the second
    /// one finds a healthy connection and returns immediately. No-op when
    /// the connection is still alive.
    pub async fn ensure_connected(&self) -> Result<(), String> {
        if !self.is_closed() {
            return Ok(());
        }
        let _guard = self.reconnect_lock.lock().await;
        if !self.is_closed() {
            // A concurrent caller already healed the connection.
            return Ok(());
        }
        let Some(connector) = &self.connector else {
            return Err(format!(
                "MCP {}: connection lost (this transport cannot reconnect)",
                self.name
            ));
        };
        tracing::info!("MCP {}: connection closed; reconnecting", self.name);
        let fresh = connector().await?;
        let fresh_parts = fresh.into_parts();
        let old = std::mem::replace(&mut *self.parts_write(), fresh_parts);
        // Reap the half-dead child/session explicitly instead of relying
        // on drop order.
        old.service.cancellation_token().cancel();
        Ok(())
    }

    fn into_parts(self) -> ConnParts {
        self.parts.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

/// Connect to a server, list its tools/resources/prompts. Capability probes
/// that the server doesn't support degrade to empty lists.
pub async fn connect(spec: &McpServerSpec) -> Result<McpConnection, String> {
    connect_with(spec, McpClientCallbacks::default()).await
}

/// True for environment variable names that typically carry credentials
/// (`OPENAI_API_KEY`, `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, ...).
/// Local copy of `tack_ext::process::is_sensitive_env_key` — duplicated
/// because tack-tools deliberately does not depend on tack-ext (the
/// dependency direction is tack-app → all), and the strip rule must
/// match the v3 process carrier's exactly: a plugin is third-party code
/// and the host's API keys are not its business.
fn is_sensitive_env_key(key: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_ACCESS_KEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_CREDENTIALS",
        "_PRIVATE_KEY",
    ];
    let upper = key.to_ascii_uppercase();
    SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
        || matches!(
            upper.as_str(),
            "API_KEY" | "TOKEN" | "SECRET" | "PASSWORD" | "CREDENTIALS"
        )
}

/// Strip sensitive inherited env vars from a plugin-carrier child
/// command, mirroring the v3 process carrier's
/// `tack_ext::process::env_vars_to_strip`: sensitive-looking vars the
/// server spec did NOT explicitly declare are `env_remove`d
/// individually (never `env_clear` — clearing breaks process startup
/// on Windows, where e.g. `SystemRoot` is required).
fn strip_credential_env(
    command: &mut tokio::process::Command,
    parent_keys: &[String],
    declared: &[(String, String)],
) {
    for key in parent_keys {
        if is_sensitive_env_key(key) && !declared.iter().any(|(dk, _)| dk == key) {
            command.env_remove(key);
        }
    }
}

/// `connect` with client-side callbacks for server-initiated requests
/// (sampling / elicitation). The returned connection is reconnectable:
/// [`McpConnection::ensure_connected`] re-runs this exact connect when the
/// session dies.
pub async fn connect_with(
    spec: &McpServerSpec,
    callbacks: McpClientCallbacks,
) -> Result<McpConnection, String> {
    let spec_for_reconnect = spec.clone();
    let callbacks_for_reconnect = callbacks.clone();
    let connector: McpConnector = Arc::new(move || {
        reconnect_boxed(spec_for_reconnect.clone(), callbacks_for_reconnect.clone())
    });
    connect_spec_transport(spec, callbacks, Some(connector)).await
}

/// Boxed re-connect behind the reconnect connector. The concrete `Send`
/// return type breaks the auto-trait inference cycle: `connect_with`'s
/// future holds the connector across awaits, so an async block INSIDE the
/// closure would make the future's Send-ness depend on itself.
fn reconnect_boxed(
    spec: McpServerSpec,
    callbacks: McpClientCallbacks,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<McpConnection, String>> + Send>> {
    Box::pin(async move { connect_with(&spec, callbacks).await })
}

async fn connect_spec_transport(
    spec: &McpServerSpec,
    callbacks: McpClientCallbacks,
    connector: Option<McpConnector>,
) -> Result<McpConnection, String> {
    use rmcp::ServiceExt;
    let shared = Arc::new(HandlerShared::new(spec.name.clone()));
    let handler = TackClientHandler::with_shared(spec.name.clone(), callbacks, shared.clone());
    let service = match &spec.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut command = tokio::process::Command::new(command);
            command.args(args);
            for (k, v) in env {
                command.env(k, v);
            }
            if spec.strip_credentials {
                // A plugin carrier is third-party code: strip the host's
                // credentials like the v3 process carrier does. Only
                // reached when the caller opted in — user-configured
                // servers inherit the full environment.
                let parent_keys: Vec<String> = std::env::vars_os()
                    .filter_map(|(k, _)| k.into_string().ok())
                    .collect();
                strip_credential_env(&mut command, &parent_keys, env);
            }
            if let Some(cwd) = cwd {
                command.current_dir(cwd);
            }
            #[cfg(windows)]
            {
                const CREATE_NO_WINDOW: u32 = 0x08000000;
                command.creation_flags(CREATE_NO_WINDOW);
            }
            let transport = TokioChildProcess::new(command)
                .map_err(|e| format!("failed to spawn MCP server {}: {e}", spec.name))?;
            handler
                .clone()
                .serve(transport)
                .await
                .map_err(|e| format!("MCP initialize failed for {}: {e}", spec.name))?
        }
        McpTransport::Http { url, headers } => {
            use std::collections::HashMap;
            use std::str::FromStr;
            let mut custom_headers: HashMap<http::HeaderName, http::HeaderValue> = HashMap::new();
            for (k, v) in headers {
                let name = http::HeaderName::from_str(k)
                    .map_err(|e| format!("bad header name {k:?}: {e}"))?;
                let value = http::HeaderValue::from_str(v)
                    .map_err(|e| format!("bad header value for {k:?}: {e}"))?;
                custom_headers.insert(name, value);
            }
            let mut config =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    url.as_str(),
                );
            config.custom_headers = custom_headers;
            let transport = StreamableHttpClientTransport::from_config(config);
            handler
                .clone()
                .serve(transport)
                .await
                .map_err(|e| format!("MCP initialize failed for {} (HTTP): {e}", spec.name))?
        }
        McpTransport::Sse { url, headers } => {
            let headers: std::collections::HashMap<String, String> =
                headers.iter().cloned().collect();
            let transport = crate::mcp_sse::SseClientTransport::connect(url, &headers)
                .map_err(|e| format!("MCP SSE connect failed for {}: {e}", spec.name))?;
            handler
                .clone()
                .serve(transport)
                .await
                .map_err(|e| format!("MCP initialize failed for {} (SSE): {e}", spec.name))?
        }
    };
    finish_connection(Some(spec.clone()), service, shared, connector).await
}

/// Test seam (also used by tack-app's plugin tests): connect over an
/// arbitrary in-memory transport instead of spawning a child or dialing
/// HTTP — a fixture server answers on the other end of the duplex.
/// In-memory transports cannot reconnect (`ensure_connected` reports the
/// connection as unrecoverable once the stream dies).
#[doc(hidden)]
pub async fn connect_transport<S>(
    name: &str,
    stream: S,
    callbacks: McpClientCallbacks,
) -> Result<McpConnection, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    connect_transport_with(name, stream, callbacks, None).await
}

/// [`connect_transport`] with a reconnect connector (test-only: the
/// connector re-establishes a fresh transport + fixture server).
#[doc(hidden)]
pub async fn connect_transport_with<S>(
    name: &str,
    stream: S,
    callbacks: McpClientCallbacks,
    connector: Option<McpConnector>,
) -> Result<McpConnection, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    use rmcp::ServiceExt;
    let shared = Arc::new(HandlerShared::new(name.to_string()));
    let handler = TackClientHandler::with_shared(name.to_string(), callbacks, shared.clone());
    let service = handler
        .serve(stream)
        .await
        .map_err(|e| format!("MCP initialize failed for {name}: {e}"))?;
    finish_connection(None, service, shared, connector).await
}

/// Shared post-handshake tail of the connect paths: publish the peer (so
/// `list_changed` refreshes can fire), probe the server's
/// tools/resources/prompts (unsupported capability probes degrade to empty
/// lists), and assemble the connection.
async fn finish_connection(
    spec: Option<McpServerSpec>,
    service: RunningService<RoleClient, TackClientHandler>,
    shared: Arc<HandlerShared>,
    connector: Option<McpConnector>,
) -> Result<McpConnection, String> {
    let name = spec
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| shared.server_name.clone());
    let _ = shared.peer.set(service.peer().clone());
    let tools = service
        .list_all_tools()
        .await
        .map_err(|e| format!("MCP tools/list failed for {name}: {e}"))?;

    // Capability probes degrade gracefully (older servers may not implement
    // resources/prompts at all).
    let resources = service.list_all_resources().await.unwrap_or_default();
    let resource_templates = service
        .list_all_resource_templates()
        .await
        .unwrap_or_default();
    let prompts = service.list_all_prompts().await.unwrap_or_default();

    *shared.lists_write() = DiscoveredLists {
        tools,
        resources,
        resource_templates,
        prompts,
    };
    Ok(McpConnection::new(name, spec, service, shared, connector))
}

/// Connect to several servers concurrently; failures are logged and skipped
/// (a broken MCP server must not fail session creation).
pub async fn connect_all(specs: Vec<McpServerSpec>) -> Vec<Arc<McpConnection>> {
    connect_all_with(specs, McpClientCallbacks::default()).await
}

/// `connect_all` with client-side callbacks applied to every server.
/// Disabled specs (`enabled: false`) are skipped up front — they stay in
/// config/status surfaces but never spawn processes.
pub async fn connect_all_with(
    specs: Vec<McpServerSpec>,
    callbacks: McpClientCallbacks,
) -> Vec<Arc<McpConnection>> {
    let tasks: Vec<_> = specs
        .into_iter()
        .filter(|spec| {
            if !spec.enabled {
                tracing::info!("MCP {}: disabled in config; skipping connect", spec.name);
            }
            spec.enabled
        })
        .map(|spec| {
            let callbacks = callbacks.clone();
            tokio::spawn(async move {
                match connect_with(&spec, callbacks).await {
                    Ok(conn) => Some(Arc::new(conn)),
                    Err(e) => {
                        tracing::warn!("{e}");
                        None
                    }
                }
            })
        })
        .collect();
    let mut out = Vec::new();
    for task in tasks {
        if let Ok(Some(conn)) = task.await {
            out.push(conn);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Run one MCP request with the connection's per-server timeout: the clock
/// resets whenever the server reports progress (a long call that streams
/// progress is not stuck); `None` disables the limit. Cancellation (user
/// abort) always wins.
async fn call_with_timeout<F, T>(
    conn: &McpConnection,
    what: &str,
    call: F,
    cancel: CancellationToken,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, rmcp::service::ServiceError>>,
{
    let Some(limit) = conn.request_timeout() else {
        return match tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = call => r,
        } {
            Ok(value) => Ok(value),
            Err(e) => Err(format!("{what} failed: {e}")),
        };
    };
    let mut call = std::pin::pin!(call);
    let mut seen_progress = conn.progress_value();
    let timer = tokio::time::sleep(limit);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = &mut call => {
                return r.map_err(|e| format!("{what} failed: {e}"));
            }
            _ = &mut timer => {
                let now = conn.progress_value();
                if now != seen_progress {
                    // Progress flowed: the server is working, reset the clock.
                    seen_progress = now;
                    timer.as_mut().reset(tokio::time::Instant::now() + limit);
                    continue;
                }
                return Err(format!(
                    "{what} failed: timed out after {}s (no progress)",
                    limit.as_secs()
                ));
            }
        }
    }
}

/// Server-declared tool annotations (MCP `ToolAnnotations`), surfaced to
/// the permission layer so read-only MCP tools can skip prompts the same
/// way built-in read tools do.
#[derive(Clone, Copy, Debug, Default)]
pub struct McpToolAnnotations {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

fn annotations_of(info: &McpToolInfo) -> Option<McpToolAnnotations> {
    let a = info.annotations.as_ref()?;
    Some(McpToolAnnotations {
        // MCP spec defaults when a hint is absent: readOnly=false,
        // destructive=true, idempotent=false, openWorld=true.
        read_only: a.read_only_hint.unwrap_or(false),
        destructive: a.destructive_hint.unwrap_or(true),
        idempotent: a.idempotent_hint.unwrap_or(false),
        open_world: a.open_world_hint.unwrap_or(true),
    })
}

/// Full-name → annotations registry: the permission layer's read-only
/// classification is a sync name→bool lookup, so `mcp_tools_with`
/// publishes each tool's hints here when the tool set is built.
static ANNOTATION_REGISTRY: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<&'static str, McpToolAnnotations>>,
> = std::sync::OnceLock::new();

fn register_tool_annotations(full_name: &'static str, info: &McpToolInfo) {
    let Some(annotations) = annotations_of(info) else {
        return;
    };
    let registry = ANNOTATION_REGISTRY
        .get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    registry
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(full_name, annotations);
}

/// Insert annotations directly (permission-layer tests; production
/// registration happens in `mcp_tools_with`).
#[doc(hidden)]
pub fn register_tool_annotations_for_test(
    full_name: &'static str,
    annotations: McpToolAnnotations,
) {
    let registry = ANNOTATION_REGISTRY
        .get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    registry
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(full_name, annotations);
}

/// Annotations an MCP server declared for the tool behind `full_name`
/// (`mcp__<server>__<tool>`), if any. Read-only gating
/// (`tack_app::permissions::is_read_only_tool`) consults this; approval
/// chains receive the same hints as evidence.
pub fn mcp_tool_annotations(full_name: &str) -> Option<McpToolAnnotations> {
    let registry = ANNOTATION_REGISTRY.get()?;
    registry
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(full_name)
        .copied()
}

/// AgentTool proxy for one MCP tool.
pub struct McpTool {
    server_name: String,
    info: McpToolInfo,
    /// The owning connection: holds the session (and its re-spawned
    /// successor after a reconnect) alive and provides the per-call peer.
    conn: Arc<McpConnection>,
    /// Interned prefixed name (see intern_tool_name).
    full_name: &'static str,
    /// Prompt-injection defense: set when a result enters the context.
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// mcp.json `exposure: "deferred"`: start in the tool_search pool.
    starts_deferred: bool,
}

/// Mark the untrusted flag and wrap text so the model treats MCP output as
/// data, not instructions (prompt-injection defense).
fn wrap_untrusted(flag: &Arc<std::sync::atomic::AtomicBool>, source: &str, text: &str) -> String {
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    format!("<untrusted_content source=\"{source}\">\n{text}\n</untrusted_content>")
}

/// Apply the same truncation budget as bash/read output: MCP servers are
/// external processes whose results must not flood the context unbounded.
fn truncate_mcp_output(text: &str) -> String {
    let truncation = crate::truncate::truncate_tail(text, None, None);
    if !truncation.truncated {
        return text.to_string();
    }
    format!(
        "{}\n\n[MCP output truncated: showing lines {}-{} of {} ({})]",
        truncation.content,
        truncation.total_lines - truncation.output_lines + 1,
        truncation.total_lines,
        truncation.total_lines,
        crate::truncate::format_size(truncation.output_bytes),
    )
}

/// Cap on MCP image blocks (base64 chars), matching the read tool's budget
/// (read.rs MAX_IMAGE_BASE64_BYTES). Text output is truncated via
/// truncate_mcp_output but image blocks would otherwise pass through
/// verbatim: a hostile/buggy server must not flood the context with
/// unbounded base64.
const MAX_MCP_IMAGE_BASE64_BYTES: usize = (4.5 * 1024.0 * 1024.0) as usize;

/// Build an image content block, capped: over-size images become an
/// in-band text note instead of the base64 payload.
fn mcp_image_block(data: &str, mime_type: &str) -> tack_ai::InputContentBlock {
    if data.len() > MAX_MCP_IMAGE_BASE64_BYTES {
        return tack_ai::InputContentBlock::text(format!(
            "[MCP image omitted: {mime_type}, {} of base64 exceeds the {} cap]",
            crate::truncate::format_size(data.len()),
            crate::truncate::format_size(MAX_MCP_IMAGE_BASE64_BYTES),
        ));
    }
    tack_ai::InputContentBlock::Image {
        data: data.to_string(),
        mime_type: mime_type.to_string(),
    }
}

/// Apply the untrusted-content defense to `content` (wrap text blocks, set
/// the permission-layer flag), then split off the error path. Error results
/// go through the SAME wrapping/flagging as success results: a malicious
/// server's error text must not enter the conversation unwrapped — the
/// permission layer relies on the flag to re-prompt for mutating tools.
fn finalize_tool_result(
    untrusted: &Option<Arc<std::sync::atomic::AtomicBool>>,
    source: &str,
    mut content: Vec<tack_ai::InputContentBlock>,
    is_error: bool,
    details: Value,
) -> Result<AgentToolResult, String> {
    if let Some(flag) = untrusted {
        for block in &mut content {
            if let tack_ai::InputContentBlock::Text { text, .. } = block {
                *text = wrap_untrusted(flag, source, text);
            }
        }
    }
    if is_error {
        let text = content
            .iter()
            .filter_map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(text);
    }
    Ok(AgentToolResult {
        content,
        details,
        usage: None,
        terminate: false,
        added_tool_names: None,
    })
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.full_name)
            .finish()
    }
}

/// Sanitize a tool name so it is accepted by model providers.
///
/// Anthropic requires tool names to match `^[a-zA-Z0-9_-]{1,64}$`; other
/// providers apply similar rules. MCP tool/server names may contain dots,
/// slashes, or other characters (e.g. `query.coupon`), which would otherwise
/// trigger "function name is invalid" API errors.
pub fn sanitize_tool_name(name: &str) -> String {
    const MAX_LEN: usize = 64;
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() <= MAX_LEN {
        return sanitized;
    }
    // Truncate with a deterministic hash suffix to reduce collision risk.
    let suffix = short_hash_suffix(name);
    let keep = MAX_LEN - suffix.len();
    format!("{}{suffix}", &sanitized[..keep])
}

/// Short deterministic `_<hex>` suffix for disambiguating names.
fn short_hash_suffix(raw: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    raw.hash(&mut hasher);
    format!("_{:x}", hasher.finish())
}

/// Intern a tool name to a `&'static str`. The AgentTool trait requires
/// 'static names; rather than leaking a fresh allocation per tool instance
/// (which grows unboundedly across reconnects), each UNIQUE name is leaked
/// exactly once and reused — the table is bounded by the number of distinct
/// names a session ever sees.
fn intern_tool_name(name: String) -> &'static str {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, &'static str>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = cache.get(&name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
    cache.insert(name, leaked);
    leaked
}

/// Produce a unique sanitized name for `raw`, appending a short hash of the
/// raw (pre-sanitization) name when sanitization collides (`a.b` and `a/b`
/// both sanitize to `a_b`).
fn unique_tool_name(raw: &str, taken: &mut std::collections::HashSet<String>) -> String {
    const MAX_LEN: usize = 64;
    let base = sanitize_tool_name(raw);
    if taken.insert(base.clone()) {
        return base;
    }
    let suffix = short_hash_suffix(raw);
    let keep = MAX_LEN.saturating_sub(suffix.len());
    let mut candidate = format!("{}{suffix}", &base[..keep.min(base.len())]);
    let mut n = 1u32;
    while !taken.insert(candidate.clone()) {
        n += 1;
        let numbered = format!("{candidate}_{n}");
        candidate = if numbered.len() <= MAX_LEN {
            numbered
        } else {
            format!("{}_{n}", &candidate[..MAX_LEN - (n.to_string().len() + 1)])
        };
    }
    candidate
}

impl McpTool {
    pub fn new(server_name: &str, info: McpToolInfo, conn: Arc<McpConnection>) -> Self {
        let full_name = intern_tool_name(sanitize_tool_name(&format!(
            "mcp__{server_name}__{}",
            info.name
        )));
        McpTool {
            server_name: server_name.to_string(),
            info,
            conn,
            full_name,
            untrusted: None,
            starts_deferred: false,
        }
    }

    /// Wire the shared untrusted-content flag (permission elevation).
    pub fn with_untrusted(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.untrusted = Some(flag);
        self
    }
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &'static str {
        self.full_name
    }
    fn label(&self) -> &str {
        self.full_name
    }
    fn description(&self) -> &str {
        self.info.description.as_deref().unwrap_or("MCP tool")
    }
    fn parameters_schema(&self) -> Value {
        Value::Object((*self.info.input_schema).clone())
    }

    fn starts_deferred(&self) -> bool {
        self.starts_deferred
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        // Heal a dropped session before failing the call (reconnects a
        // dead stdio child / re-dials HTTP; no-op when healthy).
        self.conn
            .ensure_connected()
            .await
            .map_err(|e| format!("MCP {} reconnect failed: {e}", self.server_name))?;
        let arguments = params.as_object().cloned();
        let mut request = CallToolRequestParams::new(self.info.name.clone());
        if let Some(arguments) = arguments {
            request = request.with_arguments(arguments);
        }
        let what = format!("MCP tool {} on {}", self.info.name, self.server_name);
        let result = call_with_timeout(
            &self.conn,
            &what,
            self.conn.peer().call_tool(request),
            cancel,
        )
        .await?;

        let mut content: Vec<tack_ai::InputContentBlock> = Vec::new();
        for block in &result.content {
            if let Some(text) = block.as_text() {
                content.push(tack_ai::InputContentBlock::text(truncate_mcp_output(
                    &text.text,
                )));
            } else if let Some(image) = block.as_image() {
                content.push(mcp_image_block(&image.data, &image.mime_type));
            } else if let Some(resource) = block.as_resource() {
                content.push(tack_ai::InputContentBlock::text(truncate_mcp_output(
                    &resource_text(&resource.resource),
                )));
            }
        }
        if content.is_empty() {
            if let Some(structured) = &result.structured_content {
                content.push(tack_ai::InputContentBlock::text(truncate_mcp_output(
                    &structured.to_string(),
                )));
            } else {
                content.push(tack_ai::InputContentBlock::text("(no output)"));
            }
        }

        // Error results go through the same untrusted wrapping/flagging as
        // success results (see finalize_tool_result).
        let source = format!("mcp://{}/{}", self.server_name, self.info.name);
        finalize_tool_result(
            &self.untrusted,
            &source,
            content,
            result.is_error == Some(true),
            result.structured_content.clone().unwrap_or(Value::Null),
        )
    }
}

/// Extract display text from resource contents (also used by tack-app's /mcp picker).
pub fn resource_text(contents: &rmcp::model::ResourceContents) -> String {
    match contents {
        rmcp::model::ResourceContents::TextResourceContents { text, uri, .. } => {
            format!("[resource {uri}]\n{text}")
        }
        rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => {
            format!("[resource {uri}] (binary content omitted)")
        }
        _ => "[resource]".to_string(),
    }
}

/// Meta-tool listing a server's resources (`mcp__<server>__list_resources`).
#[derive(Debug)]
pub struct McpListResourcesTool {
    conn: Arc<McpConnection>,
    full_name: &'static str,
    /// Prompt-injection defense: set when a result enters the context.
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// mcp.json `exposure: "deferred"`: start in the tool_search pool.
    starts_deferred: bool,
}

#[async_trait]
impl AgentTool for McpListResourcesTool {
    fn name(&self) -> &'static str {
        self.full_name
    }
    fn label(&self) -> &str {
        self.full_name
    }
    fn description(&self) -> &str {
        "List MCP resources exposed by this server (then read them with the read_resource tool)"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn starts_deferred(&self) -> bool {
        self.starts_deferred
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let mut text = String::new();
        // Cached capability lists (refreshed on list_changed); no network
        // round-trip here, so no reconnect/timeout handling.
        for resource in &self.conn.resources() {
            let desc = resource.description.as_deref().unwrap_or("");
            text.push_str(&format!("{} — {}\n", resource.uri, desc));
        }
        for template in &self.conn.resource_templates() {
            let desc = template.description.as_deref().unwrap_or("");
            text.push_str(&format!(
                "{} (template) — {}\n",
                template.uri_template, desc
            ));
        }
        if text.is_empty() {
            text = "(no resources)".to_string();
        }
        let text = truncate_mcp_output(&text);
        let text = match &self.untrusted {
            Some(flag) => wrap_untrusted(
                flag,
                &format!("mcp://{}/list_resources", self.conn.name),
                &text,
            ),
            None => text,
        };
        Ok(AgentToolResult::text(text))
    }
}

/// Meta-tool reading one resource by URI (`mcp__<server>__read_resource`).
#[derive(Debug)]
pub struct McpReadResourceTool {
    conn: Arc<McpConnection>,
    full_name: &'static str,
    /// Prompt-injection defense: set when a result enters the context.
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// mcp.json `exposure: "deferred"`: start in the tool_search pool.
    starts_deferred: bool,
}

#[async_trait]
impl AgentTool for McpReadResourceTool {
    fn name(&self) -> &'static str {
        self.full_name
    }
    fn label(&self) -> &str {
        self.full_name
    }
    fn description(&self) -> &str {
        "Read an MCP resource by URI (see list_resources for available URIs)"
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uri": { "type": "string", "description": "Resource URI (exact or matching a template)" }
            },
            "required": ["uri"]
        })
    }
    fn starts_deferred(&self) -> bool {
        self.starts_deferred
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required parameter: uri".to_string())?;
        self.conn
            .ensure_connected()
            .await
            .map_err(|e| format!("MCP {} reconnect failed: {e}", self.conn.name))?;
        let request = ReadResourceRequestParams::new(uri);
        let what = format!("MCP resources/read {uri} on {}", self.conn.name);
        let result = call_with_timeout(
            &self.conn,
            &what,
            self.conn.peer().read_resource(request),
            cancel,
        )
        .await?;
        let text = result
            .contents
            .iter()
            .map(resource_text)
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            "(empty resource)".to_string()
        } else {
            truncate_mcp_output(&text)
        };
        let text = match &self.untrusted {
            Some(flag) => wrap_untrusted(
                flag,
                &format!("mcp://{}/read_resource", self.conn.name),
                &text,
            ),
            None => text,
        };
        Ok(AgentToolResult::text(text))
    }
}

/// AgentTool proxy for one MCP prompt (`mcp__<server>__prompt__<name>`).
#[derive(Debug)]
pub struct McpPromptTool {
    server_name: String,
    prompt: Prompt,
    /// See [`McpTool::conn`].
    conn: Arc<McpConnection>,
    full_name: &'static str,
    /// Prompt-injection defense: set when a result enters the context.
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// mcp.json `exposure: "deferred"`: start in the tool_search pool.
    starts_deferred: bool,
}

impl McpPromptTool {
    fn new(
        server_name: &str,
        prompt: Prompt,
        conn: Arc<McpConnection>,
        full_name: &'static str,
    ) -> Self {
        McpPromptTool {
            server_name: server_name.to_string(),
            prompt,
            conn,
            full_name,
            untrusted: None,
            starts_deferred: false,
        }
    }
}

/// MCP prompt arguments are strings; coerce non-string JSON values to
/// their JSON text instead of silently dropping them (previously a number
/// or boolean argument became "").
fn prompt_arguments(params: &Value) -> Option<rmcp::model::JsonObject> {
    params.as_object().map(|obj| {
        obj.iter()
            .map(|(k, v)| {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k.clone(), Value::String(s))
            })
            .collect()
    })
}

#[async_trait]
impl AgentTool for McpPromptTool {
    fn name(&self) -> &'static str {
        self.full_name
    }
    fn label(&self) -> &str {
        self.full_name
    }
    fn description(&self) -> &str {
        self.prompt.description.as_deref().unwrap_or("MCP prompt")
    }
    fn parameters_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for arg in self.prompt.arguments.clone().unwrap_or_default() {
            properties.insert(
                arg.name.clone(),
                json!({ "type": "string", "description": arg.description.unwrap_or_default() }),
            );
            if arg.required == Some(true) {
                required.push(Value::String(arg.name.clone()));
            }
        }
        json!({ "type": "object", "properties": properties, "required": required })
    }
    fn starts_deferred(&self) -> bool {
        self.starts_deferred
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let mut request = rmcp::model::GetPromptRequestParams::new(self.prompt.name.clone());
        if let Some(args) = prompt_arguments(&params) {
            request = request.with_arguments(args);
        }
        self.conn
            .ensure_connected()
            .await
            .map_err(|e| format!("MCP {} reconnect failed: {e}", self.server_name))?;
        let what = format!(
            "MCP prompts/get {} on {}",
            self.prompt.name, self.server_name
        );
        let result = call_with_timeout(
            &self.conn,
            &what,
            self.conn.peer().get_prompt(request),
            cancel,
        )
        .await?;
        // Flatten the prompt's messages to text for the model to continue.
        let mut text = String::new();
        for message in &result.messages {
            let role = match message.role {
                rmcp::model::Role::User => "user",
                rmcp::model::Role::Assistant => "assistant",
            };
            let body = if let Some(t) = message.content.as_text() {
                t.text.clone()
            } else if message.content.as_image().is_some() {
                "[image]".to_string()
            } else if let Some(resource) = message.content.as_resource() {
                resource_text(&resource.resource)
            } else {
                "[content]".to_string()
            };
            text.push_str(&format!("[{role}] {body}\n"));
        }
        let text = if text.is_empty() {
            "(empty prompt)".to_string()
        } else {
            truncate_mcp_output(&text)
        };
        let text = match &self.untrusted {
            Some(flag) => wrap_untrusted(
                flag,
                &format!("mcp://{}/prompt/{}", self.server_name, self.prompt.name),
                &text,
            ),
            None => text,
        };
        Ok(AgentToolResult::text(text))
    }
}

/// Expand connections into AgentTool proxies (tools + resource meta-tools +
/// prompt tools).
pub fn mcp_tools(connections: &[Arc<McpConnection>]) -> Vec<Arc<dyn AgentTool>> {
    mcp_tools_with(connections, None)
}

/// mcp_tools with the shared untrusted-content flag (prompt-injection
/// defense: results get wrapped and the run is marked for the permission
/// layer).
pub fn mcp_tools_with(
    connections: &[Arc<McpConnection>],
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Vec<Arc<dyn AgentTool>> {
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    // Sanitized names can collide (`a.b` and `a/b` both become `a_b`); the
    // full set is deduplicated across all connections up front.
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for conn in connections {
        // mcp.json `exposure` / `toolExposure`: per-tool resolution (exact
        // names > longest `*` pattern > server default). `hidden` tools
        // are never built; `deferred` tools carry the marker the
        // tool_search split reads. In-memory connections (plugin/test
        // transports) have no spec and default to `direct`.
        let spec = conn.spec();
        let server_exposure = spec.map(|s| s.exposure).unwrap_or_default();
        for info in &conn.tools() {
            let exposure = spec
                .map(|s| s.effective_exposure(&info.name))
                .unwrap_or_default();
            if exposure == McpExposure::Hidden {
                continue;
            }
            let full_name = intern_tool_name(unique_tool_name(
                &format!("mcp__{}__{}", conn.name, info.name),
                &mut taken,
            ));
            // Publish the server-declared hints for the permission layer.
            register_tool_annotations(full_name, info);
            let mut tool = McpTool::new(&conn.name, info.clone(), conn.clone());
            tool.full_name = full_name;
            tool.starts_deferred = exposure == McpExposure::Deferred;
            if let Some(flag) = &untrusted {
                tool = tool.with_untrusted(flag.clone());
            }
            tools.push(Arc::new(tool));
        }
        if conn.has_resources() && server_exposure != McpExposure::Hidden {
            let starts_deferred = server_exposure == McpExposure::Deferred;
            let list_name = intern_tool_name(unique_tool_name(
                &format!("mcp__{}__list_resources", conn.name),
                &mut taken,
            ));
            tools.push(Arc::new(McpListResourcesTool {
                conn: conn.clone(),
                full_name: list_name,
                untrusted: untrusted.clone(),
                starts_deferred,
            }));
            let read_name = intern_tool_name(unique_tool_name(
                &format!("mcp__{}__read_resource", conn.name),
                &mut taken,
            ));
            tools.push(Arc::new(McpReadResourceTool {
                conn: conn.clone(),
                full_name: read_name,
                untrusted: untrusted.clone(),
                starts_deferred,
            }));
        }
        if server_exposure != McpExposure::Hidden {
            for prompt in &conn.prompts() {
                let full_name = intern_tool_name(unique_tool_name(
                    &format!("mcp__{}__prompt__{}", conn.name, prompt.name),
                    &mut taken,
                ));
                let mut tool =
                    McpPromptTool::new(&conn.name, prompt.clone(), conn.clone(), full_name);
                tool.untrusted = untrusted.clone();
                tool.starts_deferred = server_exposure == McpExposure::Deferred;
                tools.push(Arc::new(tool));
            }
        }
    }
    tools
}

/// One capability of an MCP server advertised through the plugin model
/// (Level-2 MCP server plugins, `docs/plugin-roadmap.md` §4): an
/// unprefixed tool spec for the plugin's capability list, paired with the
/// execution engine that runs the underlying MCP call (callTool /
/// resources/list+read / prompts/get) with all the usual MCP result
/// handling (truncation, image caps, error mapping).
pub struct McpPluginTool {
    /// Provider-safe tool name, unique within the plugin, WITHOUT any
    /// server prefix — the plugin host prefixes it with
    /// `ext__<plugin-id>__` like any other plugin tool.
    pub spec_name: String,
    /// Advertised description (the engine's own).
    pub description: String,
    /// Advertised JSON parameter schema (the engine's own; an object).
    pub parameters: Value,
    engine: Arc<dyn AgentTool>,
}

impl std::fmt::Debug for McpPluginTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpPluginTool")
            .field("spec_name", &self.spec_name)
            .finish()
    }
}

impl McpPluginTool {
    /// Run the underlying MCP call. Cancellation is structural: dropping
    /// the returned future aborts the await (same semantics as dropping a
    /// plugin-carrier `tools/execute` call).
    pub async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
    ) -> Result<AgentToolResult, String> {
        self.engine
            .execute(tool_call_id, params, CancellationToken::new(), &|_| {})
            .await
    }
}

fn push_plugin_tool(
    out: &mut Vec<McpPluginTool>,
    taken: &mut std::collections::HashSet<String>,
    raw_spec_name: &str,
    engine: Arc<dyn AgentTool>,
) {
    let spec_name = unique_tool_name(raw_spec_name, taken);
    out.push(McpPluginTool {
        spec_name,
        description: engine.description().to_string(),
        parameters: engine.parameters_schema(),
        engine,
    });
}

/// Build the Level-2 plugin view of one connection: the server's tools,
/// its resource meta-tools (`list_resources` / `read_resource`, when it
/// has resources), and its prompts (`prompt__<name>` tools) — the same
/// surfaces `mcp_tools_with` exposes, but with unprefixed spec names so
/// the plugin host can attribute them to the plugin's identity.
///
/// Engines are built WITHOUT the untrusted-content flag: Level-2 plugin
/// tools get the wrapping uniformly at the ExtTool layer (the plugin host
/// owns the trust decision for plugin-attributed output).
pub fn plugin_capabilities(conn: &Arc<McpConnection>) -> Vec<McpPluginTool> {
    let mut out = Vec::new();
    let mut spec_taken = std::collections::HashSet::new();
    let mut engine_taken = std::collections::HashSet::new();
    for info in &conn.tools() {
        let full_name = intern_tool_name(unique_tool_name(
            &format!("mcp__{}__{}", conn.name, info.name),
            &mut engine_taken,
        ));
        let mut tool = McpTool::new(&conn.name, info.clone(), conn.clone());
        tool.full_name = full_name;
        push_plugin_tool(&mut out, &mut spec_taken, &info.name, Arc::new(tool));
    }
    if conn.has_resources() {
        let list_name = intern_tool_name(unique_tool_name(
            &format!("mcp__{}__list_resources", conn.name),
            &mut engine_taken,
        ));
        push_plugin_tool(
            &mut out,
            &mut spec_taken,
            "list_resources",
            Arc::new(McpListResourcesTool {
                conn: conn.clone(),
                full_name: list_name,
                untrusted: None,
                // Plugin-attributed tools never defer (the plugin host
                // owns their registration; the pool is mcp__-shaped).
                starts_deferred: false,
            }),
        );
        let read_name = intern_tool_name(unique_tool_name(
            &format!("mcp__{}__read_resource", conn.name),
            &mut engine_taken,
        ));
        push_plugin_tool(
            &mut out,
            &mut spec_taken,
            "read_resource",
            Arc::new(McpReadResourceTool {
                conn: conn.clone(),
                full_name: read_name,
                untrusted: None,
                starts_deferred: false,
            }),
        );
    }
    for prompt in &conn.prompts() {
        let full_name = intern_tool_name(unique_tool_name(
            &format!("mcp__{}__prompt__{}", conn.name, prompt.name),
            &mut engine_taken,
        ));
        let tool = McpPromptTool::new(&conn.name, prompt.clone(), conn.clone(), full_name);
        push_plugin_tool(
            &mut out,
            &mut spec_taken,
            &format!("prompt__{}", prompt.name),
            Arc::new(tool),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::sanitize_tool_name;

    /// Credential stripping mirrors the v3 process carrier's rule:
    /// sensitive inherited vars are `env_remove`d unless the server
    /// spec explicitly re-declares them; lookalikes stay.
    #[test]
    fn credential_stripping_matches_the_process_carrier() {
        assert!(super::is_sensitive_env_key("ANTHROPIC_API_KEY"));
        assert!(super::is_sensitive_env_key("OPENAI_API_KEY"));
        assert!(super::is_sensitive_env_key("GITHUB_TOKEN"));
        assert!(super::is_sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!super::is_sensitive_env_key("PATH"));
        assert!(!super::is_sensitive_env_key("TOKENIZER_THREADS")); // no _TOKEN suffix
        assert!(!super::is_sensitive_env_key("SECRETARY_NAME"));

        let parent: Vec<String> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "PATH", "HOME"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // The spec explicitly declares one key: it is NOT stripped.
        let declared = vec![("OPENAI_API_KEY".to_string(), "explicit".to_string())];
        let mut command = tokio::process::Command::new("true");
        super::strip_credential_env(&mut command, &parent, &declared);
        let removed: Vec<String> = command
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert_eq!(removed, vec!["ANTHROPIC_API_KEY".to_string()]);
    }

    /// User-configured servers opt out of stripping by default; plugin
    /// carriers opt in via the builder.
    #[test]
    fn credential_stripping_is_opt_in() {
        let spec =
            super::McpServerSpec::stdio("srv".to_string(), "cmd".to_string(), vec![], vec![], None);
        assert!(!spec.strip_credentials);
        assert!(spec.with_credential_stripping().strip_credentials);
        let http =
            super::McpServerSpec::http("srv".to_string(), "https://x/mcp".to_string(), vec![]);
        assert!(!http.strip_credentials);
    }

    #[test]
    fn star_match_semantics() {
        assert!(super::star_match("get_*", "get_user"));
        assert!(!super::star_match("get_*", "set_user"));
        assert!(super::star_match("*_admin", "user_admin"));
        assert!(!super::star_match("*_admin", "admin_panel"));
        assert!(super::star_match("get_*_json", "get_user_json"));
        assert!(!super::star_match("get_*_json", "get_user_xml"));
        assert!(super::star_match("*", "anything"));
        assert!(super::star_match("search", "search"));
        assert!(!super::star_match("search", "search_code"));
        assert!(super::star_match("a*b*c", "aXbYc"));
        assert!(!super::star_match("a*b*c", "aXbY"));
    }

    #[test]
    fn effective_exposure_precedence() {
        use super::{McpExposure, McpServerSpec};
        let spec = McpServerSpec::stdio("s".into(), "c".into(), vec![], vec![], None)
            .with_exposure(McpExposure::Deferred)
            .with_tool_exposure(vec![
                ("delete_*".to_string(), McpExposure::Hidden),
                ("get_*".to_string(), McpExposure::Direct),
                ("get_user_admin".to_string(), McpExposure::Hidden),
                ("get_user_*".to_string(), McpExposure::Deferred),
            ]);
        // Exact name beats every pattern.
        assert_eq!(
            spec.effective_exposure("get_user_admin"),
            McpExposure::Hidden
        );
        // Longest pattern beats shorter ones (get_user_* > get_*).
        assert_eq!(
            spec.effective_exposure("get_user_repo"),
            McpExposure::Deferred
        );
        assert_eq!(spec.effective_exposure("get_teams"), McpExposure::Direct);
        assert_eq!(spec.effective_exposure("delete_repo"), McpExposure::Hidden);
        // No override: the server-level exposure.
        assert_eq!(
            spec.effective_exposure("anything_else"),
            McpExposure::Deferred
        );
        // Default spec: everything direct.
        let plain = McpServerSpec::http("s".into(), "http://x/mcp".into(), vec![]);
        assert_eq!(plain.effective_exposure("x"), McpExposure::Direct);
    }

    #[test]
    fn exposure_config_values() {
        use super::McpExposure;
        assert_eq!(
            McpExposure::from_config("deferred"),
            Some((McpExposure::Deferred, false))
        );
        assert_eq!(
            McpExposure::from_config("codemode"),
            Some((McpExposure::Deferred, true)),
            "pi's codemode maps to deferred with a warning flag"
        );
        assert!(McpExposure::from_config("bogus").is_none());
    }

    #[test]
    fn sanitize_replaces_invalid_chars() {
        assert_eq!(
            sanitize_tool_name("mcp__shop__query.coupon"),
            "mcp__shop__query_coupon"
        );
        assert_eq!(
            sanitize_tool_name("mcp__my.server__a/b c:d"),
            "mcp__my_server__a_b_c_d"
        );
    }

    #[test]
    fn sanitize_keeps_valid_names_unchanged() {
        assert_eq!(
            sanitize_tool_name("mcp__server__tool-name_1"),
            "mcp__server__tool-name_1"
        );
    }

    #[test]
    fn sanitize_enforces_length_limit() {
        let long = format!("mcp__{}__{}", "s".repeat(40), "t".repeat(40));
        let out = sanitize_tool_name(&long);
        assert!(out.len() <= 64);
        assert!(
            out.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        // Deterministic and distinct for distinct inputs.
        assert_eq!(out, sanitize_tool_name(&long));
        let other = sanitize_tool_name(&format!("{long}x"));
        assert!(other.len() <= 64);
        assert_ne!(out, other);
    }

    #[test]
    fn prompt_arguments_coerce_non_strings() {
        let args = super::prompt_arguments(&serde_json::json!({
            "name": "x",
            "count": 5,
            "flag": true,
        }))
        .unwrap();
        assert_eq!(args.get("name").and_then(|v| v.as_str()), Some("x"));
        assert_eq!(args.get("count").and_then(|v| v.as_str()), Some("5"));
        assert_eq!(args.get("flag").and_then(|v| v.as_str()), Some("true"));
        // Non-object params yield no arguments.
        assert!(super::prompt_arguments(&serde_json::Value::Null).is_none());
    }

    /// Regression: `a.b` and `a/b` sanitize to the same `a_b`; the full
    /// name must be disambiguated deterministically.
    #[test]
    fn sanitized_collisions_get_unique_names() {
        let mut taken = std::collections::HashSet::new();
        let a = super::unique_tool_name("mcp__srv__a.b", &mut taken);
        let b = super::unique_tool_name("mcp__srv__a/b", &mut taken);
        assert_eq!(a, "mcp__srv__a_b");
        assert_ne!(a, b, "collision must be disambiguated");
        assert!(b.len() <= 64);
        // Deterministic across runs within a process.
        let mut taken2 = std::collections::HashSet::new();
        assert_eq!(a, super::unique_tool_name("mcp__srv__a.b", &mut taken2));
        assert_eq!(b, super::unique_tool_name("mcp__srv__a/b", &mut taken2));
        // A third colliding name gets yet another name.
        let c = super::unique_tool_name("mcp__srv__a b", &mut taken);
        assert_ne!(c, a, "{c}");
        assert_ne!(c, b, "{c}");
    }

    /// Interning returns the same &'static str for the same name (no
    /// repeated leaking across reconnects).
    #[test]
    fn interned_names_are_reused() {
        let a = super::intern_tool_name("mcp__x__y".to_string());
        let b = super::intern_tool_name("mcp__x__y".to_string());
        assert!(std::ptr::eq(a, b));
        assert_eq!(a, "mcp__x__y");
    }

    #[test]
    fn mcp_output_is_truncated_like_bash() {
        let small = "hello";
        assert_eq!(super::truncate_mcp_output(small), small);
        let big = "x\n".repeat(5000);
        let out = super::truncate_mcp_output(&big);
        assert!(out.contains("[MCP output truncated:"), "{out}");
        assert!(out.len() < big.len());
        // Tail truncation: the END of the output is kept.
        assert!(out.starts_with("x"), "{out}");
    }

    #[test]
    fn wrap_untrusted_marks_flag_and_wraps() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let out = super::wrap_untrusted(&flag, "mcp://srv/tool", "body");
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            out,
            "<untrusted_content source=\"mcp://srv/tool\">\nbody\n</untrusted_content>"
        );
    }

    /// Regression: MCP ERROR results must go through the same
    /// untrusted-content wrapping/flagging as success results — a malicious
    /// server's error text used to enter the conversation unwrapped and
    /// never set the permission-layer flag.
    #[test]
    fn error_results_are_wrapped_and_flagged() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let content = vec![tack_ai::InputContentBlock::text("evil instructions")];
        let err = super::finalize_tool_result(
            &Some(flag.clone()),
            "mcp://srv/tool",
            content,
            true,
            serde_json::Value::Null,
        )
        .unwrap_err();
        // Error-ness preserved (Err path) AND content wrapped AND flag set.
        assert!(
            err.contains("<untrusted_content source=\"mcp://srv/tool\">"),
            "{err}"
        );
        assert!(err.contains("evil instructions"), "{err}");
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));

        // Success path still returns the wrapped content as Ok.
        let flag2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ok = super::finalize_tool_result(
            &Some(flag2.clone()),
            "mcp://srv/tool",
            vec![tack_ai::InputContentBlock::text("body")],
            false,
            serde_json::Value::Null,
        )
        .unwrap();
        assert!(flag2.load(std::sync::atomic::Ordering::Relaxed));
        match &ok.content[0] {
            tack_ai::InputContentBlock::Text { text, .. } => {
                assert!(text.contains("<untrusted_content"), "{text}");
            }
            other => panic!("expected text block: {other:?}"),
        }
    }

    /// Regression: MCP image blocks must be size-capped like text output —
    /// an unbounded base64 image from a hostile/buggy server used to pass
    /// through verbatim.
    #[test]
    fn mcp_images_are_size_capped() {
        // Under the cap: passes through as an image block.
        match super::mcp_image_block("aGVsbG8=", "image/png") {
            tack_ai::InputContentBlock::Image { data, mime_type } => {
                assert_eq!(data, "aGVsbG8=");
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("expected image block: {other:?}"),
        }
        // Over the cap: replaced by an in-band note, payload dropped.
        let big = "x".repeat(super::MAX_MCP_IMAGE_BASE64_BYTES + 1);
        match super::mcp_image_block(&big, "image/png") {
            tack_ai::InputContentBlock::Text { text, .. } => {
                assert!(text.contains("[MCP image omitted:"), "{text}");
                assert!(text.contains("image/png"), "{text}");
            }
            other => panic!("expected text note: {other:?}"),
        }
    }
}
