//! No-op `extension_host` replacement for builds without the `ext` feature
//! (aliased as `crate::extension_host` from lib.rs). The API mirrors the
//! real module so call sites compile unchanged; every method is an empty
//! no-op because no plugin can ever be loaded. Types that only exist in the
//! real implementation (`LoadedPlugin`, `WidgetRegistry`, marketplace/lock
//! helpers, …) are absent — their call sites are `#[cfg(feature = "ext")]`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tack_agent_core::{AgentHooks, AgentTool};
use tack_ai::provider::{Provider, StreamOptions};
use tack_ai::stream::AssistantMessageEventStream;
use tack_ai::types::{Context, Model};
use tokio::sync::oneshot;

/// A plugin→host UI/exec request awaiting resolution on the TUI main loop.
/// Never constructed without extensions; kept so `AppEvent::ExtUiRequest`
/// compiles unchanged.
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

/// PeerHandler bridge (real impl routes plugin UI calls to the TUI main
/// loop). Without extensions no plugin exists to call it.
#[derive(Debug)]
pub struct TuiExtServices {
    _private: (),
}

impl TuiExtServices {
    pub(crate) fn new(
        _tx: crate::tui::AppEventTx,
        _trusted: bool,
        _bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
    ) -> Self {
        TuiExtServices { _private: () }
    }
}

/// One registered widget. Never constructed without extensions; the type
/// exists so `ExtensionManager::widgets()` has a return type.
#[derive(Clone, Debug)]
pub struct WidgetEntry {
    _private: (),
}

impl WidgetEntry {
    /// Unreachable: no widget can exist without the ext feature.
    pub fn to_protocol(&self) -> tack_protocol::schemas::ExtWidgetState {
        unreachable!("WidgetEntry cannot be constructed without the ext feature")
    }
}

/// Off-lock widget action route (real impl holds the owning plugin's
/// connection). Never constructed without extensions.
#[derive(Debug)]
pub struct WidgetActionRoute {
    _private: (),
}

impl WidgetActionRoute {
    /// Unreachable: `widget_action_route` always returns `None` here.
    pub async fn notify(self, _action: String, _item_id: Option<String>) {
        unreachable!("WidgetActionRoute cannot be constructed without the ext feature")
    }
}

/// A registered autocomplete provider. Never constructed without
/// extensions; `App::ext_ac_providers` is always empty.
#[derive(Clone, Debug)]
pub struct ExtAutocompleteProvider {
    _private: (),
}

impl ExtAutocompleteProvider {
    /// Unreachable: no provider can exist without the ext feature.
    pub fn key(&self) -> &str {
        unreachable!("ExtAutocompleteProvider cannot be constructed without the ext feature")
    }

    /// Unreachable: no provider can exist without the ext feature.
    pub fn to_protocol(&self) -> tack_protocol::schemas::ExtAutocompleteProviderInfo {
        unreachable!("ExtAutocompleteProvider cannot be constructed without the ext feature")
    }

    /// Unreachable: no provider can exist without the ext feature.
    pub async fn provide_protocol(
        &self,
        _query: &str,
        _cursor_offset: usize,
    ) -> Vec<tack_protocol::schemas::ExtAutocompleteSuggestion> {
        unreachable!("ExtAutocompleteProvider cannot be constructed without the ext feature")
    }
}

/// Event sink handed to the provider-events wrapper. Accepts events and
/// drops them — no plugins are listening.
#[derive(Debug, Default)]
pub struct ExtSinkHandle {
    _private: (),
}

/// Pass-through provider: the real `ExtNotifyProvider` fans lifecycle
/// events out to plugins; with no plugins there is nothing to notify.
#[derive(Debug)]
pub struct ExtNotifyProvider {
    inner: Arc<dyn Provider>,
}

impl ExtNotifyProvider {
    pub fn new(inner: Arc<dyn Provider>, _sink: Arc<ExtSinkHandle>) -> Self {
        ExtNotifyProvider { inner }
    }
}

impl Provider for ExtNotifyProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream(model, context, options)
    }
}

/// Extension manager with no extension support: `load` discovers nothing
/// and every query returns empty.
#[derive(Default)]
pub struct ExtensionManager {
    /// Bundle contributions from installed extensions (merged by callers).
    pub bundle_hooks: crate::shell_hooks::HookConfig,
    pub bundle_mcp_servers: Vec<tack_tools::mcp::McpServerSpec>,
    pub bundle_skill_dirs: Vec<PathBuf>,
}

/// Owned off-loop command invoker (mirrors the real module). Never
/// constructed without extensions — `command_invoker` always returns
/// `None` — so there is nothing to invoke.
#[derive(Debug)]
pub struct CommandInvoker;

impl CommandInvoker {
    /// Unreachable: no plugin can exist to resolve a command to.
    pub async fn invoke(self, _args: String) -> Result<Value, String> {
        unreachable!("CommandInvoker cannot be constructed without the ext feature")
    }
}

impl std::fmt::Debug for ExtensionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionManager")
            .field("plugins", &0)
            .finish()
    }
}

impl ExtensionManager {
    /// Discover and start all extensions — a no-op without the `ext`
    /// feature (mirrors the real signature modulo the services trait
    /// object, which both host service types coerce to).
    pub async fn load(
        _cwd: &Path,
        _agent_dir: &Path,
        _mode: &str,
        _services: Arc<dyn Send + Sync + 'static>,
        _lock_required: bool,
        _mcp_callbacks: tack_tools::mcp::McpClientCallbacks,
        _bridge_state: Arc<crate::ext_provider_bridge::ProviderBridgeState>,
    ) -> Self {
        ExtensionManager::default()
    }

    /// All plugin tools for the agent loop.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        Vec::new()
    }

    /// All plugin tools, wiring the untrusted-content defense for
    /// MCP-carrier plugins (no plugins without the `ext` feature).
    pub fn tools_with_untrusted(
        &self,
        _untrusted: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Vec<Arc<dyn AgentTool>> {
        Vec::new()
    }

    /// Per-plugin hook bridges (tool_call interception).
    pub fn hooks(&self) -> Vec<Arc<dyn AgentHooks>> {
        Vec::new()
    }

    /// No plugins are ever loaded without the `ext` feature.
    pub fn is_empty(&self) -> bool {
        true
    }

    /// The plugin approval chain: always empty without the `ext` feature.
    pub fn approval_chain(&self) -> crate::approval::ApprovalChain {
        crate::approval::ApprovalChain::empty()
    }

    /// Fan an event out to subscribed plugins (fire-and-forget).
    pub async fn notify(&self, _event: &str, _payload: Value) {}

    /// Registered extension slash-command names.
    pub fn command_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Extension slash commands with descriptions: always empty.
    pub fn command_specs(&self) -> Vec<tack_protocol::schemas::ExtCommandSpec> {
        Vec::new()
    }

    /// Declarative widgets: always empty.
    pub fn widgets(&self) -> &[WidgetEntry] {
        &[]
    }

    /// Registered autocomplete providers: always empty.
    pub fn autocomplete_providers(&self) -> Vec<ExtAutocompleteProvider> {
        Vec::new()
    }

    /// All registered shortcuts as (action, plugin index).
    pub fn shortcut_actions(&self) -> Vec<(String, usize)> {
        Vec::new()
    }

    /// Notify one plugin that its shortcut fired.
    pub async fn notify_shortcut(&self, _plugin_index: usize, _action: &str) {}

    /// A shareable event sink for the provider-events wrapper.
    pub fn clone_sink(&self) -> Arc<ExtSinkHandle> {
        Arc::new(ExtSinkHandle::default())
    }

    /// Invoke an extension command (tests/headless; TUI uses
    /// `command_invoker`). Always errors: no extensions exist.
    pub async fn invoke_command(&self, name: &str, _args: &str) -> Result<Value, String> {
        Err(format!("unknown extension command {name:?}"))
    }

    /// No extensions, so no command ever resolves to an invoker.
    pub fn command_invoker(&self, _name: &str) -> Option<CommandInvoker> {
        None
    }

    /// Remote widget update application: always `None` (unknown widget)
    /// without extensions.
    pub fn apply_widget_update_remote(
        &mut self,
        _plugin: &str,
        _id: &str,
        _state: Value,
        _visible: Option<bool>,
    ) -> Option<tack_protocol::schemas::ExtWidgetState> {
        None
    }

    /// Widget action routing: always `None` (unknown key) without
    /// extensions.
    pub fn widget_action_route(&self, _key: &str) -> Option<WidgetActionRoute> {
        None
    }

    /// A dead plugin's widgets vanish: nothing to remove without
    /// extensions.
    pub fn remove_plugin_widgets(&mut self, _plugin: &str) -> Vec<String> {
        Vec::new()
    }

    /// Gracefully stop all plugins.
    pub async fn shutdown(&mut self) {}
}
