//! MCP (Model Context Protocol) client: connects to stdio, Streamable HTTP,
//! and legacy SSE MCP servers and exposes their tools, resources, and prompts
//! as `AgentTool`s. TS pi deliberately has no built-in MCP; tack adds it
//! because ACP clients (Zed) pass `mcpServers` in `session/new`.
//!
//! Scope: stdio + Streamable HTTP + legacy SSE (2024-11-05, via the hand-rolled
//! transport in `mcp_sse`) transports, tool proxying, resources (list/read
//! meta-tools), prompts (exposed as tools).

use std::path::PathBuf;
use std::sync::Arc;

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
use rmcp::service::{Peer, RequestContext, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// OAuth 2.1 config for a remote server (mcp.json `"oauth": {clientId,
/// scopes}`; both optional — dynamic registration fills the rest).
#[derive(Clone, Debug, Default)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
}

/// How to reach an MCP server.
#[derive(Clone, Debug)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransport,
    /// OAuth config (HTTP/SSE only). `Some(…)` = authorize on 401.
    pub oauth: Option<McpOAuthConfig>,
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
        }
    }

    pub fn http(name: String, url: String, headers: Vec<(String, String)>) -> Self {
        McpServerSpec {
            name,
            transport: McpTransport::Http { url, headers },
            oauth: None,
        }
    }

    pub fn sse(name: String, url: String, headers: Vec<(String, String)>) -> Self {
        McpServerSpec {
            name,
            transport: McpTransport::Sse { url, headers },
            oauth: None,
        }
    }

    /// Attach an OAuth config (HTTP/SSE only).
    pub fn with_oauth(mut self, oauth: McpOAuthConfig) -> Self {
        self.oauth = Some(oauth);
        self
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

/// rmcp `ClientHandler` bridging server-initiated requests to the configured
/// callbacks. Also used (with empty callbacks) for plain connections so
/// `McpConnection` has one concrete service type.
#[derive(Clone, Debug)]
pub struct TackClientHandler {
    server_name: String,
    callbacks: McpClientCallbacks,
}

impl TackClientHandler {
    pub fn new(server_name: String, callbacks: McpClientCallbacks) -> Self {
        TackClientHandler {
            server_name,
            callbacks,
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

/// A live MCP server connection plus its discovered capabilities.
pub struct McpConnection {
    pub name: String,
    service: RunningService<RoleClient, TackClientHandler>,
    pub tools: Vec<McpToolInfo>,
    pub resources: Vec<Resource>,
    pub resource_templates: Vec<ResourceTemplate>,
    pub prompts: Vec<Prompt>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection")
            .field("name", &self.name)
            .field("tools", &self.tools.len())
            .field(
                "resources",
                &(self.resources.len() + self.resource_templates.len()),
            )
            .field("prompts", &self.prompts.len())
            .finish()
    }
}

impl McpConnection {
    pub fn peer(&self) -> &Peer<RoleClient> {
        self.service.peer()
    }

    /// True when the underlying service/transport is gone (server child
    /// died, transport closed or cancelled) — a pooled connection in this
    /// state must be rebuilt, not reused.
    pub fn is_closed(&self) -> bool {
        self.service.is_closed() || self.peer().is_transport_closed()
    }

    pub fn has_resources(&self) -> bool {
        !self.resources.is_empty() || !self.resource_templates.is_empty()
    }

    /// Cancel the service (server child killed / HTTP session closed).
    /// Idempotent; in-flight calls resolve with closed-channel errors.
    /// Dropping the last `Arc<McpConnection>` cancels too — this is the
    /// explicit, prompt version for plugin shutdown.
    pub fn cancel(&self) {
        self.service.cancellation_token().cancel();
    }
}

/// Connect to a server, list its tools/resources/prompts. Capability probes
/// that the server doesn't support degrade to empty lists.
pub async fn connect(spec: &McpServerSpec) -> Result<McpConnection, String> {
    connect_with(spec, McpClientCallbacks::default()).await
}

/// `connect` with client-side callbacks for server-initiated requests
/// (sampling / elicitation).
pub async fn connect_with(
    spec: &McpServerSpec,
    callbacks: McpClientCallbacks,
) -> Result<McpConnection, String> {
    use rmcp::ServiceExt;
    let handler = TackClientHandler::new(spec.name.clone(), callbacks);
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
    finish_connection(spec.name.clone(), service).await
}

/// Test seam (also used by tack-app's plugin tests): connect over an
/// arbitrary in-memory transport instead of spawning a child or dialing
/// HTTP — a fixture server answers on the other end of the duplex.
#[doc(hidden)]
pub async fn connect_transport<S>(
    name: &str,
    stream: S,
    callbacks: McpClientCallbacks,
) -> Result<McpConnection, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    use rmcp::ServiceExt;
    let handler = TackClientHandler::new(name.to_string(), callbacks);
    let service = handler
        .serve(stream)
        .await
        .map_err(|e| format!("MCP initialize failed for {name}: {e}"))?;
    finish_connection(name.to_string(), service).await
}

/// Shared post-handshake tail of `connect_with` / `connect_transport`:
/// probe the server's tools/resources/prompts (unsupported capability
/// probes degrade to empty lists).
async fn finish_connection(
    name: String,
    service: RunningService<RoleClient, TackClientHandler>,
) -> Result<McpConnection, String> {
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

    Ok(McpConnection {
        name,
        service,
        tools,
        resources,
        resource_templates,
        prompts,
    })
}

/// Connect to several servers concurrently; failures are logged and skipped
/// (a broken MCP server must not fail session creation).
pub async fn connect_all(specs: Vec<McpServerSpec>) -> Vec<Arc<McpConnection>> {
    connect_all_with(specs, McpClientCallbacks::default()).await
}

/// `connect_all` with client-side callbacks applied to every server.
pub async fn connect_all_with(
    specs: Vec<McpServerSpec>,
    callbacks: McpClientCallbacks,
) -> Vec<Arc<McpConnection>> {
    let tasks: Vec<_> = specs
        .into_iter()
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

/// AgentTool proxy for one MCP tool.
pub struct McpTool {
    server_name: String,
    info: McpToolInfo,
    peer: Peer<RoleClient>,
    /// Keeps the connection (and its RunningService) alive for servers whose
    /// tool list is the ONLY exposed capability — otherwise dropping the
    /// caller's Arc would cancel the service while tools still exist.
    keepalive: Option<Arc<McpConnection>>,
    /// Interned prefixed name (see intern_tool_name).
    full_name: &'static str,
    /// Prompt-injection defense: set when a result enters the context.
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
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
    pub fn new(server_name: &str, info: McpToolInfo, peer: Peer<RoleClient>) -> Self {
        let full_name = intern_tool_name(sanitize_tool_name(&format!(
            "mcp__{server_name}__{}",
            info.name
        )));
        McpTool {
            server_name: server_name.to_string(),
            info,
            peer,
            keepalive: None,
            full_name,
            untrusted: None,
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

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let arguments = params.as_object().cloned();
        let mut request = CallToolRequestParams::new(self.info.name.clone());
        if let Some(arguments) = arguments {
            request = request.with_arguments(arguments);
        }
        let call = self.peer.call_tool(request);

        let result = tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = call => r,
        };
        let result = result.map_err(|e| {
            format!(
                "MCP tool {} on {} failed: {e}",
                self.info.name, self.server_name
            )
        })?;

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

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let mut text = String::new();
        for resource in &self.conn.resources {
            let desc = resource.description.as_deref().unwrap_or("");
            text.push_str(&format!("{} — {}\n", resource.uri, desc));
        }
        for template in &self.conn.resource_templates {
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
        let mut request = ReadResourceRequestParams::new(uri);
        let call = self.conn.peer().read_resource(request.clone());
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = call => r,
        };
        let result = result
            .map_err(|e| format!("MCP resources/read {uri} on {} failed: {e}", self.conn.name))?;
        let _ = &mut request;
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
    peer: Peer<RoleClient>,
    /// See [`McpTool::keepalive`].
    keepalive: Option<Arc<McpConnection>>,
    full_name: &'static str,
    /// Prompt-injection defense: set when a result enters the context.
    untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl McpPromptTool {
    fn new(
        server_name: &str,
        prompt: Prompt,
        peer: Peer<RoleClient>,
        full_name: &'static str,
    ) -> Self {
        McpPromptTool {
            server_name: server_name.to_string(),
            prompt,
            peer,
            keepalive: None,
            full_name,
            untrusted: None,
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
        let call = self.peer.get_prompt(request);
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = call => r,
        };
        let result = result.map_err(|e| {
            format!(
                "MCP prompts/get {} on {} failed: {e}",
                self.prompt.name, self.server_name
            )
        })?;
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
        for info in &conn.tools {
            let full_name = intern_tool_name(unique_tool_name(
                &format!("mcp__{}__{}", conn.name, info.name),
                &mut taken,
            ));
            let mut tool = McpTool::new(&conn.name, info.clone(), conn.peer().clone());
            tool.full_name = full_name;
            tool.keepalive = Some(conn.clone());
            if let Some(flag) = &untrusted {
                tool = tool.with_untrusted(flag.clone());
            }
            tools.push(Arc::new(tool));
        }
        if conn.has_resources() {
            let list_name = intern_tool_name(unique_tool_name(
                &format!("mcp__{}__list_resources", conn.name),
                &mut taken,
            ));
            tools.push(Arc::new(McpListResourcesTool {
                conn: conn.clone(),
                full_name: list_name,
                untrusted: untrusted.clone(),
            }));
            let read_name = intern_tool_name(unique_tool_name(
                &format!("mcp__{}__read_resource", conn.name),
                &mut taken,
            ));
            tools.push(Arc::new(McpReadResourceTool {
                conn: conn.clone(),
                full_name: read_name,
                untrusted: untrusted.clone(),
            }));
        }
        for prompt in &conn.prompts {
            let full_name = intern_tool_name(unique_tool_name(
                &format!("mcp__{}__prompt__{}", conn.name, prompt.name),
                &mut taken,
            ));
            let mut tool =
                McpPromptTool::new(&conn.name, prompt.clone(), conn.peer().clone(), full_name);
            tool.keepalive = Some(conn.clone());
            tool.untrusted = untrusted.clone();
            tools.push(Arc::new(tool));
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
    for info in &conn.tools {
        let full_name = intern_tool_name(unique_tool_name(
            &format!("mcp__{}__{}", conn.name, info.name),
            &mut engine_taken,
        ));
        let mut tool = McpTool::new(&conn.name, info.clone(), conn.peer().clone());
        tool.full_name = full_name;
        tool.keepalive = Some(conn.clone());
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
            }),
        );
    }
    for prompt in &conn.prompts {
        let full_name = intern_tool_name(unique_tool_name(
            &format!("mcp__{}__prompt__{}", conn.name, prompt.name),
            &mut engine_taken,
        ));
        let mut tool =
            McpPromptTool::new(&conn.name, prompt.clone(), conn.peer().clone(), full_name);
        tool.keepalive = Some(conn.clone());
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
