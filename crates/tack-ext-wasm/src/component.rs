//! WIT component-model carrier for tack plugins (`protocol/wit/tack-plugin.wit`,
//! `tack:plugin@0.3.0`) — the sandboxed *distribution* carrier, next to the
//! WASI-stdio core-module carrier (which stays as the debug carrier).
//!
//! A component plugin exports the `tack:plugin/tools` and/or
//! `tack:plugin/hooks` interfaces with JSON-string payloads (the rpc3 data
//! types — the OpenRPC schema stays the single source of truth; see the WIT
//! file for the rationale) and imports at most `tack:plugin/host` (`log`).
//! The world declares no WASI interfaces, so the sandbox is structural:
//! no fs, no env, no argv, no network — WASI preopen/env/args capability
//! grants are WASI-only and are ignored (with a warning) here.
//!
//! Runtime model: component calls are synchronous host→guest function
//! calls, so the guest lives on a dedicated OS thread that owns the
//! `Store`/`Instance`; the async host side exchanges commands over a
//! channel. Sandbox limits mirror the core-module carrier: per-call fuel
//! budget, per-call wall-clock deadline via engine epoch interruption
//! (a call has no protocol-I/O progress, so the p1 "no-progress watchdog"
//! becomes a plain per-call deadline), and a linear-memory cap via the
//! store `ResourceLimiter`. A trapping call kills the plugin (same
//! dead-peer semantics as the stdio carrier).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tack_ext::PluginConnection;
use tack_ext::rpc3::{
    AutocompleteProvideParams, AutocompleteProvideResult, BeforeToolCallParams,
    CommandInvokeParams, ERR_INTERNAL, ErrorObject, HookCapabilities, InitializeParams,
    InitializeResult, PluginCapabilities, PluginInfo, ToolExecuteParams, ToolOutput, ToolSpec,
    TransformContextParams, TransformContextResult, Verdict, WidgetActionParams,
};
use tack_ext::v3::{PeerError, unsupported_capability};
use tokio::sync::{mpsc, oneshot, watch};
use wasmtime::component::{Component, Instance, Linker, TypedFunc};
use wasmtime::{Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};

use crate::{EpochDemand, WasmCapabilities, WasmCarrier, WasmLimits, WasmSandboxCaps};

/// The WIT package version this carrier implements.
pub const WIT_PACKAGE: &str = "tack:plugin@0.3.0";

/// Export name of the tools interface instance.
const IFACE_TOOLS: &str = "tack:plugin/tools";
/// Export name of the hooks interface instance.
const IFACE_HOOKS: &str = "tack:plugin/hooks";
/// Import name of the host interface instance.
const IFACE_HOST: &str = "tack:plugin/host";

/// Detect a WIT component (binary or text) vs a core module: the binary
/// header's version/layer word differs (`0x0001_000d` for components,
/// `0x0000_0001` for core modules); for text input the first form after
/// whitespace/comments decides (`(component` vs `(module`).
pub fn is_component(wasm: &[u8]) -> bool {
    if wasm.starts_with(b"\0asm") {
        return matches!(wasm.get(4..8), Some([0x0d, 0x00, 0x01, 0x00]));
    }
    let Ok(text) = std::str::from_utf8(wasm) else {
        return false;
    };
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix(";;") {
            rest = match after.find('\n') {
                Some(end) => &after[end + 1..],
                None => "",
            };
            continue;
        }
        if let Some(after) = rest.strip_prefix("(;") {
            // Block comments nest in WAT; a naive close-search is good
            // enough for a detection heuristic (a nested comment simply
            // ends the scan early and the text parses as neither kind).
            match after.find(";)") {
                Some(end) => {
                    rest = &after[end + 2..];
                    continue;
                }
                None => return false,
            }
        }
        break;
    }
    rest.starts_with("(component")
}

/// Per-store state: the memory limiter plus the plugin name (log target).
struct ComponentState {
    limits: StoreLimits,
    plugin: String,
}

/// Which exported function a [`Command::Call`] targets.
#[derive(Clone, Copy, Debug)]
enum ExportFn {
    ToolsExecute,
    HooksBeforeToolCall,
}

/// Host↔guest-thread protocol.
enum Command {
    Call {
        func: ExportFn,
        arg: Option<String>,
        respond: oneshot::Sender<CallOutcome>,
    },
    Shutdown,
}

/// Result of one guest call as the thread reports it.
enum CallOutcome {
    /// Lifted `ok` payload.
    Ok(String),
    /// Lifted `err` payload (a protocol-level plugin failure).
    PluginError(String),
    /// The call trapped (fuel/epoch/memory or a guest fault) — the plugin
    /// is dead from here on (the thread exits after reporting).
    Trapped(String),
}

/// A string-in/string-out guest function (every exported WIT function in
/// `tack:plugin@0.3.0` has this shape).
type StringFunc = TypedFunc<(String,), (Result<String, String>,)>;

/// Typed exports resolved once at startup (the guest may export any
/// subset of the interfaces; a missing interface is a missing capability).
struct Resolved {
    tools_list: Option<TypedFunc<(), (String,)>>,
    tools_execute: Option<StringFunc>,
    hooks_before_tool_call: Option<StringFunc>,
}

impl Resolved {
    fn resolve(instance: &Instance, store: &mut Store<ComponentState>) -> Result<Self, String> {
        let lookup = |instance: &Instance,
                      store: &mut Store<ComponentState>,
                      iface: &str,
                      func: &str|
         -> Option<wasmtime::component::Func> {
            let iface_index = instance.get_export_index(&mut *store, None, iface)?;
            let func_index = instance.get_export_index(&mut *store, Some(&iface_index), func)?;
            instance.get_func(&mut *store, func_index)
        };
        let tools_list = lookup(instance, store, IFACE_TOOLS, "list")
            .map(|f| f.typed(&*store))
            .transpose()
            .map_err(|e| format!("tack:plugin/tools#list has an unexpected type: {e}"))?;
        let tools_execute = lookup(instance, store, IFACE_TOOLS, "execute")
            .map(|f| f.typed(&*store))
            .transpose()
            .map_err(|e| format!("tack:plugin/tools#execute has an unexpected type: {e}"))?;
        let hooks_before_tool_call = lookup(instance, store, IFACE_HOOKS, "before-tool-call")
            .map(|f| f.typed(&*store))
            .transpose()
            .map_err(|e| {
                format!("tack:plugin/hooks#before-tool-call has an unexpected type: {e}")
            })?;
        // WIT interfaces are atomic: exporting an interface means
        // exporting all of its functions.
        if tools_list.is_some() != tools_execute.is_some() {
            return Err(
                "component exports a partial tack:plugin/tools interface (need list + execute)"
                    .to_string(),
            );
        }
        if tools_list.is_none() && hooks_before_tool_call.is_none() {
            return Err(format!(
                "component exports neither {IFACE_TOOLS} nor {IFACE_HOOKS} (WIT {WIT_PACKAGE})"
            ));
        }
        Ok(Resolved {
            tools_list,
            tools_execute,
            hooks_before_tool_call,
        })
    }
}

/// Emit one guest `host.log` line at the matching tracing level.
fn emit_guest_log(plugin: &str, level: &str, message: &str) {
    match level {
        "error" => tracing::error!(target: "tack_ext_wasm::guest", plugin, "{message}"),
        "warn" => tracing::warn!(target: "tack_ext_wasm::guest", plugin, "{message}"),
        "debug" => tracing::debug!(target: "tack_ext_wasm::guest", plugin, "{message}"),
        "trace" => tracing::trace!(target: "tack_ext_wasm::guest", plugin, "{message}"),
        _ => tracing::info!(target: "tack_ext_wasm::guest", plugin, "{message}"),
    }
}

/// Epoch ticks for a wall-clock window (same formula as the p1 carrier).
fn deadline_ticks(max_execution: Option<std::time::Duration>) -> u64 {
    match max_execution {
        Some(duration) => {
            let ticks = duration.as_millis().max(1) / crate::EPOCH_TICK.as_millis().max(1);
            u64::try_from(ticks).unwrap_or(u64::MAX).saturating_add(1)
        }
        // Practically "never" — but a deadline must be set explicitly:
        // with epoch interruption enabled the default deadline is 0.
        None => u64::MAX / 2,
    }
}

/// The guest thread's whole life: instantiate, publish capabilities, then
/// serve calls until shutdown (or a trap ends the plugin).
#[allow(clippy::too_many_arguments)]
fn run_guest_thread(
    engine: wasmtime::Engine,
    component: Component,
    limits: WasmLimits,
    caps: WasmSandboxCaps,
    plugin_name: String,
    epoch_demand: Arc<EpochDemand>,
    init: oneshot::Sender<Result<(Option<String>, bool), String>>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    alive: Arc<AtomicBool>,
    dead: watch::Sender<bool>,
) {
    // The guard the thread MUST drop on every exit path (sets !alive and
    // notifies death-watchers) — structured so early returns can't miss it.
    struct DeathMark {
        alive: Arc<AtomicBool>,
        dead: watch::Sender<bool>,
    }
    impl Drop for DeathMark {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::SeqCst);
            let _ = self.dead.send(true);
        }
    }
    let _mark = DeathMark {
        alive: alive.clone(),
        dead,
    };

    let ticks = deadline_ticks(limits.max_execution);
    let max_fuel = limits.max_fuel;
    let setup = (|| -> Result<(Store<ComponentState>, Instance, Resolved), String> {
        let state = ComponentState {
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
                .tables(caps.max_tables)
                .table_elements(caps.max_table_elements)
                .instances(caps.max_instances)
                .memories(caps.max_memories)
                .build(),
            plugin: plugin_name.clone(),
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(max_fuel)
            .map_err(|e| format!("failed to set fuel: {e}"))?;
        store.set_epoch_deadline(ticks);
        let mut linker = Linker::new(&engine);
        linker
            .instance(IFACE_HOST)
            .and_then(|mut host| {
                host.func_wrap(
                    "log",
                    |store: StoreContextMut<ComponentState>,
                     (level, message): (String, String)|
                     -> wasmtime::Result<()> {
                        emit_guest_log(&store.data().plugin, &level, &message);
                        Ok(())
                    },
                )
            })
            .map_err(|e| format!("failed to link {IFACE_HOST}: {e}"))?;
        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(|e| format!("failed to instantiate plugin component: {e:#}"))?;
        let resolved = Resolved::resolve(&instance, &mut store)?;
        Ok((store, instance, resolved))
    })();
    let (mut store, _instance, resolved) = match setup {
        Ok(ok) => ok,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };

    // One startup tools.list call (its own fuel/deadline window) — the
    // advertised capability list is static per instance, like the v3
    // handshake's declaration. None = the component exports no tools
    // interface at all.
    let tools_json: Option<String> = if let Some(list) = &resolved.tools_list {
        epoch_demand.acquire();
        let _guard = crate::EpochDemandGuard(epoch_demand.clone());
        let _ = store.set_fuel(max_fuel);
        store.set_epoch_deadline(ticks);
        match list.call(&mut store, ()) {
            Ok((json,)) => Some(json),
            Err(e) => {
                let _ = init.send(Err(format!("tools/list trapped: {e:#}")));
                return;
            }
        }
    } else {
        None
    };
    if init
        .send(Ok((tools_json, resolved.hooks_before_tool_call.is_some())))
        .is_err()
    {
        return;
    }

    while let Some(command) = rx.blocking_recv() {
        match command {
            Command::Shutdown => break,
            Command::Call { func, arg, respond } => {
                epoch_demand.acquire();
                let _guard = crate::EpochDemandGuard(epoch_demand.clone());
                let _ = store.set_fuel(max_fuel);
                store.set_epoch_deadline(ticks);
                let outcome = run_call(&mut store, &resolved, func, arg);
                let trapped = matches!(outcome, CallOutcome::Trapped(_));
                let _ = respond.send(outcome);
                if trapped {
                    break;
                }
            }
        }
    }
}

/// Run one guest call inside its fresh fuel/epoch window.
fn run_call(
    store: &mut Store<ComponentState>,
    resolved: &Resolved,
    func: ExportFn,
    arg: Option<String>,
) -> CallOutcome {
    match func {
        ExportFn::ToolsExecute => {
            let Some(execute) = &resolved.tools_execute else {
                return CallOutcome::PluginError(format!(
                    "component exports no {IFACE_TOOLS} interface"
                ));
            };
            let arg = arg.unwrap_or_else(|| "null".to_string());
            match execute.call(&mut *store, (arg,)) {
                Ok((Ok(json),)) => CallOutcome::Ok(json),
                Ok((Err(message),)) => CallOutcome::PluginError(message),
                Err(e) => CallOutcome::Trapped(format!("tools/execute trapped: {e:#}")),
            }
        }
        ExportFn::HooksBeforeToolCall => {
            let Some(hook) = &resolved.hooks_before_tool_call else {
                return CallOutcome::PluginError(format!(
                    "component exports no {IFACE_HOOKS} interface"
                ));
            };
            let arg = arg.unwrap_or_else(|| "null".to_string());
            match hook.call(&mut *store, (arg,)) {
                Ok((Ok(json),)) => CallOutcome::Ok(json),
                Ok((Err(message),)) => CallOutcome::PluginError(message),
                Err(e) => CallOutcome::Trapped(format!("hooks/before-tool-call trapped: {e:#}")),
            }
        }
    }
}

/// A live component plugin: capability snapshot plus the command channel
/// to the guest thread.
pub struct WasmComponentPlugin {
    tx: mpsc::UnboundedSender<Command>,
    alive: Arc<AtomicBool>,
    dead: watch::Receiver<bool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    register: InitializeResult,
    has_tools: bool,
    has_hooks: bool,
}

impl std::fmt::Debug for WasmComponentPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmComponentPlugin")
            .field("alive", &self.alive.load(Ordering::SeqCst))
            .field("has_tools", &self.has_tools)
            .field("has_hooks", &self.has_hooks)
            .finish_non_exhaustive()
    }
}

impl WasmComponentPlugin {
    /// Send one call to the guest thread and await its outcome.
    async fn call_string(&self, func: ExportFn, arg: Option<String>) -> Result<String, PeerError> {
        let (respond, wait) = oneshot::channel();
        self.tx
            .send(Command::Call { func, arg, respond })
            .map_err(|_| PeerError::Dead)?;
        match wait.await {
            Ok(CallOutcome::Ok(json)) => Ok(json),
            Ok(CallOutcome::PluginError(message)) => Err(PeerError::Remote(ErrorObject {
                code: ERR_INTERNAL,
                message,
                data: None,
            })),
            Ok(CallOutcome::Trapped(message)) => Err(PeerError::Remote(ErrorObject {
                code: tack_ext::rpc3::ERR_PLUGIN_UNAVAILABLE,
                message,
                data: None,
            })),
            Err(_) => Err(PeerError::Dead),
        }
    }

    /// Graceful stop: the thread exits its command loop and drops the
    /// store. Idempotent; a wedged thread is leaked with a warning (the
    /// epoch deadline bounds every call, so wedging means a host bug).
    pub async fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
        let Some(thread) = self.thread.lock().expect("thread handle poisoned").take() else {
            return;
        };
        let join = tokio::task::spawn_blocking(move || thread.join());
        if tokio::time::timeout(std::time::Duration::from_secs(2), join)
            .await
            .is_err()
        {
            tracing::warn!("component plugin thread did not exit within 2s of shutdown");
        }
    }
}

#[async_trait::async_trait]
impl PluginConnection for WasmComponentPlugin {
    async fn initialize(&self, _params: &InitializeParams) -> Result<InitializeResult, PeerError> {
        // No handshake on the guest: capabilities were probed at spawn.
        Ok(self.register.clone())
    }

    async fn tool_execute(&self, params: &ToolExecuteParams) -> Result<ToolOutput, PeerError> {
        let payload =
            serde_json::to_string(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        let json = self
            .call_string(ExportFn::ToolsExecute, Some(payload))
            .await?;
        serde_json::from_str(&json)
            .map_err(|e| PeerError::Transport(format!("bad ToolOutput from component: {e}")))
    }

    async fn command_invoke(&self, _params: &CommandInvokeParams) -> Result<Value, PeerError> {
        Err(unsupported_capability("commands/invoke"))
    }

    async fn before_tool_call(&self, params: &BeforeToolCallParams) -> Result<Verdict, PeerError> {
        let payload =
            serde_json::to_string(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        let json = self
            .call_string(ExportFn::HooksBeforeToolCall, Some(payload))
            .await?;
        serde_json::from_str(&json)
            .map_err(|e| PeerError::Transport(format!("bad Verdict from component: {e}")))
    }

    async fn transform_context(
        &self,
        _params: &TransformContextParams,
    ) -> Result<Option<TransformContextResult>, PeerError> {
        Err(unsupported_capability("hooks/transformContext"))
    }

    async fn after_tool_call(
        &self,
        _params: &tack_ext::rpc3::AfterToolCallParams,
    ) -> Result<Option<tack_ext::rpc3::AfterToolCallPatch>, PeerError> {
        Err(unsupported_capability("hooks/afterToolCall"))
    }

    async fn approval_review(
        &self,
        _params: &tack_ext::rpc3::ApprovalReviewParams,
    ) -> Result<Option<tack_ext::rpc3::ApprovalDecision>, PeerError> {
        Err(unsupported_capability("approval/review"))
    }

    async fn autocomplete_provide(
        &self,
        _params: &AutocompleteProvideParams,
    ) -> Result<AutocompleteProvideResult, PeerError> {
        Err(unsupported_capability("autocomplete/provide"))
    }

    async fn lifecycle_event(&self, _event: &str, _payload: Value) -> Result<(), PeerError> {
        // Fire-and-forget notifications fan out to every plugin (the
        // default subscription set); a component plugin has no event
        // surface in 0.3.0, so swallow them silently.
        Ok(())
    }

    async fn widget_action(&self, _params: &WidgetActionParams) -> Result<(), PeerError> {
        Err(unsupported_capability("widgets/action"))
    }

    async fn call_raw(&self, rpc_method: &str, _params: Value) -> Result<Value, PeerError> {
        Err(unsupported_capability(rpc_method))
    }

    async fn notify_raw(&self, rpc_method: &str, _params: Value) -> Result<(), PeerError> {
        Err(unsupported_capability(rpc_method))
    }

    async fn shutdown(&self) -> Result<(), PeerError> {
        WasmComponentPlugin::shutdown(self).await;
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn wait_dead(&self) {
        let mut dead = self.dead.clone();
        while !*dead.borrow() {
            if dead.changed().await.is_err() {
                break;
            }
        }
    }
}

impl WasmCarrier {
    /// Instantiate a WIT component plugin (`tack:plugin@0.3.0`). Detect
    /// components vs core modules with [`is_component`]; WASI-stdio core
    /// modules keep going through [`WasmCarrier::spawn`].
    ///
    /// `capabilities` (preopens/env/args/network) is WASI-only: the
    /// component world imports no WASI interfaces, so non-default grants
    /// are ignored with a warning (never silently widened — the component
    /// gets NOTHING instead of a narrowed view).
    pub async fn spawn_component(
        &self,
        wasm: &[u8],
        limits: &WasmLimits,
        capabilities: &WasmCapabilities,
        plugin_name: String,
        plugin_version: String,
    ) -> Result<WasmComponentPlugin, String> {
        if wasm.len() > self.caps.max_module_bytes {
            return Err(format!(
                "plugin component is {} bytes, exceeding the {} byte cap",
                wasm.len(),
                self.caps.max_module_bytes
            ));
        }
        // The limits come from the plugin's own manifest downstream; a
        // plugin must not be able to weaken its own sandbox, so clamp to
        // the host-side ceilings no matter what was declared.
        let clamped = limits.clamped();
        if clamped != *limits {
            tracing::warn!(
                declared = ?limits,
                clamped = ?clamped,
                "plugin declared limits above the host ceilings; clamped"
            );
        }
        if *capabilities != WasmCapabilities::default() {
            tracing::warn!(
                plugin = %plugin_name,
                "WASM capability grants (fs/env/args/network) are WASI-stdio-carrier only; \
                 component plugins are capability-free by construction — grants ignored"
            );
        }
        if !is_component(wasm) {
            return Err(
                "not a WIT component (core modules go through the WASI-stdio carrier)".to_string(),
            );
        }
        let component = Component::new(&self.engine, wasm)
            .map_err(|e| format!("failed to compile plugin component: {e:#}"))?;

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (init_tx, init_rx) = oneshot::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let (dead_tx, dead_rx) = watch::channel(false);
        let thread = {
            let engine = self.engine.clone();
            let caps = self.caps;
            let epoch_demand = self.epoch_demand.clone();
            let alive = alive.clone();
            let limits = clamped;
            let plugin = plugin_name.clone();
            std::thread::Builder::new()
                .name(format!("tack-plugin-{plugin_name}"))
                .spawn(move || {
                    run_guest_thread(
                        engine,
                        component,
                        limits,
                        caps,
                        plugin,
                        epoch_demand,
                        init_tx,
                        cmd_rx,
                        alive,
                        dead_tx,
                    )
                })
                .map_err(|e| format!("failed to spawn plugin thread: {e}"))?
        };

        let (tools_json, has_hooks) = match init_rx.await {
            Ok(result) => result?,
            Err(_) => {
                let _ = thread.join();
                return Err("plugin thread died during startup".to_string());
            }
        };
        let specs: Option<Vec<ToolSpec>> = tools_json
            .map(|json| {
                serde_json::from_str(&json).map_err(|e| {
                    format!("component returned an invalid ToolSpec list from tools/list: {e}")
                })
            })
            .transpose()?;
        let has_tools = specs.is_some();
        let register = InitializeResult {
            // The component speaks no tack-RPC: report the host's own
            // version so the handshake check is a no-op (the interface
            // contract is versioned separately — WIT_PACKAGE).
            protocol_version: tack_ext::v3::PROTOCOL_VERSION.to_string(),
            plugin: PluginInfo {
                name: plugin_name,
                version: Some(plugin_version),
                description: None,
            },
            capabilities: PluginCapabilities {
                tools: specs,
                hooks: has_hooks.then_some(HookCapabilities {
                    before_tool_call: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
        };
        Ok(WasmComponentPlugin {
            tx: cmd_tx,
            alive,
            dead: dead_rx,
            thread: Mutex::new(Some(thread)),
            register,
            has_tools,
            has_hooks,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_ext::rpc3::{ERR_CAPABILITY_NOT_GRANTED, ToolExecuteParams, VerdictAction};

    /// The checked-in example doubles as the carrier's fixture.
    const HELLO_COMPONENT: &str =
        include_str!("../../../examples/extensions/hello-component/plugin.wat");

    #[test]
    fn detects_components_vs_core_modules() {
        assert!(is_component(HELLO_COMPONENT.as_bytes()));
        assert!(is_component(b"  ;; comment\n (; block ;) (component)"));
        assert!(is_component(b"\0asm\x0d\x00\x01\x00rest"));
        assert!(!is_component(b"(module)"));
        assert!(!is_component(b"\0asm\x01\x00\x00\x00rest"));
        assert!(!is_component(b"(; unterminated"));
        assert!(!is_component(b"garbage"));
    }

    async fn spawn_hello() -> WasmComponentPlugin {
        let carrier = WasmCarrier::new().unwrap();
        carrier
            .spawn_component(
                HELLO_COMPONENT.as_bytes(),
                &WasmLimits::default(),
                &WasmCapabilities::default(),
                "hello-component".to_string(),
                "local".to_string(),
            )
            .await
            .unwrap()
    }

    fn init_params() -> InitializeParams {
        serde_json::from_value(serde_json::json!({
            "protocolVersion": tack_ext::v3::PROTOCOL_VERSION,
            "host": {"name": "tack", "version": "0.0.0"},
            "mode": "print",
            "cwd": "/tmp",
            "trusted": false,
            "capabilities": {}
        }))
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn component_handshake_lists_tools_and_hooks() {
        let plugin = spawn_hello().await;
        assert!(plugin.is_alive());
        let register = plugin.initialize(&init_params()).await.unwrap();
        assert_eq!(register.plugin.name, "hello-component");
        let tools = register.capabilities.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "hello");
        assert!(tools[0].parameters.is_object());
        assert_eq!(
            register.capabilities.hooks.and_then(|h| h.before_tool_call),
            Some(true)
        );
        plugin.shutdown().await;
        assert!(!plugin.is_alive());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn component_tools_execute_and_hooks_roundtrip() {
        let plugin = spawn_hello().await;
        let output = plugin
            .tool_execute(&ToolExecuteParams {
                name: "hello".to_string(),
                tool_call_id: "call-1".to_string(),
                arguments: serde_json::json!({"name": "tack"}),
            })
            .await
            .unwrap();
        assert_eq!(output.is_error, None);
        assert_eq!(
            output.content[0].text.as_deref(),
            Some("hello from the component carrier")
        );

        let verdict = plugin
            .before_tool_call(
                &serde_json::from_value::<BeforeToolCallParams>(serde_json::json!({"toolCall": {
                    "toolName": "bash",
                    "toolCallId": "call-1",
                    "arguments": {}
                }}))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(verdict.action, VerdictAction::Allow);
        plugin.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsupported_namespaces_are_capability_errors() {
        let plugin = spawn_hello().await;
        let err = plugin
            .command_invoke(&tack_ext::rpc3::CommandInvokeParams {
                name: "x".to_string(),
                args: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), ERR_CAPABILITY_NOT_GRANTED);
        plugin.shutdown().await;
    }

    /// A guest that never returns must be stopped by its per-call budget:
    /// fuel exhaustion traps the call and kills the plugin.
    #[tokio::test(flavor = "multi_thread")]
    async fn fuel_exhaustion_traps_and_kills() {
        let spinner = r#"
(component
  (core module $m
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
    (func (export "execute") (param i32 i32) (result i32)
      (loop $forever (br $forever))
      (i32.const 0))
  )
  (core instance $i (instantiate $m))
  (func $execute
    (param "call" string)
    (result (result string (error string)))
    (canon lift (core func $i "execute") (memory $i "memory") (realloc (func $i "realloc"))))
  (instance $tools (export "execute" (func $execute)))
  (export "tack:plugin/tools" (instance $tools))
)
"#;
        // tools instance without list: rejected as a partial interface.
        let carrier = WasmCarrier::new().unwrap();
        let err = carrier
            .spawn_component(
                spinner.as_bytes(),
                &WasmLimits::default(),
                &WasmCapabilities::default(),
                "spinner".to_string(),
                "local".to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("partial"), "{err}");
    }

    /// The spin fixture with a complete tools interface: list answers,
    /// execute spins and dies on fuel.
    #[tokio::test(flavor = "multi_thread")]
    async fn fuel_exhaustion_on_execute_kills_the_plugin() {
        let spinner = r#"
(component
  (core module $m
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 2048))
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr (global.get $heap))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new_size)))
      (local.get $ptr))
    (func (export "list") (result i32)
      (i32.store (i32.const 1024) (i32.const 2048))
      (i32.store (i32.const 1028) (i32.const 66))
      (i32.const 1024))
    (func (export "execute") (param i32 i32) (result i32)
      (loop $forever (br $forever))
      (i32.const 0))
    (data (i32.const 2048) "[{\"name\":\"spin\",\"description\":\"s\",\"parameters\":{\"type\":\"object\"}}]")
  )
  (core instance $i (instantiate $m))
  (func $list
    (result string)
    (canon lift (core func $i "list") (memory $i "memory") (realloc (func $i "realloc"))))
  (func $execute
    (param "call" string)
    (result (result string (error string)))
    (canon lift (core func $i "execute") (memory $i "memory") (realloc (func $i "realloc"))))
  (instance $tools
    (export "list" (func $list))
    (export "execute" (func $execute)))
  (export "tack:plugin/tools" (instance $tools))
)
"#;
        let carrier = WasmCarrier::new().unwrap();
        let plugin = carrier
            .spawn_component(
                spinner.as_bytes(),
                &WasmLimits {
                    max_fuel: 100_000,
                    ..WasmLimits::default()
                },
                &WasmCapabilities::default(),
                "spinner".to_string(),
                "local".to_string(),
            )
            .await
            .unwrap();
        let err = plugin
            .tool_execute(&ToolExecuteParams {
                name: "spin".to_string(),
                tool_call_id: "call-1".to_string(),
                arguments: serde_json::Value::Null,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, PeerError::Remote(_)),
            "trap surfaces as a remote error: {err:?}"
        );
        // The trap killed the plugin; death-watchers observe it.
        tokio::time::timeout(std::time::Duration::from_secs(5), plugin.wait_dead())
            .await
            .expect("wait_dead hangs after a trap");
        assert!(!plugin.is_alive());
    }
}
