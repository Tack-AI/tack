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
    ErrorObject, HostCapabilities, HostInfo, InitializeParams, InitializeResult, RunMode,
    WidgetSpec, WidgetUpdateParams,
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
/// thread). Implements the v3 [`PeerHandler`] surface.
pub struct TuiExtServices {
    tx: crate::tui::AppEventTx,
    /// Project trust: `exec/run` is only honored for trusted contexts.
    trusted: bool,
}

impl std::fmt::Debug for TuiExtServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiExtServices")
            .field("trusted", &self.trusted)
            .finish()
    }
}

impl TuiExtServices {
    pub(crate) fn new(tx: crate::tui::AppEventTx, trusted: bool) -> Self {
        TuiExtServices { tx, trusted }
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
            // Interactive dialogs and the provider bridge cross to the TUI.
            "ui/notify"
            | "ui/select"
            | "ui/confirm"
            | "ui/input"
            | "session/get"
            | "session/sendUserMessage"
            | "host/registerProvider" => {
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

/// Per-plugin services wrapper: tags `widgets/update` notifications with
/// the originating plugin's id before delegating (widget ids are only
/// unique per plugin; the host key is `<plugin-id>:<widget-id>`).
struct TaggedServices {
    plugin: String,
    inner: Arc<dyn PeerHandler>,
}

#[async_trait::async_trait]
impl PeerHandler for TaggedServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
        self.inner.handle_request(method, params).await
    }
    async fn handle_notification(&self, method: &str, mut payload: Value) {
        if method == "widgets/update"
            && let Value::Object(map) = &mut payload
        {
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
    Process(V3Process),
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
}

impl LoadedPlugin {
    pub fn is_active(&self) -> bool {
        self.enabled && self.error.is_none() && self.policy_block.is_none()
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

#[derive(Default)]
pub struct ExtensionManager {
    /// All discovered plugins, including disabled and failed ones.
    pub plugins: Vec<LoadedPlugin>,
    /// Capability-level load warnings (a bad hooks file, one malformed
    /// MCP entry, an invalid tool schema…).
    pub load_warnings: Vec<String>,
    /// command name → plugin index.
    commands: HashMap<String, usize>,
    /// Declarative widget registry, keyed `<plugin-id>:<widget-id>`.
    widgets: WidgetRegistry,
    /// Bundle contributions from installed extensions (merged by callers).
    pub bundle_hooks: crate::shell_hooks::HookConfig,
    pub bundle_mcp_servers: Vec<tack_tools::mcp::McpServerSpec>,
    pub bundle_skill_dirs: Vec<PathBuf>,
    /// Shared wasmtime engine for WASM-carrier plugins.
    #[cfg(feature = "wasm")]
    wasm_carrier: Option<tack_ext_wasm::WasmCarrier>,
}

impl std::fmt::Debug for ExtensionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionManager")
            .field("plugins", &self.plugins.len())
            .finish()
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
    ) -> Self {
        let policy = crate::plugin_policy::PluginPolicy::load();
        Self::load_with_policy(
            cwd,
            agent_dir,
            mode,
            services,
            lock_required,
            mcp_callbacks,
            policy,
        )
        .await
    }

    /// `load` with an explicit managed plugin policy (the production
    /// entry point reads it from the managed settings file; tests pass
    /// one in directly — `TACK_MANAGED_SETTINGS` is process-global and
    /// parallel tests would race it).
    pub async fn load_with_policy(
        cwd: &Path,
        agent_dir: &Path,
        mode: &str,
        services: Arc<dyn PeerHandler>,
        lock_required: bool,
        mcp_callbacks: tack_tools::mcp::McpClientCallbacks,
        policy: Option<crate::plugin_policy::PluginPolicy>,
    ) -> Self {
        let mut manager = ExtensionManager::default();
        let trusted = crate::project_trust::is_trusted(cwd, agent_dir);
        let enabled_map = plugin_enabled_map(cwd, agent_dir);
        let discovered = discover(cwd, agent_dir);
        let lock = read_lock(agent_dir).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable extensions lockfile: {e}");
            ExtensionsLock::default()
        });
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
                    manager.plugins.push(LoadedPlugin {
                        id: entry.id,
                        enabled,
                        error: None,
                        policy_block: Some(reason),
                        register: None,
                        handle: None,
                        version: entry.version,
                        dir: entry.dir,
                    });
                    continue;
                }
            }
            let manifest_path = entry.dir.join("extension.json");
            let manifest: Option<ExtensionManifest> = match std::fs::read_to_string(&manifest_path)
            {
                Ok(content) => match serde_json::from_str::<ExtensionManifest>(&content) {
                    Ok(manifest) => Some(manifest),
                    Err(e) => {
                        manager.plugins.push(LoadedPlugin {
                            id: entry.id,
                            enabled,
                            error: Some(format!("bad extension.json: {e}")),
                            policy_block: None,
                            register: None,
                            handle: None,
                            version: entry.version,
                            dir: entry.dir,
                        });
                        continue;
                    }
                },
                Err(e) => {
                    manager.plugins.push(LoadedPlugin {
                        id: entry.id,
                        enabled,
                        error: Some(format!("cannot read extension.json: {e}")),
                        policy_block: None,
                        register: None,
                        handle: None,
                        version: entry.version,
                        dir: entry.dir,
                    });
                    continue;
                }
            };
            let manifest = manifest.expect("manifest checked");

            if !enabled {
                manager.plugins.push(LoadedPlugin {
                    id: entry.id,
                    enabled: false,
                    error: None,
                    policy_block: None,
                    register: None,
                    handle: None,
                    version: entry.version,
                    dir: entry.dir,
                });
                continue;
            }

            // Supply-chain gate: store installs with a lock entry pinned
            // to a resolved commit are skipped when the checkout drifted.
            if (entry.id.source() == "user" || !entry.id.is_reserved_source())
                && !lock_allows(&entry, &lock, lock_required)
            {
                manager.plugins.push(LoadedPlugin {
                    id: entry.id,
                    enabled,
                    error: Some(
                        "checkout drifted from the locked commit (extensionLockRequired)"
                            .to_string(),
                    ),
                    policy_block: None,
                    register: None,
                    handle: None,
                    version: entry.version,
                    dir: entry.dir,
                });
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
            let mut handle = match carrier {
                "mcp" => {
                    // Level-2 MCP server plugin: the declared server IS
                    // the plugin (no tack-RPC process is spawned).
                    let Some(server) = &manifest.mcp_server else {
                        manager.plugins.push(LoadedPlugin {
                            id: entry.id,
                            enabled,
                            error: Some("carrier mcp requires an `mcpServer` entry".to_string()),
                            policy_block: None,
                            register: None,
                            handle: None,
                            version: entry.version,
                            dir: entry.dir,
                        });
                        continue;
                    };
                    let Some(mut spec) = crate::mcp_config::spec_from_entry(
                        &id_string,
                        server,
                        &format!("extension {id_string}"),
                    ) else {
                        manager.plugins.push(LoadedPlugin {
                            id: entry.id,
                            enabled,
                            error: Some("carrier mcp: malformed `mcpServer` entry".to_string()),
                            policy_block: None,
                            register: None,
                            handle: None,
                            version: entry.version,
                            dir: entry.dir,
                        });
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
                            manager.plugins.push(LoadedPlugin {
                                id: entry.id,
                                enabled,
                                error: Some(format!("failed to start (mcp): {e}")),
                                policy_block: None,
                                register: None,
                                handle: None,
                                version: entry.version,
                                dir: entry.dir,
                            });
                            continue;
                        }
                    }
                }
                "wasm" => {
                    #[cfg(feature = "wasm")]
                    {
                        let Some(module) = &manifest.module else {
                            manager.plugins.push(LoadedPlugin {
                                id: entry.id,
                                enabled,
                                error: Some("carrier wasm requires `module`".to_string()),
                                policy_block: None,
                                register: None,
                                handle: None,
                                version: entry.version,
                                dir: entry.dir,
                            });
                            continue;
                        };
                        let module_path = entry.dir.join(module);
                        let wasm = match std::fs::read(&module_path) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                manager.plugins.push(LoadedPlugin {
                                    id: entry.id,
                                    enabled,
                                    error: Some(format!(
                                        "cannot read {}: {e}",
                                        module_path.display()
                                    )),
                                    policy_block: None,
                                    register: None,
                                    handle: None,
                                    version: entry.version,
                                    dir: entry.dir,
                                });
                                continue;
                            }
                        };
                        if manager.wasm_carrier.is_none() {
                            match tack_ext_wasm::WasmCarrier::new() {
                                Ok(carrier) => manager.wasm_carrier = Some(carrier),
                                Err(e) => {
                                    manager.plugins.push(LoadedPlugin {
                                        id: entry.id,
                                        enabled,
                                        error: Some(format!("wasmtime unavailable: {e}")),
                                        policy_block: None,
                                        register: None,
                                        handle: None,
                                        version: entry.version,
                                        dir: entry.dir,
                                    });
                                    continue;
                                }
                            }
                        }
                        let carrier_engine = manager.wasm_carrier.as_ref().expect("carrier");
                        let limits = manifest.limits.map(|l| l.to_limits()).unwrap_or_default();
                        let capabilities = manifest
                            .capabilities
                            .map(|c| c.into_capabilities(&id_string, &entry.dir))
                            .unwrap_or_default();
                        audit_capability_grants(&id_string, &capabilities);
                        // WIT component vs WASI-stdio core module: the
                        // module format selects the carrier (both are
                        // `carrier: "wasm"` in the manifest).
                        if tack_ext_wasm::component::is_component(&wasm) {
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
                                    manager.plugins.push(LoadedPlugin {
                                        id: entry.id,
                                        enabled,
                                        error: Some(format!(
                                            "failed to start (wasm component): {e}"
                                        )),
                                        policy_block: None,
                                        register: None,
                                        handle: None,
                                        version: entry.version,
                                        dir: entry.dir,
                                    });
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
                                    manager.plugins.push(LoadedPlugin {
                                        id: entry.id,
                                        enabled,
                                        error: Some(format!("failed to start (wasm): {e}")),
                                        policy_block: None,
                                        register: None,
                                        handle: None,
                                        version: entry.version,
                                        dir: entry.dir,
                                    });
                                    continue;
                                }
                            }
                        }
                    }
                    #[cfg(not(feature = "wasm"))]
                    {
                        manager.plugins.push(LoadedPlugin {
                            id: entry.id,
                            enabled,
                            error: Some(
                                "carrier wasm requested, but this build has no wasm support"
                                    .to_string(),
                            ),
                            policy_block: None,
                            register: None,
                            handle: None,
                            version: entry.version,
                            dir: entry.dir,
                        });
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
                        Ok(process) => PluginHandle::Process(process),
                        Err(e) => {
                            manager.plugins.push(LoadedPlugin {
                                id: entry.id,
                                enabled,
                                error: Some(format!("failed to start: {e}")),
                                policy_block: None,
                                register: None,
                                handle: None,
                                version: entry.version,
                                dir: entry.dir,
                            });
                            continue;
                        }
                    }
                }
                other => {
                    manager.plugins.push(LoadedPlugin {
                        id: entry.id,
                        enabled,
                        error: Some(format!("unknown carrier {other:?}")),
                        policy_block: None,
                        register: None,
                        handle: None,
                        version: entry.version,
                        dir: entry.dir,
                    });
                    continue;
                }
            };

            // Handshake (v3 initialize).
            let run_mode = match mode {
                "tui" => RunMode::Tui,
                "print" => RunMode::Print,
                "rpc" => RunMode::Rpc,
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
                    metrics: None,
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
                    tracing::info!(
                        "extension {} loaded (carrier {}, version {})",
                        id_string,
                        carrier,
                        entry.version,
                    );
                    if let Some(commands) = &register.capabilities.commands {
                        for command in commands {
                            manager
                                .commands
                                .insert(command.name.clone(), manager.plugins.len());
                        }
                    }
                    if let Some(widgets) = &register.capabilities.widgets {
                        manager.widgets.register_plugin(&id_string, widgets);
                    }
                    manager.plugins.push(LoadedPlugin {
                        id: entry.id,
                        enabled,
                        error: None,
                        policy_block: None,
                        register: Some(register),
                        handle: Some(handle),
                        version: entry.version,
                        dir: entry.dir,
                    });
                }
                Err(e) => {
                    handle.shutdown().await;
                    manager.plugins.push(LoadedPlugin {
                        id: entry.id,
                        enabled,
                        error: Some(format!("handshake failed: {e}")),
                        policy_block: None,
                        register: None,
                        handle: None,
                        version: entry.version,
                        dir: entry.dir,
                    });
                }
            }
        }
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
                let fail_mode = std::fs::read_to_string(p.dir.join("extension.json"))
                    .ok()
                    .and_then(|c| serde_json::from_str::<ExtensionManifest>(&c).ok())
                    .and_then(|m| m.fail_mode);
                let fail_mode = FailMode::from_setting(fail_mode.as_deref());
                Some(Arc::new(ExtHooks::with_fail_mode(
                    handle.client(),
                    capabilities,
                    fail_mode,
                )) as Arc<dyn AgentHooks>)
            })
            .collect()
    }

    /// No active plugins — callers can skip per-event payload
    /// serialization.
    pub fn is_empty(&self) -> bool {
        !self.plugins.iter().any(|p| p.is_active())
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

    /// Invoke an extension command (run by the TUI command dispatcher).
    pub async fn invoke_command(&self, name: &str, args: &str) -> Result<Value, String> {
        let Some(index) = self.commands.get(name) else {
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

    /// Declarative widgets: spec + current state, keyed `<plugin-id>:<id>`.
    pub fn widgets(&self) -> &[WidgetEntry] {
        self.widgets.entries()
    }

    /// Apply a plugin's widgets/update (full-state replacement). False =
    /// unknown widget id.
    pub fn apply_widget_update(&mut self, plugin: &str, update: &WidgetUpdateParams) -> bool {
        self.widgets.apply_update(plugin, update)
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
        for plugin in &mut self.plugins {
            if let Some(handle) = &mut plugin.handle {
                handle.shutdown().await;
            }
        }
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
fn read_lock(agent_dir: &Path) -> anyhow::Result<ExtensionsLock> {
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

fn write_lock(agent_dir: &Path, lock: &ExtensionsLock) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    let path = lock_path(agent_dir);
    let content = serde_json::to_string_pretty(lock)?;
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))
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
    let mut lock = read_lock(agent_dir)?;
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
    let mut lock = read_lock(agent_dir)?;
    if lock.plugins.remove(id).is_some() {
        write_lock(agent_dir, &lock)?;
    }
    Ok(())
}

/// `git rev-parse HEAD` of a checkout, or None when not a git repo / no HEAD.
fn git_head_commit(dir: &Path) -> Option<String> {
    let output = crate::sync_process::output_with_timeout(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir),
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
    let output = crate::sync_process::output_with_timeout(
        std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir),
        std::time::Duration::from_secs(10),
    )
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
fn lock_allows(discovered: &Discovered, lock: &ExtensionsLock, required: bool) -> bool {
    let id_string = discovered.id.to_string();
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

/// Split a `<git-url>[#<ref>]` install source into (url, ref).
pub fn split_source_ref(source: &str) -> (String, Option<String>) {
    match source.rsplit_once('#') {
        Some((url, git_ref)) if !url.is_empty() && !git_ref.is_empty() => {
            (url.to_string(), Some(git_ref.to_string()))
        }
        _ => (source.to_string(), None),
    }
}

/// Clone a git source into `target` (bounded, piped stdio, scrubbed git
/// environment: no terminal prompt, no inherited GIT_* config).
fn git_clone(source: &str, rev: Option<&str>, target: &Path) -> anyhow::Result<()> {
    let mut clone = std::process::Command::new("git");
    clone
        .arg("clone")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_COUNT");
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
        let output = crate::sync_process::output_with_timeout(
            std::process::Command::new("git")
                .args(["checkout", rev])
                .env("GIT_TERMINAL_PROMPT", "0")
                .current_dir(target),
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
        let staging = root.join(format!(".staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        if is_git {
            git_clone(source, rev, &staging)?;
        } else {
            if rev.is_some() {
                anyhow::bail!(
                    "`#<ref>` pinning only applies to git URLs; {source} is a local directory"
                );
            }
            let source_dir = PathBuf::from(source);
            if !source_dir.is_dir() {
                anyhow::bail!("{source} is neither a git URL nor an existing directory");
            }
            copy_dir_recursive(&source_dir, &staging)?;
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
    let staging = staging_parent.join(format!(".staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    if is_git {
        git_clone(source, rev, &staging)?;
    } else {
        if rev.is_some() {
            anyhow::bail!(
                "`#<ref>` pinning only applies to git URLs; {source} is a local directory"
            );
        }
        let source_dir = PathBuf::from(source);
        if !source_dir.is_dir() {
            anyhow::bail!("{source} is neither a git URL nor an existing directory");
        }
        copy_dir_recursive(&source_dir, &staging)?;
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
        let backup = target.with_extension(format!("backup-{}", std::process::id()));
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
        let target = cwd.join(".pi").join("extensions").join(name);
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
    let mut out = Vec::new();
    for (id_string, entry) in lock.plugins.clone() {
        if !entry.store {
            continue;
        }
        let Ok(id) = id_string.parse::<PluginId>() else {
            continue;
        };
        if let Some(name) = name {
            let want = resolve_installed_id(name, agent_dir)?;
            if id != want {
                continue;
            }
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

#[derive(Clone, Debug, serde::Deserialize)]
struct MarketplacePlugin {
    source: String,
    #[serde(default)]
    description: Option<String>,
    /// Optional git ref (tag/branch/sha) the plugin is pinned to.
    #[serde(default)]
    rev: Option<String>,
}

fn marketplaces_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("marketplaces")
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
        let response = reqwest::get(source)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch {source}: {e}"))?;
        if !response.status().is_success() {
            anyhow::bail!("fetching {source} failed: {}", response.status());
        }
        let body = response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("failed to read {source}: {e}"))?;
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
                    tracing::warn!(
                        "marketplace {name}: previously signed catalog replaced by an UNSIGNED \
                         one — the pinned key no longer protects installs"
                    );
                } else {
                    tracing::warn!(
                        "marketplace {name}: catalog is unsigned; installs are not integrity-protected"
                    );
                }
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
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && let Some(name) = path.file_stem().map(|n| n.to_string_lossy().to_string())
            {
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

/// List a marketplace's plugins as (name, source, description).
pub fn marketplace_plugins(
    agent_dir: &Path,
    marketplace: &str,
) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
    tack_ext::plugin_id::validate_marketplace_name(marketplace)?;
    let path = marketplaces_root(agent_dir).join(format!("{marketplace}.json"));
    if !path.is_file() {
        anyhow::bail!("no marketplace named {marketplace}");
    }
    let parsed = parse_marketplace(&path)?;
    let mut out: Vec<(String, String, Option<String>)> = parsed
        .plugins
        .into_iter()
        .map(|(name, plugin)| (name, plugin.source, plugin.description))
        .collect();
    out.sort();
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
        assert_eq!(command, "/ext/dir/./server.js");
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
        )
        .await;
        assert_eq!(manager.tools().len(), 1);
        manager.shutdown().await;
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
        )
        .await;
        assert_eq!(manager.plugins.len(), 1);
        let plugin = &manager.plugins[0];
        let reason = plugin.policy_block.as_deref().unwrap_or("");
        assert!(reason.contains("allowedSources"), "{reason}");
        assert!(plugin.handle.is_none(), "filtered plugins never spawn");

        // With the install root approved the same plugin loads.
        let policy = test_policy(&format!(
            r#"{{"pluginPolicy": {{"allowedSources": [
                {{"type": "local", "path": "{}"}}
            ]}}}}"#,
            _tmp.path().display()
        ));
        let manager = ExtensionManager::load_with_policy(
            &cwd,
            &agent_dir,
            "tui",
            Arc::new(NoopServices),
            true,
            Default::default(),
            Some(policy),
        )
        .await;
        assert!(
            manager.plugins[0].policy_block.is_none(),
            "approved origin loads: {:?}",
            manager.plugins[0].policy_block
        );
    }
}
