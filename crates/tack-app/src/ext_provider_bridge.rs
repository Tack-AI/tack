//! Provider bridge host machinery (P7): plugin-served inference providers.
//!
//! A plugin declaring `capabilities.provider.stream` may register a
//! provider with `bridge: true` (`host/registerProvider`); the host then
//! serves inference for its models straight from the plugin connection —
//! no HTTP hop. This module owns:
//!
//! - [`crate::ext_provider_bridge::ProviderBridgeState`] — the
//!   per-session shared state (plugin connections, in-flight stream
//!   sinks, provider registrations) threaded through the host services
//!   (TUI and headless alike) and populated by the extension load loop.
//! - [`crate::ext_provider_bridge::ExtProviderBridge`] — the tack-ai
//!   [`tack_ai::provider_bridge::ProviderStreamBridge`] impl over one
//!   plugin connection (trait erasure keeps tack-ai free of the plugin
//!   protocol, the same pattern as the approval chain).
//! - [`crate::ext_provider_bridge::handle_register_provider`] — the
//!   shared `host/registerProvider` handler (P7a made it available in
//!   every run mode; the `bridge` flag selects plain HTTP-shim
//!   registration vs bridge registration).
//!
//! Streaming model: `provider/stream` is a fast ack; the turn's events ride
//! plugin→host `provider/streamEvent` notifications demuxed by `streamId`.
//! Terminal semantics are the `Provider` contract's (`done`/`error`,
//! in-band) — every failure path (ack failure, cancel grace, carrier death,
//! protocol violation) synthesizes a terminal in-band `Error`, so a misbe-
//! having plugin degrades only its own provider.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tack_ai::provider_bridge::{BridgeStreamParams, ProviderStreamBridge};
use tack_ai::providers::RuntimeProviderSpec;
use tack_ai::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use tack_ai::types::{AssistantMessage, Model, StopReason};
use tack_ext::rpc3::{
    self, ErrorObject, ProviderEventParams, ProviderStreamEventParams, ProviderStreamParams,
};
use tack_ext::v3::PluginConnection;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Structured audit target for provider bridge events, pinned at INFO in
/// the managed audit-sink EnvFilter alongside `plugin_policy`,
/// `plugin_approval`, `plugin_metrics`, and `plugin_load`.
pub const AUDIT_TARGET: &str = "plugin_provider";

/// Grace period after `provider/streamCancel` for the plugin's own terminal
/// event before the host synthesizes one (design doc §4.2).
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Bound on waiting for a plugin's handshake capabilities during a bridge
/// registration (a plugin can only register after answering initialize; the
/// load loop publishes capabilities as soon as it processes the answer).
const CAPS_WAIT: Duration = Duration::from_secs(5);

/// Death-watch poll cadence (the MCP carrier's `wait_dead` polls at 250ms;
/// same order, no core-protocol churn).
const DEATH_POLL: Duration = Duration::from_millis(200);

fn service_error(code: i64, message: impl Into<String>) -> ErrorObject {
    ErrorObject {
        code,
        message: message.into(),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// One in-flight bridged stream.
struct StreamEntry {
    sink: AssistantMessageEventSender,
    /// The model the stream runs on (for synthesized terminal events).
    model: Model,
    /// Signalled (then dropped) when the stream terminates — terminal
    /// routed or synthesized; watchers select on it to stop watching.
    closed_tx: watch::Sender<bool>,
    /// The serving connection: guards death cleanup against connection
    /// reuse (a re-registered plugin's streams are never failed by the old
    /// connection's watcher).
    conn: Arc<dyn PluginConnection>,
}

impl std::fmt::Debug for StreamEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamEntry")
            .field("model", &self.model.id)
            .finish()
    }
}

impl StreamEntry {
    fn finish(self, event: AssistantMessageEvent) {
        let _ = self.closed_tx.send(true);
        self.sink.finish(event);
    }

    /// End the stream with a synthesized terminal in-band error.
    fn synthesize(self, reason: StopReason, message: String) {
        let mut error = AssistantMessage::pending(&self.model);
        error.stop_reason = reason;
        error.error_message = Some(message);
        self.finish(AssistantMessageEvent::Error { reason, error });
    }
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// One plugin connection known to the bridge, plus its provider-stream
/// capability state (unknown until the load loop publishes the handshake
/// result).
#[derive(Clone, Debug)]
pub struct BridgeConnection {
    pub conn: Arc<dyn PluginConnection>,
    /// The carrier can serve `provider/stream` at all (process and
    /// WASI-stdio WASM; the WIT component and MCP carriers structurally
    /// cannot — carrier matrix, design doc §4.4).
    pub serves_provider_stream: bool,
    caps: watch::Sender<Option<bool>>,
    /// Keeps the channel open: a `watch::Sender::send` with zero live
    /// receivers fails silently, so the entry pins one.
    #[allow(dead_code)]
    caps_keepalive: Arc<watch::Receiver<Option<bool>>>,
}

impl BridgeConnection {
    /// Resolve with the declared `capabilities.provider.stream` once the
    /// load loop publishes the handshake result; `None` on timeout or when
    /// the connection entry went away first.
    pub async fn wait_provider_stream_granted(&self) -> Option<bool> {
        let mut rx = self.caps.subscribe();
        match tokio::time::timeout(CAPS_WAIT, rx.wait_for(|v| v.is_some())).await {
            Ok(Ok(granted)) => *granted,
            _ => None,
        }
    }
}

/// A provider-id registration owned by one bridge instance.
#[derive(Debug)]
struct BridgeRegistration {
    instance: u64,
    conn: Arc<dyn PluginConnection>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The per-session provider bridge state, shared by the host services and
/// the extension load loop. Constructed per run mode alongside the
/// `ExtensionManager`; tack-ai's registries stay process-global (same model
/// as the runtime provider registry).
///
/// The lock is a plain `std::sync::Mutex`: every critical section is a
/// short map operation that never awaits, and callers (peer handler tasks)
/// are async — `tokio::sync::Mutex::blocking_lock` would panic there.
#[derive(Debug, Default)]
pub struct ProviderBridgeState {
    inner: std::sync::Mutex<BridgeInner>,
}

#[derive(Debug, Default)]
struct BridgeInner {
    /// plugin id -> connection entry (populated by the load loop).
    connections: HashMap<String, BridgeConnection>,
    /// (plugin id, stream id) -> in-flight stream.
    streams: HashMap<(String, String), StreamEntry>,
    /// provider id -> owning registration.
    registrations: HashMap<String, BridgeRegistration>,
    /// plugin id -> watched connection (death watchers spawn once per conn).
    watched: HashMap<String, Arc<dyn PluginConnection>>,
}

impl ProviderBridgeState {
    pub fn shared() -> Arc<Self> {
        Arc::new(ProviderBridgeState::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BridgeInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- load-loop surface --------------------------------------------------

    /// Publish a freshly-spawned plugin connection (before initialize; the
    /// capability state stays unknown until the handshake lands).
    pub fn register_connection(
        &self,
        plugin: &str,
        conn: Arc<dyn PluginConnection>,
        serves_provider_stream: bool,
    ) {
        let (caps, caps_keepalive) = watch::channel(None);
        self.lock().connections.insert(
            plugin.to_string(),
            BridgeConnection {
                conn,
                serves_provider_stream,
                caps,
                caps_keepalive: Arc::new(caps_keepalive),
            },
        );
    }

    /// Publish the plugin's declared `capabilities.provider.stream` (called
    /// by the load loop once the handshake result is known, granted or
    /// not). `false` is also the answer for handshake failures and
    /// policy-blocked plugins.
    pub fn set_provider_stream_granted(&self, plugin: &str, granted: bool) {
        if let Some(entry) = self.lock().connections.get(plugin) {
            let _ = entry.caps.send(Some(granted));
        }
    }

    /// Drop a plugin's connection entry (handshake failure, carrier gone).
    /// Pending capability waiters resolve `None`.
    pub fn remove_connection(&self, plugin: &str) {
        self.lock().connections.remove(plugin);
    }

    // -- host-services surface ----------------------------------------------

    /// The connection entry for a plugin, if the load loop published one.
    pub fn connection(&self, plugin: &str) -> Option<BridgeConnection> {
        self.lock().connections.get(plugin).cloned()
    }

    fn insert_stream(
        &self,
        plugin: &str,
        stream_id: &str,
        model: Model,
        conn: Arc<dyn PluginConnection>,
        sink: AssistantMessageEventSender,
    ) -> watch::Receiver<bool> {
        let (closed_tx, closed_rx) = watch::channel(false);
        self.lock().streams.insert(
            (plugin.to_string(), stream_id.to_string()),
            StreamEntry {
                sink,
                model,
                closed_tx,
                conn,
            },
        );
        closed_rx
    }

    fn take_stream(&self, plugin: &str, stream_id: &str) -> Option<StreamEntry> {
        self.lock()
            .streams
            .remove(&(plugin.to_string(), stream_id.to_string()))
    }

    /// End a stream with a synthesized terminal in-band error (cancel
    /// grace, ack failure, malformed event, carrier death). No-op when the
    /// stream already terminated — exactly one terminal, whoever wins.
    pub fn synthesize_terminal(
        &self,
        plugin: &str,
        stream_id: &str,
        reason: StopReason,
        message: impl Into<String>,
    ) {
        let message = message.into();
        if let Some(entry) = self.take_stream(plugin, stream_id) {
            tracing::info!(
                target: AUDIT_TARGET,
                plugin,
                provider = entry.model.provider.as_str(),
                stream_id,
                reason = ?reason,
                message = message.as_str(),
                "provider stream terminated by the host"
            );
            entry.synthesize(reason, message);
        }
    }

    /// Route a plugin's `provider/streamEvent` notification to its stream
    /// sink. Fail-open: protocol violations are contained per-stream,
    /// audited, and synthesized into an in-band `Error` while the stream is
    /// still open.
    ///
    /// Ordering note: the v3 peer dispatches each notification to its own
    /// task, so events may be processed out of wire order under scheduler
    /// pressure. Correctness does not depend on order — every event
    /// carries the full accumulated partial and the terminal carries the
    /// final message — but a delta that overtakes its terminal lands in
    /// the unknown/terminated branch below (audited, dropped) — the same
    /// "dropped frames are harmless" posture as widget updates.
    pub fn route_stream_event(&self, plugin: &str, payload: Value) {
        let params: ProviderStreamEventParams = match serde_json::from_value(payload) {
            Ok(params) => params,
            Err(e) => {
                tracing::warn!(
                    target: AUDIT_TARGET,
                    plugin,
                    error = %e,
                    "malformed provider/streamEvent envelope"
                );
                return;
            }
        };
        let stream_id = params.stream_id.as_str();
        let event = match serde_json::from_value::<AssistantMessageEvent>(params.event) {
            Ok(event) => event,
            Err(e) => {
                tracing::warn!(
                    target: AUDIT_TARGET,
                    plugin,
                    stream_id,
                    error = %e,
                    "malformed provider stream event"
                );
                self.synthesize_terminal(
                    plugin,
                    stream_id,
                    StopReason::Error,
                    format!("plugin sent a malformed stream event: {e}"),
                );
                return;
            }
        };
        if event.is_terminal() {
            let Some(entry) = self.take_stream(plugin, stream_id) else {
                tracing::warn!(
                    target: AUDIT_TARGET,
                    plugin,
                    stream_id,
                    "stream event for an unknown or already-terminated stream"
                );
                return;
            };
            let terminal = match &event {
                AssistantMessageEvent::Done { .. } => "done",
                _ => "error",
            };
            tracing::info!(
                target: AUDIT_TARGET,
                plugin,
                provider = entry.model.provider.as_str(),
                stream_id,
                terminal,
                "provider stream ended"
            );
            entry.finish(event);
            return;
        }
        let pushed = self
            .lock()
            .streams
            .get(&(plugin.to_string(), stream_id.to_string()))
            .map(|entry| entry.sink.push(event));
        match pushed {
            Some(true) => {}
            Some(false) => {
                // Consumer gone (the agent loop dropped the stream): drop
                // the sink and ask the plugin to stop wasting work.
                self.take_stream(plugin, stream_id);
                if let Some(entry) = self.connection(plugin) {
                    let stream_id = stream_id.to_string();
                    tokio::spawn(async move {
                        let _ = entry.conn.provider_stream_cancel(&stream_id).await;
                    });
                }
            }
            None => {
                tracing::warn!(
                    target: AUDIT_TARGET,
                    plugin,
                    stream_id,
                    "stream event for an unknown or already-terminated stream"
                );
            }
        }
    }

    /// Route a plugin's `provider/event` notification (P7c) onto the
    /// provider-event channel (TUI inline warning + desktop notification;
    /// headless log — identical to the native path).
    pub fn route_provider_event(&self, plugin: &str, payload: Value) {
        let params: ProviderEventParams = match serde_json::from_value(payload) {
            Ok(params) => params,
            Err(e) => {
                tracing::warn!(
                    target: AUDIT_TARGET,
                    plugin,
                    error = %e,
                    "malformed provider/event envelope"
                );
                return;
            }
        };
        let kind = match params.kind {
            rpc3::ProviderEventKind::RateLimited => tack_ai::ProviderEventKind::RateLimited,
            rpc3::ProviderEventKind::Warning => tack_ai::ProviderEventKind::Warning,
            rpc3::ProviderEventKind::Info => tack_ai::ProviderEventKind::Info,
        };
        tracing::info!(
            target: AUDIT_TARGET,
            plugin,
            provider = params.provider.as_str(),
            kind = ?params.kind,
            "provider event"
        );
        tack_ai::emit_provider_event(tack_ai::ProviderEvent {
            kind,
            provider: params.provider,
            message: params.message,
        });
    }

    // -- registration + death -------------------------------------------------

    fn track_registration(
        &self,
        provider_id: &str,
        instance: u64,
        conn: Arc<dyn PluginConnection>,
    ) {
        self.lock().registrations.insert(
            provider_id.to_string(),
            BridgeRegistration { instance, conn },
        );
    }

    /// Remove a registration only if it is still owned by `instance`
    /// (a re-registration by a reloaded plugin is never unregistered by the
    /// old instance's watcher). Returns true when this call removed it.
    fn unregister_if_instance(&self, provider_id: &str, instance: u64) -> bool {
        let mut inner = self.lock();
        let owned = inner
            .registrations
            .get(provider_id)
            .is_some_and(|registration| registration.instance == instance);
        owned && inner.registrations.remove(provider_id).is_some()
    }

    /// Watch a serving connection; on carrier death, fail its in-flight
    /// streams with synthesized in-band errors and unregister the bridge
    /// providers it serves (load-outcome semantics: a dead plugin registers
    /// nothing). One watcher per connection.
    fn ensure_death_watch(self: &Arc<Self>, plugin: &str, conn: Arc<dyn PluginConnection>) {
        {
            let mut inner = self.lock();
            if inner
                .watched
                .get(plugin)
                .is_some_and(|watched| Arc::ptr_eq(watched, &conn))
            {
                return;
            }
            inner.watched.insert(plugin.to_string(), conn.clone());
        }
        let state = self.clone();
        let plugin = plugin.to_string();
        tokio::spawn(async move {
            while conn.is_alive() {
                tokio::time::sleep(DEATH_POLL).await;
            }
            let mut inner = state.lock();
            inner.watched.remove(&plugin);
            if inner
                .connections
                .get(&plugin)
                .is_some_and(|entry| Arc::ptr_eq(&entry.conn, &conn))
            {
                inner.connections.remove(&plugin);
            }
            // In-flight streams on the dead connection: in-band errors.
            let dead: Vec<(String, String)> = inner
                .streams
                .keys()
                .filter(|(owner, _)| owner == &plugin)
                .cloned()
                .collect();
            for key in dead {
                if let Some(entry) = inner.streams.remove(&key)
                    && Arc::ptr_eq(&entry.conn, &conn)
                {
                    let provider = entry.model.provider.clone();
                    entry.synthesize(
                        StopReason::Error,
                        format!("the plugin serving provider {provider} died"),
                    );
                }
            }
            // Bridge providers this connection serves: unregister both the
            // models and the serving endpoint.
            let owned: Vec<(String, u64)> = inner
                .registrations
                .iter()
                .filter(|(_, registration)| Arc::ptr_eq(&registration.conn, &conn))
                .map(|(provider, registration)| (provider.clone(), registration.instance))
                .collect();
            drop(inner);
            for (provider_id, instance) in owned {
                if state.unregister_if_instance(&provider_id, instance) {
                    tack_ai::unregister_provider_bridge(&provider_id);
                    tack_ai::providers::unregister_runtime_provider(&provider_id);
                    tracing::info!(
                        target: AUDIT_TARGET,
                        plugin = plugin.as_str(),
                        provider = provider_id.as_str(),
                        "provider unregistered: the serving plugin died"
                    );
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// ExtProviderBridge
// ---------------------------------------------------------------------------

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// The tack-ai [`ProviderStreamBridge`] over one plugin connection: sends
/// `provider/stream` (fast ack), owns cancellation forwarding and the
/// grace-period terminal synthesis.
pub struct ExtProviderBridge {
    plugin: String,
    provider_id: String,
    instance: u64,
    conn: Arc<dyn PluginConnection>,
    state: Arc<ProviderBridgeState>,
}

impl std::fmt::Debug for ExtProviderBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtProviderBridge")
            .field("plugin", &self.plugin)
            .field("provider_id", &self.provider_id)
            .field("instance", &self.instance)
            .finish()
    }
}

impl ExtProviderBridge {
    pub fn new(
        plugin: String,
        provider_id: String,
        conn: Arc<dyn PluginConnection>,
        state: Arc<ProviderBridgeState>,
    ) -> Self {
        ExtProviderBridge {
            plugin,
            provider_id,
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            conn,
            state,
        }
    }

    pub fn instance(&self) -> u64 {
        self.instance
    }

    fn clone_ref(&self) -> Self {
        ExtProviderBridge {
            plugin: self.plugin.clone(),
            provider_id: self.provider_id.clone(),
            instance: self.instance,
            conn: self.conn.clone(),
            state: self.state.clone(),
        }
    }

    /// Send `provider/streamCancel` and arm the grace-period synthesis.
    fn abort(&self, stream_id: &str, mut closed_rx: watch::Receiver<bool>) {
        let conn = self.conn.clone();
        let state = self.state.clone();
        let plugin = self.plugin.clone();
        let stream_id = stream_id.to_string();
        tokio::spawn(async move {
            let _ = conn.provider_stream_cancel(&stream_id).await;
            tracing::info!(
                target: AUDIT_TARGET,
                plugin = plugin.as_str(),
                stream_id = stream_id.as_str(),
                "provider stream cancel sent"
            );
            tokio::select! {
                _ = tokio::time::sleep(CANCEL_GRACE) => {
                    state.synthesize_terminal(
                        &plugin,
                        &stream_id,
                        StopReason::Aborted,
                        format!(
                            "stream cancelled; the plugin did not terminate within {}s",
                            CANCEL_GRACE.as_secs()
                        ),
                    );
                }
                _ = closed_rx.changed() => {
                    // The plugin terminated in time.
                }
            }
        });
    }
}

impl ProviderStreamBridge for ExtProviderBridge {
    fn stream(
        &self,
        params: BridgeStreamParams,
        cancel: CancellationToken,
        sink: AssistantMessageEventSender,
    ) -> Result<(), String> {
        let stream_id = params.stream_id.clone();
        let mut closed_rx = self.state.insert_stream(
            &self.plugin,
            &stream_id,
            params.model.clone(),
            self.conn.clone(),
            sink,
        );
        // model/context/options cross as provider-shaped JSON (the v3
        // schema types them free-form; tack-ai's serde types own parsing).
        let wire = ProviderStreamParams {
            stream_id: stream_id.clone(),
            model: serde_json::to_value(&params.model).map_err(|e| e.to_string())?,
            context: serde_json::to_value(&params.context).map_err(|e| e.to_string())?,
            options: serde_json::to_value(&params.options).map_err(|e| e.to_string())?,
        };
        tracing::info!(
            target: AUDIT_TARGET,
            plugin = self.plugin.as_str(),
            provider = self.provider_id.as_str(),
            stream_id = stream_id.as_str(),
            "provider stream starting"
        );
        // Cancellation forwarding is armed before the ack so a cancel that
        // lands during the ack round-trip is never lost.
        if cancel.is_cancelled() {
            self.abort(&stream_id, closed_rx.clone());
        } else {
            let bridge = self.clone_ref();
            let watch_id = stream_id.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => bridge.abort(&watch_id, closed_rx),
                    _ = closed_rx.changed() => {}
                }
            });
        }
        // The ack round-trip rides its own task (`Provider::stream` is
        // sync): a failure synthesizes the terminal in-band error.
        let conn = self.conn.clone();
        let state = self.state.clone();
        let plugin = self.plugin.clone();
        tokio::spawn(async move {
            if let Err(e) = conn.provider_stream(&wire).await {
                state.synthesize_terminal(
                    &plugin,
                    &stream_id,
                    StopReason::Error,
                    format!("provider/stream rejected by the plugin: {e}"),
                );
            }
        });
        Ok(())
    }

    fn cancel(&self, stream_id: &str) {
        // External cancel (e.g. a host-initiated abort outside the
        // stream's own cancel token): same abort path when the stream is
        // still open.
        let closed_rx = self
            .inner_closed_rx(stream_id)
            .unwrap_or_else(|| watch::channel(true).1);
        if !*closed_rx.borrow() {
            self.abort(stream_id, closed_rx);
        }
    }
}

impl ExtProviderBridge {
    fn inner_closed_rx(&self, stream_id: &str) -> Option<watch::Receiver<bool>> {
        // Only the closed receiver is needed; borrow it out of the entry.
        // (The sink and model stay in the map.)
        self.state
            .lock()
            .streams
            .get(&(self.plugin.clone(), stream_id.to_string()))
            .map(|entry| entry.closed_tx.subscribe())
    }
}

// ---------------------------------------------------------------------------
// host/registerProvider (P7a + P7b)
// ---------------------------------------------------------------------------

/// The shared `host/registerProvider` handler (TUI and headless services
/// alike). Plain specs register an HTTP-shim runtime provider (P7a: now
/// available in every run mode); `bridge: true` specs additionally register
/// the plugin connection as the serving endpoint (P7b), gated on the
/// declared `provider.stream` capability and a serving carrier.
pub async fn handle_register_provider(
    state: &Arc<ProviderBridgeState>,
    params: Value,
) -> Result<Value, ErrorObject> {
    // Injected by TaggedServices (spoof-proof overwrite).
    let plugin = params
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let provider = params.get("provider").cloned().unwrap_or(Value::Null);
    let spec: RuntimeProviderSpec = serde_json::from_value(provider).map_err(|e| {
        service_error(
            rpc3::ERR_INVALID_PARAMS,
            format!("bad host/registerProvider params: {e}"),
        )
    })?;
    let provider_id = spec.id.clone();
    let bridged = spec.bridge == Some(true);
    let serving = if bridged {
        if plugin.is_empty() {
            return Err(service_error(
                rpc3::ERR_INTERNAL,
                "bridge registration without plugin attribution",
            ));
        }
        let entry = state.connection(&plugin).ok_or_else(|| {
            service_error(
                rpc3::ERR_PLUGIN_UNAVAILABLE,
                format!("plugin {plugin} has no live connection"),
            )
        })?;
        // Capability gate: same rule as every other capability — undeclared
        // means never granted.
        let Some(granted) = entry.wait_provider_stream_granted().await else {
            return Err(service_error(
                rpc3::ERR_CAPABILITY_NOT_GRANTED,
                "the plugin handshake did not complete in time",
            ));
        };
        if !granted {
            return Err(service_error(
                rpc3::ERR_CAPABILITY_NOT_GRANTED,
                "plugin did not declare capabilities.provider.stream",
            ));
        }
        if !entry.serves_provider_stream {
            return Err(service_error(
                rpc3::ERR_CAPABILITY_NOT_GRANTED,
                "the plugin's carrier does not serve provider/stream",
            ));
        }
        Some(entry)
    } else {
        None
    };
    // Validation lives in tack-ai (bridge specs get the reserved api kind;
    // a conflicting explicit api is a registration error).
    tack_ai::providers::register_runtime_provider(spec)
        .map_err(|e| service_error(rpc3::ERR_INVALID_PARAMS, e))?;
    if let Some(entry) = serving {
        let bridge = Arc::new(ExtProviderBridge::new(
            plugin.clone(),
            provider_id.clone(),
            entry.conn.clone(),
            state.clone(),
        ));
        let instance = bridge.instance();
        tack_ai::register_provider_bridge(&provider_id, bridge);
        state.track_registration(&provider_id, instance, entry.conn.clone());
        state.ensure_death_watch(&plugin, entry.conn);
    }
    tracing::info!(
        target: AUDIT_TARGET,
        plugin = plugin.as_str(),
        provider = provider_id.as_str(),
        bridge = bridged,
        "provider registered"
    );
    Ok(Value::Null)
}
