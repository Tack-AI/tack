//! Extension host: discovery, identity, lifecycle, and event fan-out for
//! tack-RPC v3 plugins, plus the install/store/marketplace machinery.
//!
//! Layout (docs/plugin-roadmap.md §8):
//!
//! ```text
//! ~/.tack/agent/extensions/
//! ├── store/<source>/<name>/<version>/   # versioned installs (P3+)
//! ├── data/<source>/<name>/              # writable per-plugin data root
//! └── <name>/                            # legacy flat installs (pre-P3)
//! ```
//!
//! Plugin identity is `name@source` ([`PluginId`]): reserved sources are
//! `user` (installed without a marketplace), `project` (`.pi/extensions`),
//! `local` (settings `extensionPaths`), and marketplace names otherwise.
//! The active version of a store plugin is `local` when present, else the
//! highest semver directory.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tack_agent_core::{AgentHooks, AgentTool};
use tack_ext::hooks::{ExtHooks, FailMode};
use tack_ext::plugin_id::PluginId;
use tack_ext::rpc3::{
    ApprovalDecisionAction, ApprovalReviewParams, ErrorObject, HostCapabilities, HostInfo,
    InitializeParams, InitializeResult, MetricsHostCapability, RunMode, ToolCall,
    WidgetActionParams, WidgetKind, WidgetSpec, WidgetUpdateParams,
};
use tack_ext::tool::ExtTool;
use tack_ext::v3::{PeerHandler, PluginConnection, V3Process};
use tokio::sync::{mpsc, oneshot};

pub use tack_ext::ExtNotifyProvider;

// ---------------------------------------------------------------------------
// Plugin → host service bridges (TUI mode)
// ---------------------------------------------------------------------------

/// A plugin→host UI/exec request awaiting resolution on the TUI main loop.
pub struct ExtUiRequest {
    pub plugin: String,
    pub method: String,
    pub params: Value,
    pub respond: oneshot::Sender<Result<Value, String>>,
}

impl std::fmt::Debug for ExtUiRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtUiRequest")
            .field("method", &self.method)
            .finish()
    }
}

/// Host services bridge for the TUI: plugin requests cross to the TUI main
/// loop via the app event channel (plugin calls never run on the loop
/// thread) or are answered inline (provider registration — see below).
/// One services object is shared by all plugins; per-plugin attribution
/// comes from `TaggedServices` injecting the `plugin` field (see the load
/// loop). Implements the v3 [`PeerHandler`] surface.
pub struct TuiExtServices {
    tx: crate::tui::AppEventTx,
    /// Project trust: `exec/run` is only honored for trusted contexts.
    trusted: bool,
    /// Provider bridge state (connections, stream sinks, registrations).
    bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
}

impl std::fmt::Debug for TuiExtServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiExtServices")
            .field("trusted", &self.trusted)
            .finish()
    }
}

impl TuiExtServices {
    pub(crate) fn new(
        tx: crate::tui::AppEventTx,
        trusted: bool,
        bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
    ) -> Self {
        TuiExtServices {
            tx,
            trusted,
            bridge_state,
        }
    }
}

fn service_error(code: i64, message: impl Into<String>) -> ErrorObject {
    ErrorObject {
        code,
        message: message.into(),
        data: None,
    }
}

#[async_trait::async_trait]
impl PeerHandler for TuiExtServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
        use tack_ext::rpc3::{ERR_CAPABILITY_NOT_GRANTED, ERR_METHOD_NOT_FOUND, ERR_POLICY_DENIED};
        match method {
            // Interactive dialogs and session control cross to the TUI.
            "ui/notify"
            | "ui/select"
            | "ui/confirm"
            | "ui/input"
            | "session/get"
            | "session/sendUserMessage" => {
                let (tx, rx) = oneshot::channel();
                self.tx
                    .send(crate::tui::AppEvent::ExtUiRequest(ExtUiRequest {
                        plugin: String::new(),
                        method: method.to_string(),
                        params,
                        respond: tx,
                    }))
                    .map_err(|_| {
                        service_error(ERR_CAPABILITY_NOT_GRANTED, "host is shutting down")
                    })?;
                rx.await
                    .map_err(|_| {
                        service_error(ERR_CAPABILITY_NOT_GRANTED, "host closed the request")
                    })?
                    .map_err(|e| service_error(tack_ext::rpc3::ERR_INTERNAL, e))
            }
            // Provider registration is mode-independent: it writes the
            // process-global runtime registry (HTTP shim), and for
            // `bridge: true` additionally wires the plugin connection as
            // the serving endpoint (P7b). Handled inline — crossing to the
            // main loop would needlessly serialize on UI work.
            "host/registerProvider" => {
                let provider_id = params
                    .get("provider")
                    .and_then(|p| p.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let result = crate::ext_provider_bridge::handle_register_provider(
                    &self.bridge_state,
                    params,
                )
                .await;
                if result.is_ok() {
                    let _ = self.tx.send(crate::tui::AppEvent::Notice(
                        crate::i18n::t(
                            crate::i18n::current(),
                            "msg.provider_registered",
                            &[("id", &provider_id)],
                        ),
                        crate::tui::chat::NoticeKind::Info,
                    ));
                }
                result
            }
            "exec/run" => {
                if !self.trusted {
                    return Err(service_error(
                        ERR_POLICY_DENIED,
                        "exec requires project trust",
                    ));
                }
                let (tx, rx) = oneshot::channel();
                self.tx
                    .send(crate::tui::AppEvent::ExtUiRequest(ExtUiRequest {
                        plugin: String::new(),
                        method: method.to_string(),
                        params,
                        respond: tx,
                    }))
                    .map_err(|_| {
                        service_error(ERR_CAPABILITY_NOT_GRANTED, "host is shutting down")
                    })?;
                rx.await
                    .map_err(|_| {
                        service_error(ERR_CAPABILITY_NOT_GRANTED, "host closed the request")
                    })?
                    .map_err(|e| service_error(tack_ext::rpc3::ERR_INTERNAL, e))
            }
            other => Err(service_error(
                ERR_METHOD_NOT_FOUND,
                format!("unknown host method {other}"),
            )),
        }
    }

    async fn handle_notification(&self, method: &str, payload: Value) {
        match method {
            "logs/emit" => {
                let level = payload
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
                match level {
                    "error" => tracing::error!(target: "tack_ext::plugin", "{message}"),
                    "warn" | "warning" => tracing::warn!(target: "tack_ext::plugin", "{message}"),
                    "debug" => tracing::debug!(target: "tack_ext::plugin", "{message}"),
                    _ => tracing::info!(target: "tack_ext::plugin", "{message}"),
                }
            }
            "warnings/emit" => {
                let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
                tracing::warn!(target: "tack_ext::plugin", "plugin warning: {message}");
            }
            // Bridge provider stream events + provider events (P7b/P7c).
            // The `plugin` field was injected by TaggedServices.
            "provider/streamEvent" => {
                let plugin = payload
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.bridge_state.route_stream_event(&plugin, payload);
            }
            "provider/event" => {
                let plugin = payload
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.bridge_state.route_provider_event(&plugin, payload);
            }
            // Widget state push. Fire-and-forget into the TUI main loop
            // (full-state replacement; dropped frames are harmless). The
            // `plugin` field was injected by TaggedServices (widget ids are
            // only unique per plugin).
            "widgets/update" => {
                let plugin = payload
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                match serde_json::from_value::<WidgetUpdateParams>(payload) {
                    Ok(update) => {
                        let _ = self
                            .tx
                            .send(crate::tui::AppEvent::ExtWidgetUpdate { plugin, update });
                    }
                    Err(e) => tracing::warn!("bad widgets/update payload: {e}"),
                }
            }
            // Unknown notifications are ignored by design (protocol
            // tolerance rule).
            _ => {}
        }
    }
}

/// Per-plugin services wrapper: tags requests and notifications with the
/// originating plugin's id (spoof-proof overwrite) before delegating — the
/// provider bridge needs reliable attribution for `host/registerProvider`
/// and `provider/streamEvent`, and `widgets/update` needs it because widget
/// ids are only unique per plugin.
struct TaggedServices {
    plugin: String,
    inner: Arc<dyn PeerHandler>,
}

#[async_trait::async_trait]
impl PeerHandler for TaggedServices {
    async fn handle_request(&self, method: &str, mut params: Value) -> Result<Value, ErrorObject> {
        if let Value::Object(map) = &mut params {
            map.insert("plugin".to_string(), Value::String(self.plugin.clone()));
        }
        self.inner.handle_request(method, params).await
    }
    async fn handle_notification(&self, method: &str, mut payload: Value) {
        if let Value::Object(map) = &mut payload {
            map.insert("plugin".to_string(), Value::String(self.plugin.clone()));
        }
        self.inner.handle_notification(method, payload).await;
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// One `extension.json`. A manifest may also contribute declarative bundle
/// resources (hooks/MCP servers/skills) without any running plugin.
#[derive(Clone, Debug, serde::Deserialize)]
struct ExtensionManifest {
    name: String,
    /// Semantic version (semver) of the plugin; becomes the store version
    /// directory on install. Absent ⇒ the install is `local`.
    #[serde(default)]
    version: Option<String>,
    /// Process carrier (default): executable to spawn.
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// "process" (default) | "wasm": run the plugin as a WASI module in
    /// the wasmtime sandbox (same v3 protocol over WASI stdio).
    #[serde(default)]
    carrier: Option<String>,
    /// WASM carrier: module file (.wasm/.wat), resolved against the
    /// extension directory.
    #[cfg(feature = "wasm")]
    #[serde(default)]
    module: Option<String>,
    /// WASM carrier resource limits (host-clamped).
    #[cfg(feature = "wasm")]
    #[serde(default)]
    limits: Option<WasmLimitsJson>,
    /// WASM carrier capability grants (default: full sandbox).
    #[cfg(feature = "wasm")]
    #[serde(default)]
    capabilities: Option<CapabilitiesJson>,
    /// Hook-failure mode: "open" (default, TS-compatible) | "closed"
    /// (a broken interceptor blocks the tool call).
    #[serde(default, rename = "failMode")]
    fail_mode: Option<String>,
    /// Bundle: hooks.json path(s) (Claude-format hook declarations).
    #[serde(default)]
    hooks: Option<Value>,
    /// Bundle: MCP servers — a file path or an inline server map.
    #[serde(default, rename = "mcpServers")]
    mcp_servers: Option<Value>,
    /// Level-2 MCP server plugin (`carrier: "mcp"`): ONE MCP server
    /// entry (same shape as an mcp.json server); the server IS the
    /// plugin — its tools/resources/prompts become the plugin's
    /// capabilities with the plugin's identity.
    #[serde(default, rename = "mcpServer")]
    mcp_server: Option<Value>,
    /// Bundle: skill directories (each containing SKILL.md files).
    #[serde(default)]
    skills: Option<Vec<String>>,
}

/// Level-2 MCP plugins: a stdio server runs with the extension directory
/// as cwd so bundled server scripts resolve, and a path-like relative
/// command resolves against that directory (same rule as the process
/// carrier — args are NOT rewritten: npm package names like `@scope/pkg`
/// contain '/'). HTTP/SSE specs are untouched.
fn resolve_mcp_stdio_spec(spec: &mut tack_tools::mcp::McpServerSpec, dir: &Path) {
    if let tack_tools::mcp::McpTransport::Stdio { command, cwd, .. } = &mut spec.transport {
        let path_like = command.contains('/') || command.contains('\\') || command.starts_with('.');
        if path_like && !PathBuf::from(command.as_str()).is_absolute() {
            *command = dir.join(command.as_str()).to_string_lossy().to_string();
        }
        *cwd = Some(dir.to_path_buf());
    }
}

/// WASM carrier: resolve the manifest's `module` against the extension
/// directory, rejecting anything that escapes it. An absolute `module`
/// (`dir.join(abs)` discards `dir`) or a `../` traversal would load
/// arbitrary host code under this plugin's identity, so absolute/prefixed
/// paths and `..` components are rejected lexically, and the
/// canonicalized module must stay under the canonicalized extension
/// directory (symlink defense).
#[cfg(feature = "wasm")]
fn resolve_module_path(dir: &Path, module: &str) -> anyhow::Result<PathBuf> {
    let rel = Path::new(module);
    let escapes = rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        });
    if escapes {
        anyhow::bail!("wasm module {module:?} must be a relative path inside the extension");
    }
    let joined = dir.join(rel);
    let resolved = joined
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", joined.display()))?;
    let root = dir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve extension dir {}: {e}", dir.display()))?;
    if !resolved.starts_with(&root) {
        anyhow::bail!("wasm module {module:?} escapes the extension directory");
    }
    Ok(resolved)
}

/// True for environment variable names that typically carry credentials
/// (manifest env pass-through must not receive the live host values for
/// WASM guests — same rule as the process carrier's strip list).
#[cfg_attr(not(feature = "wasm"), allow(dead_code))]
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

/// Declared capability grants for a WASM guest. Anything not declared is
/// denied (the wasmtime ctx stays fully sandboxed).
#[cfg(feature = "wasm")]
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CapabilitiesJson {
    /// Filesystem preopens: `[{"host": "dir", "guest": "/data", "access":
    /// "read-only"|"read-write"}]`. Relative `host` resolves against the
    /// extension directory.
    fs: Vec<FsGrantJson>,
    /// Environment: `{"K": "V"}` injects literal values; `["K"]` passes
    /// the host's current value of K through (unset vars are skipped).
    env: Option<Value>,
    /// argv visible to the guest.
    args: Vec<String>,
    /// Network flags. Inert for WASI p1 guests today.
    network: Option<NetworkJson>,
}

#[cfg(feature = "wasm")]
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FsGrantJson {
    host: String,
    guest: Option<String>,
    access: Option<String>,
}

#[cfg(feature = "wasm")]
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct NetworkJson {
    tcp: bool,
    udp: bool,
    dns: bool,
}

#[cfg(feature = "wasm")]
impl CapabilitiesJson {
    /// Lower to the carrier's capability struct. Relative host paths
    /// resolve against the extension directory; malformed grants are
    /// skipped with a warning (never silently widened).
    fn into_capabilities(self, name: &str, dir: &Path) -> tack_ext_wasm::WasmCapabilities {
        let mut caps = tack_ext_wasm::WasmCapabilities::default();
        for grant in self.fs {
            let Some(guest) = grant.guest.clone() else {
                tracing::warn!("extension {name}: fs grant without `guest` path skipped");
                continue;
            };
            let host_path = {
                let path = PathBuf::from(&grant.host);
                if path.is_absolute() {
                    path
                } else {
                    dir.join(path)
                }
            };
            let access = match grant.access.as_deref() {
                None | Some("read-only") => tack_ext_wasm::PreopenAccess::ReadOnly,
                Some("read-write") => tack_ext_wasm::PreopenAccess::ReadWrite,
                Some(other) => {
                    tracing::warn!(
                        "extension {name}: unknown fs access {other:?}, defaulting to read-only"
                    );
                    tack_ext_wasm::PreopenAccess::ReadOnly
                }
            };
            caps.preopens.push(tack_ext_wasm::PreopenGrant {
                host_path,
                guest_path: guest,
                access,
            });
        }
        match self.env {
            Some(Value::Object(map)) => {
                for (key, value) in map {
                    match value {
                        Value::String(value) => caps.env.push((key, value)),
                        other => tracing::warn!(
                            "extension {name}: env {key}: non-string value {other} skipped"
                        ),
                    }
                }
            }
            Some(Value::Array(names)) => {
                for name_ in names.iter().filter_map(Value::as_str) {
                    // Fail closed on host secrets (same rule as the
                    // process carrier's strip list).
                    if is_sensitive_env_key(name_) {
                        tracing::warn!(
                            "extension {name}: env pass-through {name_} matches the \
                             sensitive-name patterns the process carrier strips — skipped"
                        );
                        continue;
                    }
                    match std::env::var(name_) {
                        Ok(value) => caps.env.push((name_.to_string(), value)),
                        Err(_) => tracing::warn!(
                            "extension {name}: env pass-through {name_} unset, skipped"
                        ),
                    }
                }
            }
            Some(other) => {
                tracing::warn!(
                    "extension {name}: `capabilities.env` must be an object or a list of names, got {other}"
                )
            }
            None => {}
        }
        caps.args = self.args;
        if let Some(network) = self.network {
            caps.network = tack_ext_wasm::NetworkGrants {
                allow_tcp: network.tcp,
                allow_udp: network.udp,
                allow_dns: network.dns,
            };
        }
        caps
    }
}

#[cfg(feature = "wasm")]
#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WasmLimitsJson {
    max_fuel: Option<u64>,
    max_memory_bytes: Option<usize>,
    max_execution_ms: Option<u64>,
}

/// Host-side hard ceilings for WASM guests. Manifest limits are
/// self-declared by the plugin, so they may only TIGHTEN the sandbox —
/// anything above these caps is clamped down, never honored.
#[cfg(feature = "wasm")]
const HARD_MAX_FUEL: u64 = 10_000_000_000; // 1e10
#[cfg(feature = "wasm")]
const HARD_MAX_MEMORY_BYTES: usize = 1024 * 1024 * 1024; // 1 GiB

#[cfg(feature = "wasm")]
impl WasmLimitsJson {
    fn to_limits(self) -> tack_ext_wasm::WasmLimits {
        let defaults = tack_ext_wasm::WasmLimits::default();
        tack_ext_wasm::WasmLimits {
            max_fuel: self
                .max_fuel
                .unwrap_or(defaults.max_fuel)
                .min(HARD_MAX_FUEL),
            max_memory_bytes: self
                .max_memory_bytes
                .unwrap_or(defaults.max_memory_bytes)
                .min(HARD_MAX_MEMORY_BYTES),
            max_execution: self
                .max_execution_ms
                .map(std::time::Duration::from_millis)
                .or(defaults.max_execution),
        }
    }
}

/// Audit-log every non-default capability grant at load time — sandbox
/// widenings must be greppable in the host log.
#[cfg(feature = "wasm")]
fn audit_capability_grants(name: &str, caps: &tack_ext_wasm::WasmCapabilities) {
    for grant in &caps.preopens {
        tracing::warn!(
            "extension {name}: wasm fs grant {} -> {} ({:?})",
            grant.host_path.display(),
            grant.guest_path,
            grant.access,
        );
    }
    if !caps.env.is_empty() {
        let keys: Vec<&str> = caps.env.iter().map(|(k, _)| k.as_str()).collect();
        tracing::warn!("extension {name}: wasm env grant {keys:?}");
    }
    if caps.network.allow_tcp || caps.network.allow_udp || caps.network.allow_dns {
        tracing::warn!("extension {name}: wasm network grant {:?}", caps.network);
    }
}

// ---------------------------------------------------------------------------
// Running plugins
// ---------------------------------------------------------------------------

/// A running plugin's carrier handle (process or WASM module).
pub enum PluginHandle {
    /// Boxed: `V3Process` embeds `tokio::process::Child`, which dwarfs
    /// the other variants (especially on Windows).
    Process(Box<V3Process>),
    /// Option: shutdown consumes the plugin (take()).
    #[cfg(feature = "wasm")]
    Wasm(Option<tack_ext_wasm::WasmPlugin>),
    /// WIT component plugin (tack:plugin@0.3.0; the module format, not
    /// the manifest, selects this over the WASI-stdio carrier).
    #[cfg(feature = "wasm")]
    WasmComponent(Arc<tack_ext_wasm::component::WasmComponentPlugin>),
    /// Level-2 MCP server plugin (Option: shutdown consumes it).
    Mcp(Option<Arc<crate::mcp_plugin::McpPluginConnection>>),
}

impl std::fmt::Debug for PluginHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginHandle::Process(_) => f.debug_struct("PluginHandle::Process").finish(),
            #[cfg(feature = "wasm")]
            PluginHandle::Wasm(_) => f.debug_struct("PluginHandle::Wasm").finish(),
            #[cfg(feature = "wasm")]
            PluginHandle::WasmComponent(_) => {
                f.debug_struct("PluginHandle::WasmComponent").finish()
            }
            PluginHandle::Mcp(_) => f.debug_struct("PluginHandle::Mcp").finish(),
        }
    }
}

impl PluginHandle {
    /// The host→plugin call surface (carrier-agnostic).
    ///
    /// Panics when the WASM/MCP carrier has been shut down (its inner
    /// connection is consumed by `shutdown`). The upheld invariant is
    /// that callers gate on `LoadedPlugin::is_active`, which reports
    /// false once the plugin has been shut down — see
    /// `LoadedPlugin::stopped`.
    pub fn client(&self) -> Arc<dyn PluginConnection> {
        match self {
            PluginHandle::Process(process) => Arc::new(process.client.clone()),
            #[cfg(feature = "wasm")]
            PluginHandle::Wasm(plugin) => {
                Arc::new(plugin.as_ref().expect("wasm plugin taken").client.clone())
            }
            #[cfg(feature = "wasm")]
            PluginHandle::WasmComponent(plugin) => plugin.clone(),
            PluginHandle::Mcp(conn) => conn.as_ref().expect("mcp plugin taken").clone(),
        }
    }

    pub async fn shutdown(&mut self) {
        match self {
            PluginHandle::Process(process) => process.shutdown().await,
            #[cfg(feature = "wasm")]
            PluginHandle::Wasm(plugin) => {
                if let Some(plugin) = plugin.take() {
                    plugin.shutdown().await;
                }
            }
            #[cfg(feature = "wasm")]
            PluginHandle::WasmComponent(plugin) => plugin.shutdown().await,
            PluginHandle::Mcp(conn) => {
                if let Some(conn) = conn.take() {
                    let _ = PluginConnection::shutdown(&*conn).await;
                }
            }
        }
    }
}

/// Why a plugin failed to load. The telemetry vocabulary of roadmap §9 —
/// counts by outcome are broken down by these classes. `Policy` is not
/// constructed here: policy-filtered plugins carry `policy_block` instead
/// of `error`, so the class is implied by the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LoadErrorClass {
    /// Manifest missing/unparseable, or a manifest-declared capability
    /// (carrier, module, mcpServer entry) is malformed.
    Manifest,
    /// Carrier start or the initialize handshake failed.
    Handshake,
    /// Registration rejected after a successful handshake.
    Register,
    /// Managed plugin policy filtered the plugin (telemetry only — the
    /// row state is `policy_block`, not `error`).
    Policy,
    /// Store/lock layer rejected the checkout (drift, pinning).
    Store,
}

impl LoadErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            LoadErrorClass::Manifest => "manifest",
            LoadErrorClass::Handshake => "handshake",
            LoadErrorClass::Register => "register",
            LoadErrorClass::Policy => "policy",
            LoadErrorClass::Store => "store",
        }
    }
}

/// One discovered plugin (running or not). Failure is first-class state:
/// a plugin that failed to load stays in the list with `error` set, and
/// every consumer filters on [`LoadedPlugin::is_active`].
pub struct LoadedPlugin {
    pub id: PluginId,
    /// From settings `plugins."<id>".enabled` (default true); a managed
    /// `pluginPolicy.plugins."<id>".enabled` wins over the user/project
    /// layers in both directions.
    pub enabled: bool,
    /// Load/handshake failure, when any (capabilities empty then).
    pub error: Option<String>,
    /// Telemetry class of `error` (roadmap §9); None when `error` is None.
    pub error_class: Option<LoadErrorClass>,
    /// Managed plugin-policy block reason, when the load-time filter
    /// rejected this plugin (rows, not absences — roadmap §8.1/§8.3).
    pub policy_block: Option<String>,
    /// The handshake result (capabilities), when running.
    pub register: Option<InitializeResult>,
    /// The carrier, when running.
    pub handle: Option<PluginHandle>,
    /// The store version this plugin was loaded from (`local` or semver).
    pub version: String,
    /// The directory the plugin was loaded from.
    pub dir: PathBuf,
    /// Set by [`ExtensionManager::shutdown`]: WASM/MCP carrier shutdown
    /// consumes the inner connection (`Option::take`), so a post-shutdown
    /// `client()` on a still-"active" row would panic — `is_active` must
    /// report false once the handle has been shut down. Tracked here
    /// rather than by dropping `handle` so the row keeps its diagnostics.
    stopped: bool,
    /// The manifest's `failMode`, cached at load time. `hooks()` runs
    /// per agent run and per subagent spawn; re-reading extension.json
    /// there would let failMode drift from the loaded manifest and
    /// silently revert to fail-open if the file were corrupted
    /// mid-session.
    fail_mode: FailMode,
}

impl LoadedPlugin {
    /// A plain row (disabled or pending), no failure state.
    fn row(entry: Discovered, enabled: bool) -> Self {
        LoadedPlugin {
            id: entry.id,
            enabled,
            error: None,
            error_class: None,
            policy_block: None,
            register: None,
            handle: None,
            version: entry.version,
            dir: entry.dir,
            stopped: false,
            fail_mode: FailMode::default(),
        }
    }

    /// A failed row with a telemetry error class.
    fn failed(
        entry: Discovered,
        enabled: bool,
        class: LoadErrorClass,
        error: impl Into<String>,
    ) -> Self {
        LoadedPlugin {
            id: entry.id,
            enabled,
            error: Some(error.into()),
            error_class: Some(class),
            policy_block: None,
            register: None,
            handle: None,
            version: entry.version,
            dir: entry.dir,
            stopped: false,
            fail_mode: FailMode::default(),
        }
    }

    /// A policy-filtered row (the managed policy block reason).
    fn policy_blocked(entry: Discovered, enabled: bool, reason: String) -> Self {
        LoadedPlugin {
            id: entry.id,
            enabled,
            error: None,
            error_class: None,
            policy_block: Some(reason),
            register: None,
            handle: None,
            version: entry.version,
            dir: entry.dir,
            stopped: false,
            fail_mode: FailMode::default(),
        }
    }

    /// A running row (handshake done, carrier live).
    fn loaded(
        entry: Discovered,
        enabled: bool,
        register: InitializeResult,
        handle: PluginHandle,
        fail_mode: FailMode,
    ) -> Self {
        LoadedPlugin {
            id: entry.id,
            enabled,
            error: None,
            error_class: None,
            policy_block: None,
            register: Some(register),
            handle: Some(handle),
            version: entry.version,
            dir: entry.dir,
            stopped: false,
            fail_mode,
        }
    }

    pub fn is_active(&self) -> bool {
        self.enabled && !self.stopped && self.error.is_none() && self.policy_block.is_none()
    }

    /// The load outcome bucket for telemetry and the doctor report:
    /// `active | disabled | failed | policy-filtered` (roadmap §9).
    pub fn outcome(&self) -> &'static str {
        if self.policy_block.is_some() {
            "policy-filtered"
        } else if self.error.is_some() {
            "failed"
        } else if !self.enabled {
            "disabled"
        } else {
            "active"
        }
    }

    /// The plugin name (id's name segment).
    pub fn name(&self) -> &str {
        self.id.name()
    }

    fn capabilities(&self) -> Option<&tack_ext::rpc3::PluginCapabilities> {
        self.register.as_ref().map(|r| &r.capabilities)
    }
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("id", &self.id.to_string())
            .field("enabled", &self.enabled)
            .field("error", &self.error)
            .field("policy_block", &self.policy_block)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Load telemetry + doctor report
// ---------------------------------------------------------------------------

/// The on-disk load report (`<agentDir>/extensions/last-load.json`): the
/// most recent load outcome, consumed by `tack doctor` (which must not
/// spawn plugins to learn load health). Best-effort — a write failure is
/// a debug log, never a load problem.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LoadReport {
    pub version: u32,
    /// Unix seconds when the load finished.
    pub ts: u64,
    /// Run mode of the load (`tui | print | rpc | acp`).
    pub mode: String,
    pub plugins: Vec<LoadReportRow>,
    pub warnings: Vec<String>,
}

/// One plugin's row in the load report.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LoadReportRow {
    pub id: String,
    /// `active | disabled | failed | policy-filtered`.
    pub outcome: String,
    /// Telemetry error class (`manifest | handshake | register | policy |
    /// | store`); present for failed and policy-filtered rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    /// Failure cause or policy-block reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub version: String,
    pub dir: String,
}

/// The load-report path (`<agentDir>/extensions/last-load.json`).
pub fn load_report_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions").join("last-load.json")
}

/// Read the persisted load report; a missing/corrupt file is None (doctor
/// reports "no load recorded" rather than failing).
pub fn read_load_report(agent_dir: &Path) -> Option<LoadReport> {
    let content = std::fs::read_to_string(load_report_path(agent_dir)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Aggregate the load outcome into one structured `plugin_load` tracing
/// event (counts by outcome, broken down by error class — roadmap §9) and
/// persist the doctor report. The event flows to the observability JSONL
/// and the managed audit sink like any other structured event.
fn emit_load_telemetry(manager: &ExtensionManager, agent_dir: &Path, mode: &str) {
    let mut active = 0u64;
    let mut disabled = 0u64;
    let mut failed = 0u64;
    let mut policy_filtered = 0u64;
    let mut provider_bridges = 0u64;
    let mut class_counts = [0u64; 5]; // manifest, handshake, register, policy, store
    for plugin in &manager.plugins {
        match plugin.outcome() {
            "active" => {
                active += 1;
                if plugin
                    .capabilities()
                    .and_then(|caps| caps.provider.as_ref())
                    .and_then(|p| p.stream)
                    == Some(true)
                {
                    provider_bridges += 1;
                }
            }
            "disabled" => disabled += 1,
            "failed" => {
                failed += 1;
                let index = match plugin.error_class {
                    Some(LoadErrorClass::Manifest) => 0,
                    Some(LoadErrorClass::Handshake) => 1,
                    Some(LoadErrorClass::Register) => 2,
                    Some(LoadErrorClass::Policy) => 3,
                    Some(LoadErrorClass::Store) => 4,
                    None => continue,
                };
                class_counts[index] += 1;
            }
            _ => {
                policy_filtered += 1;
                class_counts[3] += 1; // policy
            }
        }
    }
    tracing::info!(
        target: "plugin_load",
        active,
        disabled,
        failed,
        policy_filtered,
        provider_bridges,
        class_manifest = class_counts[0],
        class_handshake = class_counts[1],
        class_register = class_counts[2],
        class_policy = class_counts[3],
        class_store = class_counts[4],
        "plugin load: {active} active, {disabled} disabled, {failed} failed, \
         {policy_filtered} policy-filtered, {provider_bridges} provider-bridging"
    );
    let report = LoadReport {
        version: 1,
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        mode: mode.to_string(),
        plugins: manager
            .plugins
            .iter()
            .map(|plugin| LoadReportRow {
                id: plugin.id.to_string(),
                outcome: plugin.outcome().to_string(),
                error_class: if plugin.policy_block.is_some() {
                    Some(LoadErrorClass::Policy.as_str().to_string())
                } else {
                    plugin.error_class.map(|c| c.as_str().to_string())
                },
                detail: plugin.error.clone().or_else(|| plugin.policy_block.clone()),
                version: plugin.version.clone(),
                dir: plugin.dir.display().to_string(),
            })
            .collect(),
        warnings: manager.load_warnings.clone(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        let path = load_report_path(agent_dir);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = crate::atomic_write::atomic_write(&path, &json) {
            tracing::debug!("cannot write plugin load report {}: {e}", path.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Declarative widgets and autocomplete providers
// ---------------------------------------------------------------------------

/// One registered widget: spec + latest full-state snapshot. The host key
/// is `<plugin-id>:<widget-id>`; updates are idempotent state replacements.
#[derive(Clone, Debug)]
pub struct WidgetEntry {
    pub key: String,
    pub plugin: String,
    pub spec: WidgetSpec,
    /// Latest state (spec.initial, replaced wholesale by widgets/update).
    pub state: Option<Value>,
    /// Panel visibility (spec.visible, then widgets/update.visible).
    pub visible: bool,
    /// Bumped on each applied update (TUI render-cache key).
    pub rev: u64,
}

impl WidgetEntry {
    /// Protocol snapshot for the remote surface (`list_ext_widgets`,
    /// `ext_widget_update`).
    pub fn to_protocol(&self) -> tack_protocol::schemas::ExtWidgetState {
        tack_protocol::schemas::ExtWidgetState {
            key: self.key.clone(),
            plugin: self.plugin.clone(),
            kind: match self.spec.r#type {
                WidgetKind::StatusLineSegment => "statusLineSegment",
                WidgetKind::MarkdownPanel => "markdownPanel",
                WidgetKind::ListPanel => "listPanel",
            }
            .to_string(),
            title: self.spec.title.clone(),
            state: self.state.clone(),
            visible: self.visible,
            rev: self.rev,
        }
    }
}

/// Everything needed to report a widget interaction back to the owning
/// plugin, resolved from the widget key off-lock (the JsonRpcPeer
/// multiplexes; `widgets/action` is a fire-and-forget notification).
pub struct WidgetActionRoute {
    client: Arc<dyn PluginConnection>,
    widget_id: String,
}

impl std::fmt::Debug for WidgetActionRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WidgetActionRoute")
            .field("widget_id", &self.widget_id)
            .finish()
    }
}

impl WidgetActionRoute {
    /// Send the `widgets/action` notification (owning plugin only).
    pub async fn notify(self, action: String, item_id: Option<String>) {
        let _ = self
            .client
            .widget_action(&WidgetActionParams {
                id: self.widget_id,
                action,
                item_id,
            })
            .await;
    }
}

/// Host-side widget registry. A dead plugin's widgets are removed (no UI
/// residue).
#[derive(Default, Debug)]
pub struct WidgetRegistry {
    entries: Vec<WidgetEntry>,
}

impl WidgetRegistry {
    /// Register a plugin's declared widgets (called after the handshake).
    pub(crate) fn register_plugin(&mut self, plugin: &str, widgets: &[WidgetSpec]) {
        for spec in widgets {
            self.entries.push(WidgetEntry {
                key: format!("{plugin}:{}", spec.id),
                plugin: plugin.to_string(),
                state: spec.initial.clone(),
                visible: spec.visible.unwrap_or(true),
                spec: spec.clone(),
                rev: 0,
            });
        }
    }

    /// Apply a widgets/update (full-state replacement). False = unknown
    /// widget id (the caller warns and ignores).
    pub fn apply_update(&mut self, plugin: &str, update: &WidgetUpdateParams) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.plugin == plugin && e.spec.id == update.id)
        else {
            return false;
        };
        entry.state = Some(update.state.clone());
        if let Some(visible) = update.visible {
            entry.visible = visible;
        }
        entry.rev += 1;
        true
    }

    /// Drop every widget owned by a dead plugin; returns the removed keys.
    pub fn remove_plugin(&mut self, plugin: &str) -> Vec<String> {
        let removed: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.plugin == plugin)
            .map(|e| e.key.clone())
            .collect();
        self.entries.retain(|e| e.plugin != plugin);
        removed
    }

    pub fn entries(&self) -> &[WidgetEntry] {
        &self.entries
    }
}

/// A registered autocomplete provider. Cloneable so queries can run on
/// spawned tasks off the TUI main loop.
#[derive(Clone)]
pub struct ExtAutocompleteProvider {
    /// Host key `<plugin-id>:<provider-id>`.
    pub key: String,
    pub plugin: String,
    pub spec: tack_ext::rpc3::AutocompleteProviderSpec,
    client: Arc<dyn PluginConnection>,
}

impl std::fmt::Debug for ExtAutocompleteProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtAutocompleteProvider")
            .field("key", &self.key)
            .finish()
    }
}

impl ExtAutocompleteProvider {
    /// Host key `<plugin>:<provider-id>`.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Protocol descriptor for the remote surface
    /// (`list_ext_autocomplete`).
    pub fn to_protocol(&self) -> tack_protocol::schemas::ExtAutocompleteProviderInfo {
        tack_protocol::schemas::ExtAutocompleteProviderInfo {
            key: self.key.clone(),
            plugin: self.plugin.clone(),
            id: self.spec.id.clone(),
            description: self.spec.description.clone(),
            trigger: self.spec.trigger.clone(),
        }
    }

    /// Query the plugin, converted to the protocol suggestion shape
    /// (remote `ext_autocomplete`).
    pub async fn provide_protocol(
        &self,
        query: &str,
        cursor_offset: usize,
    ) -> Vec<tack_protocol::schemas::ExtAutocompleteSuggestion> {
        self.provide(query, cursor_offset)
            .await
            .into_iter()
            .map(|s| tack_protocol::schemas::ExtAutocompleteSuggestion {
                label: s.label,
                value: s.value,
                insert_text: s.insert_text,
                detail: s.detail,
            })
            .collect()
    }

    /// Query the plugin's `autocomplete/provide`. Contract degradation:
    /// dead plugins, error responses, and malformed results all yield no
    /// suggestions.
    pub async fn provide(
        &self,
        query: &str,
        cursor_offset: usize,
    ) -> Vec<tack_ext::rpc3::AutocompleteSuggestion> {
        let params = tack_ext::rpc3::AutocompleteProvideParams {
            provider_id: self.spec.id.clone(),
            query: query.to_string(),
            cursor_offset: cursor_offset as u64,
        };
        self.client
            .autocomplete_provide(&params)
            .await
            .map(|r| r.suggestions)
            .unwrap_or_default()
    }
}

/// Spawn a watcher firing `on_dead` once the plugin's connection observes
/// EOF (carrier exit / guest trap / MCP server exit). The TUI uses it to
/// drop the plugin's widgets — a dead plugin leaves no UI residue.
pub fn watch_plugin_death(
    conn: Arc<dyn PluginConnection>,
    on_dead: impl FnOnce() + Send + 'static,
) {
    tokio::spawn(async move {
        conn.wait_dead().await;
        on_dead();
    });
}

// ---------------------------------------------------------------------------
// Lifecycle events (host → plugins)
// ---------------------------------------------------------------------------

/// Well-known lifecycle event names (the `events/lifecycle` `event`
/// field). Plugins subscribe via `capabilities.events`; an absent/empty
/// list means the default set below.
pub const DEFAULT_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionShutdown",
    "agentStart",
    "agentEnd",
    "turnStart",
    "turnEnd",
    "messageStart",
    "messageEnd",
    "toolExecutionStart",
    "toolExecutionEnd",
    "modelSelect",
    "thinkingLevelSelect",
];

// ---------------------------------------------------------------------------
// ExtensionManager
// ---------------------------------------------------------------------------

/// Owned handle for invoking one extension slash-command off the UI
/// loop (see [`ExtensionManager::command_invoker`] for why the invoke
/// must not be awaited inline).
pub struct CommandInvoker {
    client: Arc<dyn PluginConnection>,
    name: String,
}

impl std::fmt::Debug for CommandInvoker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandInvoker")
            .field("name", &self.name)
            .finish()
    }
}

impl CommandInvoker {
    /// `commands/invoke` against the owning plugin; the result lands as
    /// an `AppEvent::ExtCommandResult` on the TUI main loop.
    pub async fn invoke(self, args: String) -> Result<Value, String> {
        self.client
            .command_invoke(&tack_ext::rpc3::CommandInvokeParams {
                name: self.name,
                args: Some(args),
            })
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct ExtensionManager {
    /// All discovered plugins, including disabled and failed ones.
    pub plugins: Vec<LoadedPlugin>,
    /// Capability-level load warnings (a bad hooks file, one malformed
    /// MCP entry, an invalid tool schema…).
    pub load_warnings: Vec<String>,
    /// command name → (plugin index, contributed description).
    commands: HashMap<String, (usize, Option<String>)>,
    /// Declarative widget registry, keyed `<plugin-id>:<widget-id>`.
    widgets: WidgetRegistry,
    /// Bundle contributions from installed extensions (merged by callers).
    pub bundle_hooks: crate::shell_hooks::HookConfig,
    pub bundle_mcp_servers: Vec<tack_tools::mcp::McpServerSpec>,
    pub bundle_skill_dirs: Vec<PathBuf>,
    /// Shared wasmtime engine for WASM-carrier plugins.
    #[cfg(feature = "wasm")]
    wasm_carrier: Option<tack_ext_wasm::WasmCarrier>,
    /// Metrics sidecars for plugins whose declaration validated (drained
    /// every 30s and at shutdown; roadmap §9).
    metrics_sidecars: std::sync::Arc<std::sync::Mutex<Vec<crate::plugin_metrics::MetricsSidecar>>>,
    /// Stop signal for the drain task (set by shutdown and Drop).
    metrics_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Unique-per-manager tag for metrics scratch isolation
    /// (pid + counter): concurrent sessions (ACP keeps one manager per
    /// session) and concurrent tack processes sharing one agent_dir
    /// must never truncate or double-drain each other's live sidecar.
    metrics_instance: String,
}

impl Default for ExtensionManager {
    fn default() -> Self {
        static INSTANCE_COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        ExtensionManager {
            plugins: Vec::new(),
            load_warnings: Vec::new(),
            commands: HashMap::new(),
            widgets: WidgetRegistry::default(),
            bundle_hooks: crate::shell_hooks::HookConfig::default(),
            bundle_mcp_servers: Vec::new(),
            bundle_skill_dirs: Vec::new(),
            #[cfg(feature = "wasm")]
            wasm_carrier: None,
            metrics_sidecars: Default::default(),
            metrics_stop: Default::default(),
            metrics_instance: format!(
                "{}-{}",
                std::process::id(),
                INSTANCE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
        }
    }
}

impl Drop for ExtensionManager {
    fn drop(&mut self) {
        let already_stopped = self
            .metrics_stop
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if !already_stopped {
            // shutdown() never ran (a surface dropped the manager
            // without teardown): flush synchronously here rather than
            // losing the final batch — draining is plain sync file I/O.
            if let Ok(mut sidecars) = self.metrics_sidecars.lock() {
                for sidecar in sidecars.iter_mut() {
                    crate::plugin_metrics::drain_final(sidecar);
                }
            }
        }
    }
}

impl std::fmt::Debug for ExtensionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionManager")
            .field("plugins", &self.plugins.len())
            .finish()
    }
}

/// Whether the plugin's handshake capabilities opt into the approval
/// chain (`capabilities.hooks.approvalReview`).
fn declares_approval_review(caps: Option<&tack_ext::rpc3::PluginCapabilities>) -> bool {
    caps.and_then(|c| c.hooks.as_ref())
        .and_then(|h| h.approval_review)
        == Some(true)
}

/// Approval-chain reviewer over a plugin connection (`approval/review`).
/// Errors degrade to "pass" (fail-open: a broken or incapable reviewer
/// must not wedge every prompt — carriers that do not implement
/// `approval/review` answer `unsupported_capability`).
#[derive(Debug)]
struct PluginApprovalReviewer {
    client: Arc<dyn PluginConnection>,
}

#[async_trait::async_trait]
impl crate::approval::ApprovalReviewer for PluginApprovalReviewer {
    async fn review(
        &self,
        request: &crate::approval::ApprovalRequest,
    ) -> Option<crate::approval::ChainDecision> {
        let params = ApprovalReviewParams {
            approval_id: request.approval_id.clone(),
            approval_policy: request.approval_policy.clone(),
            evidence: Some(request.evidence.clone()),
            tool_call: ToolCall {
                arguments: request.arguments.clone(),
                tool_call_id: request.tool_call_id.clone(),
                tool_name: request.tool_name.clone(),
            },
        };
        match self.client.approval_review(&params).await {
            Ok(Some(decision)) => {
                let action = match decision.action {
                    ApprovalDecisionAction::Allow => crate::approval::ChainAction::Allow,
                    ApprovalDecisionAction::Reviewed => crate::approval::ChainAction::Reviewed,
                    ApprovalDecisionAction::AskUser => crate::approval::ChainAction::AskUser,
                };
                Some(crate::approval::ChainDecision {
                    action,
                    reason: decision.reason,
                })
            }
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(
                    target: "plugin_approval",
                    decision = "reviewer-error",
                    error = %e,
                    approval_id = request.approval_id.as_str(),
                    "approval reviewer failed; passing to the next reviewer"
                );
                None
            }
        }
    }
}

/// One discovered plugin directory: its id, version, and path.
#[derive(Debug)]
struct Discovered {
    id: PluginId,
    version: String,
    dir: PathBuf,
}

/// The store root (`<agentDir>/extensions/store`).
fn store_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions").join("store")
}

/// The per-plugin data root (`<agentDir>/extensions/data/<source>/<name>`).
pub fn plugin_data_dir(agent_dir: &Path, id: &PluginId) -> PathBuf {
    agent_dir
        .join("extensions")
        .join("data")
        .join(id.source())
        .join(id.name())
}

/// Parse a version directory name: `local` or a semver triple.
fn is_version_dir(name: &str) -> bool {
    name == "local" || parse_semver(name).is_some()
}

fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The active version directory of `store/<source>/<name>`: `local` when
/// present, else the highest semver directory. None when the plugin has
/// no version dirs.
fn active_version_dir(source_dir: &Path) -> Option<(String, PathBuf)> {
    let read = std::fs::read_dir(source_dir).ok()?;
    let mut local: Option<PathBuf> = None;
    let mut best: Option<((u64, u64, u64), String, PathBuf)> = None;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "local" {
            local = Some(path);
        } else if let Some(version) = parse_semver(&name) {
            let candidate = (version, name, path);
            if best
                .as_ref()
                .is_none_or(|(current, _, _)| candidate.0 > *current)
            {
                best = Some(candidate);
            }
        }
    }
    if let Some(path) = local {
        return Some(("local".to_string(), path));
    }
    best.map(|(_, name, path)| (name, path))
}

/// Enumerate installed store plugins as (id, version, dir).
fn discover_store(agent_dir: &Path) -> Vec<Discovered> {
    let mut out = Vec::new();
    let root = store_root(agent_dir);
    let Ok(sources) = std::fs::read_dir(&root) else {
        return out;
    };
    for source_entry in sources.flatten() {
        let source_path = source_entry.path();
        if !source_path.is_dir() {
            continue;
        }
        let source = source_entry.file_name().to_string_lossy().to_string();
        let Ok(names) = std::fs::read_dir(&source_path) else {
            continue;
        };
        for name_entry in names.flatten() {
            let name_path = name_entry.path();
            if !name_path.is_dir() {
                continue;
            }
            let name = name_entry.file_name().to_string_lossy().to_string();
            let Some((version, dir)) = active_version_dir(&name_path) else {
                continue;
            };
            if !dir.join("extension.json").is_file() {
                continue;
            }
            let Ok(id) = PluginId::new(&name, &source) else {
                tracing::warn!("ignoring store entry with invalid id {name}@{source}");
                continue;
            };
            out.push(Discovered { id, version, dir });
        }
    }
    out
}

/// Discover all plugin directories: store → legacy flat user dir →
/// settings extensionPaths → project dir (trust-gated).
fn discover(cwd: &Path, agent_dir: &Path) -> Vec<Discovered> {
    let mut out = discover_store(agent_dir);
    // Legacy flat user dir: <agentDir>/extensions/<name>/ (pre-P3
    // installs) loads as name@user, shadowed by a store install of the
    // same id.
    let user_root = agent_dir.join("extensions");
    if let Ok(read) = std::fs::read_dir(&user_root) {
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "store" || name == "data" {
                continue;
            }
            if !path.join("extension.json").is_file() {
                continue;
            }
            let Ok(id) = PluginId::new(&name, "user") else {
                continue;
            };
            if out.iter().any(|d| d.id == id) {
                continue;
            }
            out.push(Discovered {
                id,
                version: "local".to_string(),
                dir: path,
            });
        }
    }
    // settings extensionPaths → @local (single ext dir or a parent dir).
    for extra in crate::settings::Settings::extra_paths(agent_dir, "extensionPaths") {
        let path = PathBuf::from(extra);
        let candidates: Vec<PathBuf> = if path.join("extension.json").is_file() {
            vec![path]
        } else if path.is_dir() {
            std::fs::read_dir(&path)
                .map(|read| {
                    read.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_dir() && p.join("extension.json").is_file())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for dir in candidates {
            let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let Ok(id) = PluginId::new(&name, "local") else {
                continue;
            };
            out.push(Discovered {
                id,
                version: "local".to_string(),
                dir,
            });
        }
    }
    // Project extensions: trust-gated (they execute code).
    if crate::project_trust::is_trusted(cwd, agent_dir) {
        let project_ext = cwd.join(".pi").join("extensions");
        if let Ok(read) = std::fs::read_dir(project_ext) {
            for entry in read.flatten() {
                let path = entry.path();
                if !path.is_dir() || !path.join("extension.json").is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Ok(id) = PluginId::new(&name, "project") else {
                    continue;
                };
                out.push(Discovered {
                    id,
                    version: "local".to_string(),
                    dir: path,
                });
            }
        }
    }
    out
}

/// The `plugins."<id>".enabled` map from settings (delegates to
/// [`crate::settings::Settings::plugin_enabled_map`]; missing entries
/// default to enabled).
fn plugin_enabled_map(cwd: &Path, agent_dir: &Path) -> HashMap<String, bool> {
    crate::settings::Settings::plugin_enabled_map(cwd, agent_dir)
}

/// Write the enabled flag for one plugin id into the user settings file
/// (`tack ext enable|disable`).
pub fn set_plugin_enabled(agent_dir: &Path, id: &str, enabled: bool) -> anyhow::Result<()> {
    id.parse::<PluginId>()?;
    let path = agent_dir.join("settings.json");
    let mut raw: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !raw.is_object() {
        raw = serde_json::json!({});
    }
    let plugins = raw
        .as_object_mut()
        .expect("object")
        .entry("plugins".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let plugins = plugins
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings `plugins` is not an object"))?;
    let entry = plugins
        .entry(id.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let entry = entry
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings entry for {id} is not an object"))?;
    entry.insert("enabled".to_string(), Value::Bool(enabled));
    std::fs::create_dir_all(agent_dir)?;
    std::fs::write(&path, serde_json::to_string_pretty(&raw)?)?;
    Ok(())
}

impl ExtensionManager {
    /// Discover, enable-filter, and start all extensions. Failures are
    /// recorded per plugin (`LoadedPlugin::error`) and never fail session
    /// creation; capability-level problems become `load_warnings`.
    pub async fn load(
        cwd: &Path,
        agent_dir: &Path,
        mode: &str,
        services: Arc<dyn PeerHandler>,
        lock_required: bool,
        mcp_callbacks: tack_tools::mcp::McpClientCallbacks,
        bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
    ) -> Self {
        let policy = crate::plugin_policy::PluginPolicy::load();
        // Curated marketplace startup sync (roadmap §9): background,
        // best-effort — a failing sync never blocks the session.
        crate::marketplace_sync::start_background_sync(agent_dir.to_path_buf());
        Self::load_with_policy(
            cwd,
            agent_dir,
            mode,
            services,
            lock_required,
            mcp_callbacks,
            policy,
            bridge_state,
        )
        .await
    }

    /// `load` with an explicit managed plugin policy (the production
    /// entry point reads it from the managed settings file; tests pass
    /// one in directly — `TACK_MANAGED_SETTINGS` is process-global and
    /// parallel tests would race it).
    #[allow(clippy::too_many_arguments)]
    pub async fn load_with_policy(
        cwd: &Path,
        agent_dir: &Path,
        mode: &str,
        services: Arc<dyn PeerHandler>,
        lock_required: bool,
        mcp_callbacks: tack_tools::mcp::McpClientCallbacks,
        policy: Option<crate::plugin_policy::PluginPolicy>,
        bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
    ) -> Self {
        let mut manager = ExtensionManager::default();
        let trusted = crate::project_trust::is_trusted(cwd, agent_dir);
        let enabled_map = plugin_enabled_map(cwd, agent_dir);
        let discovered = discover(cwd, agent_dir);
        let (lock, lock_corrupt) = read_lock_for_gate(agent_dir);
        for entry in discovered {
            let id_string = entry.id.to_string();
            let mut enabled = enabled_map.get(&id_string).copied().unwrap_or(true);
            // Managed `enabled` wins over the user/project layers.
            if let Some(policy) = &policy
                && let Some(managed) = policy.managed_enabled(&id_string)
                && managed != enabled
            {
                policy.audit_enabled_override(&id_string, managed);
                enabled = managed;
            }
            // Load-time policy filter (the backstop): managedPluginsOnly
            // membership + origin re-verified against allowedSources.
            if let Some(policy) = &policy {
                let origin = match lock.plugins.get(&id_string) {
                    Some(locked) => crate::plugin_policy::LoadOrigin::Locked {
                        source: &locked.source,
                        rev: locked.rev.as_deref(),
                    },
                    None => crate::plugin_policy::LoadOrigin::Dir(&entry.dir),
                };
                if let Some(reason) = policy.load_block(&entry.id, origin) {
                    manager
                        .plugins
                        .push(LoadedPlugin::policy_blocked(entry, enabled, reason));
                    continue;
                }
            }
            // A disabled plugin needs no manifest: skip the parse so a
            // broken extension.json in a deliberately-disabled plugin
            // reports "disabled", not a spurious "failed" that doctor
            // would warn about forever.
            if !enabled {
                manager.plugins.push(LoadedPlugin::row(entry, false));
                continue;
            }
            let manifest_path = entry.dir.join("extension.json");
            let manifest: Option<ExtensionManifest> = match std::fs::read_to_string(&manifest_path)
            {
                Ok(content) => match serde_json::from_str::<ExtensionManifest>(&content) {
                    Ok(manifest) => Some(manifest),
                    Err(e) => {
                        manager.plugins.push(LoadedPlugin::failed(
                            entry,
                            enabled,
                            LoadErrorClass::Manifest,
                            format!("bad extension.json: {e}"),
                        ));
                        continue;
                    }
                },
                Err(e) => {
                    manager.plugins.push(LoadedPlugin::failed(
                        entry,
                        enabled,
                        LoadErrorClass::Manifest,
                        format!("cannot read extension.json: {e}"),
                    ));
                    continue;
                }
            };
            let manifest = manifest.expect("manifest checked");

            // Supply-chain gate: store installs with a lock entry pinned
            // to a resolved commit are skipped when the checkout drifted.
            if (entry.id.source() == "user" || !entry.id.is_reserved_source())
                && !lock_allows(&entry, &lock, lock_required, lock_corrupt)
            {
                manager.plugins.push(LoadedPlugin::failed(
                    entry,
                    enabled,
                    LoadErrorClass::Store,
                    "checkout drifted from the locked commit (extensionLockRequired)",
                ));
                continue;
            }

            // Bundle contributions (hooks/MCP/skills) are collected for
            // every enabled plugin, even when its carrier fails to start.
            manager.collect_bundle_resources(&id_string, &entry.dir, &manifest, policy.as_ref());

            // Tag this plugin's notifications (widgets/update needs the
            // origin).
            let services: Arc<dyn PeerHandler> = Arc::new(TaggedServices {
                plugin: id_string.clone(),
                inner: services.clone(),
            });
            let carrier = manifest.carrier.as_deref().unwrap_or("process");
            // Metrics sidecar scratch for carriers that support it
            // ((host path, scratchFile value)); set inside the spawn arms.
            let mut metrics_scratch: Option<(PathBuf, String)> = None;
            let mut handle = match carrier {
                "mcp" => {
                    // Level-2 MCP server plugin: the declared server IS
                    // the plugin (no tack-RPC process is spawned).
                    let Some(server) = &manifest.mcp_server else {
                        manager.plugins.push(LoadedPlugin::failed(
                            entry,
                            enabled,
                            LoadErrorClass::Manifest,
                            "carrier mcp requires an `mcpServer` entry",
                        ));
                        continue;
                    };
                    // Managed `plugins.mcpServers` narrowing applies to
                    // the plugin's OWN server too — for an mcp-carrier
                    // plugin the server IS the plugin, so a policy list
                    // that excludes it blocks the load outright (it was
                    // previously accepted but silently not enforced).
                    // Two entry forms are accepted in the list: the full
                    // plugin id (`jira@acme`) and the bare plugin name
                    // (`jira`). The list matches server names for bundle
                    // contributions, but an mcp-carrier's server has no
                    // name distinct from the plugin's, and an admin can
                    // naturally write either form — requiring one would
                    // silently block plugins listed in the other form.
                    if let Some(policy) = &policy
                        && let Some(allow) = policy.narrowed_mcp_servers(&id_string)
                        && !allow
                            .iter()
                            .any(|name| name == &id_string || name == entry.id.name())
                    {
                        policy.audit_narrow(
                            &id_string,
                            "plugins.mcpServers",
                            std::slice::from_ref(&id_string),
                        );
                        manager.plugins.push(LoadedPlugin::policy_blocked(
                            entry,
                            enabled,
                            format!(
                                "the plugin's own MCP server is not in the managed \
                                 plugins.mcpServers allow-list ({})",
                                policy.origin
                            ),
                        ));
                        continue;
                    }
                    let Some(mut spec) = crate::mcp_config::spec_from_entry(
                        &id_string,
                        server,
                        &format!("extension {id_string}"),
                    ) else {
                        manager.plugins.push(LoadedPlugin::failed(
                            entry,
                            enabled,
                            LoadErrorClass::Manifest,
                            "carrier mcp: malformed `mcpServer` entry",
                        ));
                        continue;
                    };
                    // A stdio server runs with the extension directory as
                    // cwd so bundled server scripts resolve; a path-like
                    // relative command resolves against it (same rule as
                    // the process carrier — args are NOT rewritten: npm
                    // package names like @scope/pkg contain '/').
                    resolve_mcp_stdio_spec(&mut spec, &entry.dir);
                    match crate::mcp_plugin::McpPluginConnection::connect(
                        &spec,
                        mcp_callbacks.clone(),
                        entry.id.name().to_string(),
                        entry.version.clone(),
                    )
                    .await
                    {
                        Ok(conn) => PluginHandle::Mcp(Some(Arc::new(conn))),
                        Err(e) => {
                            manager.plugins.push(LoadedPlugin::failed(
                                entry,
                                enabled,
                                LoadErrorClass::Handshake,
                                format!("failed to start (mcp): {e}"),
                            ));
                            continue;
                        }
                    }
                }
                "wasm" => {
                    #[cfg(feature = "wasm")]
                    {
                        let Some(module) = &manifest.module else {
                            manager.plugins.push(LoadedPlugin::failed(
                                entry,
                                enabled,
                                LoadErrorClass::Manifest,
                                "carrier wasm requires `module`",
                            ));
                            continue;
                        };
                        let module_path = match resolve_module_path(&entry.dir, module) {
                            Ok(path) => path,
                            Err(e) => {
                                manager.plugins.push(LoadedPlugin::failed(
                                    entry,
                                    enabled,
                                    LoadErrorClass::Manifest,
                                    e.to_string(),
                                ));
                                continue;
                            }
                        };
                        let wasm = match std::fs::read(&module_path) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                manager.plugins.push(LoadedPlugin::failed(
                                    entry,
                                    enabled,
                                    LoadErrorClass::Manifest,
                                    format!("cannot read {}: {e}", module_path.display()),
                                ));
                                continue;
                            }
                        };
                        if manager.wasm_carrier.is_none() {
                            match tack_ext_wasm::WasmCarrier::new() {
                                Ok(carrier) => manager.wasm_carrier = Some(carrier),
                                Err(e) => {
                                    manager.plugins.push(LoadedPlugin::failed(
                                        entry,
                                        enabled,
                                        LoadErrorClass::Handshake,
                                        format!("wasmtime unavailable: {e}"),
                                    ));
                                    continue;
                                }
                            }
                        }
                        let carrier_engine = manager.wasm_carrier.as_ref().expect("carrier");
                        let limits = manifest.limits.map(|l| l.to_limits()).unwrap_or_default();
                        let mut capabilities = manifest
                            .capabilities
                            .map(|c| c.into_capabilities(&id_string, &entry.dir))
                            .unwrap_or_default();
                        // WIT component vs WASI-stdio core module: the
                        // module format selects the carrier (both are
                        // `carrier: "wasm"` in the manifest).
                        let is_component = tack_ext_wasm::component::is_component(&wasm);
                        if !is_component {
                            // Metrics sidecar: a dedicated preopen for the
                            // scratch file (audited with the other grants).
                            match crate::plugin_metrics::create_scratch(
                                &plugin_data_dir(agent_dir, &entry.id),
                                &manager.metrics_instance,
                            ) {
                                Ok(path) => {
                                    let host_dir =
                                        path.parent().map(PathBuf::from).unwrap_or_default();
                                    capabilities.preopens.push(tack_ext_wasm::PreopenGrant {
                                        host_path: host_dir,
                                        guest_path: "/metrics".to_string(),
                                        access: tack_ext_wasm::PreopenAccess::ReadWrite,
                                    });
                                    metrics_scratch =
                                        Some((path, "/metrics/metrics.ndjson".to_string()));
                                }
                                Err(e) => manager.load_warnings.push(format!(
                                    "extension {id_string}: metrics scratch file unavailable: {e}"
                                )),
                            }
                        }
                        audit_capability_grants(&id_string, &capabilities);
                        if is_component {
                            match carrier_engine
                                .spawn_component(
                                    &wasm,
                                    &limits,
                                    &capabilities,
                                    entry.id.name().to_string(),
                                    entry.version.clone(),
                                )
                                .await
                            {
                                Ok(plugin) => PluginHandle::WasmComponent(Arc::new(plugin)),
                                Err(e) => {
                                    manager.plugins.push(LoadedPlugin::failed(
                                        entry,
                                        enabled,
                                        LoadErrorClass::Handshake,
                                        format!("failed to start (wasm component): {e}"),
                                    ));
                                    continue;
                                }
                            }
                        } else {
                            match carrier_engine
                                .spawn_with_capabilities(
                                    &wasm,
                                    &limits,
                                    services.clone(),
                                    &capabilities,
                                )
                                .await
                            {
                                Ok(plugin) => PluginHandle::Wasm(Some(plugin)),
                                Err(e) => {
                                    manager.plugins.push(LoadedPlugin::failed(
                                        entry,
                                        enabled,
                                        LoadErrorClass::Handshake,
                                        format!("failed to start (wasm): {e}"),
                                    ));
                                    continue;
                                }
                            }
                        }
                    }
                    #[cfg(not(feature = "wasm"))]
                    {
                        manager.plugins.push(LoadedPlugin::failed(
                            entry,
                            enabled,
                            LoadErrorClass::Handshake,
                            "carrier wasm requested, but this build has no wasm support",
                        ));
                        continue;
                    }
                }
                "process" => {
                    let Some(command) = &manifest.command else {
                        // Bundle-only extension (no runnable plugin):
                        // bundle resources were already collected.
                        continue;
                    };
                    let args: Vec<String> = manifest
                        .args
                        .iter()
                        .map(|a| {
                            let path_like =
                                a.contains('/') || a.contains('\\') || a.starts_with('.');
                            if path_like && !PathBuf::from(a).is_absolute() {
                                entry.dir.join(a).to_string_lossy().to_string()
                            } else {
                                a.clone()
                            }
                        })
                        .collect();
                    let env: Vec<(String, String)> = manifest.env.clone().into_iter().collect();
                    match V3Process::spawn(command, &args, &env, &entry.dir, services.clone()).await
                    {
                        Ok(process) => {
                            // Metrics sidecar: per-session scratch file.
                            match crate::plugin_metrics::create_scratch(
                                &plugin_data_dir(agent_dir, &entry.id),
                                &manager.metrics_instance,
                            ) {
                                Ok(path) => {
                                    let absolute =
                                        path.canonicalize().unwrap_or_else(|_| path.clone());
                                    metrics_scratch =
                                        Some((path, absolute.to_string_lossy().to_string()));
                                }
                                Err(e) => manager.load_warnings.push(format!(
                                    "extension {id_string}: metrics scratch file unavailable: {e}"
                                )),
                            }
                            PluginHandle::Process(Box::new(process))
                        }
                        Err(e) => {
                            manager.plugins.push(LoadedPlugin::failed(
                                entry,
                                enabled,
                                LoadErrorClass::Handshake,
                                format!("failed to start: {e}"),
                            ));
                            continue;
                        }
                    }
                }
                other => {
                    manager.plugins.push(LoadedPlugin::failed(
                        entry,
                        enabled,
                        LoadErrorClass::Manifest,
                        format!("unknown carrier {other:?}"),
                    ));
                    continue;
                }
            };

            // Handshake (v3 initialize). The connection is published to
            // the provider bridge first so a registration racing the
            // handshake waits on the capability state instead of failing.
            let serves_provider_stream = match &handle {
                PluginHandle::Process(_) => true,
                #[cfg(feature = "wasm")]
                PluginHandle::Wasm(_) => true,
                #[cfg(feature = "wasm")]
                PluginHandle::WasmComponent(_) => false,
                PluginHandle::Mcp(_) => false,
            };
            bridge_state.register_connection(&id_string, handle.client(), serves_provider_stream);
            let run_mode = match mode {
                "tui" => RunMode::Tui,
                "print" => RunMode::Print,
                // The remote host is client-driven, multi-turn and
                // permission-prompt-capable like rpc (the rpc3 RunMode
                // enum has no remote variant).
                "rpc" | "remote" => RunMode::Rpc,
                "acp" => RunMode::Acp,
                _ => RunMode::Print,
            };
            let init = InitializeParams {
                protocol_version: tack_ext::v3::PROTOCOL_VERSION.to_string(),
                host: HostInfo {
                    name: "tack".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                mode: run_mode,
                cwd: cwd.to_string_lossy().to_string(),
                trusted,
                capabilities: HostCapabilities {
                    widgets: Some(true),
                    autocomplete: Some(true),
                    session_control: Some(true),
                    snapshot: Some(false),
                    ui_dialogs: Some(mode == "tui"),
                    exec: Some(trusted),
                    provider_registration: Some(true),
                    metrics: metrics_scratch.as_ref().map(|(_, scratch_file)| {
                        MetricsHostCapability {
                            scratch_file: scratch_file.clone(),
                        }
                    }),
                },
                config: None,
            };
            let client = handle.client();
            match client.initialize(&init).await {
                Ok(mut register) => {
                    // Reject malformed tool parameter schemas at
                    // registration: a non-object schema would break
                    // provider request serialization.
                    if let Some(tools) = &mut register.capabilities.tools {
                        let name = id_string.clone();
                        let warnings = &mut manager.load_warnings;
                        tools.retain(|spec| {
                            if spec.parameters.is_object() {
                                true
                            } else {
                                warnings.push(format!(
                                    "Tool {:?} registered by extension {name} must define an object parameter schema.",
                                    spec.name
                                ));
                                false
                            }
                        });
                    }
                    // Managed policy tool narrowing (intersect-only): a
                    // tool not in the managed allow-list is dropped at
                    // registration, so every downstream consumer is
                    // compliant by construction.
                    if let Some(policy) = &policy
                        && let Some(allow) = policy.narrowed_tools(&id_string)
                        && let Some(tools) = &mut register.capabilities.tools
                    {
                        let registered: Vec<String> =
                            tools.iter().map(|spec| spec.name.clone()).collect();
                        tools.retain(|spec| allow.iter().any(|name| name == &spec.name));
                        let kept: Vec<String> =
                            tools.iter().map(|spec| spec.name.clone()).collect();
                        let dropped: Vec<String> = registered
                            .iter()
                            .filter(|name| !kept.contains(name))
                            .cloned()
                            .collect();
                        let unknown: Vec<String> = allow
                            .iter()
                            .filter(|name| !registered.contains(name))
                            .cloned()
                            .collect();
                        policy.audit_narrow(&id_string, "plugins.tools", &dropped);
                        if !dropped.is_empty() {
                            manager.load_warnings.push(format!(
                                "extension {id_string}: managed policy dropped tool(s): {}",
                                dropped.join(", ")
                            ));
                        }
                        if !unknown.is_empty() {
                            manager.load_warnings.push(format!(
                                "extension {id_string}: managed policy allows tool(s) the plugin does not register (narrow-only): {}",
                                unknown.join(", ")
                            ));
                        }
                    }
                    // Managed policy hook-capability gate (P5): a
                    // managed `hooks: false` strips the plugin's
                    // `capabilities.hooks` at registration, so it
                    // contributes NO hook bridges (beforeToolCall
                    // rewrite/block over every tool call, context
                    // transform, result patch, approval review) while
                    // its tools/commands still load — the same
                    // intersect-at-initialize rule as tools, except the
                    // capability is dropped whole rather than
                    // intersected. An org allowing a plugin for one
                    // benign tool must not silently grant it
                    // interception over the whole session.
                    if let Some(policy) = &policy
                        && !policy.hooks_allowed(&id_string)
                        && let Some(hooks) = register.capabilities.hooks.take()
                    {
                        let declared: Vec<String> = [
                            (hooks.before_tool_call == Some(true), "beforeToolCall"),
                            (hooks.transform_context == Some(true), "transformContext"),
                            (hooks.after_tool_call == Some(true), "afterToolCall"),
                            (hooks.approval_review == Some(true), "approvalReview"),
                        ]
                        .into_iter()
                        .filter(|(declared, _)| *declared)
                        .map(|(_, name)| name.to_string())
                        .collect();
                        policy.audit_narrow(&id_string, "plugins.hooks", &declared);
                        if !declared.is_empty() {
                            manager.load_warnings.push(format!(
                                "extension {id_string}: managed policy dropped hook capability(ies): {}",
                                declared.join(", ")
                            ));
                        }
                    }
                    // Managed policy provider-capability narrowing (P5):
                    // a managed deny of `provider` turns a plugin that
                    // declares provider.stream or provider.register
                    // policy-blocked — the same intersect-at-initialize
                    // rule as tools, except the capability is indivisible
                    // so denial blocks the load.
                    let declares_provider_stream = register
                        .capabilities
                        .provider
                        .as_ref()
                        .and_then(|p| p.stream)
                        == Some(true);
                    let declares_provider_register = register
                        .capabilities
                        .provider
                        .as_ref()
                        .and_then(|p| p.register)
                        == Some(true);
                    if (declares_provider_stream || declares_provider_register)
                        && let Some(policy) = &policy
                        && !policy.provider_allowed(&id_string)
                    {
                        let declared: Vec<String> = [
                            (declares_provider_stream, "stream"),
                            (declares_provider_register, "register"),
                        ]
                        .into_iter()
                        .filter(|(declared, _)| *declared)
                        .map(|(_, name)| name.to_string())
                        .collect();
                        policy.audit_narrow(&id_string, "plugins.provider", &declared);
                        bridge_state.set_provider_stream_granted(&id_string, false);
                        bridge_state.set_provider_register_granted(&id_string, false);
                        // The connection was published before the
                        // handshake; withdraw it like the
                        // handshake-failure path does, or the bridge
                        // keeps routing to a shut-down plugin.
                        bridge_state.remove_connection(&id_string);
                        handle.shutdown().await;
                        manager.plugins.push(LoadedPlugin::policy_blocked(
                            entry,
                            enabled,
                            format!(
                                "provider capability is denied by managed policy ({})",
                                policy.origin
                            ),
                        ));
                        continue;
                    }
                    bridge_state.set_provider_stream_granted(&id_string, declares_provider_stream);
                    bridge_state
                        .set_provider_register_granted(&id_string, declares_provider_register);
                    // Metrics sidecar (roadmap §9): the declared schema
                    // validates all-or-nothing; a valid declaration on a
                    // supported carrier gets a drained sidecar.
                    if let Some(declaration) = &register.capabilities.metrics {
                        match crate::plugin_metrics::validate_declaration(declaration) {
                            Err(reason) => manager.load_warnings.push(format!(
                                "extension {id_string}: metrics declaration voided: {reason}"
                            )),
                            Ok(()) => {
                                if let Some((path, _)) = &metrics_scratch {
                                    manager
                                        .metrics_sidecars
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .push(crate::plugin_metrics::MetricsSidecar::new(
                                            id_string.clone(),
                                            path.clone(),
                                            declaration.clone(),
                                        ));
                                } else {
                                    manager.load_warnings.push(format!(
                                        "extension {id_string}: metrics declaration voided: the \
                                         {carrier} carrier has no metrics sidecar (process and \
                                         WASI-stdio WASM only)"
                                    ));
                                }
                            }
                        }
                    }
                    tracing::info!(
                        "extension {} loaded (carrier {}, version {})",
                        id_string,
                        carrier,
                        entry.version,
                    );
                    if let Some(commands) = &register.capabilities.commands {
                        for command in commands {
                            manager.commands.insert(
                                command.name.clone(),
                                (manager.plugins.len(), command.description.clone()),
                            );
                        }
                    }
                    if let Some(widgets) = &register.capabilities.widgets {
                        manager.widgets.register_plugin(&id_string, widgets);
                    }
                    manager.plugins.push(LoadedPlugin::loaded(
                        entry,
                        enabled,
                        register,
                        handle,
                        FailMode::from_setting(manifest.fail_mode.as_deref()),
                    ));
                }
                Err(e) => {
                    bridge_state.remove_connection(&id_string);
                    handle.shutdown().await;
                    manager.plugins.push(LoadedPlugin::failed(
                        entry,
                        enabled,
                        LoadErrorClass::Handshake,
                        format!("handshake failed: {e}"),
                    ));
                }
            }
        }
        emit_load_telemetry(&manager, agent_dir, mode);
        manager.start_metrics_drain();
        manager
    }

    /// Collect a manifest's declarative bundle resources (hooks / MCP
    /// servers / skill dirs). Paths resolve against the extension
    /// directory. A managed policy `mcpServers` list narrows the
    /// collected servers (intersect-only).
    fn collect_bundle_resources(
        &mut self,
        name: &str,
        dir: &Path,
        manifest: &ExtensionManifest,
        policy: Option<&crate::plugin_policy::PluginPolicy>,
    ) {
        if let Some(hooks) = &manifest.hooks {
            let paths: Vec<&str> = match hooks {
                Value::String(path) => vec![path.as_str()],
                Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                _ => {
                    self.load_warnings.push(format!(
                        "extension {name}: `hooks` must be a path or a list of paths"
                    ));
                    vec![]
                }
            };
            for path in paths {
                let path = dir.join(path);
                match std::fs::read_to_string(&path) {
                    Ok(content) => match crate::shell_hooks::parse_hooks_file(&content) {
                        Ok(config) => self.bundle_hooks.extend(config),
                        Err(e) => self.load_warnings.push(format!(
                            "extension {name}: bad hooks file {}: {e}",
                            path.display()
                        )),
                    },
                    Err(e) => self.load_warnings.push(format!(
                        "extension {name}: cannot read {}: {e}",
                        path.display()
                    )),
                }
            }
        }
        let mcp_specs: Vec<tack_tools::mcp::McpServerSpec> = match &manifest.mcp_servers {
            Some(Value::String(path)) => {
                let path = dir.join(path);
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<Value>(&content) {
                        Ok(value) => {
                            crate::mcp_config::specs_from_value(&value, &path.display().to_string())
                        }
                        Err(e) => {
                            self.load_warnings.push(format!(
                                "extension {name}: bad MCP servers file {}: {e}",
                                path.display()
                            ));
                            Vec::new()
                        }
                    },
                    Err(e) => {
                        self.load_warnings.push(format!(
                            "extension {name}: cannot read {}: {e}",
                            path.display()
                        ));
                        Vec::new()
                    }
                }
            }
            Some(value @ Value::Object(_)) => {
                crate::mcp_config::specs_from_value(value, &format!("extension {name}"))
            }
            Some(_) => {
                self.load_warnings.push(format!(
                    "extension {name}: `mcpServers` must be a path or an object"
                ));
                Vec::new()
            }
            None => Vec::new(),
        };
        let mut mcp_specs = mcp_specs;
        if let Some(policy) = policy
            && let Some(allow) = policy.narrowed_mcp_servers(name)
            && !mcp_specs.is_empty()
        {
            let declared: Vec<String> = mcp_specs.iter().map(|spec| spec.name.clone()).collect();
            mcp_specs.retain(|spec| allow.iter().any(|name| name == &spec.name));
            let dropped: Vec<String> = declared
                .iter()
                .filter(|declared| !mcp_specs.iter().any(|spec| &spec.name == *declared))
                .cloned()
                .collect();
            let unknown: Vec<String> = allow
                .iter()
                .filter(|name| !declared.contains(name))
                .cloned()
                .collect();
            policy.audit_narrow(name, "plugins.mcpServers", &dropped);
            if !dropped.is_empty() {
                self.load_warnings.push(format!(
                    "extension {name}: managed policy dropped MCP server(s): {}",
                    dropped.join(", ")
                ));
            }
            if !unknown.is_empty() {
                self.load_warnings.push(format!(
                    "extension {name}: managed policy allows MCP server(s) the plugin does not declare (narrow-only): {}",
                    unknown.join(", ")
                ));
            }
        }
        self.bundle_mcp_servers.extend(mcp_specs);
        if let Some(skills) = &manifest.skills {
            for path in skills {
                let path = dir.join(path);
                if path.is_dir() {
                    self.bundle_skill_dirs.push(path);
                } else {
                    self.load_warnings.push(format!(
                        "extension {name}: skill dir {} missing",
                        path.display()
                    ));
                }
            }
        }
    }

    /// All active plugins' tools for the agent loop.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools_with_untrusted(None)
    }

    /// All active plugins' tools, wiring the prompt-injection defense for
    /// MCP-carrier plugins: their tool output is untrusted server data,
    /// so results are wrapped in `<untrusted_content>` and `untrusted`
    /// is set for the permission layer (same treatment as config-file
    /// MCP tools). Process/WASM plugin tools are host-trusted and pass
    /// through unwrapped.
    pub fn tools_with_untrusted(
        &self,
        untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Vec<Arc<dyn AgentTool>> {
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        // Sanitized tool names are NOT injective (`my.plugin@ac-me` and
        // `my-plugin@ac.me` sanitize identically) and dispatch is
        // first-match-wins — without a collision check, one plugin
        // silently shadows another's tool. First registration wins;
        // the duplicate is skipped loudly.
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for plugin in self.plugins.iter().filter(|p| p.is_active()) {
            let Some(handle) = &plugin.handle else {
                continue;
            };
            let Some(capabilities) = plugin.capabilities() else {
                continue;
            };
            if let Some(specs) = &capabilities.tools {
                for spec in specs {
                    let mut tool =
                        ExtTool::new(&plugin.id.to_string(), spec.clone(), handle.client());
                    if let (Some(flag), PluginHandle::Mcp(_)) = (&untrusted, handle) {
                        tool = tool.with_untrusted(
                            flag.clone(),
                            format!("mcp://{}/{}", plugin.id, spec.name),
                        );
                    }
                    if !seen.insert(tool.name()) {
                        tracing::warn!(
                            "extension {}: tool name {} collides with an already-registered \
                             tool; skipping the duplicate",
                            plugin.id,
                            tool.name()
                        );
                        continue;
                    }
                    tools.push(Arc::new(tool));
                }
            }
        }
        tools
    }

    /// Per-plugin hook bridges (tool-call interception, context
    /// transform, result patching) for active plugins that declared the
    /// capabilities.
    pub fn hooks(&self) -> Vec<Arc<dyn AgentHooks>> {
        self.plugins
            .iter()
            .filter(|p| p.is_active())
            .filter_map(|p| {
                let capabilities = p.capabilities()?.hooks.clone()?;
                let handle = p.handle.as_ref()?;
                // failMode comes from the load-time cache on the row, not
                // a fresh extension.json read (see `LoadedPlugin::fail_mode`).
                Some(Arc::new(ExtHooks::with_fail_mode(
                    handle.client(),
                    capabilities,
                    p.fail_mode,
                )) as Arc<dyn AgentHooks>)
            })
            .collect()
    }

    /// No active plugins — callers can skip per-event payload
    /// serialization.
    pub fn is_empty(&self) -> bool {
        !self.plugins.iter().any(|p| p.is_active())
    }

    /// The session's plugin approval chain (`approval/review`): active
    /// plugins that declared `capabilities.hooks.approvalReview`, in load
    /// order. Consulted by the permission layer at the point it would
    /// prompt a human (see `crate::approval`).
    pub fn approval_chain(&self) -> crate::approval::ApprovalChain {
        let mut chain = crate::approval::ApprovalChain::empty();
        for plugin in self.plugins.iter().filter(|p| p.is_active()) {
            if !declares_approval_review(plugin.capabilities()) {
                continue;
            }
            let Some(handle) = &plugin.handle else {
                continue;
            };
            chain.push(
                plugin.id.to_string(),
                Arc::new(PluginApprovalReviewer {
                    client: handle.client(),
                }),
            );
        }
        chain
    }

    /// Fan a lifecycle event out to subscribed plugins (fire-and-forget).
    pub async fn notify(&self, event: &str, payload: Value) {
        for plugin in self.plugins.iter().filter(|p| p.is_active()) {
            let Some(handle) = &plugin.handle else {
                continue;
            };
            let subscribed = match plugin.capabilities().and_then(|c| c.events.as_ref()) {
                None => DEFAULT_EVENTS.contains(&event),
                Some(events) if events.is_empty() => DEFAULT_EVENTS.contains(&event),
                Some(events) => events.iter().any(|s| s == event),
            };
            if subscribed {
                let _ = handle
                    .client()
                    .lifecycle_event(event, payload.clone())
                    .await;
            }
        }
    }

    /// Registered extension slash-command names.
    pub fn command_names(&self) -> Vec<String> {
        self.commands.keys().cloned().collect()
    }

    /// Registered extension slash commands with their contributed
    /// descriptions (remote `list_ext_commands`; sorted for a stable
    /// wire shape).
    pub fn command_specs(&self) -> Vec<tack_protocol::schemas::ExtCommandSpec> {
        let mut specs: Vec<tack_protocol::schemas::ExtCommandSpec> = self
            .commands
            .iter()
            .map(
                |(name, (_, description))| tack_protocol::schemas::ExtCommandSpec {
                    name: name.clone(),
                    description: description.clone(),
                },
            )
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Invoke an extension command. Direct caller-side await — used by
    /// tests and headless paths where no UI loop pumps plugin→host
    /// callbacks. The TUI uses [`Self::command_invoker`] instead.
    pub async fn invoke_command(&self, name: &str, args: &str) -> Result<Value, String> {
        let Some((index, _)) = self.commands.get(name) else {
            return Err(format!("unknown extension command {name:?}"));
        };
        let plugin = &self.plugins[*index];
        let Some(handle) = &plugin.handle else {
            return Err(format!("extension of command {name:?} is not running"));
        };
        handle
            .client()
            .command_invoke(&tack_ext::rpc3::CommandInvokeParams {
                name: name.to_string(),
                args: Some(args.to_string()),
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Resolve an extension command to an owned invoker for off-loop
    /// dispatch. The TUI must never await `commands/invoke` inline on
    /// its event loop: a plugin whose handler calls back into the host
    /// (ui/notify, ui/select, …) is answered by that very loop, so an
    /// inline await deadlocks until the 30s request timeout. Cloning
    /// the connection is cheap (Arc inside).
    pub fn command_invoker(&self, name: &str) -> Option<CommandInvoker> {
        let (index, _) = *self.commands.get(name)?;
        let handle = self.plugins.get(index)?.handle.as_ref()?;
        Some(CommandInvoker {
            client: handle.client(),
            name: name.to_string(),
        })
    }

    /// Declarative widgets: spec + current state, keyed `<plugin-id>:<id>`.
    pub fn widgets(&self) -> &[WidgetEntry] {
        self.widgets.entries()
    }

    /// Apply a plugin's widgets/update (full-state replacement). False =
    /// unknown widget id.
    pub fn apply_widget_update(&mut self, plugin: &str, update: &WidgetUpdateParams) -> bool {
        self.widgets.apply_update(plugin, update)
    }

    /// Remote `widgets/update` application: values-shaped (the remote
    /// bridge cannot name rpc3 types), returning the updated entry's
    /// protocol snapshot for the `ext_widget_update` broadcast. None =
    /// unknown widget id.
    pub fn apply_widget_update_remote(
        &mut self,
        plugin: &str,
        id: &str,
        state: Value,
        visible: Option<bool>,
    ) -> Option<tack_protocol::schemas::ExtWidgetState> {
        let update = WidgetUpdateParams {
            id: id.to_string(),
            state,
            visible,
        };
        if !self.apply_widget_update(plugin, &update) {
            return None;
        }
        self.widgets
            .entries()
            .iter()
            .find(|e| e.plugin == plugin && e.spec.id == id)
            .map(WidgetEntry::to_protocol)
    }

    /// Resolve a widget key to an off-lock action route (remote
    /// `ext_widget_action`). None = unknown key or the owning plugin is
    /// not running.
    pub fn widget_action_route(&self, key: &str) -> Option<WidgetActionRoute> {
        let entry = self.widgets.entries().iter().find(|e| e.key == key)?;
        let plugin = self
            .plugins
            .iter()
            .find(|p| p.id.to_string() == entry.plugin)?;
        let handle = plugin.handle.as_ref()?;
        Some(WidgetActionRoute {
            client: handle.client(),
            widget_id: entry.spec.id.clone(),
        })
    }

    /// A plugin died (peer EOF): its widgets vanish with it.
    pub fn remove_plugin_widgets(&mut self, plugin: &str) -> Vec<String> {
        self.widgets.remove_plugin(plugin)
    }

    /// Test hook: register a widget without a running plugin.
    #[doc(hidden)]
    pub fn test_insert_widget(&mut self, plugin: &str, spec: WidgetSpec) {
        self.widgets.register_plugin(plugin, &[spec]);
    }

    /// Registered autocomplete providers, in register order.
    pub fn autocomplete_providers(&self) -> Vec<ExtAutocompleteProvider> {
        let mut out = Vec::new();
        for plugin in self.plugins.iter().filter(|p| p.is_active()) {
            let Some(handle) = &plugin.handle else {
                continue;
            };
            let Some(capabilities) = plugin.capabilities() else {
                continue;
            };
            if let Some(providers) = &capabilities.autocomplete_providers {
                for spec in providers {
                    out.push(ExtAutocompleteProvider {
                        key: format!("{}:{}", plugin.id, spec.id),
                        plugin: plugin.id.to_string(),
                        spec: spec.clone(),
                        client: handle.client(),
                    });
                }
            }
        }
        out
    }

    /// Query one autocomplete provider by host key (contract degradation:
    /// unknown key / dead plugin → no suggestions).
    pub async fn autocomplete_provide(
        &self,
        provider_key: &str,
        query: &str,
        cursor_offset: usize,
    ) -> Vec<tack_ext::rpc3::AutocompleteSuggestion> {
        let Some(provider) = self
            .autocomplete_providers()
            .into_iter()
            .find(|p| p.key == provider_key)
        else {
            return Vec::new();
        };
        provider.provide(query, cursor_offset).await
    }

    /// Report a widget interaction back to the OWNING plugin only.
    pub async fn notify_widget_action(
        &self,
        plugin: &str,
        action: tack_ext::rpc3::WidgetActionParams,
    ) {
        let Some(plugin) = self.plugins.iter().find(|p| p.id.to_string() == plugin) else {
            return;
        };
        let Some(handle) = &plugin.handle else {
            return;
        };
        let _ = handle.client().widget_action(&action).await;
    }

    /// A shareable event sink for the provider-events wrapper.
    pub fn clone_sink(&self) -> Arc<ExtSinkHandle> {
        ExtSinkHandle::spawn(
            self.plugins
                .iter()
                .filter(|p| p.is_active())
                .filter_map(|p| {
                    let handle = p.handle.as_ref()?;
                    let subscriptions = p.capabilities().and_then(|c| c.events.clone());
                    Some((handle.client(), subscriptions))
                })
                .collect(),
        )
    }

    /// Gracefully stop all running plugins.
    pub async fn shutdown(&mut self) {
        // Plugins shut down CONCURRENTLY: their shutdowns are
        // independent (each only touches its own carrier handle — the
        // per-plugin bridge connection, child process, and wasm/mcp
        // connection are not shared), so sequential awaiting would just
        // multiply app-exit latency by the count of unresponsive
        // plugins. The only ordering dependency is the metrics sidecar
        // final drain below: plugins flush final measurements during
        // their own shutdown, so it runs AFTER the join.
        futures_util::future::join_all(self.plugins.iter_mut().map(|plugin| async move {
            if let Some(handle) = &mut plugin.handle {
                handle.shutdown().await;
            }
            // The carrier is stopped; `is_active` must report false now
            // (WASM/MCP shutdown consumed the inner connection, so a
            // post-shutdown `client()` would panic otherwise).
            plugin.stopped = true;
        }))
        .await;
        self.metrics_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut sidecars) = self.metrics_sidecars.lock() {
            for sidecar in sidecars.iter_mut() {
                crate::plugin_metrics::drain_final(sidecar);
            }
        }
    }

    /// Spawn the interval drain task when any sidecar registered (30s).
    fn start_metrics_drain(&self) {
        {
            let Ok(sidecars) = self.metrics_sidecars.lock() else {
                return;
            };
            if sidecars.is_empty() {
                return;
            }
        }
        let shared = self.metrics_sidecars.clone();
        let stop = self.metrics_stop.clone();
        crate::task::spawn_guarded("plugin-metrics-drain", async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                if let Ok(mut sidecars) = shared.lock() {
                    for sidecar in sidecars.iter_mut() {
                        crate::plugin_metrics::drain(sidecar);
                    }
                }
            }
            Some(())
        });
    }
}

/// A cheap-cloneable event sink sharing the plugins' clients (for the
/// provider-events wrapper, which outlives borrows of the manager).
///
/// `notify` is sync fire-and-forget called from provider code — one task
/// spawn PER EVENT used to flood the runtime under streaming load. Instead
/// events go into a single bounded queue drained by ONE consumer task; a
/// full queue drops events (they are best-effort lifecycle notifications,
/// never required for correctness).
#[derive(Debug)]
pub struct ExtSinkHandle {
    queue: mpsc::Sender<(String, Value)>,
}

/// Backlog bound for plugin-bound events before they are dropped.
const EXT_EVENT_QUEUE_DEPTH: usize = 256;

/// One event-sink target: the plugin connection plus its event
/// subscriptions (None/empty = the default set).
type EventTarget = (Arc<dyn PluginConnection>, Option<Vec<String>>);

impl ExtSinkHandle {
    fn spawn(peers: Vec<EventTarget>) -> Arc<Self> {
        let (tx, mut rx) = mpsc::channel::<(String, Value)>(EXT_EVENT_QUEUE_DEPTH);
        tokio::spawn(async move {
            while let Some((event, payload)) = rx.recv().await {
                for (client, subscriptions) in &peers {
                    let subscribed = match subscriptions {
                        None => DEFAULT_EVENTS.contains(&event.as_str()),
                        Some(events) if events.is_empty() => {
                            DEFAULT_EVENTS.contains(&event.as_str())
                        }
                        Some(events) => events.iter().any(|s| s == &event),
                    };
                    if subscribed {
                        let _ = client.lifecycle_event(&event, payload.clone()).await;
                    }
                }
            }
        });
        Arc::new(ExtSinkHandle { queue: tx })
    }
}

impl tack_ext::EventSink for ExtSinkHandle {
    /// Sync fire-and-forget (provider-boundary events are emitted from sync
    /// provider code); never blocks the caller.
    fn notify(&self, event: &str, payload: Value) {
        if self.queue.try_send((event.to_string(), payload)).is_err() {
            tracing::debug!("ext event queue full or closed; dropping event {event:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Store layout, lockfile, install, upgrade, verify
// ---------------------------------------------------------------------------

/// Extensions lockfile (~/.tack/agent/extensions-lock.json), v2: pins
/// every store install to the commit that was actually installed.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExtensionsLock {
    pub version: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, LockEntry>,
}

impl Default for ExtensionsLock {
    fn default() -> Self {
        ExtensionsLock {
            version: 2,
            plugins: BTreeMap::new(),
        }
    }
}

/// One locked plugin. `rev` is the user-requested ref (tag/branch/sha),
/// `resolved_commit` the 40-char sha HEAD actually resolved to (null for
/// local-directory installs), `version` the store version directory,
/// `store` whether the install lives in the versioned store (v2 installs)
/// or the legacy flat directory (v1 installs), `installed_at` unix
/// seconds.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockEntry {
    pub source: String,
    pub rev: Option<String>,
    pub resolved_commit: Option<String>,
    pub installed_at: u64,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub store: bool,
    #[serde(default)]
    pub marketplace: Option<String>,
}

pub fn lock_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions-lock.json")
}

/// Read the lockfile; a missing file is an empty lock (not an error).
/// v1 files (bare-name keys, no `store`/`version` fields) are upgraded in
/// memory: their plugins are `name@user` legacy flat installs.
pub(crate) fn read_lock(agent_dir: &Path) -> anyhow::Result<ExtensionsLock> {
    let path = lock_path(agent_dir);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ok(ExtensionsLock::default());
    };
    let mut lock: ExtensionsLock = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("bad extensions lockfile {}: {e}", path.display()))?;
    if lock.version < 2 {
        // v1 → v2 key upgrade (bare names are @user installs).
        let upgraded: BTreeMap<String, LockEntry> = lock
            .plugins
            .into_iter()
            .map(|(name, entry)| {
                let id = if name.contains('@') {
                    name
                } else {
                    format!("{name}@user")
                };
                (id, entry)
            })
            .collect();
        lock.plugins = upgraded;
        lock.version = 2;
    }
    Ok(lock)
}

/// Read the lockfile for the startup gate. The second tuple element is
/// true when a lockfile EXISTS but could not be read or parsed: with
/// `extensionLockRequired` the gate must fail CLOSED in that state —
/// falling back to an empty lock would silently disengage the
/// supply-chain check for every plugin (no entry ⇒ `lock_allows` says
/// yes), and the next lock write would destroy every resolved_commit
/// pin with no record they ever existed.
fn read_lock_for_gate(agent_dir: &Path) -> (ExtensionsLock, bool) {
    let path = lock_path(agent_dir);
    match std::fs::read_to_string(&path) {
        Ok(_) => match read_lock(agent_dir) {
            Ok(lock) => (lock, false),
            Err(e) => {
                tracing::warn!("ignoring unparseable extensions lockfile: {e}");
                (ExtensionsLock::default(), true)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (ExtensionsLock::default(), false),
        Err(e) => {
            tracing::warn!("extensions lockfile {} is unreadable: {e}", path.display());
            (ExtensionsLock::default(), true)
        }
    }
}

/// Rename a corrupt lockfile aside before it is rewritten, so the
/// resolved_commit pins it carried survive for forensics/recovery
/// instead of being silently destroyed by the next write.
fn backup_corrupt_lock(path: &Path, reason: &str) -> anyhow::Result<()> {
    let backup = path.with_extension("json.corrupt");
    tracing::warn!(
        "extensions lockfile is corrupt ({reason}); moving it to {} before rewriting",
        backup.display()
    );
    // Best-effort replace of a previous backup: losing the OLD backup
    // is acceptable, losing the CURRENT corrupt file is not.
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(path, &backup).map_err(|e| {
        anyhow::anyhow!(
            "extensions lockfile is corrupt and could not be backed up to {}: {e}",
            backup.display()
        )
    })
}

/// Read the lockfile for a read-modify-write cycle. A file that exists
/// but cannot be read or parsed is renamed aside (`…lock.json.corrupt`)
/// before the caller rewrites from an empty lock; when the rename fails
/// the update is refused outright (overwriting without a backup would
/// lose every pin irrecoverably).
fn read_lock_for_update(agent_dir: &Path) -> anyhow::Result<ExtensionsLock> {
    let path = lock_path(agent_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            if let Err(e) = serde_json::from_str::<ExtensionsLock>(&content) {
                backup_corrupt_lock(&path, &e.to_string())?;
                return Ok(ExtensionsLock::default());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            backup_corrupt_lock(&path, &format!("unreadable: {e}"))?;
            return Ok(ExtensionsLock::default());
        }
    }
    read_lock(agent_dir)
}

fn write_lock(agent_dir: &Path, lock: &ExtensionsLock) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    let path = lock_path(agent_dir);
    let content = serde_json::to_string_pretty(lock)?;
    // Atomic (tmp + rename): a plain fs::write can tear the JSON when
    // two processes write concurrently — and a torn lockfile made the
    // marketplace default-installs re-clone EVERY default plugin on
    // every startup without ever healing (read fell back to empty).
    crate::atomic_write::atomic_write(&path, &content)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))
}

/// Cross-process guard for lockfile read-modify-write. Atomic writes
/// prevent TORN files; they cannot stop two processes' read-modify-write
/// sequences from losing each other's entries (background marketplace
/// default-installs vs a concurrent `tack ext install`). Best-effort:
/// a guard abandoned by a crashed holder is reclaimed after
/// LOCK_GUARD_STALE_SECS, and a process that cannot acquire within the
/// wait budget proceeds without it rather than blocking startup.
const LOCK_GUARD_STALE_SECS: u64 = 120;
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn lock_guard(agent_dir: &Path) -> LockGuard {
    let path = lock_path(agent_dir).with_extension("guard");
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_000);
    loop {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(_) => return LockGuard { path },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|mtime| mtime.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > LOCK_GUARD_STALE_SECS);
                if stale {
                    // Rename-claim the stale guard aside (never delete
                    // in place — two concurrent reclaims could then
                    // both create_new successfully). Ownership is
                    // decided ONLY by the create_new below.
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    let claimed =
                        path.with_extension(format!("stale-{}-{nanos}", std::process::id()));
                    if std::fs::rename(&path, &claimed).is_ok() {
                        let _ = std::fs::remove_file(&claimed);
                        continue;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        "extensions lockfile guard held by another process; proceeding without it"
                    );
                    return LockGuard {
                        path: PathBuf::new(),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                return LockGuard {
                    path: PathBuf::new(),
                };
            }
        }
    }
}

/// Upsert the lock entry for a freshly installed extension.
fn lock_record_install(
    agent_dir: &Path,
    id: &PluginId,
    source: &str,
    rev: Option<&str>,
    resolved_commit: Option<String>,
    version: &str,
    marketplace: Option<&str>,
) -> anyhow::Result<()> {
    let _guard = lock_guard(agent_dir);
    let mut lock = read_lock_for_update(agent_dir)?;
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    lock.plugins.insert(
        id.to_string(),
        LockEntry {
            source: source.to_string(),
            rev: rev.map(str::to_string),
            resolved_commit,
            installed_at,
            version: Some(version.to_string()),
            store: true,
            marketplace: marketplace.map(str::to_string),
        },
    );
    write_lock(agent_dir, &lock)
}

/// Drop a removed extension's lock entry (no-op when absent).
fn lock_remove(agent_dir: &Path, id: &str) -> anyhow::Result<()> {
    let _guard = lock_guard(agent_dir);
    let mut lock = read_lock_for_update(agent_dir)?;
    if lock.plugins.remove(id).is_some() {
        write_lock(agent_dir, &lock)?;
    }
    Ok(())
}

/// `git rev-parse HEAD` of a checkout, or None when not a git repo / no HEAD.
fn git_head_commit(dir: &Path) -> Option<String> {
    let mut rev_parse = std::process::Command::new("git");
    rev_parse.args(["rev-parse", "HEAD"]).current_dir(dir);
    scrub_git_env(&mut rev_parse);
    let output = crate::sync_process::output_with_timeout(
        &mut rev_parse,
        std::time::Duration::from_secs(10),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if commit.is_empty() {
        None
    } else {
        Some(commit)
    }
}

/// `git status --porcelain` of a checkout: Some(true) when the working
/// tree is clean, Some(false) when dirty, None when git itself fails.
fn git_worktree_clean(dir: &Path) -> Option<bool> {
    let mut status = std::process::Command::new("git");
    status.args(["status", "--porcelain"]).current_dir(dir);
    scrub_git_env(&mut status);
    let output =
        crate::sync_process::output_with_timeout(&mut status, std::time::Duration::from_secs(10))
            .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout.iter().all(|b| b.is_ascii_whitespace()))
}

/// The directory a lock entry points at (store layout vs legacy flat).
fn lock_entry_dir(agent_dir: &Path, id: &PluginId, entry: &LockEntry) -> PathBuf {
    if entry.store {
        store_root(agent_dir)
            .join(id.source())
            .join(id.name())
            .join(entry.version.as_deref().unwrap_or("local"))
    } else {
        agent_dir.join("extensions").join(id.name())
    }
}

/// Outcome of checking one lock entry against the installed directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyStatus {
    /// HEAD matches the locked commit.
    Ok(String),
    /// HEAD drifted from the locked commit (tamper or manual update).
    Changed { expected: String, actual: String },
    /// The install directory exists but is not a git checkout.
    NotAGitRepo,
    /// The install directory is gone.
    Missing,
}

/// Check every lock entry with a resolved commit against its install
/// directory (`tack ext verify`).
pub fn verify_extensions(agent_dir: &Path) -> anyhow::Result<Vec<(String, VerifyStatus)>> {
    let lock = read_lock(agent_dir)?;
    let mut out = Vec::new();
    for (id_string, entry) in &lock.plugins {
        let Some(expected) = &entry.resolved_commit else {
            continue;
        };
        let Ok(id) = id_string.parse::<PluginId>() else {
            continue;
        };
        let dir = lock_entry_dir(agent_dir, &id, entry);
        let status = if !dir.is_dir() {
            VerifyStatus::Missing
        } else if !dir.join(".git").exists() {
            VerifyStatus::NotAGitRepo
        } else {
            match git_head_commit(&dir) {
                Some(actual) if &actual == expected => match git_worktree_clean(&dir) {
                    Some(true) => VerifyStatus::Ok(actual),
                    Some(false) => VerifyStatus::Changed {
                        expected: expected.clone(),
                        actual: format!("{actual} (+ uncommitted changes)"),
                    },
                    None => VerifyStatus::NotAGitRepo,
                },
                Some(actual) => VerifyStatus::Changed {
                    expected: expected.clone(),
                    actual,
                },
                None => VerifyStatus::NotAGitRepo,
            }
        };
        out.push((id_string.clone(), status));
    }
    Ok(out)
}

/// Startup gate: may this discovered plugin load, given the lockfile?
/// Only installs with a lock entry carrying a resolved commit are gated.
/// `lock_corrupt` marks a lockfile that existed but could not be read or
/// parsed: with `required` the gate fails CLOSED (no plugin has a
/// verifiable pin then); without it the warn-and-continue behavior is
/// kept.
fn lock_allows(
    discovered: &Discovered,
    lock: &ExtensionsLock,
    required: bool,
    lock_corrupt: bool,
) -> bool {
    let id_string = discovered.id.to_string();
    if lock_corrupt {
        if required {
            tracing::warn!(
                "extension {id_string}: the extensions lockfile is corrupt/unreadable and \
                 extensionLockRequired is set — skipping (fail-closed)"
            );
            return false;
        }
        return true;
    }
    let Some(entry) = lock.plugins.get(&id_string) else {
        return true;
    };
    let Some(expected) = &entry.resolved_commit else {
        return true;
    };
    let dir = &discovered.dir;
    let actual = if dir.join(".git").exists() {
        git_head_commit(dir)
    } else {
        None
    };
    match actual {
        Some(actual) if &actual == expected => match git_worktree_clean(dir) {
            Some(true) => true,
            dirty => {
                tracing::warn!(
                    "extension {id_string}: HEAD matches the locked commit {expected} but {} — {}",
                    if dirty == Some(false) {
                        "the working tree has uncommitted/untracked changes"
                    } else {
                        "the working tree could not be inspected"
                    },
                    if required {
                        "skipping (extensionLockRequired)"
                    } else {
                        "loading anyway (extensionLockRequired=false)"
                    }
                );
                !required
            }
        },
        Some(actual) => {
            tracing::warn!(
                "extension {id_string}: HEAD {actual} differs from locked commit {expected} — {}",
                if required {
                    "skipping (extensionLockRequired)"
                } else {
                    "loading anyway (extensionLockRequired=false)"
                }
            );
            !required
        }
        None => {
            tracing::warn!(
                "extension {id_string}: locked to commit {expected} but the install is not a git \
                 checkout — {}",
                if required {
                    "skipping (extensionLockRequired)"
                } else {
                    "loading anyway (extensionLockRequired=false)"
                }
            );
            !required
        }
    }
}

// ---------------------------------------------------------------------------
// Install channel (tack ext install/upgrade/remove)
// ---------------------------------------------------------------------------

fn copy_dir_recursive(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let dest = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Stage a local (non-git) source: a directory copy or a `.tgz` bundle
/// extraction (hostile-input rules — the air-gapped distribution unit).
fn stage_local_source(source: &str, staging: &Path) -> anyhow::Result<()> {
    let source_path = PathBuf::from(source);
    if crate::ext_bundle::is_bundle_path(source) && source_path.is_file() {
        crate::ext_bundle::stage_bundle_archive(&source_path, staging)
    } else if source_path.is_dir() {
        copy_dir_recursive(&source_path, staging)?;
        Ok(())
    } else {
        anyhow::bail!("{source} is neither a git URL, an existing directory, nor a .tgz bundle")
    }
}

/// Split a `<git-url>[#<ref>]` install source into (url, ref).
pub fn split_source_ref(source: &str) -> (String, Option<String>) {
    match source.rsplit_once('#') {
        Some((url, git_ref)) if !url.is_empty() && !git_ref.is_empty() => {
            (url.to_string(), Some(git_ref.to_string()))
        }
        _ => (source.to_string(), None),
    }
}

/// Scrub the git environment for plugin-store git invocations: no
/// terminal prompt and no inherited GIT_* config. This must wrap EVERY
/// git call that touches a checkout (clone, checkout, rev-parse,
/// status): an inherited `GIT_DIR`/`GIT_WORK_TREE` (tack run from a git
/// hook, a bare-repo script, some CI setups) otherwise silently
/// redirects the command to a different repository — installs would
/// record the wrong commit and the startup lock check would report
/// drift for every git-installed plugin.
pub(crate) fn scrub_git_env(command: &mut std::process::Command) -> &mut std::process::Command {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_COUNT")
}

/// Clone a git source into `target` (bounded, piped stdio, scrubbed git
/// environment: no terminal prompt, no inherited GIT_* config).
pub(crate) fn git_clone(source: &str, rev: Option<&str>, target: &Path) -> anyhow::Result<()> {
    let mut clone = std::process::Command::new("git");
    clone.arg("clone");
    scrub_git_env(&mut clone);
    if rev.is_none() {
        clone.args(["--depth", "1"]);
    }
    let output = crate::sync_process::output_with_timeout(
        clone.arg(source).arg(target),
        std::time::Duration::from_secs(600),
    )
    .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(target);
        anyhow::bail!(
            "git clone failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if let Some(rev) = rev {
        let mut checkout = std::process::Command::new("git");
        checkout.args(["checkout", rev]).current_dir(target);
        scrub_git_env(&mut checkout);
        let output = crate::sync_process::output_with_timeout(
            &mut checkout,
            std::time::Duration::from_secs(60),
        )
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
        if !output.status.success() {
            let _ = std::fs::remove_dir_all(target);
            anyhow::bail!(
                "git checkout {rev} failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

/// Per-process scratch-dir uniqueness (the pattern
/// `ext_bundle::BUNDLE_SCRATCH_COUNTER` established for bundle
/// extraction): concurrent same-process installs share one PID, so a
/// bare `.staging-<pid>` / `backup-<pid>` name would let one install
/// `remove_dir_all` another's staging mid-clone.
static STAGING_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A unique-per-process suffix for staging/backup directory names.
fn scratch_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        STAGING_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// The store version for an install: the manifest's `version` when it is
/// a semver, else the install rev stripped of a leading `v` when that is
/// a semver, else `local`.
fn install_version(manifest: &ExtensionManifest, rev: Option<&str>) -> String {
    if let Some(version) = &manifest.version
        && parse_semver(version).is_some()
    {
        return version.clone();
    }
    if let Some(rev) = rev {
        let stripped = rev.strip_prefix('v').unwrap_or(rev);
        if parse_semver(stripped).is_some() {
            return stripped.to_string();
        }
    }
    "local".to_string()
}

/// Install an extension from a git URL or a local directory. Returns the
/// installed directory (the active version dir).
pub fn install_extension(
    source: &str,
    cwd: &Path,
    agent_dir: &Path,
    local: bool,
) -> anyhow::Result<PathBuf> {
    install_extension_named(source, cwd, agent_dir, local, None, None, None)
}

/// Install with an optional name override (marketplace installs use the
/// plugin's marketplace key), an optional git ref pin, and the originating
/// marketplace name (recorded in the lockfile).
///
/// Git installs keep their `.git` directory so `ext verify` (and the
/// startup lock check) can compare `HEAD` against the lockfile's resolved
/// commit. User-dir installs land in the versioned store and write a
/// lockfile entry; project-local (`--local`) installs stay flat under
/// `.pi/extensions` and out of the lockfile.
pub fn install_extension_named(
    source: &str,
    cwd: &Path,
    agent_dir: &Path,
    local: bool,
    name_override: Option<&str>,
    rev: Option<&str>,
    marketplace: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let is_git = source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.ends_with(".git");

    // Managed plugin policy, first enforcement point: the source
    // allow-list is checked BEFORE any clone or network access; the
    // per-plugin rules run after the manifest is parsed (the id is
    // known) but before activation. Denials name the rule and layer.
    let policy = crate::plugin_policy::PluginPolicy::load();
    if let Some(policy) = &policy {
        policy.check_install_source(source, rev)?;
    }

    // Stage: fetch the plugin into a temporary sibling directory, then
    // validate and activate atomically.
    if local {
        // Project installs stay flat (trust-gated, no lockfile).
        let root = cwd.join(".pi").join("extensions");
        std::fs::create_dir_all(&root)?;
        let staging = root.join(format!(".staging-{}", scratch_suffix()));
        let _ = std::fs::remove_dir_all(&staging);
        if is_git {
            git_clone(source, rev, &staging)?;
        } else {
            if rev.is_some() {
                anyhow::bail!("`#<ref>` pinning only applies to git URLs");
            }
            stage_local_source(source, &staging)?;
        }
        let manifest = read_and_check_manifest(&staging)?;
        let name = name_override
            .map(str::to_string)
            .unwrap_or_else(|| manifest.name.clone());
        let id = PluginId::new(&name, "project")?;
        if let Some(policy) = &policy
            && let Err(denial) = policy.check_install_allowed(&id)
        {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(denial.into());
        }
        let target = root.join(id.name());
        activate_staging(&staging, &target)?;
        return Ok(target);
    }

    let id_source = marketplace.unwrap_or("user");
    if let Some(marketplace) = marketplace {
        tack_ext::plugin_id::validate_marketplace_name(marketplace)?;
    }
    let staging_parent = store_root(agent_dir).join(id_source);
    std::fs::create_dir_all(&staging_parent)?;

    // Stage under a scratch name; the manifest decides the plugin name.
    let staging = staging_parent.join(format!(".staging-{}", scratch_suffix()));
    let _ = std::fs::remove_dir_all(&staging);
    if is_git {
        git_clone(source, rev, &staging)?;
    } else {
        if rev.is_some() {
            anyhow::bail!("`#<ref>` pinning only applies to git URLs");
        }
        stage_local_source(source, &staging)?;
    }

    let manifest = read_and_check_manifest(&staging)?;
    let name = name_override
        .map(str::to_string)
        .unwrap_or_else(|| manifest.name.clone());
    let id = PluginId::new(&name, id_source)?;
    if let Some(policy) = &policy
        && let Err(denial) = policy.check_install_allowed(&id)
    {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(denial.into());
    }
    let version = install_version(&manifest, rev);
    let plugin_root = store_root(agent_dir).join(id.source()).join(id.name());
    std::fs::create_dir_all(&plugin_root)?;
    let target = plugin_root.join(&version);
    activate_staging(&staging, &target)?;

    // Record the pin and prune superseded versions.
    let resolved = if is_git {
        git_head_commit(&target)
    } else {
        None
    };
    lock_record_install(agent_dir, &id, source, rev, resolved, &version, marketplace)?;
    prune_versions(&plugin_root, &version);
    Ok(target)
}

/// Read the staged manifest and require the staged content to be stable
/// across the read (TOCTOU package-swap defense): the manifest is parsed
/// twice and must be byte-identical.
fn read_and_check_manifest(staging: &Path) -> anyhow::Result<ExtensionManifest> {
    let path = staging.join("extension.json");
    let first = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no extension.json in {}: {e}", staging.display()))?;
    let second = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("extension.json vanished mid-install: {e}"))?;
    if first != second {
        let _ = std::fs::remove_dir_all(staging);
        anyhow::bail!("extension.json changed during install (possible package swap)");
    }
    let manifest: ExtensionManifest =
        serde_json::from_str(&first).map_err(|e| anyhow::anyhow!("bad extension.json: {e}"))?;
    // The manifest name feeds the store path — validate it now.
    PluginId::new(&manifest.name, "user")?;
    Ok(manifest)
}

/// Move a staged directory into place. An existing target is swapped out
/// to a backup first (rename), then the staging is renamed in; any
/// failure rolls the backup back.
fn activate_staging(staging: &Path, target: &Path) -> anyhow::Result<()> {
    if target.exists() {
        let backup = target.with_extension(format!("backup-{}", scratch_suffix()));
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(target, &backup)?;
        if let Err(e) = std::fs::rename(staging, target) {
            let _ = std::fs::rename(&backup, target);
            return Err(e.into());
        }
        let _ = std::fs::remove_dir_all(&backup);
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(staging, target)?;
    }
    Ok(())
}

/// Remove version directories superseded by `keep` (`local` is never
/// pruned unless it IS `keep`). Prune failures are logged, never fatal.
fn prune_versions(plugin_root: &Path, keep: &str) {
    let Ok(read) = std::fs::read_dir(plugin_root) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == keep || name.starts_with(".staging-") || !is_version_dir(&name) {
            continue;
        }
        if name == "local" {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(&path) {
            tracing::warn!("failed to prune superseded version {}: {e}", path.display());
        }
    }
}

/// Remove an installed extension by id (`name@source`) or bare name
/// (matches a unique user/marketplace install).
pub fn remove_extension(
    name: &str,
    cwd: &Path,
    agent_dir: &Path,
    local: bool,
) -> anyhow::Result<()> {
    if local {
        // `name` becomes a path component under .pi/extensions —
        // validate it against the PluginId name grammar like the store
        // branch does, otherwise `tack ext remove --local ../../somedir`
        // deletes an arbitrary directory.
        let id = PluginId::new(name, "project")?;
        let target = cwd.join(".pi").join("extensions").join(id.name());
        if !target.is_dir() {
            anyhow::bail!("no extension named {name:?} installed");
        }
        std::fs::remove_dir_all(&target)?;
        return Ok(());
    }
    let id = resolve_installed_id(name, agent_dir)?;
    let plugin_root = store_root(agent_dir).join(id.source()).join(id.name());
    let legacy = agent_dir.join("extensions").join(id.name());
    let legacy_exists = id.source() == "user" && legacy.is_dir();
    if !plugin_root.is_dir() && !legacy_exists {
        anyhow::bail!("no extension named {name:?} installed");
    }
    if plugin_root.is_dir() {
        std::fs::remove_dir_all(&plugin_root)?;
    }
    // A legacy flat install of the same name may also exist.
    if legacy_exists {
        std::fs::remove_dir_all(&legacy)?;
    }
    lock_remove(agent_dir, &id.to_string())?;
    let _ = std::fs::remove_dir_all(plugin_data_dir(agent_dir, &id));
    Ok(())
}

/// Resolve a CLI name (id or bare name) to an installed plugin id.
fn resolve_installed_id(name: &str, agent_dir: &Path) -> anyhow::Result<PluginId> {
    if let Ok(id) = name.parse::<PluginId>() {
        return Ok(id);
    }
    // Bare name: search the store for a unique match.
    let mut matches = Vec::new();
    for discovered in discover_store(agent_dir) {
        if discovered.id.name() == name {
            matches.push(discovered.id);
        }
    }
    let legacy = agent_dir.join("extensions").join(name);
    if legacy.is_dir()
        && !matches.iter().any(|id| id.source() == "user")
        && let Ok(id) = PluginId::new(name, "user")
    {
        matches.push(id);
    }
    match matches.len() {
        0 => anyhow::bail!("no extension named {name:?} installed"),
        1 => Ok(matches.remove(0)),
        _ => anyhow::bail!(
            "extension name {name:?} is ambiguous (installed from several sources); use the full id name@source"
        ),
    }
}

/// Upgrade installed store extensions: re-fetch the locked source and
/// install the new resolved version. `name` selects one plugin (id or
/// bare name); None upgrades every git-sourced store install. Returns
/// (id, outcome) pairs.
pub fn upgrade_extensions(
    agent_dir: &Path,
    name: Option<&str>,
) -> anyhow::Result<Vec<(String, String)>> {
    let lock = read_lock(agent_dir)?;
    // A managed policy change since install also constrains upgrades:
    // re-check the recorded source before any network access.
    let policy = crate::plugin_policy::PluginPolicy::load();
    // Resolve the selector ONCE: an unresolvable name must error even
    // when the lockfile is empty (silently succeeding on a typo is how
    // "upgrade everything" mistakes go unnoticed), and per-entry
    // resolution would bail mid-loop after partial upgrades.
    let want = match name {
        Some(name) => Some(resolve_installed_id(name, agent_dir)?),
        None => None,
    };
    let mut out = Vec::new();
    for (id_string, entry) in lock.plugins.clone() {
        if !entry.store {
            continue;
        }
        let Ok(id) = id_string.parse::<PluginId>() else {
            continue;
        };
        if let Some(want) = &want
            && &id != want
        {
            continue;
        }
        let source = entry.source.clone();
        let is_git = source.starts_with("http://")
            || source.starts_with("https://")
            || source.starts_with("git@")
            || source.ends_with(".git");
        if !is_git {
            out.push((id_string.clone(), "skipped (not a git install)".to_string()));
            continue;
        }
        if let Some(policy) = &policy
            && let Err(denial) = policy.check_install_source(&source, entry.rev.as_deref())
        {
            out.push((id_string.clone(), format!("blocked: {denial}")));
            continue;
        }
        // Stage a fresh clone and compare the resolved commit first
        // (fingerprint idempotence: nothing to do).
        let staging = store_root(agent_dir)
            .join(id.source())
            .join(format!(".staging-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        if let Err(e) = git_clone(&source, entry.rev.as_deref(), &staging) {
            out.push((id_string.clone(), format!("failed: {e}")));
            continue;
        }
        let resolved = git_head_commit(&staging);
        if resolved.is_some() && resolved == entry.resolved_commit {
            let _ = std::fs::remove_dir_all(&staging);
            out.push((id_string.clone(), "up to date".to_string()));
            continue;
        }
        let manifest = match read_and_check_manifest(&staging) {
            Ok(manifest) => manifest,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                out.push((id_string.clone(), format!("failed: {e}")));
                continue;
            }
        };
        // The per-plugin policy gate from the install channel applies
        // here too: a managed policy change since install (e.g.
        // managedPluginsOnly newly enabled) must stop the new version
        // from being fetched and activated, not just from loading.
        if let Some(policy) = &policy
            && let Err(denial) = policy.check_install_allowed(&id)
        {
            let _ = std::fs::remove_dir_all(&staging);
            out.push((id_string.clone(), format!("blocked: {denial}")));
            continue;
        }
        let version = install_version(&manifest, entry.rev.as_deref());
        let plugin_root = store_root(agent_dir).join(id.source()).join(id.name());
        std::fs::create_dir_all(&plugin_root)?;
        let target = plugin_root.join(&version);
        if let Err(e) = activate_staging(&staging, &target) {
            out.push((id_string.clone(), format!("failed: {e}")));
            continue;
        }
        lock_record_install(
            agent_dir,
            &id,
            &source,
            entry.rev.as_deref(),
            resolved,
            &version,
            entry.marketplace.as_deref(),
        )?;
        prune_versions(&plugin_root, &version);
        out.push((id_string.clone(), format!("upgraded to {version}")));
    }
    Ok(out)
}

/// Installed extensions, for `tack ext list`: id, active version dir,
/// store vs legacy layout, enabled state, lock drift marker.
#[derive(Debug)]
pub struct InstalledInfo {
    pub id: String,
    pub dir: PathBuf,
    pub version: String,
    pub legacy: bool,
    pub enabled: bool,
    pub locked_commit: Option<String>,
    /// Managed policy block reason (the load-time filter's verdict);
    /// the plugin never runs while this is set.
    pub policy_block: Option<String>,
}

/// List installed extensions across the store, legacy dir, and project
/// dir (display layer; the session loader applies the same discovery).
pub fn list_extensions(cwd: &Path, agent_dir: &Path) -> Vec<InstalledInfo> {
    let enabled_map = plugin_enabled_map(cwd, agent_dir);
    let lock = read_lock(agent_dir).unwrap_or_default();
    let mut out = Vec::new();
    for discovered in discover_store(agent_dir) {
        let id_string = discovered.id.to_string();
        out.push(InstalledInfo {
            locked_commit: lock
                .plugins
                .get(&id_string)
                .and_then(|e| e.resolved_commit.clone()),
            enabled: enabled_map.get(&id_string).copied().unwrap_or(true),
            id: id_string,
            dir: discovered.dir,
            version: discovered.version,
            legacy: false,
            policy_block: None,
        });
    }
    // Legacy flat installs not shadowed by a store install.
    let user_root = agent_dir.join("extensions");
    if let Ok(read) = std::fs::read_dir(&user_root) {
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join("extension.json").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "store" || name == "data" {
                continue;
            }
            let Ok(id) = PluginId::new(&name, "user") else {
                continue;
            };
            if out.iter().any(|i| i.id == id.to_string()) {
                continue;
            }
            let id_string = id.to_string();
            out.push(InstalledInfo {
                locked_commit: lock
                    .plugins
                    .get(&id_string)
                    .and_then(|e| e.resolved_commit.clone()),
                enabled: enabled_map.get(&id_string).copied().unwrap_or(true),
                id: id_string,
                dir: path,
                version: "local".to_string(),
                legacy: true,
                policy_block: None,
            });
        }
    }
    // Project installs.
    let project_root = cwd.join(".pi").join("extensions");
    if let Ok(read) = std::fs::read_dir(&project_root) {
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join("extension.json").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(id) = PluginId::new(&name, "project") else {
                continue;
            };
            let id_string = id.to_string();
            out.push(InstalledInfo {
                locked_commit: None,
                enabled: enabled_map.get(&id_string).copied().unwrap_or(true),
                id: id_string,
                dir: path,
                version: "local".to_string(),
                legacy: true,
                policy_block: None,
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    // Managed policy overlay (display mirror of the load-time filter):
    // a managed `enabled` wins and blocked rows show their reason.
    if let Some(policy) = crate::plugin_policy::PluginPolicy::load() {
        for info in &mut out {
            let Ok(id) = info.id.parse::<PluginId>() else {
                continue;
            };
            if let Some(managed) = policy.managed_enabled(&info.id) {
                info.enabled = managed;
            }
            let origin = match lock.plugins.get(&info.id) {
                Some(locked) => crate::plugin_policy::LoadOrigin::Locked {
                    source: &locked.source,
                    rev: locked.rev.as_deref(),
                },
                None => crate::plugin_policy::LoadOrigin::Dir(&info.dir),
            };
            info.policy_block = policy.load_block(&id, origin);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Marketplaces (kept from the pre-v3 design; names now use the PluginId
// grammar)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Deserialize)]
struct MarketplaceFile {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    plugins: HashMap<String, MarketplacePlugin>,
    #[serde(default)]
    signature: Option<MarketplaceSignature>,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct MarketplaceSignature {
    algorithm: String,
    value: String,
}

/// Catalog v2 `installation` state (roadmap §9): `available` (default)
/// installs on demand; `not-available` refuses resolution with a clear
/// error; `installed-by-default` is auto-installed by the curated
/// marketplace startup sync (still policy-gated at install time).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarketplaceInstallation {
    #[default]
    Available,
    NotAvailable,
    InstalledByDefault,
}

impl MarketplaceInstallation {
    pub fn as_str(self) -> &'static str {
        match self {
            MarketplaceInstallation::Available => "available",
            MarketplaceInstallation::NotAvailable => "not-available",
            MarketplaceInstallation::InstalledByDefault => "installed-by-default",
        }
    }
}

impl<'de> serde::Deserialize<'de> for MarketplaceInstallation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "available" => Ok(MarketplaceInstallation::Available),
            "not-available" => Ok(MarketplaceInstallation::NotAvailable),
            "installed-by-default" => Ok(MarketplaceInstallation::InstalledByDefault),
            other => {
                // Forward-compatible: an unknown state must not break
                // listing older/newer catalogs — warn and treat as
                // available (the safe default: no auto-install).
                tracing::warn!(
                    "marketplace catalog: unknown installation state {other:?}; treating as available"
                );
                Ok(MarketplaceInstallation::Available)
            }
        }
    }
}

/// The keys a catalog v2 entry may carry; anything else is skipped with
/// a warning (forward compatibility with future catalog versions).
const MARKETPLACE_PLUGIN_KEYS: &[&str] =
    &["source", "description", "rev", "installation", "manifest"];

#[derive(Clone, Debug, serde::Deserialize)]
struct MarketplacePlugin {
    source: String,
    #[serde(default)]
    description: Option<String>,
    /// Optional git ref (tag/branch/sha) the plugin is pinned to.
    #[serde(default)]
    rev: Option<String>,
    /// Catalog v2 installation state (default `available`).
    #[serde(default)]
    installation: MarketplaceInstallation,
    /// Catalog v2 inline manifest fallback: rich listing (version,
    /// declared capabilities) without materializing the plugin.
    #[serde(default)]
    manifest: Option<Value>,
}

fn marketplaces_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("marketplaces")
}

/// `marketplaces_root` for the sibling sync module.
pub(crate) fn marketplaces_root_path(agent_dir: &Path) -> PathBuf {
    marketplaces_root(agent_dir)
}

/// The pinned ed25519 public key (hex) for a signed marketplace (TOFU).
fn marketplace_key_path(agent_dir: &Path, name: &str) -> PathBuf {
    marketplaces_root(agent_dir).join(format!("{name}.key"))
}

fn read_pinned_key(agent_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(marketplace_key_path(agent_dir, name))
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn parse_marketplace(path: &Path) -> anyhow::Result<MarketplaceFile> {
    let (parsed, _) = parse_marketplace_with_content(path)?;
    Ok(parsed)
}

fn parse_marketplace_with_content(path: &Path) -> anyhow::Result<(MarketplaceFile, String)> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    // Catalog v2 forward compatibility: unknown entry keys are skipped
    // with a warning, never a parse failure.
    if let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&content)
        && let Some(Value::Object(plugins)) = root.get("plugins")
    {
        for (name, entry) in plugins {
            if let Value::Object(fields) = entry {
                for key in fields.keys() {
                    if !MARKETPLACE_PLUGIN_KEYS.contains(&key.as_str()) {
                        tracing::warn!(
                            "marketplace {}: plugin {name}: unknown catalog key {key:?} (skipped)",
                            path.display()
                        );
                    }
                }
            }
        }
    }
    let parsed: MarketplaceFile = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("bad marketplace JSON {}: {e}", path.display()))?;
    Ok((parsed, content))
}

/// Canonical signed payload: the catalog JSON with the top-level
/// `signature` key removed, reserialized with serde_json (BTreeMap key
/// order). This is THE canonicalization rule — see docs/extensions.md.
fn canonical_marketplace_bytes(content: &str) -> anyhow::Result<Vec<u8>> {
    let mut value: Value =
        serde_json::from_str(content).map_err(|e| anyhow::anyhow!("bad marketplace JSON: {e}"))?;
    let Some(object) = value.as_object_mut() else {
        anyhow::bail!("marketplace catalog must be a JSON object");
    };
    object.remove("signature");
    Ok(serde_json::to_string(&value)?.into_bytes())
}

#[cfg(test)]
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(hex: &str) -> anyhow::Result<Vec<u8>> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        anyhow::bail!("hex string has odd length");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| anyhow::anyhow!("bad hex: {e}"))
        })
        .collect()
}

/// Verify a catalog's ed25519 signature against a hex public key.
fn verify_marketplace_signature(
    content: &str,
    signature: &MarketplaceSignature,
    public_key_hex: &str,
) -> anyhow::Result<()> {
    if signature.algorithm != "ed25519" {
        anyhow::bail!(
            "unsupported signature algorithm {:?} (expected ed25519)",
            signature.algorithm
        );
    }
    let key_bytes: [u8; 32] = hex_decode(public_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes (64 hex chars)"))?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| anyhow::anyhow!("bad ed25519 public key: {e}"))?;
    let sig_bytes: [u8; 64] = hex_decode(&signature.value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature must be 64 bytes (128 hex chars)"))?;
    let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    let message = canonical_marketplace_bytes(content)?;
    key.verify_strict(&message, &signature)
        .map_err(|e| anyhow::anyhow!("marketplace signature verification failed: {e}"))
}

/// Enforce the signature policy for an already-registered catalog.
fn verify_registered_marketplace(
    agent_dir: &Path,
    marketplace: &str,
    parsed: &MarketplaceFile,
    content: &str,
) -> anyhow::Result<()> {
    let Some(signature) = &parsed.signature else {
        return Ok(());
    };
    let Some(key) = read_pinned_key(agent_dir, marketplace) else {
        anyhow::bail!(
            "marketplace {marketplace} is signed but has no pinned key — re-register it with \
             `tack ext marketplace add {marketplace} <source> --public-key <hex>`"
        );
    };
    verify_marketplace_signature(content, signature, &key)
}

/// Activate a synced catalog: validate + signature-check, then
/// backup/rename/swap over the registered `<name>.json` (roadmap §9 —
/// a failing or hostile sync never destroys the working catalog).
/// `declared_key` is the settings-declared ed25519 key; a pinned TOFU
/// key takes precedence when both exist (rotation = delete the .key).
/// Returns true when the registered catalog changed.
pub(crate) fn activate_synced_catalog(
    agent_dir: &Path,
    name: &str,
    content: &[u8],
    declared_key: Option<&str>,
) -> anyhow::Result<bool> {
    tack_ext::plugin_id::validate_marketplace_name(name)?;
    let root = marketplaces_root(agent_dir);
    std::fs::create_dir_all(&root)?;
    let target = root.join(format!("{name}.json"));
    // Crash-window self-heal: a previous activation that died between
    // target→bak and staged→target left NO live catalog (nothing else
    // ever reads the .bak). Restore the last good one before deciding
    // anything — the source may be unreachable right now.
    if !target.exists() {
        let backup = root.join(format!("{name}.json.bak"));
        if backup.exists() {
            tracing::warn!(
                "marketplace {name}: live catalog missing after an interrupted sync; \
                 restoring the last backup"
            );
            let _ = std::fs::rename(&backup, &target);
        }
    }
    // Fingerprint-idempotent: byte-identical content is a no-op (the
    // caller's cheap fingerprint already short-circuits; this is the
    // exact backstop).
    if let Ok(existing) = std::fs::read(&target)
        && existing == content
    {
        return Ok(false);
    }
    let staged = root.join(format!(".staged-{name}-{}.json", std::process::id()));
    std::fs::write(&staged, content)?;
    let validation = (|| -> anyhow::Result<Option<String>> {
        let (parsed, content) = parse_marketplace_with_content(&staged)?;
        match &parsed.signature {
            Some(signature) => {
                let pinned = read_pinned_key(agent_dir, name);
                if let (Some(pinned), Some(declared)) = (&pinned, declared_key)
                    && !pinned.eq_ignore_ascii_case(declared)
                {
                    tracing::warn!(
                        "marketplace {name}: settings-declared public key differs from the \
                         pinned TOFU key; the pinned key wins (remove {} to rotate)",
                        marketplace_key_path(agent_dir, name).display()
                    );
                }
                let key = pinned.or_else(|| declared_key.map(str::to_string));
                let Some(key) = key else {
                    anyhow::bail!(
                        "marketplace {name}: catalog is signed; declare `publicKey` in \
                         pluginMarketplaces (trust on first use)"
                    );
                };
                verify_marketplace_signature(&content, signature, &key)?;
                Ok(Some(key))
            }
            None => {
                if read_pinned_key(agent_dir, name).is_some() {
                    // Signature-stripping downgrade: a pinned TOFU key
                    // means this marketplace MUST stay signed forever.
                    // Accepting an unsigned replacement would let
                    // anyone controlling the transport (not the key)
                    // void the pin and push code via
                    // installed-by-default.
                    anyhow::bail!(
                        "marketplace {name}: catalog lost its signature but a pinned TOFU key \
                         exists ({}) — refusing to activate an unsigned catalog",
                        marketplace_key_path(agent_dir, name).display()
                    );
                }
                Ok(None)
            }
        }
    })();
    let pin = match validation {
        Ok(pin) => pin,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    };
    // TOFU pinning: the first signed sync pins the key for every later
    // resolve/install (same rule as `marketplace add --public-key`).
    if let Some(key) = pin
        && read_pinned_key(agent_dir, name).is_none()
    {
        std::fs::write(
            marketplace_key_path(agent_dir, name),
            format!("{}\n", key.to_lowercase()),
        )?;
    }
    // Backup/rename/swap activation; best-effort rollback on failure.
    let backup = root.join(format!("{name}.json.bak"));
    if target.exists() {
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&target, &backup)?;
    }
    if let Err(e) = std::fs::rename(&staged, &target) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, &target);
        }
        // A leaked .staged-*.json would show up in list_marketplaces
        // as a phantom marketplace the CLI cannot remove.
        let _ = std::fs::remove_file(&staged);
        return Err(e.into());
    }
    Ok(true)
}

/// Register a marketplace from a local JSON file or an http(s) URL.
pub async fn add_marketplace(
    name: &str,
    source: &str,
    agent_dir: &Path,
    public_key: Option<&str>,
) -> anyhow::Result<PathBuf> {
    tack_ext::plugin_id::validate_marketplace_name(name)?;
    let root = marketplaces_root(agent_dir);
    std::fs::create_dir_all(&root)?;
    let target = root.join(format!("{name}.json"));
    if source.starts_with("http://") || source.starts_with("https://") {
        let mut response = reqwest::get(source)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch {source}: {e}"))?;
        if !response.status().is_success() {
            anyhow::bail!("fetching {source} failed: {}", response.status());
        }
        // Catalogs are small JSON documents; read with a hard cap
        // instead of buffering an unbounded body.
        const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;
        let body = crate::catalog_refresh::read_body_capped(
            &mut response,
            MAX_CATALOG_BYTES,
            &format!("marketplace catalog {source}"),
        )
        .await?;
        std::fs::write(&target, body)?;
    } else {
        let path = PathBuf::from(source);
        if !path.is_file() {
            anyhow::bail!("{source} is neither a URL nor an existing file");
        }
        std::fs::copy(&path, &target)?;
    }
    let validation = (|| -> anyhow::Result<Option<String>> {
        let (parsed, content) = parse_marketplace_with_content(&target)?;
        match &parsed.signature {
            Some(signature) => {
                let key = public_key
                    .map(str::to_string)
                    .or_else(|| read_pinned_key(agent_dir, name));
                let Some(key) = key else {
                    anyhow::bail!(
                        "marketplace catalog is signed; pass --public-key <hex> to pin the \
                         signer's ed25519 key (trust on first use)"
                    );
                };
                verify_marketplace_signature(&content, signature, &key)?;
                Ok(Some(key))
            }
            None => {
                if read_pinned_key(agent_dir, name).is_some() {
                    anyhow::bail!(
                        "marketplace {name}: catalog lost its signature but a pinned TOFU key \
                         exists ({}) — refusing to register an unsigned catalog",
                        marketplace_key_path(agent_dir, name).display()
                    );
                }
                tracing::warn!(
                    "marketplace {name}: catalog is unsigned; installs are not integrity-protected"
                );
                Ok(None)
            }
        }
    })();
    match validation {
        Ok(Some(key)) => {
            std::fs::write(
                marketplace_key_path(agent_dir, name),
                format!("{}\n", key.to_lowercase()),
            )?;
        }
        Ok(None) => {}
        Err(e) => {
            let _ = std::fs::remove_file(&target);
            return Err(e);
        }
    }
    Ok(target)
}

/// List registered marketplaces as (name, path).
pub fn list_marketplaces(agent_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(read) = std::fs::read_dir(marketplaces_root(agent_dir)) {
        for entry in read.flatten() {
            let path = entry.path();
            let Some(name) = path.file_stem().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            // Dotfiles (staged temps, editor backups) are never
            // marketplaces; without this filter a leaked
            // `.staged-acme-<pid>.json` listed as a phantom marketplace
            // that `ext marketplace remove` cannot delete.
            if name.starts_with('.') {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                out.push((name, path));
            }
        }
    }
    out.sort();
    out
}

/// Remove a registered marketplace.
pub fn remove_marketplace(name: &str, agent_dir: &Path) -> anyhow::Result<()> {
    tack_ext::plugin_id::validate_marketplace_name(name)?;
    let path = marketplaces_root(agent_dir).join(format!("{name}.json"));
    if !path.is_file() {
        anyhow::bail!("no marketplace named {name}");
    }
    std::fs::remove_file(&path)?;
    Ok(())
}

/// One catalog entry for listing (catalog v2: installation state + the
/// inline-manifest version when the entry carries one).
#[derive(Clone, Debug)]
pub struct MarketplacePluginInfo {
    pub name: String,
    pub source: String,
    pub description: Option<String>,
    pub rev: Option<String>,
    pub installation: MarketplaceInstallation,
    /// `version` from the inline manifest fallback, when present.
    pub version: Option<String>,
}

/// List a marketplace's plugins (catalog v2: installation state and the
/// inline manifest's version ride along for rich listing without
/// materializing the plugin).
pub fn marketplace_plugins(
    agent_dir: &Path,
    marketplace: &str,
) -> anyhow::Result<Vec<MarketplacePluginInfo>> {
    tack_ext::plugin_id::validate_marketplace_name(marketplace)?;
    let path = marketplaces_root(agent_dir).join(format!("{marketplace}.json"));
    if !path.is_file() {
        anyhow::bail!("no marketplace named {marketplace}");
    }
    let parsed = parse_marketplace(&path)?;
    let mut out: Vec<MarketplacePluginInfo> = parsed
        .plugins
        .into_iter()
        .map(|(name, plugin)| MarketplacePluginInfo {
            name,
            source: plugin.source,
            description: plugin.description,
            rev: plugin.rev,
            installation: plugin.installation,
            version: plugin
                .manifest
                .as_ref()
                .and_then(|m| m.get("version"))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Catalog entries marked `installed-by-default`, as install specs —
/// consumed by the curated marketplace startup sync (roadmap §9).
pub fn marketplace_default_installs(
    agent_dir: &Path,
    marketplace: &str,
) -> anyhow::Result<Vec<MarketplaceResolution>> {
    tack_ext::plugin_id::validate_marketplace_name(marketplace)?;
    let path = marketplaces_root(agent_dir).join(format!("{marketplace}.json"));
    if !path.is_file() {
        anyhow::bail!("no marketplace named {marketplace}");
    }
    let (parsed, content) = parse_marketplace_with_content(&path)?;
    verify_registered_marketplace(agent_dir, marketplace, &parsed, &content)?;
    let mut out = Vec::new();
    for (name, plugin) in &parsed.plugins {
        if plugin.installation != MarketplaceInstallation::InstalledByDefault {
            continue;
        }
        // The catalog key becomes the install name — a hostile catalog
        // must not escape the store via `../` or separators.
        PluginId::new(name, marketplace)?;
        out.push(MarketplaceResolution {
            plugin: name.clone(),
            source: plugin.source.clone(),
            rev: plugin.rev.clone(),
            marketplace: marketplace.to_string(),
        });
    }
    out.sort_by(|a, b| a.plugin.cmp(&b.plugin));
    Ok(out)
}

/// A resolved `<plugin>@<marketplace>` spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketplaceResolution {
    /// Plugin name (marketplace key; also the install name).
    pub plugin: String,
    /// Install source from the catalog.
    pub source: String,
    /// Catalog-pinned git ref, when the entry declares `"rev"`.
    pub rev: Option<String>,
    /// Marketplace the plugin resolved through.
    pub marketplace: String,
}

/// Resolve a `<plugin>@<marketplace>` spec. A signed catalog is
/// re-verified against its pinned key before anything is resolved.
pub fn resolve_marketplace_spec(
    spec: &str,
    agent_dir: &Path,
) -> anyhow::Result<Option<MarketplaceResolution>> {
    let Some((plugin, marketplace)) = spec.rsplit_once('@') else {
        return Ok(None);
    };
    // Not a marketplace spec: ssh-style git URLs (git@host:path) contain
    // an '@' too; marketplace names are bare file stems.
    if marketplace.contains(['/', '\\', ':']) {
        return Ok(None);
    }
    if plugin.is_empty() || marketplace.is_empty() {
        anyhow::bail!("invalid extension spec {spec:?}; expected <plugin>@<marketplace>");
    }
    let path = marketplaces_root(agent_dir).join(format!("{marketplace}.json"));
    if !path.is_file() {
        anyhow::bail!("no marketplace named {marketplace} (see `tack ext marketplace list`)");
    }
    let (parsed, content) = parse_marketplace_with_content(&path)?;
    verify_registered_marketplace(agent_dir, marketplace, &parsed, &content)?;
    let Some(entry) = parsed.plugins.get(plugin) else {
        anyhow::bail!("marketplace {marketplace} has no plugin named {plugin}");
    };
    if entry.installation == MarketplaceInstallation::NotAvailable {
        anyhow::bail!(
            "marketplace {marketplace} marks {plugin} as not-available (usually: withdrawn by the curator)"
        );
    }
    // The catalog key becomes the install name — a hostile catalog must
    // not escape the store via `../` or separators.
    PluginId::new(plugin, marketplace)?;
    Ok(Some(MarketplaceResolution {
        plugin: plugin.to_string(),
        source: entry.source.clone(),
        rev: entry.rev.clone(),
        marketplace: marketplace.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        // Tests also commit into fresh clones, which do not inherit the
        // source repo's local identity; CI runners have no global one.
        let output = crate::sync_process::output_with_timeout(
            std::process::Command::new("git")
                .args(["-c", "user.email=test@example.com", "-c", "user.name=test"])
                .args(args)
                .current_dir(dir),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn make_git_extension(repo: &Path) {
        make_git_extension_named(repo, "demo", None);
    }

    fn make_git_extension_named(repo: &Path, name: &str, version: Option<&str>) {
        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q", "-b", "main"]);
        let manifest = match version {
            Some(version) => serde_json::json!({"name": name, "version": version}).to_string(),
            None => serde_json::json!({"name": name}).to_string(),
        };
        std::fs::write(repo.join("extension.json"), manifest).unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "initial"]);
    }

    struct NoopServices;

    #[async_trait::async_trait]
    impl PeerHandler for NoopServices {}

    // ---- store layout ----

    #[test]
    fn active_version_prefers_local_then_highest_semver() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for dir in ["1.0.0", "1.2.0", "0.9.9"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let (version, _) = active_version_dir(root).unwrap();
        assert_eq!(version, "1.2.0");
        std::fs::create_dir_all(root.join("local")).unwrap();
        let (version, _) = active_version_dir(root).unwrap();
        assert_eq!(version, "local");
    }

    #[test]
    fn semver_parsing() {
        assert_eq!(parse_semver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.0.1"), Some((0, 0, 1)));
        assert!(parse_semver("v1.2.3").is_none());
        assert!(parse_semver("1.2").is_none());
        assert!(parse_semver("local").is_none());
        assert!(parse_semver("1.2.3.4").is_none());
    }

    // ---- install / lock / verify / upgrade ----

    #[test]
    fn local_install_lands_in_store_and_writes_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("extension.json"), r#"{"name":"plain"}"#).unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let target = install_extension(plain.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        assert_eq!(
            target,
            store_root(&agent_dir)
                .join("user")
                .join("plain")
                .join("local"),
            "unversioned local installs land in the store's local dir"
        );
        assert!(target.join("extension.json").is_file());

        let lock = read_lock(&agent_dir).unwrap();
        assert_eq!(lock.version, 2);
        let entry = lock.plugins.get("plain@user").expect("lock entry");
        assert_eq!(entry.version.as_deref(), Some("local"));
        assert!(entry.store);
        assert_eq!(entry.resolved_commit, None);

        // A bare-name remove resolves to the store install.
        remove_extension("plain", &cwd, &agent_dir, false).unwrap();
        assert!(!target.exists());
        assert!(read_lock(&agent_dir).unwrap().plugins.is_empty());
    }

    #[test]
    fn git_install_writes_lock_and_verifies_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let target = install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        assert!(target.join(".git").exists(), "git installs keep .git");
        let head = git_head_commit(&target).unwrap();

        let lock = read_lock(&agent_dir).unwrap();
        let entry = lock.plugins.get("demo@user").expect("lock entry");
        assert_eq!(entry.source, repo.to_string_lossy());
        assert_eq!(entry.resolved_commit.as_deref(), Some(head.as_str()));

        let results = verify_extensions(&agent_dir).unwrap();
        assert_eq!(
            results,
            vec![("demo@user".to_string(), VerifyStatus::Ok(head))]
        );
    }

    /// Every plugin-store git invocation must scrub the inherited git
    /// environment: with GIT_DIR set (tack run from a git hook),
    /// rev-parse/status would otherwise silently resolve the WRONG
    /// repository — recording bogus lockfile commits and falsely
    /// reporting checkout drift for every git-installed plugin.
    #[test]
    fn scrub_git_env_removes_inherited_git_vars() {
        let mut command = std::process::Command::new("git");
        scrub_git_env(&mut command);
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_EXEC_PATH",
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_COUNT",
        ] {
            assert!(
                envs.iter().any(|(k, v)| k == var && v.is_none()),
                "{var} must be env_remove'd: {envs:?}"
            );
        }
        assert!(
            envs.iter()
                .any(|(k, v)| k == "GIT_TERMINAL_PROMPT" && v.as_deref() == Some("0"))
        );
    }

    /// Lockfile mutations are atomic (intact JSON under concurrent
    /// writers) and the cross-process guard is released on drop.
    #[test]
    fn lock_record_install_is_atomic_and_releases_the_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let id = PluginId::new("demo", "user").unwrap();
        lock_record_install(
            &agent_dir,
            &id,
            "https://example.com/x.git",
            None,
            Some("abc".to_string()),
            "1.0.0",
            None,
        )
        .unwrap();
        let lock = read_lock(&agent_dir).unwrap();
        assert!(lock.plugins.contains_key("demo@user"));
        assert!(
            !lock_path(&agent_dir).with_extension("guard").exists(),
            "the guard file is released when the mutation returns"
        );
        lock_remove(&agent_dir, "demo@user").unwrap();
        assert!(read_lock(&agent_dir).unwrap().plugins.is_empty());
        assert!(!lock_path(&agent_dir).with_extension("guard").exists());
    }

    /// Concurrent same-process installs share one PID; the staging and
    /// backup dirs must still be unique per install (the
    /// `.staging-<pid>-<n>` counter suffix), or one install's
    /// `remove_dir_all` deletes another's staging mid-clone.
    #[test]
    fn parallel_installs_do_not_clobber_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut sources = Vec::new();
        for name in ["aaa", "bbb", "ccc", "ddd"] {
            let src = tmp.path().join(name);
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(
                src.join("extension.json"),
                format!(r#"{{"name":"{name}"}}"#),
            )
            .unwrap();
            sources.push(src);
        }
        std::thread::scope(|scope| {
            for src in &sources {
                scope.spawn(|| {
                    install_extension(src.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
                });
            }
        });
        for name in ["aaa", "bbb", "ccc", "ddd"] {
            assert!(
                store_root(&agent_dir)
                    .join("user")
                    .join(name)
                    .join("local")
                    .join("extension.json")
                    .is_file(),
                "{name} installed"
            );
        }
        let lock = read_lock(&agent_dir).unwrap();
        assert_eq!(lock.plugins.len(), 4, "no lock entry lost");
        let leftovers: Vec<_> = std::fs::read_dir(store_root(&agent_dir).join("user"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging dirs cleaned up: {leftovers:?}"
        );
    }

    /// A corrupt lockfile must not silently destroy the pins it carried:
    /// the next lock write renames it aside first.
    #[test]
    fn corrupt_lockfile_is_backed_up_before_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(lock_path(&agent_dir), "{ not json").unwrap();

        let id = PluginId::new("demo", "user").unwrap();
        lock_record_install(
            &agent_dir,
            &id,
            "https://example.com/x.git",
            None,
            Some("abc".to_string()),
            "1.0.0",
            None,
        )
        .unwrap();
        let backup = lock_path(&agent_dir).with_extension("json.corrupt");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "{ not json",
            "the corrupt original survives as a backup"
        );
        let lock = read_lock(&agent_dir).unwrap();
        assert!(lock.plugins.contains_key("demo@user"));

        // lock_remove on a corrupt lockfile backs up too (and the
        // corrupt file does not come back).
        std::fs::remove_file(&backup).unwrap();
        std::fs::write(lock_path(&agent_dir), "{ still not json").unwrap();
        lock_remove(&agent_dir, "demo@user").unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "{ still not json"
        );
    }

    /// Fail-closed gate: with `extensionLockRequired`, a corrupt
    /// lockfile rejects gated plugins; without it, the warn-and-continue
    /// behavior is kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn corrupt_lockfile_fails_closed_only_when_required() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        // Bundle-only legacy flat install (no `command`): passes the
        // gate ⇒ no row at all; rejected ⇒ a Store-class failure row.
        let ext_dir = agent_dir.join("extensions").join("demo");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("extension.json"), r#"{"name":"demo"}"#).unwrap();
        std::fs::write(lock_path(&agent_dir), "{ not json").unwrap();

        let manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "print",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        let plugin = manager
            .plugins
            .iter()
            .find(|p| p.id.name() == "demo")
            .expect("a fail-closed row");
        assert_eq!(plugin.error_class, Some(LoadErrorClass::Store));
        assert!(!plugin.is_active());

        let manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "print",
            Arc::new(NoopServices),
            false,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert!(
            manager
                .plugins
                .iter()
                .all(|p| p.error_class != Some(LoadErrorClass::Store)),
            "not required ⇒ the gate warns and continues: {:?}",
            manager.plugins
        );
    }

    /// The wasm `module` path must stay inside the extension directory —
    /// absolute paths, `..` traversal, and escaping symlinks would load
    /// arbitrary code under the plugin's identity.
    #[cfg(feature = "wasm")]
    #[test]
    fn wasm_module_path_must_stay_inside_the_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ext");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/mod.wasm"), b"\0asm").unwrap();
        assert!(resolve_module_path(&dir, "sub/mod.wasm").is_ok());
        assert!(resolve_module_path(&dir, "../escape.wasm").is_err());
        assert!(resolve_module_path(&dir, "/etc/hostname").is_err());
        assert!(resolve_module_path(&dir, "missing.wasm").is_err());
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside.wasm");
            std::fs::write(&outside, b"\0asm").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("link.wasm")).unwrap();
            assert!(
                resolve_module_path(&dir, "link.wasm").is_err(),
                "a symlink pointing outside the extension dir is an escape"
            );
        }
    }

    /// `remove --local` validates the name before joining it under
    /// .pi/extensions — a traversal name must not delete arbitrary dirs.
    #[test]
    fn remove_local_rejects_traversal_names() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        // `.pi/extensions/../../victim` resolves to `<cwd>/victim`.
        let victim = cwd.join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), "x").unwrap();
        for name in ["../../victim", "..", "a/b", "/abs", "name@project"] {
            assert!(
                remove_extension(name, &cwd, &agent_dir, true).is_err(),
                "{name} rejected"
            );
        }
        assert!(
            victim.join("keep.txt").is_file(),
            "the traversal target is untouched"
        );
    }

    /// Moving HEAD in the installed checkout makes verify report changed.
    #[test]
    fn verify_detects_head_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let target = install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        let locked = git_head_commit(&target).unwrap();

        std::fs::write(target.join("extra.txt"), "tampered").unwrap();
        git(&target, &["add", "."]);
        git(&target, &["commit", "-q", "-m", "tamper"]);

        let results = verify_extensions(&agent_dir).unwrap();
        let [(id, VerifyStatus::Changed { expected, actual })] = results.as_slice() else {
            panic!("expected one changed entry, got {results:?}");
        };
        assert_eq!(id, "demo@user");
        assert_eq!(expected, &locked);
        assert_ne!(actual, &locked);
    }

    /// `#<ref>` pins the checkout; a semver tag becomes the store version.
    #[test]
    fn rev_pin_checks_out_and_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let first = git_head_commit(&repo).unwrap();
        git(&repo, &["tag", "v1.2.0"]);
        std::fs::write(repo.join("v2.txt"), "v2").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "second"]);

        let (source, rev) = split_source_ref(&format!("{}#v1.2.0", repo.display()));
        assert_eq!(rev.as_deref(), Some("v1.2.0"));

        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let target =
            install_extension_named(&source, &cwd, &agent_dir, false, None, rev.as_deref(), None)
                .unwrap();
        assert_eq!(git_head_commit(&target).unwrap(), first);
        assert_eq!(
            target,
            store_root(&agent_dir)
                .join("user")
                .join("demo")
                .join("1.2.0"),
            "a semver tag rev becomes the store version"
        );
        let lock = read_lock(&agent_dir).unwrap();
        assert_eq!(
            lock.plugins.get("demo@user").unwrap().version.as_deref(),
            Some("1.2.0")
        );
    }

    /// upgrade is fingerprint-idempotent, then installs the new commit
    /// when upstream advances.
    #[test]
    fn upgrade_is_idempotent_then_advances() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();

        let outcomes = upgrade_extensions(&agent_dir, None).unwrap();
        assert_eq!(
            outcomes,
            vec![("demo@user".to_string(), "up to date".to_string())]
        );

        // Advance upstream: the next upgrade replaces the local dir.
        std::fs::write(repo.join("v2.txt"), "v2").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "second"]);
        let outcomes = upgrade_extensions(&agent_dir, None).unwrap();
        assert_eq!(
            outcomes,
            vec![("demo@user".to_string(), "upgraded to local".to_string())]
        );
        let target = store_root(&agent_dir)
            .join("user")
            .join("demo")
            .join("local");
        assert!(target.join("v2.txt").is_file());
        let lock = read_lock(&agent_dir).unwrap();
        assert_eq!(
            lock.plugins
                .get("demo@user")
                .unwrap()
                .resolved_commit
                .as_deref(),
            git_head_commit(&target).as_deref()
        );
    }

    // ---- enable / disable ----

    #[test]
    fn enable_disable_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        set_plugin_enabled(&agent_dir, "demo@user", false).unwrap();
        assert_eq!(
            plugin_enabled_map(&cwd, &agent_dir).get("demo@user"),
            Some(&false)
        );
        set_plugin_enabled(&agent_dir, "demo@user", true).unwrap();
        assert_eq!(
            plugin_enabled_map(&cwd, &agent_dir).get("demo@user"),
            Some(&true)
        );
        // Bad ids are rejected before touching the file.
        assert!(set_plugin_enabled(&agent_dir, "../escape", false).is_err());
    }

    // ---- marketplace signatures (ported) ----

    #[tokio::test]
    async fn signed_marketplace_pins_key_and_rejects_tampering() {
        use ed25519_dalek::Signer as _;
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let key_hex = hex_encode(signing.verifying_key().as_bytes());

        // Build a catalog with one plugin, sign the canonical payload.
        let plugin_dir = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("extension.json"), r#"{"name":"demo"}"#).unwrap();
        let catalog = serde_json::json!({
            "name": "acme",
            "plugins": {
                "demo": { "source": plugin_dir.to_string_lossy() }
            }
        });
        let canonical = serde_json::to_string(&catalog).unwrap();
        let signature = signing.sign(canonical.as_bytes());
        let mut signed_catalog = catalog.clone();
        signed_catalog["signature"] = serde_json::json!({
            "algorithm": "ed25519",
            "value": hex_encode(&signature.to_bytes()),
        });
        let catalog_path = tmp.path().join("acme.json");
        std::fs::write(
            &catalog_path,
            serde_json::to_string(&signed_catalog).unwrap(),
        )
        .unwrap();

        // Registration without the key is rejected.
        let err = add_marketplace("acme", catalog_path.to_str().unwrap(), &agent_dir, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--public-key"), "{err}");

        // With the key: registered and pinned (TOFU).
        add_marketplace(
            "acme",
            catalog_path.to_str().unwrap(),
            &agent_dir,
            Some(&key_hex),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(marketplace_key_path(&agent_dir, "acme"))
                .unwrap()
                .trim(),
            key_hex
        );

        // Resolve through the signed catalog: verifies, yields the source.
        let resolution = resolve_marketplace_spec("demo@acme", &agent_dir)
            .unwrap()
            .expect("resolves");
        assert_eq!(resolution.source, plugin_dir.to_string_lossy());
        assert_eq!(resolution.marketplace, "acme");

        // Tamper with the catalog after registration: resolution fails.
        let mut tampered = catalog.clone();
        tampered["plugins"]["demo"]["source"] = serde_json::json!("/tmp/evil");
        let tampered_canonical = serde_json::to_string(&tampered).unwrap();
        tampered["signature"] = signed_catalog["signature"].clone();
        let _ = tampered_canonical; // (the signature no longer matches)
        std::fs::write(
            marketplaces_root(&agent_dir).join("acme.json"),
            serde_json::to_string(&tampered).unwrap(),
        )
        .unwrap();
        let err = resolve_marketplace_spec("demo@acme", &agent_dir).unwrap_err();
        assert!(err.to_string().contains("signature"), "{err}");
    }

    /// Signature-stripping downgrade: once a marketplace pinned a TOFU
    /// key, an UNSIGNED replacement catalog must be REFUSED (previously
    /// it was activated with only a warning, voiding the pin on a
    /// code-delivery channel).
    #[tokio::test(flavor = "multi_thread")]
    async fn pinned_marketplace_refuses_an_unsigned_replacement() {
        use ed25519_dalek::Signer as _;
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let signing = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let key_hex = hex_encode(signing.verifying_key().as_bytes());

        let catalog = serde_json::json!({
            "name": "acme",
            "plugins": { "demo": { "source": "/srv/demo" } }
        });
        let canonical = serde_json::to_string(&catalog).unwrap();
        let signature = signing.sign(canonical.as_bytes());
        let mut signed_catalog = catalog.clone();
        signed_catalog["signature"] = serde_json::json!({
            "algorithm": "ed25519",
            "value": hex_encode(&signature.to_bytes()),
        });
        let catalog_path = tmp.path().join("acme.json");
        std::fs::write(
            &catalog_path,
            serde_json::to_string(&signed_catalog).unwrap(),
        )
        .unwrap();
        add_marketplace(
            "acme",
            catalog_path.to_str().unwrap(),
            &agent_dir,
            Some(&key_hex),
        )
        .await
        .unwrap();

        // A sync delivering the SAME plugins but no signature must fail
        // and leave the signed catalog untouched.
        let unsigned = serde_json::to_string(&serde_json::json!({
            "name": "acme",
            "plugins": { "demo": { "source": "/srv/evil" } }
        }))
        .unwrap();
        let err =
            activate_synced_catalog(&agent_dir, "acme", unsigned.as_bytes(), None).unwrap_err();
        assert!(err.to_string().contains("unsigned"), "{err}");
        let live =
            std::fs::read_to_string(marketplaces_root(&agent_dir).join("acme.json")).unwrap();
        assert!(live.contains("signature"), "signed catalog preserved");
        assert!(live.contains("/srv/demo"), "original content preserved");
        // No staged temp leaked into the marketplaces dir.
        for entry in std::fs::read_dir(marketplaces_root(&agent_dir)).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.starts_with(".staged-"), "staged temp leaked: {name}");
        }
    }

    /// Catalog v2: installation states gate resolution and drive
    /// default installs; the inline manifest enriches listing; unknown
    /// keys are skipped (forward compatibility).
    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_v2_installation_states_and_inline_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let catalog_path = tmp.path().join("corp.json");
        std::fs::write(
            &catalog_path,
            serde_json::to_string(&serde_json::json!({
                "name": "corp",
                "plugins": {
                    "review": {
                        "source": "https://git.acme.com/review.git",
                        "installation": "installed-by-default",
                        "manifest": {"name": "review", "version": "1.4.2"}
                    },
                    "old": {
                        "source": "https://git.acme.com/old.git",
                        "installation": "not-available"
                    },
                    "plain": {
                        "source": "https://git.acme.com/plain.git",
                        "futureCatalogKey": {"ignored": true}
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        add_marketplace("corp", catalog_path.to_str().unwrap(), &agent_dir, None)
            .await
            .unwrap();

        let plugins = marketplace_plugins(&agent_dir, "corp").unwrap();
        assert_eq!(plugins.len(), 3);
        let review = plugins.iter().find(|p| p.name == "review").unwrap();
        assert_eq!(
            review.installation,
            MarketplaceInstallation::InstalledByDefault
        );
        assert_eq!(review.version.as_deref(), Some("1.4.2"));
        let plain = plugins.iter().find(|p| p.name == "plain").unwrap();
        assert_eq!(plain.installation, MarketplaceInstallation::Available);
        assert!(plain.version.is_none());
        assert_eq!(plain.source, "https://git.acme.com/plain.git");

        // not-available refuses resolution with a clear error.
        let err = resolve_marketplace_spec("old@corp", &agent_dir).unwrap_err();
        assert!(err.to_string().contains("not-available"), "{err}");
        // available + installed-by-default resolve normally.
        assert!(
            resolve_marketplace_spec("plain@corp", &agent_dir)
                .unwrap()
                .is_some()
        );
        assert!(
            resolve_marketplace_spec("review@corp", &agent_dir)
                .unwrap()
                .is_some()
        );

        // Only installed-by-default entries feed the startup sync.
        let defaults = marketplace_default_installs(&agent_dir, "corp").unwrap();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].plugin, "review");
        assert_eq!(defaults[0].marketplace, "corp");
    }

    /// A `.tgz` bundle installs through the normal store channel
    /// (staging → manifest check → lockfile), and upgrade skips it.
    #[test]
    fn bundle_install_lands_in_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let plugin = tmp.path().join("demo");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("extension.json"),
            r#"{"name": "demo", "version": "1.2.3"}"#,
        )
        .unwrap();
        let bundle =
            crate::ext_bundle::pack_bundle(&plugin, Some(&tmp.path().join("demo-1.2.3.tgz")))
                .unwrap();
        let target = install_extension(bundle.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        assert!(target.join("extension.json").is_file());
        assert!(target.ends_with("store/user/demo/1.2.3"), "{target:?}");
        let lock = read_lock(&agent_dir).unwrap();
        let entry = lock.plugins.get("demo@user").expect("locked");
        assert_eq!(entry.source, bundle.display().to_string());
        assert!(
            entry.resolved_commit.is_none(),
            "a bundle has no commit pin"
        );
        // Upgrades skip non-git installs (no remote to advance to).
        let outcomes = upgrade_extensions(&agent_dir, None).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].1.contains("not a git install"), "{outcomes:?}");
    }

    /// A hostile bundle (path traversal) never escapes the staging dir.
    #[test]
    fn hostile_bundle_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        // Hand-roll a traversal archive: raw header bytes (append_data
        // refuses to write such paths, like any conforming archiver).
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        let body = b"evil";
        header.set_size(body.len() as u64);
        header.as_old_mut().name[..11].copy_from_slice(b"../evil.txt");
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, body.as_slice()).unwrap();
        let tar = builder.into_inner().unwrap();
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        let bundle = tmp.path().join("hostile.tgz");
        std::fs::write(&bundle, gz.finish().unwrap()).unwrap();
        let err = install_extension(bundle.to_str().unwrap(), &cwd, &agent_dir, false).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
        assert!(!agent_dir.join("evil.txt").exists());
    }

    /// Path of the built demo-plugin bin (fixture), derived from the
    /// test binary location (…/target/<profile>/deps/…).
    fn demo_plugin_bin() -> Option<PathBuf> {
        let mut path = std::env::current_exe().ok()?;
        path.pop(); // deps/
        path.pop(); // profile/
        path.push(format!(
            "tack-v3-demo-plugin{}",
            std::env::consts::EXE_SUFFIX
        ));
        path.is_file().then_some(path)
    }

    /// Metrics sidecar e2e (roadmap §9): the host offers a scratch file
    /// at initialize, the plugin appends a schema-valid measurement, and
    /// the drain validates + consumes it without violations.
    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_sidecar_records_and_drains() {
        let Some(bin) = demo_plugin_bin() else {
            eprintln!("demo plugin bin not built; skipping");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let ext_dir = agent_dir.join("extensions").join("demo");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            serde_json::json!({"name": "demo", "command": bin}).to_string(),
        )
        .unwrap();

        let mut manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "print",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert!(
            manager.plugins[0].is_active(),
            "{:?}",
            manager.plugins[0].error
        );
        // The validated declaration registered a sidecar with a
        // pre-created scratch file under the plugin's data root (unique
        // per manager instance: <data>/metrics/<instance>/metrics.ndjson).
        assert_eq!(manager.metrics_sidecars.lock().unwrap().len(), 1);
        let scratch = manager.metrics_sidecars.lock().unwrap()[0]
            .path()
            .to_path_buf();
        assert!(scratch.is_file(), "scratch file pre-created");
        assert!(
            scratch.starts_with(agent_dir.join("extensions/data/user/demo/metrics")),
            "scratch lives under the plugin data dir: {}",
            scratch.display()
        );

        // The tool writes through the host-provided scratchFile path — a
        // wiring failure makes it error out.
        let tool = manager
            .tools()
            .into_iter()
            .find(|t| t.name().contains("hello_metric"))
            .expect("hello.metric registered");
        let result = tool
            .execute(
                "call-1",
                serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let text = format!("{:?}", result.content);
        assert!(text.contains("recorded demo.metric"), "{text}");

        // Drain (sessions drain on a 30s interval; tests drive it
        // directly): the measurement validates and is consumed.
        {
            let mut sidecars = manager.metrics_sidecars.lock().unwrap();
            crate::plugin_metrics::drain(&mut sidecars[0]);
            assert_eq!(sidecars[0].violations(), 0);
            assert_eq!(
                sidecars[0].offset(),
                std::fs::metadata(&scratch).unwrap().len(),
                "the measurement is fully consumed"
            );
        }
        manager.shutdown().await;
    }

    // ---- widget registry ----

    #[test]
    fn widget_registry_lifecycle() {
        let mut registry = WidgetRegistry::default();
        registry.register_plugin(
            "demo@user",
            &[WidgetSpec {
                id: "status".to_string(),
                r#type: tack_ext::rpc3::WidgetKind::StatusLineSegment,
                priority: Some(5),
                title: None,
                visible: None,
                initial: Some(serde_json::json!({"text": "ok"})),
            }],
        );
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(registry.entries()[0].key, "demo@user:status");
        assert!(registry.entries()[0].visible);

        assert!(registry.apply_update(
            "demo@user",
            &WidgetUpdateParams {
                id: "status".to_string(),
                state: serde_json::json!({"text": "busy"}),
                visible: Some(false),
            },
        ));
        assert_eq!(
            registry.entries()[0].state,
            Some(serde_json::json!({"text": "busy"}))
        );
        assert!(!registry.entries()[0].visible);
        assert_eq!(registry.entries()[0].rev, 1);

        assert!(!registry.apply_update(
            "demo@user",
            &WidgetUpdateParams {
                id: "nope".to_string(),
                state: serde_json::json!({}),
                visible: None,
            },
        ));

        let removed = registry.remove_plugin("demo@user");
        assert_eq!(removed, vec!["demo@user:status"]);
        assert!(registry.entries().is_empty());
    }

    // ---- bundle resources (v3 load) ----

    /// A bundle-only manifest contributes hooks/MCP/skills without running
    /// a plugin process.
    #[tokio::test(flavor = "multi_thread")]
    async fn bundle_resources_are_collected() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let ext_dir = agent_dir.join("extensions").join("bundle");
        std::fs::create_dir_all(ext_dir.join("skills").join("demo")).unwrap();
        std::fs::write(
            ext_dir.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: bundle skill\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("hooks.json"),
            r#"{"hooks": {"PreToolUse": [{"matcher": "bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{
  "name": "bundle",
  "hooks": "hooks.json",
  "mcpServers": { "docs": { "url": "https://docs.example.com/mcp" } },
  "skills": ["skills"]
}"#,
        )
        .unwrap();
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert!(
            manager.plugins.is_empty(),
            "bundle-only manifest runs no plugin"
        );
        assert_eq!(
            manager
                .bundle_hooks
                .groups_for(crate::shell_hooks::HookEvent::PreToolUse)
                .len(),
            1
        );
        assert_eq!(manager.bundle_mcp_servers.len(), 1);
        assert_eq!(manager.bundle_skill_dirs.len(), 1);
    }

    /// Level-2 MCP plugins: `carrier: "mcp"` without an `mcpServer`
    /// entry is a load error (failure-as-data), and so is a malformed one.
    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_carrier_requires_a_valid_mcp_server_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let missing_dir = agent_dir.join("extensions").join("missing");
        let malformed_dir = agent_dir.join("extensions").join("malformed");
        std::fs::create_dir_all(&missing_dir).unwrap();
        std::fs::create_dir_all(&malformed_dir).unwrap();
        std::fs::write(
            missing_dir.join("extension.json"),
            r#"{"name": "missing", "carrier": "mcp"}"#,
        )
        .unwrap();
        std::fs::write(
            malformed_dir.join("extension.json"),
            r#"{"name": "malformed", "carrier": "mcp", "mcpServer": {"neither": 1}}"#,
        )
        .unwrap();
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(manager.plugins.len(), 2);
        let missing = manager
            .plugins
            .iter()
            .find(|p| p.id.name() == "missing")
            .unwrap();
        assert!(
            missing.error.as_deref().unwrap_or("").contains("mcpServer"),
            "{missing:?}"
        );
        let malformed = manager
            .plugins
            .iter()
            .find(|p| p.id.name() == "malformed")
            .unwrap();
        assert!(
            malformed
                .error
                .as_deref()
                .unwrap_or("")
                .contains("malformed"),
            "{malformed:?}"
        );
        assert!(manager.is_empty(), "no ACTIVE plugins");
    }

    /// Level-2 MCP plugins: a relative stdio command resolves against the
    /// extension directory and the server runs with it as cwd; HTTP specs
    /// are untouched.
    #[test]
    fn mcp_stdio_spec_resolves_against_extension_dir() {
        let dir = std::path::Path::new("/ext/dir");
        let mut spec = crate::mcp_config::spec_from_entry(
            "p@user",
            &serde_json::json!({"command": "./server.js", "args": ["--port", "8080"]}),
            "test",
        )
        .unwrap();
        resolve_mcp_stdio_spec(&mut spec, dir);
        let tack_tools::mcp::McpTransport::Stdio {
            command, cwd, args, ..
        } = &spec.transport
        else {
            panic!("expected stdio transport");
        };
        assert_eq!(command, &dir.join("./server.js").to_string_lossy().as_ref());
        assert_eq!(cwd.as_deref(), Some(dir));
        assert_eq!(args, &["--port", "8080"], "args are never rewritten");

        // Bare commands (PATH lookup) stay as-is; only cwd is set.
        let mut spec = crate::mcp_config::spec_from_entry(
            "p@user",
            &serde_json::json!({"command": "npx", "args": ["-y", "@scope/pkg"]}),
            "test",
        )
        .unwrap();
        resolve_mcp_stdio_spec(&mut spec, dir);
        let tack_tools::mcp::McpTransport::Stdio { command, args, .. } = &spec.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(command, "npx");
        assert_eq!(args[1], "@scope/pkg", "npm package names are not paths");

        let mut spec = crate::mcp_config::spec_from_entry(
            "p@user",
            &serde_json::json!({"url": "https://example.com/mcp"}),
            "test",
        )
        .unwrap();
        resolve_mcp_stdio_spec(&mut spec, dir);
        assert!(matches!(
            spec.transport,
            tack_tools::mcp::McpTransport::Http { .. }
        ));
    }

    /// A plugin that fails its handshake stays in the outcome with the
    /// error recorded (failure is first-class state, not a skip).
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_plugin_is_listed_with_error() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let ext_dir = agent_dir.join("extensions").join("broken");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{"name": "broken", "command": "definitely-not-a-real-command-xyz"}"#,
        )
        .unwrap();
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(manager.plugins.len(), 1);
        let plugin = &manager.plugins[0];
        assert!(!plugin.is_active());
        assert!(
            plugin
                .error
                .as_deref()
                .unwrap_or("")
                .contains("failed to start"),
            "{plugin:?}"
        );
        assert!(manager.is_empty(), "no ACTIVE plugins");
    }

    /// A disabled plugin is discovered but never spawned.
    #[tokio::test(flavor = "multi_thread")]
    async fn disabled_plugin_is_not_spawned() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let ext_dir = agent_dir.join("extensions").join("demo");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{"name": "demo", "command": "definitely-not-a-real-command-xyz"}"#,
        )
        .unwrap();
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        set_plugin_enabled(&agent_dir, "demo@user", false).unwrap();

        let manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(manager.plugins.len(), 1);
        let plugin = &manager.plugins[0];
        assert!(!plugin.enabled);
        assert!(plugin.error.is_none(), "disabled is not an error");
        assert!(plugin.handle.is_none(), "disabled plugins never spawn");
    }

    // ---- wasm manifest limits (ported) ----

    /// Manifest-declared WASM limits are plugin-supplied data: they may
    /// tighten the sandbox but never exceed the host's hard caps.
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_are_clamped_to_host_caps() {
        let greedy = WasmLimitsJson {
            max_fuel: Some(u64::MAX),
            max_memory_bytes: Some(usize::MAX),
            max_execution_ms: None,
        }
        .to_limits();
        assert_eq!(greedy.max_fuel, HARD_MAX_FUEL);
        assert_eq!(greedy.max_memory_bytes, HARD_MAX_MEMORY_BYTES);

        let tight = WasmLimitsJson {
            max_fuel: Some(1_000),
            max_memory_bytes: Some(4096),
            max_execution_ms: None,
        }
        .to_limits();
        assert_eq!(tight.max_fuel, 1_000);
        assert_eq!(tight.max_memory_bytes, 4096);
    }

    /// The manifest JSON uses camelCase keys; unknown keys are ignored.
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_deserialize_from_manifest_json() {
        let json: WasmLimitsJson = serde_json::from_str(
            r#"{"maxFuel": 500, "maxMemoryBytes": 2048, "maxExecutionMs": 30, "extra": true}"#,
        )
        .unwrap();
        let limits = json.to_limits();
        assert_eq!(limits.max_fuel, 500);
        assert_eq!(limits.max_memory_bytes, 2048);
        assert_eq!(
            limits.max_execution,
            Some(std::time::Duration::from_millis(30))
        );
    }

    /// WASM capability declarations lower correctly, and sensitive env
    /// pass-through names are refused.
    #[test]
    #[cfg(feature = "wasm")]
    fn capabilities_env_passthrough_refuses_sensitive_names() {
        let json: CapabilitiesJson =
            serde_json::from_str(r#"{"env": ["ANTHROPIC_API_KEY", "NPM_TOKEN", "PATH"]}"#).unwrap();
        let caps = json.into_capabilities("test", Path::new("/x"));
        assert_eq!(caps.env.len(), 1, "env: {:?}", caps.env);
        assert_eq!(caps.env[0].0, "PATH");
    }

    /// e2e: a wasm-carrier extension is discovered, instantiated,
    /// handshakes (v3), and answers tools/execute + commands/invoke.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(feature = "wasm")]
    async fn wasm_carrier_extension_loads_and_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let ext_dir = agent_dir.join("extensions").join("hello-wasm");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{
  "name": "hello-wasm",
  "carrier": "wasm",
  "module": "plugin.wat",
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16777216 }
}"#,
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("plugin.wat"),
            include_str!("../../../examples/extensions/hello-wasm/plugin.wat"),
        )
        .unwrap();
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(manager.plugins.len(), 1, "wasm plugin must load");
        assert!(matches!(
            manager.plugins[0].handle,
            Some(PluginHandle::Wasm(_))
        ));

        let tools = manager.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "ext__hello-wasm_user__ping");
        let result = tools[0]
            .execute(
                "call-1",
                serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!("expected text")
        };
        assert_eq!(text, "pong from the WASM sandbox");

        let command_result = manager.invoke_command("hello-wasm", "").await.unwrap();
        assert_eq!(command_result["ok"], serde_json::json!(true));

        manager.shutdown().await;
    }

    /// e2e: a WIT-component extension (`carrier: "wasm"` + a component
    /// module) is detected, driven via the tack:plugin exports, and
    /// answers tools/execute + hooks/beforeToolCall — no JSON-RPC peer.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(feature = "wasm")]
    async fn wasm_component_extension_loads_and_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let ext_dir = agent_dir.join("extensions").join("hello-component");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{
  "name": "hello-component",
  "carrier": "wasm",
  "module": "plugin.wat",
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16777216 }
}"#,
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("plugin.wat"),
            include_str!("../../../examples/extensions/hello-component/plugin.wat"),
        )
        .unwrap();
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(
            manager.plugins.len(),
            1,
            "component plugin must load: {:?}",
            manager.plugins
        );
        assert!(matches!(
            manager.plugins[0].handle,
            Some(PluginHandle::WasmComponent(_))
        ));

        let tools = manager.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "ext__hello-component_user__hello");
        let result = tools[0]
            .execute(
                "call-1",
                serde_json::json!({"name": "tack"}),
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!("expected text")
        };
        assert_eq!(text, "hello from the component carrier");

        // The hooks interface surfaces through the AgentHooks chain.
        let hooks = manager.hooks();
        assert_eq!(hooks.len(), 1, "component declared before-tool-call");

        manager.shutdown().await;
    }

    /// e2e: `capabilities.fs` preopens the extension's data dir into the
    /// guest; without the grant the same module gets an errno.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(feature = "wasm")]
    async fn wasm_capability_fs_grant_readfile() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let wat = include_str!("../../../examples/extensions/hello-wasm-caps/plugin.wat");

        let ext_dir = agent_dir.join("extensions").join("hello-wasm-caps");
        std::fs::create_dir_all(ext_dir.join("data")).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{
  "name": "hello-wasm-caps",
  "carrier": "wasm",
  "module": "plugin.wat",
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16777216 },
  "capabilities": { "fs": [{ "host": "data", "guest": "/data", "access": "read-only" }] }
}"#,
        )
        .unwrap();
        std::fs::write(ext_dir.join("plugin.wat"), wat).unwrap();
        std::fs::write(
            ext_dir.join("data").join("hello.txt"),
            "hello from the host filesystem",
        )
        .unwrap();

        let mut manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        let tools = manager.tools();
        let readfile = tools
            .iter()
            .find(|t| t.name() == "ext__hello-wasm-caps_user__readfile")
            .expect("readfile tool registered");
        let result = readfile
            .execute(
                "call-1",
                serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!("expected text")
        };
        assert_eq!(text, "hello from the host filesystem");
        manager.shutdown().await;

        // Without the grant: same module, sandbox denies the open.
        std::fs::remove_dir_all(&ext_dir).unwrap();
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{
  "name": "hello-wasm-caps",
  "carrier": "wasm",
  "module": "plugin.wat",
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16777216 }
}"#,
        )
        .unwrap();
        std::fs::write(ext_dir.join("plugin.wat"), wat).unwrap();

        let mut manager = ExtensionManager::load(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        let tools = manager.tools();
        let readfile = tools
            .iter()
            .find(|t| t.name() == "ext__hello-wasm-caps_user__readfile")
            .expect("readfile tool registered");
        let result = readfile
            .execute(
                "call-2",
                serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        let tack_ai::InputContentBlock::Text { text, .. } = &result.content[0] else {
            panic!("expected text")
        };
        assert!(
            text.starts_with("read failed: errno "),
            "sandbox denial should surface as errno, got: {text}"
        );
        manager.shutdown().await;
    }
}

#[cfg(test)]
mod policy_tests {
    //! Managed plugin policy (P5) wired through `load_with_policy` (the
    //! production `load` reads the policy from the managed settings
    //! file; passing one in keeps these tests free of the process-global
    //! TACK_MANAGED_SETTINGS).
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct NoopServices;

    #[async_trait::async_trait]
    impl PeerHandler for NoopServices {}

    fn test_policy(raw: &str) -> crate::plugin_policy::PluginPolicy {
        let raw: Value = serde_json::from_str(raw).unwrap();
        crate::plugin_policy::PluginPolicy::from_raw(&raw, "/test/managed.json".to_string())
            .expect("policy parses")
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        (tmp, agent_dir, cwd)
    }

    fn write_flat_plugin(agent_dir: &Path, name: &str, manifest: &str) {
        let dir = agent_dir.join("extensions").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extension.json"), manifest).unwrap();
    }

    /// managedPluginsOnly: an unlisted plugin never spawns — it stays as
    /// a policy-blocked row (rows, not absences).
    #[tokio::test(flavor = "multi_thread")]
    async fn managed_plugins_only_blocks_unlisted_plugin() {
        let (_tmp, agent_dir, cwd) = dirs();
        write_flat_plugin(
            &agent_dir,
            "sketchy",
            r#"{"name": "sketchy", "command": "definitely-not-a-real-command-xyz"}"#,
        );
        let policy = test_policy(
            r#"{"pluginPolicy": {"managedPluginsOnly": true,
                "plugins": {"review@acme": {"enabled": true}}}}"#,
        );
        let manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(manager.plugins.len(), 1);
        let plugin = &manager.plugins[0];
        assert!(!plugin.is_active());
        assert!(plugin.error.is_none(), "a policy block is not a failure");
        assert!(plugin.handle.is_none(), "blocked plugins never spawn");
        let reason = plugin.policy_block.as_deref().unwrap_or("");
        assert!(reason.contains("managedPluginsOnly"), "{reason}");
        assert!(reason.contains("/test/managed.json"), "{reason}");
    }

    /// Managed `enabled` wins over the user layer in both directions.
    #[tokio::test(flavor = "multi_thread")]
    async fn managed_enabled_wins_over_user_settings() {
        let (_tmp, agent_dir, cwd) = dirs();
        write_flat_plugin(
            &agent_dir,
            "forced",
            r#"{"name": "forced", "command": "definitely-not-a-real-command-xyz"}"#,
        );
        write_flat_plugin(
            &agent_dir,
            "pinned-off",
            r#"{"name": "pinned-off", "command": "definitely-not-a-real-command-xyz"}"#,
        );
        // User layer: forced@user disabled; pinned-off@user enabled.
        set_plugin_enabled(&agent_dir, "forced@user", false).unwrap();
        let policy = test_policy(
            r#"{"pluginPolicy": {"plugins": {
                "forced@user": {"enabled": true},
                "pinned-off@user": {"enabled": false}
            }}}"#,
        );
        let manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        let forced = manager
            .plugins
            .iter()
            .find(|p| p.id.name() == "forced")
            .unwrap();
        assert!(
            forced.enabled,
            "managed enabled=true overrides the user disable"
        );
        assert!(
            forced.error.is_some(),
            "a force-enabled plugin attempts the spawn"
        );
        let pinned = manager
            .plugins
            .iter()
            .find(|p| p.id.name() == "pinned-off")
            .unwrap();
        assert!(!pinned.enabled, "managed enabled=false wins too");
        assert!(pinned.handle.is_none(), "disabled plugins never spawn");
    }

    /// A managed `mcpServers` list narrows bundle contributions
    /// (intersect-only; unknown entries warn, never expand).
    #[tokio::test(flavor = "multi_thread")]
    async fn policy_narrows_bundle_mcp_servers() {
        let (_tmp, agent_dir, cwd) = dirs();
        write_flat_plugin(
            &agent_dir,
            "bundle",
            r#"{
  "name": "bundle",
  "mcpServers": {
    "jira": { "url": "https://jira.example.com/mcp" },
    "docs": { "url": "https://docs.example.com/mcp" }
  }
}"#,
        );
        let policy = test_policy(
            r#"{"pluginPolicy": {"plugins": {
                "bundle@user": {"mcpServers": ["jira", "nonexistent"]}
            }}}"#,
        );
        let manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(
            manager.bundle_mcp_servers.len(),
            1,
            "only the allowed server survives: {:?}",
            manager
                .bundle_mcp_servers
                .iter()
                .map(|s| &s.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(manager.bundle_mcp_servers[0].name, "jira");
        let warnings = manager.load_warnings.join("\n");
        assert!(
            warnings.contains("dropped MCP server(s): docs"),
            "{warnings}"
        );
        assert!(warnings.contains("nonexistent"), "{warnings}");
    }

    /// For an mcp-carrier plugin (the server IS the plugin) the managed
    /// `mcpServers` list accepts BOTH the full plugin id and the bare
    /// plugin name — admins can naturally write either, and requiring
    /// one form would silently block plugins listed in the other form.
    #[tokio::test(flavor = "multi_thread")]
    async fn policy_mcp_servers_matches_mcp_carrier_by_id_or_bare_name() {
        let manifest = r#"{
  "name": "jira",
  "carrier": "mcp",
  "mcpServer": { "command": "definitely-not-a-real-command-xyz" }
}"#;
        // (allow-list, expect a policy block?)
        for (allow, blocked) in [
            ("jira", false),      // bare plugin name
            ("jira@user", false), // full plugin id
            ("other", true),      // neither form ⇒ blocked
        ] {
            let (_tmp, agent_dir, cwd) = dirs();
            write_flat_plugin(&agent_dir, "jira", manifest);
            let policy = test_policy(
                &serde_json::json!({
                    "pluginPolicy": {"plugins": {"jira@user": {"mcpServers": [allow]}}}
                })
                .to_string(),
            );
            let manager = ExtensionManager::load_with_policy(
                &cwd,
                &agent_dir,
                "tui",
                Arc::new(NoopServices),
                true,
                Default::default(),
                Some(policy),
                crate::ext_provider_bridge::ProviderBridgeState::shared(),
            )
            .await;
            assert_eq!(manager.plugins.len(), 1);
            let plugin = &manager.plugins[0];
            if blocked {
                let reason = plugin.policy_block.as_deref().unwrap_or("");
                assert!(reason.contains("mcpServers"), "{reason}");
            } else {
                assert!(
                    plugin.policy_block.is_none(),
                    "allow-list entry {allow:?} must not block: {:?}",
                    plugin.policy_block
                );
                // The plugin proceeded to the carrier connect, which
                // fails on the bogus command — a handshake failure row,
                // NOT a policy block.
                assert!(plugin.error.is_some());
            }
        }
    }

    /// A managed `tools` list narrows the registered tool set at
    /// registration time (intersect-only).
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(feature = "wasm")]
    async fn policy_narrows_registered_tools() {
        let (_tmp, agent_dir, cwd) = dirs();
        let ext_dir = agent_dir.join("extensions").join("hello-wasm");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("extension.json"),
            r#"{
  "name": "hello-wasm",
  "carrier": "wasm",
  "module": "plugin.wat",
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16787216 }
}"#,
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("plugin.wat"),
            include_str!("../../../examples/extensions/hello-wasm/plugin.wat"),
        )
        .unwrap();

        // Allow only a tool the plugin does not register: everything is
        // dropped (narrow-only never expands).
        let policy = test_policy(
            r#"{"pluginPolicy": {"plugins": {
                "hello-wasm@user": {"tools": ["nope"]}
            }}}"#,
        );
        let mut manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert!(
            manager.tools().is_empty(),
            "ping is dropped by the managed tool list"
        );
        let warnings = manager.load_warnings.join("\n");
        assert!(warnings.contains("dropped tool(s): ping"), "{warnings}");
        assert!(warnings.contains("does not register"), "{warnings}");
        manager.shutdown().await;

        // Allow exactly the registered tool: it survives.
        let policy = test_policy(
            r#"{"pluginPolicy": {"plugins": {
                "hello-wasm@user": {"tools": ["ping"]}
            }}}"#,
        );
        let mut manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(manager.tools().len(), 1);
        manager.shutdown().await;
    }

    /// Managed `hooks: false` strips a plugin's hook bridges at
    /// registration (no beforeToolCall interception over the session)
    /// while its tools still load; an absent entry allows hooks, and an
    /// explicit `hooks: true` allows too (managed wins by construction
    /// — there is no user-layer hooks grant to override).
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(feature = "wasm")]
    async fn plugin_policy_hooks_gate_drops_bridges_keeps_tools() {
        async fn load(entry: &str) -> (tempfile::TempDir, ExtensionManager) {
            let (_tmp, agent_dir, cwd) = dirs();
            let ext_dir = agent_dir.join("extensions").join("hello-component");
            std::fs::create_dir_all(&ext_dir).unwrap();
            std::fs::write(
                ext_dir.join("extension.json"),
                r#"{
  "name": "hello-component",
  "carrier": "wasm",
  "module": "plugin.wat",
  "limits": { "maxFuel": 100000000, "maxMemoryBytes": 16777216 }
}"#,
            )
            .unwrap();
            std::fs::write(
                ext_dir.join("plugin.wat"),
                include_str!("../../../examples/extensions/hello-component/plugin.wat"),
            )
            .unwrap();
            let policy = test_policy(&format!(
                r#"{{"pluginPolicy": {{"plugins": {{"hello-component@user": {entry}}}}}}}"#
            ));
            (
                _tmp,
                ExtensionManager::load_with_policy(
                    &cwd,
                    &agent_dir,
                    "tui",
                    Arc::new(NoopServices),
                    true,
                    Default::default(),
                    Some(policy),
                    crate::ext_provider_bridge::ProviderBridgeState::shared(),
                )
                .await,
            )
        }

        // hooks: false — the declared beforeToolCall bridge is gone, the
        // tool survives, the drop is surfaced as a load warning.
        let (_tmp, mut manager) = load(r#"{"hooks": false}"#).await;
        assert!(
            manager.hooks().is_empty(),
            "managed hooks:false strips the hook bridges"
        );
        assert_eq!(
            manager.tools().len(),
            1,
            "tools are unaffected by the hooks gate"
        );
        let plugin = &manager.plugins[0];
        assert!(plugin.is_active(), "the plugin itself still loads");
        assert!(plugin.policy_block.is_none(), "not a load-time block");
        let warnings = manager.load_warnings.join("\n");
        assert!(
            warnings.contains("dropped hook capability(ies): beforeToolCall"),
            "{warnings}"
        );
        manager.shutdown().await;

        // Absent key and explicit true both allow the bridges.
        for entry in [r#"{"tools": ["hello"]}"#, r#"{"hooks": true}"#] {
            let (_tmp, mut manager) = load(entry).await;
            assert_eq!(
                manager.hooks().len(),
                1,
                "entry {entry} must allow the hook bridge"
            );
            assert_eq!(manager.tools().len(), 1);
            manager.shutdown().await;
        }
    }

    /// Load-time source backstop: a store install whose locked source
    /// matches no allowedSources rule is filtered at load.
    #[tokio::test(flavor = "multi_thread")]
    async fn load_time_source_backstop_filters_store_install() {
        let (_tmp, agent_dir, cwd) = dirs();
        let plain = _tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(
            plain.join("extension.json"),
            r#"{"name": "plain", "command": "definitely-not-a-real-command-xyz"}"#,
        )
        .unwrap();
        install_extension(plain.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        let policy = test_policy(
            r#"{"pluginPolicy": {"allowedSources": [
                {"type": "local", "path": "/opt/acme/approved"}
            ]}}"#,
        );
        let manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert_eq!(manager.plugins.len(), 1);
        let plugin = &manager.plugins[0];
        let reason = plugin.policy_block.as_deref().unwrap_or("");
        assert!(reason.contains("allowedSources"), "{reason}");
        assert!(plugin.handle.is_none(), "filtered plugins never spawn");

        // With the install root approved the same plugin loads. Build
        // the JSON with `json!` — interpolating `path.display()` into a
        // raw string produces invalid escapes on Windows (`C:\Users\…`).
        let raw = serde_json::json!({"pluginPolicy": {"allowedSources": [
            {"type": "local", "path": _tmp.path()}
        ]}});
        let policy =
            crate::plugin_policy::PluginPolicy::from_raw(&raw, "/test/managed.json".to_string())
                .expect("policy parses");
        let manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        assert!(
            manager.plugins[0].policy_block.is_none(),
            "approved origin loads: {:?}",
            manager.plugins[0].policy_block
        );
    }

    /// The load report persists every outcome bucket with error classes
    /// (doctor's data source; roadmap §9 load telemetry).
    #[tokio::test(flavor = "multi_thread")]
    async fn load_report_records_outcomes_and_classes() {
        let (_tmp, agent_dir, cwd) = dirs();
        // failed / manifest: unparseable extension.json.
        write_flat_plugin(&agent_dir, "broken", "{not json");
        // failed / handshake: spawn failure.
        write_flat_plugin(
            &agent_dir,
            "nospawn",
            r#"{"name": "nospawn", "command": "definitely-not-a-real-command-xyz"}"#,
        );
        // policy-filtered.
        write_flat_plugin(
            &agent_dir,
            "filtered",
            r#"{"name": "filtered", "command": "definitely-not-a-real-command-xyz"}"#,
        );
        // disabled.
        write_flat_plugin(
            &agent_dir,
            "off",
            r#"{"name": "off", "command": "definitely-not-a-real-command-xyz"}"#,
        );
        set_plugin_enabled(&agent_dir, "off@user", false).unwrap();
        // managedPluginsOnly: the unlisted `filtered@user` is
        // policy-filtered; the other three are managed.
        let policy = test_policy(
            "{\"pluginPolicy\": {\"managedPluginsOnly\": true, \"plugins\": {
                \"broken@user\": {}, \"nospawn@user\": {}, \"off@user\": {}
            }}}",
        );
        let _manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "print",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
            crate::ext_provider_bridge::ProviderBridgeState::shared(),
        )
        .await;
        let report = read_load_report(&agent_dir).expect("report written");
        assert_eq!(report.version, 1);
        assert_eq!(report.mode, "print");
        assert_eq!(report.plugins.len(), 4);
        let row = |name: &str| {
            report
                .plugins
                .iter()
                .find(|r| r.id == format!("{name}@user"))
                .unwrap_or_else(|| panic!("row for {name}: {:?}", report.plugins))
        };
        let broken = row("broken");
        assert_eq!(broken.outcome, "failed");
        assert_eq!(broken.error_class.as_deref(), Some("manifest"));
        assert!(
            broken
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("extension.json")
        );
        let nospawn = row("nospawn");
        assert_eq!(nospawn.outcome, "failed");
        assert_eq!(nospawn.error_class.as_deref(), Some("handshake"));
        let filtered = row("filtered");
        assert_eq!(filtered.outcome, "policy-filtered");
        assert_eq!(filtered.error_class.as_deref(), Some("policy"));
        assert!(filtered.detail.is_some(), "policy reason recorded");
        let off = row("off");
        assert_eq!(off.outcome, "disabled");
        assert!(off.error_class.is_none());
    }

    // ---- approval chain (approval/review) ----

    #[test]
    fn approval_chain_capability_gating() {
        use tack_ext::rpc3::{HookCapabilities, PluginCapabilities};
        let caps = |approval_review: Option<bool>| PluginCapabilities {
            hooks: Some(HookCapabilities {
                approval_review,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(declares_approval_review(Some(&caps(Some(true)))));
        assert!(!declares_approval_review(Some(&caps(Some(false)))));
        // Absent flag = not implemented (same rule as the other hooks).
        assert!(!declares_approval_review(Some(&caps(None))));
        assert!(!declares_approval_review(Some(
            &PluginCapabilities::default()
        )));
        assert!(!declares_approval_review(None));
    }

    /// A handle-less plugin row (e.g. register kept for a carrier that
    /// died after the handshake) must not enter the chain.
    #[test]
    fn approval_chain_skips_plugins_without_handle() {
        let id = PluginId::new("reviewer", "user").unwrap();
        let register = InitializeResult {
            capabilities: tack_ext::rpc3::PluginCapabilities {
                hooks: Some(tack_ext::rpc3::HookCapabilities {
                    approval_review: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            plugin: tack_ext::rpc3::PluginInfo {
                description: None,
                name: "reviewer".into(),
                version: None,
            },
            protocol_version: "3".into(),
        };
        let manager = ExtensionManager {
            plugins: vec![LoadedPlugin {
                id,
                enabled: true,
                error: None,
                error_class: None,
                policy_block: None,
                register: Some(register),
                handle: None,
                version: "1.0.0".into(),
                dir: PathBuf::new(),
                stopped: false,
                fail_mode: FailMode::default(),
            }],
            load_warnings: Vec::new(),
            commands: HashMap::new(),
            widgets: WidgetRegistry::default(),
            bundle_hooks: crate::shell_hooks::HookConfig::default(),
            bundle_mcp_servers: Vec::new(),
            bundle_skill_dirs: Vec::new(),
            #[cfg(feature = "wasm")]
            wasm_carrier: None,
            metrics_sidecars: Default::default(),
            metrics_stop: Default::default(),
            metrics_instance: "test-approval".to_string(),
        };
        assert!(manager.approval_chain().is_empty());
    }

    /// Plugin-side answer script for the reviewer wire test.
    struct ScriptedApproval {
        reply: Result<Value, tack_ext::rpc3::ErrorObject>,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    #[async_trait::async_trait]
    impl PeerHandler for ScriptedApproval {
        async fn handle_request(
            &self,
            method: &str,
            params: Value,
        ) -> Result<Value, tack_ext::rpc3::ErrorObject> {
            assert_eq!(method, "approval/review");
            self.seen.lock().unwrap().push(params);
            self.reply.clone()
        }
    }

    /// (reviewer, plugin peer guard, captured params) over an in-memory
    /// duplex — same seam as the tack-ext hooks tests.
    fn reviewer_pair(
        reply: Result<Value, tack_ext::rpc3::ErrorObject>,
    ) -> (
        PluginApprovalReviewer,
        Arc<tack_ext::v3::JsonRpcPeer>,
        Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        struct Noop;
        #[async_trait::async_trait]
        impl PeerHandler for Noop {}
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (s1, s2) = tokio::io::duplex(8192);
        let (r1, w1) = tokio::io::split(s1);
        let (r2, w2) = tokio::io::split(s2);
        let client =
            tack_ext::v3::HostClient::new(tack_ext::v3::JsonRpcPeer::new(r1, w1, Arc::new(Noop)));
        let plugin = tack_ext::v3::JsonRpcPeer::new(
            r2,
            w2,
            Arc::new(ScriptedApproval {
                reply,
                seen: seen.clone(),
            }),
        );
        (
            PluginApprovalReviewer {
                client: Arc::new(client),
            },
            plugin,
            seen,
        )
    }

    fn approval_request() -> crate::approval::ApprovalRequest {
        crate::approval::ApprovalRequest {
            approval_id: "call-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: serde_json::json!({"command": "rm -rf build"}),
            approval_policy: "ask".into(),
            evidence: serde_json::json!({"surface": "tui", "untrustedSeen": false}),
        }
    }

    #[tokio::test]
    async fn approval_reviewer_maps_decisions() {
        use crate::approval::{ApprovalReviewer, ChainAction};
        for (wire, expected) in [
            ("allow", ChainAction::Allow),
            ("reviewed", ChainAction::Reviewed),
            ("askUser", ChainAction::AskUser),
        ] {
            let (reviewer, _plugin, seen) = reviewer_pair(Ok(serde_json::json!({
                "action": wire,
                "reason": "checked"
            })));
            let decision = reviewer.review(&approval_request()).await.expect("claim");
            assert_eq!(decision.action, expected);
            assert_eq!(decision.reason.as_deref(), Some("checked"));
            let params = seen.lock().unwrap().remove(0);
            assert_eq!(params["approvalId"], "call-1");
            assert_eq!(params["approvalPolicy"], "ask");
            assert_eq!(params["toolCall"]["toolName"], "bash");
            assert_eq!(params["toolCall"]["toolCallId"], "call-1");
            assert!(params["evidence"].is_object());
        }
    }

    #[tokio::test]
    async fn approval_reviewer_null_and_error_pass() {
        use crate::approval::ApprovalReviewer as _;
        // Null result = pass to the next reviewer.
        let (reviewer, _plugin, _seen) = reviewer_pair(Ok(Value::Null));
        assert_eq!(reviewer.review(&approval_request()).await, None);
        // Transport/method error (e.g. unsupported_capability from a
        // carrier) = pass, never a block.
        let (reviewer, _plugin, _seen) = reviewer_pair(Err(tack_ext::rpc3::ErrorObject {
            code: -32002,
            message: "capability not granted".into(),
            data: None,
        }));
        assert_eq!(reviewer.review(&approval_request()).await, None);
    }
}
