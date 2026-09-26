//! Extension host: discovery, lifecycle, and event fan-out for tack-ext
//! plugins. Layout mirrors skills: each extension is a directory with an
//! `extension.json` manifest:
//!
//! ```json
//! { "name": "hello", "command": "node", "args": ["plugin.js"] }
//! ```
//!
//! Discovery order: `~/.tack/agent/extensions/*`, settings `extensionPaths`,
//! `<project>/.pi/extensions/*` (trust-gated, TS parity). Relative manifest
//! args resolve against the extension directory.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tack_agent_core::{AgentHooks, AgentTool};
use tack_ext::{
    DEFAULT_EVENTS, ExtHooks, ExtTool, HostServices, InitializePayload, PluginProcess,
    RegisterPayload,
};
// Call sites wrap providers via `crate::extension_host::ExtNotifyProvider`
// so the no-ext stub can substitute a pass-through.
use serde_json::Value;
pub use tack_ext::ExtNotifyProvider;
use tokio::sync::{mpsc, oneshot};

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

/// HostServices bridge: plugin requests cross to the TUI main loop via the
/// app event channel (plugin calls never run on the loop thread).
pub struct TuiExtServices {
    tx: crate::tui::AppEventTx,
    /// Project trust: `exec` is only honored for trusted contexts.
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

#[async_trait::async_trait]
impl HostServices for TuiExtServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "ui.notify" | "ui.select" | "ui.confirm" | "ui.input" | "ui.set_status"
            | "provider.register" => {
                let (tx, rx) = oneshot::channel();
                self.tx
                    .send(crate::tui::AppEvent::ExtUiRequest(ExtUiRequest {
                        plugin: String::new(),
                        method: method.to_string(),
                        params,
                        respond: tx,
                    }))
                    .map_err(|_| "host is shutting down".to_string())?;
                rx.await
                    .map_err(|_| "host closed the request".to_string())?
            }
            "exec" => {
                if !self.trusted {
                    return Err("exec requires project trust".to_string());
                }
                let (tx, rx) = oneshot::channel();
                self.tx
                    .send(crate::tui::AppEvent::ExtUiRequest(ExtUiRequest {
                        plugin: String::new(),
                        method: method.to_string(),
                        params,
                        respond: tx,
                    }))
                    .map_err(|_| "host is shutting down".to_string())?;
                rx.await
                    .map_err(|_| "host closed the request".to_string())?
            }
            other => Err(format!("unknown host method {other}")),
        }
    }

    async fn handle_event(&self, event: &str, payload: Value) {
        if event == "log" {
            let level = payload
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("info");
            let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
            match level {
                "error" => tracing::error!(target: "tack_ext::plugin", "{message}"),
                "warn" => tracing::warn!(target: "tack_ext::plugin", "{message}"),
                "debug" => tracing::debug!(target: "tack_ext::plugin", "{message}"),
                _ => tracing::info!(target: "tack_ext::plugin", "{message}"),
            }
            return;
        }
        // v2.1: plugin → host widget state push. Fire-and-forget into the
        // TUI main loop (full-state replacement; dropped frames are
        // harmless by contract). The `plugin` field was injected by
        // TaggedServices (widget ids are only unique per plugin).
        if event == "widget.update" {
            let plugin = payload
                .get("plugin")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match serde_json::from_value::<tack_ext::WidgetUpdatePayload>(payload) {
                Ok(update) => {
                    let _ = self
                        .tx
                        .send(crate::tui::AppEvent::ExtWidgetUpdate { plugin, update });
                }
                Err(e) => tracing::warn!("bad widget.update payload: {e}"),
            }
        }
        // Unknown events are ignored by design (protocol tolerance rule).
    }
}

/// Per-plugin services wrapper: tags `widget.update` events with the
/// originating plugin's name before delegating. `HostServices::handle_event`
/// carries no plugin attribution (one services object fans out to every
/// plugin), but widget ids are only unique per plugin — the host key is
/// `<plugin>:<id>`.
struct TaggedServices {
    plugin: String,
    inner: Arc<dyn HostServices>,
}

#[async_trait::async_trait]
impl HostServices for TaggedServices {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.inner.handle_request(method, params).await
    }
    async fn handle_event(&self, event: &str, mut payload: Value) {
        if event == "widget.update"
            && let Value::Object(map) = &mut payload
        {
            map.insert("plugin".to_string(), Value::String(self.plugin.clone()));
        }
        self.inner.handle_event(event, payload).await;
    }
}

/// One manifest entry. A manifest may also contribute declarative bundle
/// resources (hooks/MCP servers/skills) without any running plugin.
#[derive(Clone, Debug, serde::Deserialize)]
struct ExtensionManifest {
    name: String,
    /// Process carrier (default): executable to spawn.
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// "process" (default) | "wasm": run the plugin as a WASI module in
    /// the wasmtime sandbox (same NDJSON protocol over WASI stdio).
    /// Requires the `wasm` cargo feature (default on).
    #[serde(default)]
    carrier: Option<String>,
    /// WASM carrier: module file (.wasm/.wat), resolved against the
    /// extension directory.
    #[cfg(feature = "wasm")]
    #[serde(default)]
    module: Option<String>,
    /// WASM carrier resource limits (defaults: 1e9 fuel, 256MB memory).
    #[cfg(feature = "wasm")]
    #[serde(default)]
    limits: Option<WasmLimitsJson>,
    /// WASM carrier capability grants (default: full sandbox — nothing).
    /// Grants are explicit, self-declared, and audit-logged at load time.
    /// Trust model: user-dir extensions are trusted like any installed
    /// program; project extensions only load under project trust, so their
    /// grants are gated too.
    #[cfg(feature = "wasm")]
    #[serde(default)]
    capabilities: Option<CapabilitiesJson>,
    /// Bundle: hooks.json path(s) (Claude-format hook declarations).
    #[serde(default)]
    hooks: Option<Value>,
    /// Bundle: MCP servers — a file path or an inline server map.
    #[serde(default, rename = "mcpServers")]
    mcp_servers: Option<Value>,
    /// Bundle: skill directories (each containing SKILL.md files).
    #[serde(default)]
    skills: Option<Vec<String>>,
}

/// True for environment variable names that typically carry credentials
/// (`OPENAI_API_KEY`, `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, ...).
/// Mirrors `is_sensitive_env_key` in tack-ext's process carrier: that
/// carrier strips these names from plugin child processes, and WASM
/// guests must not receive the live host values either — a manifest
/// `env: ["ANTHROPIC_API_KEY"]` pass-through is refused, not injected.
// Only consumed by the wasm-gated capability lowering (and tests).
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
    /// Network flags. Inert for WASI p1 guests today (p1 has no socket
    /// ABI wired in wasmtime-wasi); honored forward-compatibly.
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
                    // Fail closed on host secrets: the process carrier
                    // strips these names from plugin children; a WASM
                    // guest must not receive the live values either.
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

// Deserialized only in wasm builds (the manifest fields are gated too);
// manifests may still carry these keys in slim builds — serde ignores them.
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

/// A running plugin's carrier handle (process or WASM module).
pub enum PluginHandle {
    /// Boxed: `PluginProcess` (~280 B) dwarfs the Wasm variant, so keep it
    /// off the enum's inline layout (clippy::large_enum_variant).
    Process(Box<PluginProcess>),
    /// Option: shutdown consumes the plugin (take()).
    #[cfg(feature = "wasm")]
    Wasm(Option<tack_ext_wasm::WasmPlugin>),
}

impl std::fmt::Debug for PluginHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginHandle::Process(_) => f.debug_struct("PluginHandle::Process").finish(),
            #[cfg(feature = "wasm")]
            PluginHandle::Wasm(_) => f.debug_struct("PluginHandle::Wasm").finish(),
        }
    }
}

impl PluginHandle {
    pub fn peer(&self) -> &Arc<tack_ext::PluginPeer> {
        match self {
            PluginHandle::Process(process) => &process.peer,
            #[cfg(feature = "wasm")]
            PluginHandle::Wasm(plugin) => &plugin.as_ref().expect("wasm plugin taken").peer,
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
        }
    }
}

/// A running plugin.
pub struct LoadedPlugin {
    pub name: String,
    pub handle: PluginHandle,
    pub register: RegisterPayload,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("name", &self.name)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Declarative widgets (v2.1) and autocomplete providers (v2.2)
// ---------------------------------------------------------------------------

/// One registered widget: spec + latest full-state snapshot. The host key
/// is `<plugin>:<id>`; updates are idempotent state replacements.
#[derive(Clone, Debug)]
pub struct WidgetEntry {
    pub key: String,
    pub plugin: String,
    pub spec: tack_ext::WidgetSpec,
    /// Latest state (spec.initial, replaced wholesale by widget.update).
    pub state: Option<Value>,
    /// Panel visibility (spec.visible, then widget.update.visible).
    pub visible: bool,
    /// Bumped on each applied update (TUI render-cache key).
    pub rev: u64,
}

/// Host-side widget registry (v2.1). Plugins declare widgets at register
/// time; the TUI renders from this snapshot and applies widget.update
/// events here. A dead plugin's widgets are removed (no UI residue).
#[derive(Default, Debug)]
pub struct WidgetRegistry {
    entries: Vec<WidgetEntry>,
}

impl WidgetRegistry {
    /// Register a plugin's declared widgets (called after the handshake).
    pub(crate) fn register_plugin(&mut self, plugin: &str, widgets: &[tack_ext::WidgetSpec]) {
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

    /// Apply a widget.update (full-state replacement). False = unknown
    /// widget id (the caller warns and ignores).
    pub fn apply_update(&mut self, plugin: &str, update: &tack_ext::WidgetUpdatePayload) -> bool {
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

/// A registered autocomplete provider (v2.2). Cloneable so queries can run
/// on spawned tasks off the TUI main loop (the TUI adds a 300ms UI-level
/// timeout on top of the protocol's 30s request timeout).
#[derive(Clone)]
pub struct ExtAutocompleteProvider {
    /// Host key `<plugin>:<id>`.
    pub key: String,
    pub plugin: String,
    pub spec: tack_ext::AutocompleteProviderSpec,
    peer: Arc<tack_ext::PluginPeer>,
}

impl std::fmt::Debug for ExtAutocompleteProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtAutocompleteProvider")
            .field("key", &self.key)
            .finish()
    }
}

impl ExtAutocompleteProvider {
    /// Query the plugin's `autocomplete.provide`. Contract degradation:
    /// dead plugins, error responses (e.g. a v1 plugin that doesn't know
    /// the method) and malformed results all yield no suggestions.
    pub async fn provide(
        &self,
        query: &str,
        cursor_offset: usize,
    ) -> Vec<tack_ext::AutocompleteSuggestion> {
        let params = tack_ext::AutocompleteProvideParams {
            provider_id: self.spec.id.clone(),
            query: query.to_string(),
            cursor_offset,
        };
        let Ok(params) = serde_json::to_value(params) else {
            return Vec::new();
        };
        let Ok(result) = self.peer.call("autocomplete.provide", params).await else {
            return Vec::new();
        };
        serde_json::from_value::<tack_ext::AutocompleteProvideResult>(result)
            .map(|r| r.suggestions)
            .unwrap_or_default()
    }
}

/// Spawn a watcher firing `on_dead` once the plugin's peer observes EOF
/// (process exit / guest trap). The TUI uses it to drop the plugin's
/// widgets — a dead plugin leaves no UI residue (extensions-v2.md §3.2).
/// Also fires on graceful shutdown; by then the receiver is gone and the
/// send fails silently.
pub fn watch_plugin_death(
    peer: Arc<tack_ext::PluginPeer>,
    on_dead: impl FnOnce() + Send + 'static,
) {
    tokio::spawn(async move {
        peer.wait_dead().await;
        on_dead();
    });
}

#[derive(Default)]
pub struct ExtensionManager {
    pub plugins: Vec<LoadedPlugin>,
    /// command name → plugin index.
    commands: HashMap<String, usize>,
    /// Declarative widget registry (v2.1), keyed `<plugin>:<id>`.
    widgets: WidgetRegistry,
    /// Bundle contributions from installed extensions (merged by callers).
    pub bundle_hooks: crate::shell_hooks::HookConfig,
    pub bundle_mcp_servers: Vec<tack_tools::mcp::McpServerSpec>,
    pub bundle_skill_dirs: Vec<PathBuf>,
    /// Shared wasmtime engine for WASM-carrier plugins (kept alive for the
    /// epoch ticker driving wall-clock limits).
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

/// Discover extension manifests (user dir → extensionPaths → project dir).
fn discover(cwd: &Path, agent_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let user_ext = agent_dir.join("extensions");
    if let Ok(read) = std::fs::read_dir(&user_ext) {
        dirs.extend(read.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    for extra in crate::settings::Settings::extra_paths(agent_dir, "extensionPaths") {
        let path = PathBuf::from(extra);
        if path.is_dir() {
            if path.join("extension.json").is_file() {
                dirs.push(path);
            } else if let Ok(read) = std::fs::read_dir(&path) {
                dirs.extend(read.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
            }
        }
    }
    // Project extensions: trust-gated (they execute code).
    if crate::project_trust::is_trusted(cwd, agent_dir) {
        let project_ext = cwd.join(".pi").join("extensions");
        if let Ok(read) = std::fs::read_dir(project_ext) {
            dirs.extend(read.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }

    let mut manifests = Vec::new();
    for dir in dirs {
        let manifest_path = dir.join("extension.json");
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        match serde_json::from_str::<ExtensionManifest>(&content) {
            Ok(manifest) => manifests.push((manifest.name.clone(), dir)),
            Err(e) => tracing::warn!(
                "ignoring bad extension manifest {}: {e}",
                manifest_path.display()
            ),
        }
    }
    manifests
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

impl ExtensionManager {
    /// Discover and start all extensions. Failures are logged and skipped —
    /// a broken plugin must never fail session creation (TS loader semantics).
    pub async fn load(
        cwd: &Path,
        agent_dir: &Path,
        mode: &str,
        services: Arc<dyn HostServices>,
        lock_required: bool,
    ) -> Self {
        let mut manager = ExtensionManager::default();
        let trusted = crate::project_trust::is_trusted(cwd, agent_dir);
        let manifests = discover(cwd, agent_dir);
        // Supply-chain gate: user-dir extensions installed via `ext install`
        // carry a lockfile entry pinning the resolved commit. A plugin whose
        // checkout drifted from the pin is skipped (default) or merely warned
        // about (settings extensionLockRequired=false). Plugins without a
        // lock entry (manually copied, local-dir installs) are unaffected.
        let lock = read_lock(agent_dir).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable extensions lockfile: {e}");
            ExtensionsLock::default()
        });
        let user_root = agent_dir.join("extensions");
        for (name, dir) in manifests {
            if !lock_allows(&dir, &user_root, &lock, lock_required) {
                continue;
            }
            let manifest_path = dir.join("extension.json");
            let Ok(content) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<ExtensionManifest>(&content) else {
                continue;
            };
            // Bundle contributions are collected even when the manifest has
            // no runnable plugin (a hooks/skills/MCP-only bundle is valid).
            manager.collect_bundle_resources(&name, &dir, &manifest);
            // Tag this plugin's events (widget.update needs the origin).
            let services: Arc<dyn HostServices> = Arc::new(TaggedServices {
                plugin: name.clone(),
                inner: services.clone(),
            });

            let carrier = manifest.carrier.as_deref().unwrap_or("process");
            let mut handle = match carrier {
                "wasm" => {
                    #[cfg(feature = "wasm")]
                    {
                        let Some(module) = &manifest.module else {
                            tracing::warn!("extension {name}: carrier wasm requires `module`");
                            continue;
                        };
                        let module_path = dir.join(module);
                        let wasm = match std::fs::read(&module_path) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                tracing::warn!(
                                    "extension {name}: cannot read {}: {e}",
                                    module_path.display()
                                );
                                continue;
                            }
                        };
                        if manager.wasm_carrier.is_none() {
                            match tack_ext_wasm::WasmCarrier::new() {
                                Ok(carrier) => manager.wasm_carrier = Some(carrier),
                                Err(e) => {
                                    tracing::warn!("extension {name}: wasmtime unavailable: {e}");
                                    continue;
                                }
                            }
                        }
                        let carrier_engine = manager.wasm_carrier.as_ref().expect("carrier");
                        let limits = manifest.limits.map(|l| l.to_limits()).unwrap_or_default();
                        let capabilities = manifest
                            .capabilities
                            .map(|c| c.into_capabilities(&name, &dir))
                            .unwrap_or_default();
                        audit_capability_grants(&name, &capabilities);
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
                                tracing::warn!("extension {name} failed to start (wasm): {e}");
                                continue;
                            }
                        }
                    }
                    #[cfg(not(feature = "wasm"))]
                    {
                        tracing::warn!(
                            "extension {name}: carrier wasm requested, but this build \
                             has no wasm support (built with --no-default-features)"
                        );
                        continue;
                    }
                }
                "process" => {
                    let Some(command) = &manifest.command else {
                        // Bundle-only extension (no runnable plugin).
                        continue;
                    };
                    let args: Vec<String> = manifest
                        .args
                        .iter()
                        .map(|a| {
                            // Only path-like args resolve against the extension dir;
                            // plain words (subcommands, flags, node args) pass through.
                            let path_like =
                                a.contains('/') || a.contains('\\') || a.starts_with('.');
                            if path_like && !PathBuf::from(a).is_absolute() {
                                dir.join(a).to_string_lossy().to_string()
                            } else {
                                a.clone()
                            }
                        })
                        .collect();
                    let env: Vec<(String, String)> = manifest.env.clone().into_iter().collect();
                    match PluginProcess::spawn(command, &args, &env, &dir, services.clone()).await {
                        Ok(process) => PluginHandle::Process(Box::new(process)),
                        Err(e) => {
                            tracing::warn!("extension {name} failed to start: {e}");
                            continue;
                        }
                    }
                }
                other => {
                    tracing::warn!("extension {name}: unknown carrier {other:?}");
                    continue;
                }
            };
            // v2.1: every carrier negotiates protocol 2. The declarative
            // widget / autocomplete surface (extensions-v2.md §3) is
            // carrier-orthogonal, and a v1 plugin simply ignores the higher
            // number (register parsing tolerates unknown fields both ways).
            let protocol = 2;
            let init = InitializePayload {
                protocol,
                mode: mode.to_string(),
                cwd: cwd.to_string_lossy().to_string(),
                trusted,
                host: format!("tack/{}", env!("CARGO_PKG_VERSION")),
            };
            match handle.peer().initialize(init).await {
                Ok(mut register) => {
                    // Reject malformed tool parameter schemas at
                    // registration (upstream pi #9300): a non-object schema
                    // would break provider request serialization.
                    register
                        .tools
                        .retain(|spec| match spec.validate_parameters(&name) {
                            Ok(()) => true,
                            Err(e) => {
                                tracing::warn!("extension {name}: {e}");
                                false
                            }
                        });
                    tracing::info!(
                        "extension {} loaded ({} tools, {} commands, carrier {})",
                        name,
                        register.tools.len(),
                        register.commands.len(),
                        carrier,
                    );
                    for command in &register.commands {
                        manager
                            .commands
                            .insert(command.name.clone(), manager.plugins.len());
                    }
                    manager.widgets.register_plugin(&name, &register.widgets);
                    manager.plugins.push(LoadedPlugin {
                        name,
                        handle,
                        register,
                    });
                }
                Err(e) => {
                    tracing::warn!("extension {name} handshake failed: {e}");
                    handle.shutdown().await;
                }
            }
        }
        manager
    }

    /// Collect a manifest's declarative bundle resources (hooks / MCP
    /// servers / skill dirs) into the manager. Paths resolve against the
    /// extension directory.
    fn collect_bundle_resources(&mut self, name: &str, dir: &Path, manifest: &ExtensionManifest) {
        if let Some(hooks) = &manifest.hooks {
            let paths: Vec<&str> = match hooks {
                Value::String(path) => vec![path.as_str()],
                Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                _ => {
                    tracing::warn!("extension {name}: `hooks` must be a path or a list of paths");
                    vec![]
                }
            };
            for path in paths {
                let path = dir.join(path);
                match std::fs::read_to_string(&path) {
                    Ok(content) => match crate::shell_hooks::parse_hooks_file(&content) {
                        Ok(config) => self.bundle_hooks.extend(config),
                        Err(e) => tracing::warn!(
                            "extension {name}: bad hooks file {}: {e}",
                            path.display()
                        ),
                    },
                    Err(e) => {
                        tracing::warn!("extension {name}: cannot read {}: {e}", path.display())
                    }
                }
            }
        }
        match &manifest.mcp_servers {
            Some(Value::String(path)) => {
                let path = dir.join(path);
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<Value>(&content) {
                        Ok(value) => {
                            self.bundle_mcp_servers
                                .extend(crate::mcp_config::specs_from_value(
                                    &value,
                                    &path.display().to_string(),
                                ))
                        }
                        Err(e) => tracing::warn!(
                            "extension {name}: bad MCP servers file {}: {e}",
                            path.display()
                        ),
                    },
                    Err(e) => {
                        tracing::warn!("extension {name}: cannot read {}: {e}", path.display())
                    }
                }
            }
            Some(value @ Value::Object(_)) => {
                self.bundle_mcp_servers
                    .extend(crate::mcp_config::specs_from_value(
                        value,
                        &format!("extension {name}"),
                    ));
            }
            Some(_) => {
                tracing::warn!("extension {name}: `mcpServers` must be a path or an object");
            }
            None => {}
        }
        if let Some(skills) = &manifest.skills {
            for path in skills {
                let path = dir.join(path);
                if path.is_dir() {
                    self.bundle_skill_dirs.push(path);
                } else {
                    tracing::warn!("extension {name}: skill dir {} missing", path.display());
                }
            }
        }
    }

    /// All plugin tools for the agent loop.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        for plugin in &self.plugins {
            for spec in &plugin.register.tools {
                tools.push(Arc::new(ExtTool::new(
                    &plugin.name,
                    spec.clone(),
                    plugin.handle.peer().clone(),
                )));
            }
        }
        tools
    }

    /// Per-plugin hook bridges (tool_call interception).
    pub fn hooks(&self) -> Vec<Arc<dyn AgentHooks>> {
        self.plugins
            .iter()
            .map(|p| {
                Arc::new(ExtHooks::new(
                    p.handle.peer().clone(),
                    &p.register.subscriptions,
                )) as Arc<dyn AgentHooks>
            })
            .collect()
    }

    /// No plugins loaded — callers can skip per-event payload
    /// serialization (e.g. the TUI's event fan-out).
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Fan an event out to subscribed plugins (fire-and-forget).
    pub async fn notify(&self, event: &str, payload: Value) {
        for plugin in &self.plugins {
            let subscribed = if plugin.register.subscriptions.is_empty() {
                DEFAULT_EVENTS.contains(&event)
            } else {
                plugin.register.subscriptions.iter().any(|s| s == event)
            };
            if subscribed {
                let _ = plugin
                    .handle
                    .peer()
                    .send_event(event, payload.clone())
                    .await;
            }
        }
    }

    /// Registered extension slash-command names.
    pub fn command_names(&self) -> Vec<String> {
        self.commands.keys().cloned().collect()
    }

    /// Declarative widgets (v2.1): spec + current state, keyed `<plugin>:<id>`.
    pub fn widgets(&self) -> &[WidgetEntry] {
        self.widgets.entries()
    }

    /// Apply a plugin's widget.update (full-state replacement). False =
    /// unknown widget id — the caller warns and ignores (a plugin racing
    /// its own death, or a typo'd id, must never break the UI).
    pub fn apply_widget_update(
        &mut self,
        plugin: &str,
        update: &tack_ext::WidgetUpdatePayload,
    ) -> bool {
        self.widgets.apply_update(plugin, update)
    }

    /// A plugin died (peer EOF): its widgets vanish with it. Returns the
    /// removed host keys so the UI can drop focus/scroll state.
    pub fn remove_plugin_widgets(&mut self, plugin: &str) -> Vec<String> {
        self.widgets.remove_plugin(plugin)
    }

    /// Test hook: register a widget without a running plugin.
    #[doc(hidden)]
    pub fn test_insert_widget(&mut self, plugin: &str, spec: tack_ext::WidgetSpec) {
        self.widgets.register_plugin(plugin, &[spec]);
    }

    /// Registered autocomplete providers (v2.2), in register order.
    pub fn autocomplete_providers(&self) -> Vec<ExtAutocompleteProvider> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
            for spec in &plugin.register.autocomplete_providers {
                out.push(ExtAutocompleteProvider {
                    key: format!("{}:{}", plugin.name, spec.id),
                    plugin: plugin.name.clone(),
                    spec: spec.clone(),
                    peer: plugin.handle.peer().clone(),
                });
            }
        }
        out
    }

    /// Query one autocomplete provider by host key. Unknown key, dead
    /// plugin or error response → no suggestions (contract degradation).
    pub async fn autocomplete_provide(
        &self,
        provider_key: &str,
        query: &str,
        cursor_offset: usize,
    ) -> Vec<tack_ext::AutocompleteSuggestion> {
        let Some(provider) = self
            .autocomplete_providers()
            .into_iter()
            .find(|p| p.key == provider_key)
        else {
            return Vec::new();
        };
        provider.provide(query, cursor_offset).await
    }

    /// Report a widget interaction (e.g. a list selection) back to the
    /// OWNING plugin only — widget.action is not subscription-gated and
    /// never broadcast (fire-and-forget; dead plugins silently drop).
    pub async fn notify_widget_action(&self, plugin: &str, action: tack_ext::WidgetActionPayload) {
        let Some(plugin) = self.plugins.iter().find(|p| p.name == plugin) else {
            return;
        };
        let Ok(payload) = serde_json::to_value(action) else {
            return;
        };
        let _ = plugin
            .handle
            .peer()
            .send_event("widget.action", payload)
            .await;
    }

    /// All registered shortcuts as (action, plugin index) — wired into the
    /// keybinding registry by the TUI.
    pub fn shortcut_actions(&self) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        for (index, plugin) in self.plugins.iter().enumerate() {
            for shortcut in &plugin.register.shortcuts {
                out.push((shortcut.action.clone(), index));
            }
        }
        out
    }

    /// Notify one plugin that its shortcut fired.
    pub async fn notify_shortcut(&self, plugin_index: usize, action: &str) {
        if let Some(plugin) = self.plugins.get(plugin_index) {
            let _ = plugin
                .handle
                .peer()
                .send_event("shortcut", serde_json::json!({ "action": action }))
                .await;
        }
    }

    /// A shareable event sink for the provider-events wrapper.
    pub fn clone_sink(&self) -> Arc<ExtSinkHandle> {
        ExtSinkHandle::spawn(
            self.plugins
                .iter()
                .map(|p| (p.handle.peer().clone(), p.register.subscriptions.clone()))
                .collect(),
        )
    }
}

/// A cheap-cloneable event sink sharing the plugins' peers (for the
/// provider-events wrapper, which outlives borrows of the manager).
///
/// `notify` is sync fire-and-forget called from provider code — one task
/// spawn PER EVENT used to flood the runtime under streaming load. Instead
/// events go into a single bounded queue drained by ONE consumer task
/// (spawned with the sink); a full queue drops events (they are
/// best-effort lifecycle notifications, never required for correctness).
#[derive(Debug)]
pub struct ExtSinkHandle {
    queue: mpsc::Sender<(String, Value)>,
}

/// Backlog bound for plugin-bound events before they are dropped.
const EXT_EVENT_QUEUE_DEPTH: usize = 256;

impl ExtSinkHandle {
    fn spawn(peers: Vec<(Arc<tack_ext::PluginPeer>, Vec<String>)>) -> Arc<Self> {
        let (tx, mut rx) = mpsc::channel::<(String, Value)>(EXT_EVENT_QUEUE_DEPTH);
        tokio::spawn(async move {
            while let Some((event, payload)) = rx.recv().await {
                for (peer, subscriptions) in &peers {
                    let subscribed = if subscriptions.is_empty() {
                        DEFAULT_EVENTS.contains(&event.as_str())
                    } else {
                        subscriptions.iter().any(|s| s == &event)
                    };
                    if subscribed {
                        let _ = peer.send_event(&event, payload.clone()).await;
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

impl ExtensionManager {
    /// Invoke an extension command (run by the TUI command dispatcher).
    pub async fn invoke_command(&self, name: &str, args: &str) -> Result<Value, String> {
        let Some(index) = self.commands.get(name) else {
            return Err(format!("unknown extension command {name:?}"));
        };
        let plugin = &self.plugins[*index];
        plugin
            .handle
            .peer()
            .call(
                "command.invoke",
                serde_json::json!({ "name": name, "args": args }),
            )
            .await
    }

    /// Gracefully stop all plugins.
    pub async fn shutdown(&mut self) {
        for plugin in &mut self.plugins {
            plugin.handle.shutdown().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Install channel (tack ext install/list/remove)
// ---------------------------------------------------------------------------

fn extensions_root(cwd: &Path, agent_dir: &Path, local: bool) -> PathBuf {
    if local {
        cwd.join(".pi").join("extensions")
    } else {
        agent_dir.join("extensions")
    }
}

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

/// Split a `<git-url>[#<ref>]` install source into (url, ref). The ref may
/// be a tag, branch, or full/short commit sha. Returns the source unchanged
/// when no usable `#ref` suffix is present.
pub fn split_source_ref(source: &str) -> (String, Option<String>) {
    match source.rsplit_once('#') {
        Some((url, git_ref)) if !url.is_empty() && !git_ref.is_empty() => {
            (url.to_string(), Some(git_ref.to_string()))
        }
        _ => (source.to_string(), None),
    }
}

/// Extension and marketplace names become filesystem paths
/// (`extensions/<name>`, `marketplaces/<name>.json`): reject anything
/// that could escape the target directory or hide (path separators,
/// `.`/`..`, leading dots, empty).
pub fn validate_extension_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) || name.contains('\0')
    {
        anyhow::bail!(
            "invalid extension name {name:?}: must be a plain directory name \
             (no path separators, no leading dot)"
        );
    }
    Ok(())
}

/// Install an extension from a git URL or a local directory. Returns the
/// installed directory.
pub fn install_extension(
    source: &str,
    cwd: &Path,
    agent_dir: &Path,
    local: bool,
) -> anyhow::Result<PathBuf> {
    install_extension_named(source, cwd, agent_dir, local, None, None, None)
}

/// Derive the install directory name from a git source: the last path
/// segment with any trailing `.git` stripped. Split on both separator
/// kinds (and `:` for scp-style git@host:user/repo sources): a local
/// source path on Windows arrives with backslashes (C:\path\upstream.git).
fn git_source_name(source: &str) -> String {
    source
        .trim_end_matches(['/', '\\'])
        .trim_end_matches(".git")
        .rsplit(['/', '\\', ':'])
        .next()
        .unwrap_or("extension")
        .to_string()
}

/// Install with an optional name override (marketplace installs use the
/// plugin's marketplace key instead of the source-derived name), an optional
/// git ref pin (`rev`), and the originating marketplace name (recorded in
/// the lockfile).
///
/// Git installs keep their `.git` directory so `ext verify` (and the startup
/// lock check) can compare `HEAD` against the lockfile's resolved commit.
/// User-dir installs write/update a lockfile entry; project-local (`--local`)
/// installs are trust-gated already and stay out of the lockfile.
pub fn install_extension_named(
    source: &str,
    cwd: &Path,
    agent_dir: &Path,
    local: bool,
    name_override: Option<&str>,
    rev: Option<&str>,
    marketplace: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let root = extensions_root(cwd, agent_dir, local);
    std::fs::create_dir_all(&root)?;

    let is_git = source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.ends_with(".git");
    if is_git {
        let name = name_override
            .map(str::to_string)
            .unwrap_or_else(|| git_source_name(source));
        validate_extension_name(&name)?;
        let target = root.join(&name);
        if target.exists() {
            anyhow::bail!("{} already exists (remove it first)", target.display());
        }
        // A pinned rev may point at an arbitrary commit, so it needs a full
        // clone; unpinned installs stay shallow. The clone must be bounded
        // with piped stdio (never bare `.status()`): an unbounded wait on a
        // wedged child (credential prompt nobody answers, hung transport)
        // stalls the caller forever, and with INHERITED stdio the orphaned
        // child keeps the parent's/test-runner's stdout pipe open past any
        // kill — the wedge that repeatedly stalled the ubuntu CI job.
        let mut clone = std::process::Command::new("git");
        clone.arg("clone");
        if rev.is_none() {
            clone.args(["--depth", "1"]);
        }
        let output = crate::sync_process::output_with_timeout(
            clone.arg(source).arg(&target),
            std::time::Duration::from_secs(600),
        )
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
        if !output.status.success() {
            let _ = std::fs::remove_dir_all(&target);
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
                    .current_dir(&target),
                std::time::Duration::from_secs(60),
            )
            .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
            if !output.status.success() {
                let _ = std::fs::remove_dir_all(&target);
                anyhow::bail!(
                    "git checkout {rev} failed with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        validate_extension(&target)?;
        if !local {
            let resolved = git_head_commit(&target);
            lock_record_install(agent_dir, &name, source, rev, resolved, marketplace)?;
        }
        return Ok(target);
    }

    if rev.is_some() {
        anyhow::bail!("`#<ref>` pinning only applies to git URLs; {source} is a local directory");
    }
    let source_dir = PathBuf::from(source);
    if !source_dir.is_dir() {
        anyhow::bail!("{source} is neither a git URL nor an existing directory");
    }
    let name = name_override.map(str::to_string).unwrap_or_else(|| {
        source_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "extension".to_string())
    });
    validate_extension_name(&name)?;
    let target = root.join(&name);
    if target.exists() {
        anyhow::bail!("{} already exists (remove it first)", target.display());
    }
    copy_dir_recursive(&source_dir, &target)?;
    validate_extension(&target)?;
    if !local {
        // Local-directory installs carry no commit to pin.
        lock_record_install(agent_dir, &name, source, None, None, marketplace)?;
    }
    Ok(target)
}

fn validate_extension(dir: &Path) -> anyhow::Result<()> {
    let manifest = dir.join("extension.json");
    if !manifest.is_file() {
        let _ = std::fs::remove_dir_all(dir);
        anyhow::bail!(
            "no extension.json in {} — not an extension directory",
            dir.display()
        );
    }
    Ok(())
}

/// Remove an installed extension by directory name.
pub fn remove_extension(
    name: &str,
    cwd: &Path,
    agent_dir: &Path,
    local: bool,
) -> anyhow::Result<()> {
    validate_extension_name(name)?;
    let target = extensions_root(cwd, agent_dir, local).join(name);
    if !target.is_dir() {
        anyhow::bail!("no extension named {name:?} installed");
    }
    std::fs::remove_dir_all(&target)?;
    if !local {
        lock_remove(agent_dir, name)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Extensions lockfile (~/.tack/agent/extensions-lock.json): pins every
// user-dir `ext install` to the commit that was actually installed, so
// `ext verify` and the startup check can detect post-install tampering.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExtensionsLock {
    pub version: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, LockEntry>,
}

impl Default for ExtensionsLock {
    fn default() -> Self {
        ExtensionsLock {
            version: 1,
            plugins: BTreeMap::new(),
        }
    }
}

/// One locked plugin. `rev` is the user-requested ref (tag/branch/sha),
/// `resolved_commit` the 40-char sha HEAD actually resolved to (null for
/// local-directory installs), `installed_at` unix seconds.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockEntry {
    pub source: String,
    pub rev: Option<String>,
    pub resolved_commit: Option<String>,
    pub installed_at: u64,
    pub marketplace: Option<String>,
}

pub fn lock_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions-lock.json")
}

/// Read the lockfile; a missing file is an empty lock (not an error).
fn read_lock(agent_dir: &Path) -> anyhow::Result<ExtensionsLock> {
    let path = lock_path(agent_dir);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ok(ExtensionsLock::default());
    };
    let lock: ExtensionsLock = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("bad extensions lockfile {}: {e}", path.display()))?;
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
    name: &str,
    source: &str,
    rev: Option<&str>,
    resolved_commit: Option<String>,
    marketplace: Option<&str>,
) -> anyhow::Result<()> {
    let mut lock = read_lock(agent_dir)?;
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    lock.plugins.insert(
        name.to_string(),
        LockEntry {
            source: source.to_string(),
            rev: rev.map(str::to_string),
            resolved_commit,
            installed_at,
            marketplace: marketplace.map(str::to_string),
        },
    );
    write_lock(agent_dir, &lock)
}

/// Drop a removed extension's lock entry (no-op when absent).
fn lock_remove(agent_dir: &Path, name: &str) -> anyhow::Result<()> {
    let mut lock = read_lock(agent_dir)?;
    if lock.plugins.remove(name).is_some() {
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
/// tree is clean (no modified tracked files, no untracked files),
/// Some(false) when dirty, None when git itself fails.
///
/// HEAD alone proves nothing about tampering: in-place edits without a
/// commit leave `rev-parse HEAD` unchanged, so the lockfile gate also
/// requires a clean tree.
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
/// directory (`tack ext verify`). Entries without a resolved commit
/// (local-directory installs) have nothing to check and are skipped.
pub fn verify_extensions(agent_dir: &Path) -> anyhow::Result<Vec<(String, VerifyStatus)>> {
    let lock = read_lock(agent_dir)?;
    let mut out = Vec::new();
    for (name, entry) in &lock.plugins {
        let Some(expected) = &entry.resolved_commit else {
            continue;
        };
        let dir = agent_dir.join("extensions").join(name);
        let status = if !dir.is_dir() {
            VerifyStatus::Missing
        } else if !dir.join(".git").exists() {
            VerifyStatus::NotAGitRepo
        } else {
            match git_head_commit(&dir) {
                Some(actual) if &actual == expected => match git_worktree_clean(&dir) {
                    Some(true) => VerifyStatus::Ok(actual),
                    // HEAD matches but the tree was edited in place
                    // (uncommitted tracked changes or untracked files).
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
        out.push((name.clone(), status));
    }
    Ok(out)
}

/// Startup gate for ExtensionManager::load: may this discovered extension
/// directory load? Only user-dir extensions with a lock entry carrying a
/// resolved commit are gated; everything else loads unconditionally.
fn lock_allows(dir: &Path, user_root: &Path, lock: &ExtensionsLock, required: bool) -> bool {
    if dir.parent() != Some(user_root) {
        return true;
    }
    let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return true;
    };
    let Some(entry) = lock.plugins.get(&name) else {
        return true;
    };
    let Some(expected) = &entry.resolved_commit else {
        return true;
    };
    let actual = if dir.join(".git").exists() {
        git_head_commit(dir)
    } else {
        None
    };
    match actual {
        Some(actual) if &actual == expected => match git_worktree_clean(dir) {
            Some(true) => true,
            dirty => {
                // HEAD matches but the tree was edited in place (or can't
                // be inspected): treat as tampering, like a HEAD drift.
                tracing::warn!(
                    "extension {name}: HEAD matches the locked commit {expected} but {} — {}",
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
                "extension {name}: HEAD {actual} differs from locked commit {expected} — {}",
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
                "extension {name}: locked to commit {expected} but the install is not a git \
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

/// List installed extensions (name + manifest description source dir).
pub fn list_extensions(cwd: &Path, agent_dir: &Path) -> Vec<(String, PathBuf, bool)> {
    let mut out = Vec::new();
    for (dir, local) in [
        (agent_dir.join("extensions"), false),
        (cwd.join(".pi").join("extensions"), true),
    ] {
        if let Ok(read) = std::fs::read_dir(&dir) {
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() && path.join("extension.json").is_file() {
                    out.push((entry.file_name().to_string_lossy().to_string(), path, local));
                }
            }
        }
    }
    out.sort();
    out
}

/// A tiny tack-ext plugin speaking NDJSON on stdio: registers one tool
/// (`echo`), one command (`hello`), and subscriptions; echoes tool arguments
/// back, answers `ui.notify` on agent_start, and allows every intercepted
/// tool call. Serves as the protocol reference for plugin authors.
pub async fn run_demo_plugin() -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut out = tokio::io::stdout();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    // Expect initialize.
    let Some(init) = lines.next_line().await? else {
        anyhow::bail!("no initialize")
    };
    let init: tack_ext::Envelope = serde_json::from_str(&init)?;
    let tack_ext::Envelope::Event { event, .. } = init else {
        anyhow::bail!("expected initialize")
    };
    anyhow::ensure!(event == "initialize");

    let register = tack_ext::Envelope::event(
        "register",
        serde_json::json!({
            "name": "demo",
            "tools": [{
                "name": "echo",
                "description": "Echo the arguments back",
                "parameters": { "type": "object", "properties": { "text": { "type": "string" } } }
            }],
            "commands": [{ "name": "hello", "description": "Say hello" }],
            "subscriptions": ["agent_start", "message_end", "tool_call"],
            // v2.1: declarative widgets — a status segment and a list panel.
            "widgets": [
                { "id": "demo-status", "type": "status_line_segment", "priority": 10,
                  "initial": { "text": "demo:ok", "style": "info" } },
                { "id": "demo-list", "type": "list_panel", "title": "Demo items",
                  "visible": true,
                  "initial": { "items": [
                      { "id": "a", "label": "Alpha", "detail": "first" },
                      { "id": "b", "label": "Beta" }
                  ] } }
            ],
            // v2.2: an autocomplete provider on the '#' trigger.
            "autocompleteProviders": [
                { "id": "hash", "trigger": "#", "description": "Demo tags" }
            ],
        }),
    );
    out.write_all(format!("{}\n", serde_json::to_string(&register)?).as_bytes())
        .await?;
    out.flush().await?;

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(envelope) = serde_json::from_str::<tack_ext::Envelope>(&line) else {
            continue;
        };
        match envelope {
            tack_ext::Envelope::Request { id, method, params } => {
                let response = match method.as_str() {
                    "tool.execute" => {
                        let text = params
                            .get("arguments")
                            .and_then(|a| a.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        tack_ext::Envelope::result(
                            id,
                            serde_json::json!({ "content": format!("echo: {text}") }),
                        )
                    }
                    "command.invoke" => {
                        // Ask the host to notify the user, then answer.
                        let notify = tack_ext::Envelope::request(
                            9001,
                            "ui.notify",
                            serde_json::json!({ "message": "hello from the demo plugin!" }),
                        );
                        out.write_all(format!("{}\n", serde_json::to_string(&notify)?).as_bytes())
                            .await?;
                        out.flush().await?;
                        tack_ext::Envelope::result(id, serde_json::json!({ "ok": true }))
                    }
                    "intercept.tool_call" => {
                        tack_ext::Envelope::result(id, serde_json::json!({ "action": "allow" }))
                    }
                    "autocomplete.provide" => {
                        // v2.2: filter the demo tags by the query.
                        let query = params
                            .get("query")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_lowercase();
                        let tags = ["#alpha", "#beta", "#wasm"];
                        let suggestions: Vec<Value> = tags
                            .iter()
                            .filter(|t| query.is_empty() || t.contains(&query))
                            .map(|t| {
                                serde_json::json!({
                                    "value": t,
                                    "label": format!("{t} demo tag"),
                                    "detail": "demo",
                                })
                            })
                            .collect();
                        tack_ext::Envelope::result(
                            id,
                            serde_json::json!({ "suggestions": suggestions }),
                        )
                    }
                    other => tack_ext::Envelope::error(id, format!("unknown method {other}")),
                };
                out.write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
                    .await?;
                out.flush().await?;
            }
            tack_ext::Envelope::Event { event, .. } if event == "agent_start" => {
                let notify = tack_ext::Envelope::request(
                    9002,
                    "ui.notify",
                    serde_json::json!({ "message": "demo plugin saw agent_start" }),
                );
                out.write_all(format!("{}\n", serde_json::to_string(&notify)?).as_bytes())
                    .await?;
                out.flush().await?;
            }
            // v2.1: the host reports a list-panel selection; echo it back
            // via ui.notify so e2e tests can observe the round-trip.
            tack_ext::Envelope::Event { event, payload } if event == "widget.action" => {
                let notify = tack_ext::Envelope::request(
                    9003,
                    "ui.notify",
                    serde_json::json!({ "message": format!("demo plugin saw widget.action {payload}") }),
                );
                out.write_all(format!("{}\n", serde_json::to_string(&notify)?).as_bytes())
                    .await?;
                out.flush().await?;
            }
            tack_ext::Envelope::Event { event, .. } if event == "shutdown" => break,
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Marketplaces (tack ext marketplace …): named JSON catalogs mapping
// plugin names to install sources, stored in <agentDir>/marketplaces/.
// A marketplace file looks like:
//
// ```json
// {
//   "name": "acme-tools",
//   "plugins": {
//     "code-review": { "source": "https://github.com/acme/tack-ext-review.git",
//                      "description": "review buddy", "rev": "v1.2.0" },
//     "policies":    { "source": "/opt/acme/tack-ext-policies" }
//   },
//   "signature": { "algorithm": "ed25519", "value": "<hex>" }
// }
// ```
//
// Install with `tack ext install <plugin>@<marketplace>`.
//
// Signatures: the signed payload is the canonical catalog — the JSON with
// the top-level `signature` key removed, reserialized via serde_json
// (Map = BTreeMap, so object keys sort lexicographically). Registration is
// TOFU: the first `--public-key` that verifies is pinned to
// `<name>.key` next to the catalog, and every later install re-verifies the
// catalog against the pinned key.

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
    #[allow(dead_code)]
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

/// Enforce the signature policy for an already-registered catalog: a signed
/// catalog must verify against its pinned key (TOFU, see add_marketplace).
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

/// Register a marketplace from a local JSON file or an http(s) URL. The
/// catalog is copied into `<agentDir>/marketplaces/<name>.json`.
///
/// A signed catalog (`signature` field) requires `--public-key <hex>` on
/// first registration (or an already pinned key); the verifying key is
/// pinned to `<name>.key` (TOFU) and re-checked on every install.
pub async fn add_marketplace(
    name: &str,
    source: &str,
    agent_dir: &Path,
    public_key: Option<&str>,
) -> anyhow::Result<PathBuf> {
    validate_extension_name(name)?;
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
    // Validate (and verify, when signed) before accepting; on any failure
    // the catalog is not registered.
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
    validate_extension_name(name)?;
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
    validate_extension_name(marketplace)?;
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
    /// Plugin name (marketplace key; also the install directory name).
    pub plugin: String,
    /// Install source from the catalog.
    pub source: String,
    /// Catalog-pinned git ref, when the entry declares `"rev"`.
    pub rev: Option<String>,
    /// Marketplace the plugin resolved through.
    pub marketplace: String,
}

/// Resolve a `<plugin>@<marketplace>` spec. A signed catalog is re-verified
/// against its pinned key before anything is resolved from it.
pub fn resolve_marketplace_spec(
    spec: &str,
    agent_dir: &Path,
) -> anyhow::Result<Option<MarketplaceResolution>> {
    let Some((plugin, marketplace)) = spec.rsplit_once('@') else {
        return Ok(None);
    };
    // Not a marketplace spec: ssh-style git URLs (git@host:path) contain an
    // '@' too; marketplace names are bare file stems.
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
    // The catalog key becomes the install directory name — a hostile
    // catalog must not escape extensions/ via `../` or separators.
    validate_extension_name(plugin)?;
    Ok(Some(MarketplaceResolution {
        plugin: plugin.to_string(),
        source: entry.source.clone(),
        rev: entry.rev.clone(),
        marketplace: marketplace.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

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

        // Tighter-than-cap values pass through untouched.
        let tight = WasmLimitsJson {
            max_fuel: Some(1_000),
            max_memory_bytes: Some(4096),
            max_execution_ms: None,
        }
        .to_limits();
        assert_eq!(tight.max_fuel, 1_000);
        assert_eq!(tight.max_memory_bytes, 4096);

        // Defaults stay below the caps.
        let defaults = WasmLimitsJson::default().to_limits();
        assert!(defaults.max_fuel <= HARD_MAX_FUEL);
        assert!(defaults.max_memory_bytes <= HARD_MAX_MEMORY_BYTES);
    }

    /// Undeclared fields fall back to the tack-ext-wasm engine defaults
    /// exactly (1e9 fuel, 256 MiB memory, 10-minute hang watchdog).
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_defaults_match_engine_defaults() {
        let limits = WasmLimitsJson::default().to_limits();
        let engine_defaults = tack_ext_wasm::WasmLimits::default();
        assert_eq!(limits.max_fuel, engine_defaults.max_fuel);
        assert_eq!(limits.max_memory_bytes, engine_defaults.max_memory_bytes);
        assert_eq!(limits.max_execution, engine_defaults.max_execution);
        assert_eq!(limits.max_fuel, 1_000_000_000);
        assert_eq!(limits.max_memory_bytes, 256 * 1024 * 1024);
        assert_eq!(
            limits.max_execution,
            Some(tack_ext_wasm::DEFAULT_MAX_EXECUTION)
        );
    }

    /// Values exactly AT the hard caps are not reduced (min with an
    /// equal value is a no-op).
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_values_at_cap_pass_through() {
        let limits = WasmLimitsJson {
            max_fuel: Some(HARD_MAX_FUEL),
            max_memory_bytes: Some(HARD_MAX_MEMORY_BYTES),
            max_execution_ms: None,
        }
        .to_limits();
        assert_eq!(limits.max_fuel, HARD_MAX_FUEL);
        assert_eq!(limits.max_memory_bytes, HARD_MAX_MEMORY_BYTES);
    }

    /// One past the cap is already clamped down to the cap.
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_one_above_cap_is_clamped() {
        let limits = WasmLimitsJson {
            max_fuel: Some(HARD_MAX_FUEL + 1),
            max_memory_bytes: Some(HARD_MAX_MEMORY_BYTES + 1),
            max_execution_ms: None,
        }
        .to_limits();
        assert_eq!(limits.max_fuel, HARD_MAX_FUEL);
        assert_eq!(limits.max_memory_bytes, HARD_MAX_MEMORY_BYTES);
    }

    /// Zero is the tightest possible declaration and passes through
    /// untouched — the host never LOOSENS a declared limit.
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_zero_is_honored() {
        let limits = WasmLimitsJson {
            max_fuel: Some(0),
            max_memory_bytes: Some(0),
            max_execution_ms: Some(0),
        }
        .to_limits();
        assert_eq!(limits.max_fuel, 0);
        assert_eq!(limits.max_memory_bytes, 0);
        assert_eq!(limits.max_execution, Some(std::time::Duration::ZERO));
    }

    /// Each field is clamped independently: an over-cap fuel declaration
    /// must not disturb a tight memory declaration (and vice versa), and
    /// undeclared fields keep their defaults.
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_fields_clamp_independently() {
        let greedy_fuel = WasmLimitsJson {
            max_fuel: Some(u64::MAX),
            max_memory_bytes: Some(1024),
            max_execution_ms: None,
        }
        .to_limits();
        assert_eq!(greedy_fuel.max_fuel, HARD_MAX_FUEL);
        assert_eq!(greedy_fuel.max_memory_bytes, 1024);
        // Undeclared execution limit keeps the engine default watchdog.
        assert_eq!(
            greedy_fuel.max_execution,
            Some(tack_ext_wasm::DEFAULT_MAX_EXECUTION)
        );

        let greedy_memory = WasmLimitsJson {
            max_fuel: Some(1_000),
            max_memory_bytes: Some(usize::MAX),
            max_execution_ms: Some(5_000),
        }
        .to_limits();
        assert_eq!(greedy_memory.max_fuel, 1_000);
        assert_eq!(greedy_memory.max_memory_bytes, HARD_MAX_MEMORY_BYTES);
        assert_eq!(
            greedy_memory.max_execution,
            Some(std::time::Duration::from_millis(5_000))
        );
    }

    /// max_execution_ms has NO host-side hard cap (there is no `.min`
    /// on this field): it converts to a Duration verbatim, and a missing
    /// value leaves the engine default (10-minute hang watchdog).
    #[test]
    #[cfg(feature = "wasm")]
    fn wasm_limits_execution_ms_is_not_capped() {
        let huge = u64::from(u32::MAX) + 1;
        let limits = WasmLimitsJson {
            max_fuel: None,
            max_memory_bytes: None,
            max_execution_ms: Some(huge),
        }
        .to_limits();
        assert_eq!(
            limits.max_execution,
            Some(std::time::Duration::from_millis(huge))
        );
        assert_eq!(
            WasmLimitsJson::default().to_limits().max_execution,
            Some(tack_ext_wasm::DEFAULT_MAX_EXECUTION)
        );
    }

    /// The manifest JSON uses camelCase keys (maxFuel / maxMemoryBytes /
    /// maxExecutionMs); unknown keys are ignored and missing fields keep
    /// their defaults.
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

        let partial: WasmLimitsJson = serde_json::from_str(r#"{"maxFuel": 500}"#).unwrap();
        let limits = partial.to_limits();
        assert_eq!(limits.max_fuel, 500);
        assert_eq!(
            limits.max_memory_bytes,
            tack_ext_wasm::WasmLimits::default().max_memory_bytes
        );
        assert_eq!(
            limits.max_execution,
            Some(tack_ext_wasm::DEFAULT_MAX_EXECUTION)
        );
    }

    #[test]
    #[cfg(feature = "wasm")]
    fn capabilities_deserialize_and_lower() {
        let dir = Path::new("/ext/dir");
        let json: CapabilitiesJson = serde_json::from_str(
            r#"{
                "fs": [
                    {"host": "data", "guest": "/data"},
                    {"host": "/abs/out", "guest": "/out", "access": "read-write"},
                    {"host": "no-guest"}
                ],
                "env": {"A": "1"},
                "args": ["--verbose"],
                "network": {"tcp": true}
            }"#,
        )
        .unwrap();
        let caps = json.into_capabilities("test", dir);
        // The grant without a guest path is skipped (never widened).
        assert_eq!(caps.preopens.len(), 2);
        // Relative host resolves against the extension dir; default ro.
        assert_eq!(caps.preopens[0].host_path, Path::new("/ext/dir/data"));
        assert!(matches!(
            caps.preopens[0].access,
            tack_ext_wasm::PreopenAccess::ReadOnly
        ));
        assert!(matches!(
            caps.preopens[1].access,
            tack_ext_wasm::PreopenAccess::ReadWrite
        ));
        assert_eq!(caps.env, vec![("A".to_string(), "1".to_string())]);
        assert_eq!(caps.args, vec!["--verbose".to_string()]);
        assert!(caps.network.allow_tcp);
        assert!(!caps.network.allow_udp);
        assert!(!caps.network.allow_dns);
    }

    #[test]
    #[cfg(feature = "wasm")]
    fn capabilities_env_passthrough_and_empty_default() {
        // Default (no `capabilities` key) = full sandbox.
        let caps = CapabilitiesJson::default().into_capabilities("test", Path::new("/x"));
        assert!(caps.preopens.is_empty());
        assert!(caps.env.is_empty());
        assert!(caps.args.is_empty());
        assert!(!caps.network.allow_tcp);

        // Array form passes host env through; unset names are skipped.
        // (PATH is universal; the lib target forbids `unsafe` set_var.)
        let json: CapabilitiesJson =
            serde_json::from_str(r#"{"env": ["PATH", "TACK_TEST_CAP_ENV_MISSING"]}"#).unwrap();
        let caps = json.into_capabilities("test", Path::new("/x"));
        assert_eq!(caps.env.len(), 1);
        assert_eq!(caps.env[0].0, "PATH");
    }

    #[test]
    fn sensitive_env_names_are_detected() {
        // Same patterns the tack-ext process carrier strips.
        assert!(is_sensitive_env_key("ANTHROPIC_API_KEY"));
        assert!(is_sensitive_env_key("GITHUB_TOKEN"));
        assert!(is_sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive_env_key("app_password")); // case-insensitive
        assert!(!is_sensitive_env_key("PATH"));
        assert!(!is_sensitive_env_key("TOKENIZER_THREADS")); // no _TOKEN suffix
        assert!(!is_sensitive_env_key("SECRETARY_NAME"));
    }

    /// A manifest `env: ["*_API_KEY", ...]` pass-through must NOT inject
    /// live host secrets into a WASM guest: sensitive names are skipped
    /// with a warning (fail closed on the secret, not the extension).
    #[test]
    #[cfg(feature = "wasm")]
    fn capabilities_env_passthrough_refuses_sensitive_names() {
        let json: CapabilitiesJson =
            serde_json::from_str(r#"{"env": ["ANTHROPIC_API_KEY", "NPM_TOKEN", "PATH"]}"#).unwrap();
        let caps = json.into_capabilities("test", Path::new("/x"));
        assert_eq!(caps.env.len(), 1, "env: {:?}", caps.env);
        assert_eq!(caps.env[0].0, "PATH");
    }

    struct NoopServices;

    #[async_trait::async_trait]
    impl HostServices for NoopServices {
        async fn handle_request(&self, _method: &str, _params: Value) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn handle_event(&self, _event: &str, _payload: Value) {}
    }

    /// e2e: a wasm-carrier extension (hand-written WAT) is discovered,
    /// instantiated in the sandbox, completes the handshake, and answers
    /// tool.execute / command.invoke through the shared PluginPeer.
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

        let mut manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
        assert_eq!(manager.plugins.len(), 1, "wasm plugin must load");
        assert!(matches!(manager.plugins[0].handle, PluginHandle::Wasm(_)));

        // Protocol 2 was negotiated for the wasm carrier — and the plugin
        // registered its tool and command.
        let tools = manager.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "ext__hello-wasm__ping");
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

    /// A process-carrier manifest with bundle resources contributes hooks,
    /// MCP servers and skill dirs even when the plugin itself fails to
    /// start (unknown command).
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

        let mut manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
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
        manager.shutdown().await;
    }

    /// e2e: `capabilities.fs` preopens the extension's data dir into the
    /// guest (readfile answers with the host file's content); without the
    /// grant the same module gets an errno — no ambient authority.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(feature = "wasm")]
    async fn wasm_capability_fs_grant_readfile() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let wat = include_str!("../../../examples/extensions/hello-wasm-caps/plugin.wat");

        // With the grant: the tool reads data/hello.txt through the preopen.
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

        let mut manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
        assert_eq!(manager.plugins.len(), 1, "wasm plugin must load");
        let tools = manager.tools();
        let readfile = tools
            .iter()
            .find(|t| t.name() == "ext__hello-wasm-caps__readfile")
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

        let mut manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
        let tools = manager.tools();
        let readfile = tools
            .iter()
            .find(|t| t.name() == "ext__hello-wasm-caps__readfile")
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

    // ---- install pinning / lockfile / verify / signatures ----

    #[test]
    fn git_source_name_handles_windows_and_url_forms() {
        // URLs and scp-style sources.
        assert_eq!(git_source_name("https://example.com/x.git"), "x");
        assert_eq!(git_source_name("git@example.com:user/repo.git"), "repo");
        assert_eq!(git_source_name("https://example.com/x/y/"), "y");
        // Local paths: POSIX and Windows (the source string is passed
        // through verbatim, so backslashes must split too).
        assert_eq!(git_source_name("/tmp/upstream.git"), "upstream");
        assert_eq!(git_source_name("C:\\Users\\u\\upstream.git"), "upstream");
        assert_eq!(git_source_name("C:\\Users\\u\\repo"), "repo");
        assert!(validate_extension_name(&git_source_name("C:\\Users\\u\\upstream.git")).is_ok());
    }

    #[test]
    fn extension_names_are_validated_before_path_joins() {
        // Plain names pass.
        assert!(validate_extension_name("demo").is_ok());
        assert!(validate_extension_name("my-ext.v2").is_ok());
        // Traversal, separators, hidden/empty names are rejected.
        for bad in [
            "",
            "..",
            ".",
            ".hidden",
            "../escape",
            "a/b",
            "a\\b",
            "up..\\..\\x",
        ] {
            assert!(
                validate_extension_name(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }

        // A marketplace key / name override flows into the install path:
        // a hostile value must error, not escape extensions/.
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        let err = install_extension_named(
            "https://example.com/x.git",
            &cwd,
            &agent_dir,
            false,
            Some("../escape"),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid extension name"), "{err}");

        // remove_extension must never delete outside extensions/ either.
        let err = remove_extension("..", &cwd, &agent_dir, false).unwrap_err();
        assert!(err.to_string().contains("invalid extension name"), "{err}");

        // Marketplace names become marketplaces/<name>.json paths.
        let err = marketplace_plugins(&agent_dir, "../settings").unwrap_err();
        assert!(err.to_string().contains("invalid extension name"), "{err}");
        let err = remove_marketplace("a/b", &agent_dir).unwrap_err();
        assert!(err.to_string().contains("invalid extension name"), "{err}");
    }

    /// A marketplace catalog whose plugin KEY contains a path separator
    /// resolves to an error (the key becomes the install directory name).
    #[tokio::test]
    async fn marketplace_rejects_hostile_plugin_names() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(
            &catalog,
            r#"{"name":"acme","plugins":{"../escape":{"source":"https://example.com/x.git"}}}"#,
        )
        .unwrap();
        add_marketplace("acme", catalog.to_str().unwrap(), &agent_dir, None)
            .await
            .unwrap();
        let err = resolve_marketplace_spec("../escape@acme", &agent_dir).unwrap_err();
        assert!(err.to_string().contains("invalid extension name"), "{err}");
    }

    /// Marketplace registration rejects names that would escape
    /// marketplaces/ (or write hidden files).
    #[tokio::test]
    async fn add_marketplace_validates_name() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(&catalog, r#"{"name":"acme","plugins":{}}"#).unwrap();
        for bad in ["../evil", ".hidden", "a\\b", ""] {
            let err = add_marketplace(bad, catalog.to_str().unwrap(), &agent_dir, None)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("invalid extension name"),
                "{bad:?}: {err}"
            );
        }
        assert!(list_marketplaces(&agent_dir).is_empty());
    }

    fn git(dir: &Path, args: &[&str]) {
        // Tests also commit into fresh clones, which do not inherit the
        // source repo's local identity; CI runners have no global one. The
        // call must stay bounded with piped stdio (output_with_timeout):
        // a bare `.status()` inherits the test's stdout/stderr, so a git
        // child that never exits keeps those pipes open — killing the test
        // then still wedges the runner waiting for pipe EOF (the repeated
        // ubuntu CI stall).
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

    /// A git repo (named *.git so the installer treats it as a git source)
    /// with one commit containing a bundle-only extension manifest.
    fn make_git_extension(repo: &Path) {
        std::fs::create_dir_all(repo.join("skills").join("demo")).unwrap();
        git(repo, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.join("extension.json"),
            r#"{"name":"demo","skills":["skills"]}"#,
        )
        .unwrap();
        std::fs::write(
            repo.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: bundle skill\n---\nbody\n",
        )
        .unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "initial"]);
    }

    /// Installing from a git source keeps .git, writes a lockfile entry with
    /// the resolved HEAD commit, and verifies ok.
    #[test]
    fn git_install_writes_lock_and_verifies_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let target = install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        assert!(
            target.join(".git").exists(),
            "git installs keep .git so verify can compare HEAD"
        );
        let head = git_head_commit(&target).unwrap();
        assert_eq!(head.len(), 40);

        let lock: ExtensionsLock =
            serde_json::from_str(&std::fs::read_to_string(lock_path(&agent_dir)).unwrap()).unwrap();
        assert_eq!(lock.version, 1);
        let entry = lock.plugins.get("upstream").expect("lock entry");
        assert_eq!(entry.source, repo.to_string_lossy());
        assert_eq!(entry.rev, None);
        assert_eq!(entry.resolved_commit.as_deref(), Some(head.as_str()));
        assert_eq!(entry.marketplace, None);
        assert!(entry.installed_at > 0);

        let results = verify_extensions(&agent_dir).unwrap();
        assert_eq!(
            results,
            vec![("upstream".to_string(), VerifyStatus::Ok(head))]
        );
    }

    /// Moving HEAD in the installed checkout makes verify report changed;
    /// removing the extension drops its lock entry.
    #[test]
    fn verify_detects_head_drift_and_remove_clears_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let target = install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        let locked = git_head_commit(&target).unwrap();

        // Tamper: an extra commit inside the installed checkout.
        std::fs::write(target.join("extra.txt"), "tampered").unwrap();
        git(&target, &["add", "."]);
        git(&target, &["commit", "-q", "-m", "tamper"]);

        let results = verify_extensions(&agent_dir).unwrap();
        let [(name, VerifyStatus::Changed { expected, actual })] = results.as_slice() else {
            panic!("expected one changed entry, got {results:?}");
        };
        assert_eq!(name, "upstream");
        assert_eq!(expected, &locked);
        assert_ne!(actual, &locked);

        remove_extension("upstream", &cwd, &agent_dir, false).unwrap();
        assert!(verify_extensions(&agent_dir).unwrap().is_empty());

        // A deleted install directory reports missing.
        let target = install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
        let results = verify_extensions(&agent_dir).unwrap();
        assert_eq!(
            results,
            vec![("upstream".to_string(), VerifyStatus::Missing)]
        );
    }

    /// `#<ref>` pins the checkout to the requested commit; the lock records
    /// both the requested rev and the resolved sha. Local directories reject
    /// ref pins.
    #[test]
    fn rev_pin_checks_out_requested_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let first = git_head_commit(&repo).unwrap();
        std::fs::write(repo.join("v2.txt"), "v2").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "second"]);
        let second = git_head_commit(&repo).unwrap();
        assert_ne!(first, second);

        let (source, rev) = split_source_ref(&format!("{}#{first}", repo.display()));
        assert_eq!(rev.as_deref(), Some(first.as_str()));
        // No ref suffix: source passes through unchanged.
        assert_eq!(
            split_source_ref("https://x/y.git"),
            ("https://x/y.git".to_string(), None)
        );

        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let target =
            install_extension_named(&source, &cwd, &agent_dir, false, None, rev.as_deref(), None)
                .unwrap();
        assert_eq!(git_head_commit(&target).unwrap(), first);

        let lock: ExtensionsLock =
            serde_json::from_str(&std::fs::read_to_string(lock_path(&agent_dir)).unwrap()).unwrap();
        let entry = lock.plugins.get("upstream").unwrap();
        assert_eq!(entry.rev.as_deref(), Some(first.as_str()));
        assert_eq!(entry.resolved_commit.as_deref(), Some(first.as_str()));

        // Local directory installs reject ref pins.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("extension.json"), r#"{"name":"plain"}"#).unwrap();
        let err = install_extension_named(
            plain.to_str().unwrap(),
            &cwd,
            &agent_dir,
            false,
            None,
            Some("v1.0.0"),
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("pinning only applies to git URLs"),
            "{err}"
        );

        // A plain local-directory install locks with resolvedCommit: null.
        install_extension(plain.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        let lock: ExtensionsLock =
            serde_json::from_str(&std::fs::read_to_string(lock_path(&agent_dir)).unwrap()).unwrap();
        let entry = lock.plugins.get("plain").unwrap();
        assert_eq!(entry.resolved_commit, None);
        // Nothing to verify for unpinned entries.
        let results = verify_extensions(&agent_dir).unwrap();
        assert_eq!(
            results.len(),
            1,
            "only the git entry is verifiable: {results:?}"
        );
    }

    /// Signed marketplace: registration pins the ed25519 key (TOFU), resolve
    /// re-verifies, and a tampered catalog is rejected.
    #[tokio::test]
    async fn signed_marketplace_pins_key_and_rejects_tampering() {
        use ed25519_dalek::Signer as _;
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let key_hex = hex_encode(signing.verifying_key().as_bytes());

        // Sign the canonical (unsigned) serialization, then attach the signature.
        let unsigned = serde_json::json!({
            "name": "acme",
            "plugins": {
                "demo": { "source": "https://example.com/demo.git", "rev": "v1.0.0" }
            }
        });
        let canonical = serde_json::to_string(&unsigned).unwrap();
        let signature = signing.sign(canonical.as_bytes());
        let mut signed = unsigned.clone();
        signed["signature"] = serde_json::json!({
            "algorithm": "ed25519",
            "value": hex_encode(&signature.to_bytes()),
        });
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(&catalog, serde_json::to_string_pretty(&signed).unwrap()).unwrap();

        // A signed catalog without --public-key is refused.
        let err = add_marketplace("acme", catalog.to_str().unwrap(), &agent_dir, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--public-key"), "{err}");
        assert!(list_marketplaces(&agent_dir).is_empty());

        // A wrong key is refused too.
        let wrong = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let err = add_marketplace(
            "acme",
            catalog.to_str().unwrap(),
            &agent_dir,
            Some(&hex_encode(wrong.verifying_key().as_bytes())),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("verification failed"), "{err}");

        // Correct key: registered and pinned (TOFU).
        add_marketplace(
            "acme",
            catalog.to_str().unwrap(),
            &agent_dir,
            Some(&key_hex),
        )
        .await
        .unwrap();
        assert_eq!(
            read_pinned_key(&agent_dir, "acme").as_deref(),
            Some(key_hex.as_str())
        );

        // Resolve re-verifies and surfaces the catalog's rev pin.
        let resolved = resolve_marketplace_spec("demo@acme", &agent_dir)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.plugin, "demo");
        assert_eq!(resolved.source, "https://example.com/demo.git");
        assert_eq!(resolved.rev.as_deref(), Some("v1.0.0"));
        assert_eq!(resolved.marketplace, "acme");

        // Re-registration with the pinned key alone (no --public-key) works.
        add_marketplace("acme", catalog.to_str().unwrap(), &agent_dir, None)
            .await
            .unwrap();

        // Tampering with the registered catalog breaks verification.
        let registered = marketplaces_root(&agent_dir).join("acme.json");
        let tampered = std::fs::read_to_string(&registered)
            .unwrap()
            .replace("demo.git", "evil.git");
        std::fs::write(&registered, tampered).unwrap();
        let err = resolve_marketplace_spec("demo@acme", &agent_dir).unwrap_err();
        assert!(err.to_string().contains("verification failed"), "{err}");
    }

    /// Startup gate: a plugin whose HEAD drifted from the lockfile is skipped
    /// when the lock is required, loads (with a warning) when it is not, and
    /// plugins without lock entries are never gated.
    #[tokio::test(flavor = "multi_thread")]
    async fn lock_gate_skips_drifted_plugin_unless_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let target = install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();

        // Undrifted: loads.
        let manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
        assert_eq!(manager.bundle_skill_dirs.len(), 1);

        // Drift the installed checkout.
        std::fs::write(target.join("extra.txt"), "tampered").unwrap();
        git(&target, &["add", "."]);
        git(&target, &["commit", "-q", "-m", "tamper"]);

        let manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
        assert!(
            manager.bundle_skill_dirs.is_empty(),
            "drifted plugin must be skipped when the lock is required"
        );

        let manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), false).await;
        assert_eq!(
            manager.bundle_skill_dirs.len(),
            1,
            "extensionLockRequired=false downgrades to a warning"
        );

        // A manually placed extension (no lock entry) is never gated.
        let manual = agent_dir.join("extensions").join("manual");
        std::fs::create_dir_all(manual.join("skills").join("demo")).unwrap();
        std::fs::write(
            manual.join("extension.json"),
            r#"{"name":"manual","skills":["skills"]}"#,
        )
        .unwrap();
        std::fs::write(
            manual.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: manual skill\n---\nbody\n",
        )
        .unwrap();
        let manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
        assert_eq!(
            manager.bundle_skill_dirs.len(),
            1,
            "only the lock-less manual extension loads"
        );
    }

    /// In-place edits WITHOUT a commit leave HEAD unchanged; the lock
    /// gate and `ext verify` must still catch them via the clean-tree
    /// requirement (the lockfile's stated purpose is detecting
    /// post-install tampering).
    #[tokio::test(flavor = "multi_thread")]
    async fn dirty_worktree_defeats_verify_and_lock_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("upstream.git");
        make_git_extension(&repo);
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let target = install_extension(repo.to_str().unwrap(), &cwd, &agent_dir, false).unwrap();
        let locked = git_head_commit(&target).unwrap();

        // Tamper in place: edit a tracked file, no commit — HEAD still
        // equals the locked commit.
        std::fs::write(target.join("skills").join("demo").join("SKILL.md"), "pwned").unwrap();
        assert_eq!(git_head_commit(&target).unwrap(), locked);

        let results = verify_extensions(&agent_dir).unwrap();
        let [(name, VerifyStatus::Changed { expected, actual })] = results.as_slice() else {
            panic!("expected one changed entry, got {results:?}");
        };
        assert_eq!(name, "upstream");
        assert_eq!(expected, &locked);
        assert!(actual.contains("uncommitted"), "{actual}");

        let manager =
            ExtensionManager::load(&cwd, &agent_dir, "tui", Arc::new(NoopServices), true).await;
        assert!(
            manager.bundle_skill_dirs.is_empty(),
            "dirty-tree plugin must be skipped when the lock is required"
        );

        // Restoring the tree to the locked commit verifies ok again.
        git(&target, &["checkout", "--", "."]);
        let results = verify_extensions(&agent_dir).unwrap();
        assert_eq!(
            results,
            vec![("upstream".to_string(), VerifyStatus::Ok(locked))]
        );
    }

    /// v2.1 registry: widgets register keyed `<plugin>:<id>`; widget.update
    /// is an idempotent full-state replacement (state + visibility, rev
    /// bumped); unknown ids are tolerated (false, no mutation); a dead
    /// plugin's widgets are all removed.
    #[test]
    fn widget_registry_lifecycle() {
        let mut registry = WidgetRegistry::default();
        let spec = |id: &str, priority: i64| tack_ext::WidgetSpec {
            id: id.to_string(),
            kind: tack_ext::WidgetKind::StatusLineSegment,
            priority: Some(priority),
            title: None,
            visible: None,
            initial: Some(serde_json::json!({ "text": "init" })),
        };
        registry.register_plugin("alpha", &[spec("s1", 10), spec("s2", 20)]);
        registry.register_plugin("beta", &[spec("s1", 30)]);
        assert_eq!(registry.entries().len(), 3);
        assert_eq!(registry.entries()[0].key, "alpha:s1");
        assert!(registry.entries()[0].visible);

        // Update: full-state replacement + visibility, scoped by plugin
        // (beta:s1 is a different widget than alpha:s1).
        let update = tack_ext::WidgetUpdatePayload {
            id: "s1".to_string(),
            state: serde_json::json!({ "text": "new" }),
            visible: Some(false),
        };
        assert!(registry.apply_update("alpha", &update));
        let entry = &registry.entries()[0];
        assert_eq!(entry.state, Some(serde_json::json!({ "text": "new" })));
        assert!(!entry.visible);
        assert_eq!(entry.rev, 1);
        assert_eq!(
            registry.entries()[2].state,
            Some(serde_json::json!({ "text": "init" })),
            "beta:s1 must not be touched by alpha's update"
        );

        // Idempotent: applying the same snapshot twice only bumps rev.
        assert!(registry.apply_update("alpha", &update));
        assert_eq!(registry.entries()[0].rev, 2);

        // Unknown widget id / unknown plugin: tolerated, no mutation.
        let unknown = tack_ext::WidgetUpdatePayload {
            id: "nope".to_string(),
            state: serde_json::json!({}),
            visible: None,
        };
        assert!(!registry.apply_update("alpha", &unknown));
        assert!(!registry.apply_update("ghost", &update));
        assert_eq!(registry.entries().len(), 3);

        // Plugin death: only that plugin's widgets vanish.
        let removed = registry.remove_plugin("alpha");
        assert_eq!(
            removed,
            vec!["alpha:s1".to_string(), "alpha:s2".to_string()]
        );
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(registry.entries()[0].key, "beta:s1");
    }

    /// widget.update routing: TaggedServices injects the plugin name and
    /// TuiExtServices forwards the parsed update onto the app event bus.
    #[tokio::test]
    async fn widget_update_event_is_tagged_and_routed() {
        let (tx, rx) = crate::tui::app_event_bus();
        let inner: Arc<dyn HostServices> = Arc::new(TuiExtServices::new(tx, false));
        let tagged = TaggedServices {
            plugin: "alpha".to_string(),
            inner,
        };
        tagged
            .handle_event(
                "widget.update",
                serde_json::json!({ "id": "s1", "state": { "text": "hi" }, "visible": true }),
            )
            .await;
        let Some(crate::tui::AppEvent::ExtWidgetUpdate { plugin, update }) = rx.try_recv() else {
            panic!("expected ExtWidgetUpdate");
        };
        assert_eq!(plugin, "alpha");
        assert_eq!(update.id, "s1");
        assert_eq!(update.visible, Some(true));

        // Bad payloads warn and are dropped (no event, no panic).
        tagged
            .handle_event("widget.update", serde_json::json!({ "state": 1 }))
            .await;
        assert!(rx.try_recv().is_none());
    }

    /// Peer EOF (plugin death) fires the death watcher used to drop widgets.
    #[tokio::test]
    async fn plugin_death_watcher_fires_on_eof() {
        let (plugin_out, host_in) = tokio::io::duplex(4096);
        let (host_out, plugin_in) = tokio::io::duplex(4096);
        let peer = tack_ext::PluginPeer::new(host_in, host_out, Arc::new(NoopServices));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        watch_plugin_death(peer, move || {
            let _ = tx.try_send(());
        });
        // The plugin vanishes (process exit closes both pipe ends).
        drop(plugin_out);
        drop(plugin_in);
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("death watcher must fire on EOF")
            .expect("channel open");
    }

    /// v2.2: autocomplete.provide round-trips over the peer; error
    /// responses (v1 plugins answering "unknown method") and UI-level
    /// timeouts both degrade to no suggestions.
    #[tokio::test(flavor = "multi_thread")]
    async fn autocomplete_provide_contract() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (plugin_out, host_in) = tokio::io::duplex(4096);
        let (host_out, plugin_in) = tokio::io::duplex(4096);
        let peer = tack_ext::PluginPeer::new(host_in, host_out, Arc::new(NoopServices));
        let provider = ExtAutocompleteProvider {
            key: "demo:hash".to_string(),
            plugin: "demo".to_string(),
            spec: tack_ext::AutocompleteProviderSpec {
                id: "good".to_string(),
                trigger: "#".to_string(),
                description: None,
            },
            peer,
        };
        // The plugin: answer autocomplete.provide for provider "good",
        // error on "bad", and never answer "slow" (30s protocol timeout
        // would be far too slow for the UI — the TUI's 300ms cap applies).
        tokio::spawn(async move {
            let mut lines = BufReader::new(plugin_in).lines();
            let mut out = plugin_out;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(tack_ext::Envelope::Request { id, method, params }) =
                    serde_json::from_str::<tack_ext::Envelope>(&line)
                else {
                    continue;
                };
                assert_eq!(method, "autocomplete.provide");
                let provider_id = params
                    .get("providerId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let response = match provider_id.as_str() {
                    "good" => tack_ext::Envelope::result(
                        id,
                        serde_json::json!({ "suggestions": [
                            { "value": "#wasm", "label": "#wasm WASM carrier", "detail": "demo" }
                        ] }),
                    ),
                    "slow" => continue, // never answers
                    _ => tack_ext::Envelope::error(id, "unknown method autocomplete.provide"),
                };
                out.write_all(
                    format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes(),
                )
                .await
                .unwrap();
                out.flush().await.unwrap();
            }
        });

        // Round-trip.
        let suggestions = provider.provide("wa", 2).await;
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "#wasm");
        assert_eq!(suggestions[0].detail.as_deref(), Some("demo"));

        // Error response → no suggestions.
        let bad = ExtAutocompleteProvider {
            spec: tack_ext::AutocompleteProviderSpec {
                id: "bad".to_string(),
                ..provider.spec.clone()
            },
            ..provider.clone()
        };
        assert!(bad.provide("x", 0).await.is_empty());

        // UI-level timeout (the TUI wraps provide in a 300ms cap): a silent
        // plugin degrades to no suggestions instead of stalling the input.
        let slow = ExtAutocompleteProvider {
            spec: tack_ext::AutocompleteProviderSpec {
                id: "slow".to_string(),
                ..provider.spec.clone()
            },
            ..provider.clone()
        };
        let timed =
            tokio::time::timeout(std::time::Duration::from_millis(100), slow.provide("x", 0)).await;
        assert!(
            timed.is_err(),
            "the 30s protocol timeout must not fire first"
        );

        // Unknown provider key on an empty manager → no suggestions.
        let manager = ExtensionManager::default();
        assert!(
            manager
                .autocomplete_provide("ghost:hash", "x", 0)
                .await
                .is_empty()
        );
    }
}
