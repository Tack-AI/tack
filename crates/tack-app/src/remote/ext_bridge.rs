//! Remote extension UI bridge: plugin-initiated UI (declarative widgets,
//! `ui/select`/`ui/confirm`/`ui/input` dialogs, MCP elicitation) routed to
//! connected clients over the protocol.
//!
//! Plugin connections are host-global (one `ExtensionManager` shared by
//! every session), so plugin UI has no session attribution. Widgets are
//! pulled by clients (`list_ext_widgets`) and pushed as
//! `ext_widget_update` / `ext_widgets_removed` events; dialogs are
//! broadcast to every connection that opted into `CAP_EXT_DIALOGS` and
//! the FIRST `ext_dialog_response` wins — the same multi-client policy as
//! `PermissionRequest`. The bridge is shared by:
//!
//! - the host (`SessionHost`), answering `ext_dialog_response` commands
//!   and tracking dialog-capable connections;
//! - the plugin host services (`ext_headless::HeadlessExtServices` in
//!   "remote" mode), parking `ui/*` requests on a client answer and
//!   applying `widgets/update` to the manager;
//! - the MCP elicitation handler (`mcp_elicitation` in `Remote` mode),
//!   which reuses the dialog machinery.
//!
//! Lock discipline: the bridge's own mutex is only ever held for
//! in-memory registry mutations and is NEVER held while acquiring the
//! host lock (widget application extracts the host handle first), so the
//! two locks cannot deadlock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tack_protocol::schemas::{ExtDialogKind, ExtElicitField, ServerEvent};
use tokio::sync::{broadcast, oneshot};

use super::host::SharedHost;

/// Max time a plugin dialog waits for a client answer. Same rationale as
/// `PERMISSION_PROMPT_TIMEOUT` in host.rs: without a bound, a prompt no
/// client ever answers would park the calling plugin request forever.
const EXT_DIALOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Unique id for dialog requests (millis alone can collide).
fn next_dialog_request_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "dlg-{}-{}",
        tack_ai::now_millis(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// A client's answer to one dialog (semantics per kind are documented on
/// `ExtDialogKind` in the protocol schemas).
#[derive(Debug)]
pub struct ExtDialogAnswer {
    /// True when the client dismissed the dialog without a value.
    pub cancelled: bool,
    /// The answer payload (kind-shaped; see `ExtDialogKind`).
    pub value: Option<Value>,
}

/// Everything the bridge needs to broadcast one dialog request.
#[derive(Debug)]
pub struct ExtDialogSpec {
    /// Originating plugin id or MCP server name.
    pub source: String,
    /// Which dialog to show.
    pub kind: ExtDialogKind,
    /// Dialog title.
    pub title: String,
    /// Body text (`confirm` / `elicitation`).
    pub message: Option<String>,
    /// Choices (`select`).
    pub options: Vec<String>,
    /// Placeholder text (`input`).
    pub placeholder: Option<String>,
    /// Form fields (`elicitation`).
    pub fields: Vec<ExtElicitField>,
}

/// A `widgets/update` received before the host existed (plugins connect
/// during `ExtensionManager::load`, which runs before `build_host`).
#[derive(Debug)]
struct BufferedWidgetUpdate {
    plugin: String,
    id: String,
    state: Value,
    visible: Option<bool>,
}

#[derive(Default)]
struct BridgeInner {
    /// Set by `attach` once the host exists (services are constructed
    /// first — `ExtensionManager::load` needs them).
    host: Option<SharedHost>,
    /// Cloned out of the host at attach time so broadcasts never need
    /// the host lock.
    events: Option<broadcast::Sender<ServerEvent>>,
    buffered_widget_updates: Vec<BufferedWidgetUpdate>,
    /// Dialogs parked on a client answer, keyed by request id.
    pending_dialogs: HashMap<String, oneshot::Sender<ExtDialogAnswer>>,
    /// Live connections that negotiated `CAP_EXT_DIALOGS`. With zero
    /// answerers a dialog fails fast (capability-not-granted semantics,
    /// like the plain headless modes) instead of parking unanswered.
    dialog_capable_connections: u32,
}

/// See the module doc. Cheap to clone (Arc inside).
pub struct RemoteExtBridge {
    inner: Mutex<BridgeInner>,
    /// Test hook: dialog timeout override (10 min is untestable).
    #[cfg(test)]
    dialog_timeout: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for RemoteExtBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteExtBridge").finish_non_exhaustive()
    }
}

impl RemoteExtBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(RemoteExtBridge {
            inner: Mutex::new(BridgeInner::default()),
            #[cfg(test)]
            dialog_timeout: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, BridgeInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dialog_timeout(&self) -> std::time::Duration {
        #[cfg(test)]
        {
            let override_ms = self
                .dialog_timeout
                .load(std::sync::atomic::Ordering::Relaxed);
            if override_ms > 0 {
                return std::time::Duration::from_millis(override_ms);
            }
        }
        EXT_DIALOG_TIMEOUT
    }

    /// Point the bridge at the freshly built host and flush widget updates
    /// buffered while plugins were loading.
    pub async fn attach(self: &Arc<Self>, host: &SharedHost) {
        let buffered: Vec<BufferedWidgetUpdate> = {
            let events = host.lock().await.events.clone();
            let mut inner = self.lock();
            inner.events = Some(events);
            inner.host = Some(host.clone());
            std::mem::take(&mut inner.buffered_widget_updates)
        };
        for update in buffered {
            self.apply_widget_update(&update.plugin, &update.id, update.state, update.visible)
                .await;
        }
    }

    /// A handshaken connection arrived (`dialog_capable` = it negotiated
    /// `CAP_EXT_DIALOGS`).
    pub fn client_connected(&self, dialog_capable: bool) {
        if dialog_capable {
            self.lock().dialog_capable_connections += 1;
        }
    }

    /// A connection went away. When the LAST dialog-capable client
    /// leaves, every parked dialog is failed (dropped senders make
    /// `ask_dialog` return an error): nobody can ever answer them.
    pub fn client_disconnected(&self, dialog_capable: bool) {
        if !dialog_capable {
            return;
        }
        let mut inner = self.lock();
        inner.dialog_capable_connections = inner.dialog_capable_connections.saturating_sub(1);
        if inner.dialog_capable_connections == 0 {
            inner.pending_dialogs.clear();
        }
    }

    /// Live dialog-capable connection count (test assertion seam).
    #[doc(hidden)]
    pub fn dialog_answerers(&self) -> u32 {
        self.lock().dialog_capable_connections
    }

    /// Request ids of the currently parked dialogs (test assertion seam).
    #[doc(hidden)]
    pub fn pending_dialog_ids(&self) -> Vec<String> {
        self.lock().pending_dialogs.keys().cloned().collect()
    }

    /// Broadcast `ext_dialog_closed` so other clients dismiss the prompt.
    fn broadcast_closed(&self, request_id: &str) {
        let events = self.lock().events.clone();
        if let Some(events) = events {
            let _ = events.send(ServerEvent::ExtDialogClosed {
                request_id: request_id.to_string(),
            });
        }
    }

    /// Park a plugin/MCP dialog until a client answers, it times out, or
    /// the last dialog-capable client disconnects. Fails fast when no
    /// capable client is connected (the caller maps this to
    /// capability-not-granted / elicitation-decline).
    pub async fn ask_dialog(&self, spec: ExtDialogSpec) -> Result<ExtDialogAnswer, String> {
        let request_id = next_dialog_request_id();
        let (tx, rx) = oneshot::channel();
        let events = {
            let mut inner = self.lock();
            if inner.dialog_capable_connections == 0 {
                return Err("no dialog-capable client connected".to_string());
            }
            inner.pending_dialogs.insert(request_id.clone(), tx);
            inner.events.clone()
        };
        if let Some(events) = events {
            let _ = events.send(ServerEvent::ExtDialogRequest {
                request_id: request_id.clone(),
                source: spec.source,
                kind: spec.kind,
                title: spec.title,
                message: spec.message,
                options: spec.options,
                placeholder: spec.placeholder,
                fields: spec.fields,
            });
        }
        match tokio::time::timeout(self.dialog_timeout(), rx).await {
            Ok(Ok(answer)) => Ok(answer),
            // Sender dropped: the last capable client disconnected (or
            // shutdown). Distinct from a user cancel.
            Ok(Err(_)) => Err("dialog closed: no client can answer it anymore".to_string()),
            Err(_) => {
                let removed = {
                    let mut inner = self.lock();
                    inner.pending_dialogs.remove(&request_id).is_some()
                };
                if removed {
                    self.broadcast_closed(&request_id);
                }
                Err("dialog timed out without an answer".to_string())
            }
        }
    }

    /// Deliver a client's `ext_dialog_response`. False = unknown/expired
    /// id (already answered by another client, timed out, or drained).
    pub fn answer_dialog(&self, request_id: &str, cancelled: bool, value: Option<Value>) -> bool {
        let pending = {
            let mut inner = self.lock();
            inner.pending_dialogs.remove(request_id)
        };
        let Some(respond) = pending else {
            return false;
        };
        let _ = respond.send(ExtDialogAnswer { cancelled, value });
        // First answer wins; every other client dismisses the prompt.
        self.broadcast_closed(request_id);
        true
    }

    /// Apply a plugin's `widgets/update` to the shared manager and
    /// broadcast the new full state. Before `attach` (plugins still
    /// loading) the update is buffered and flushed by `attach`.
    pub async fn apply_widget_update(
        &self,
        plugin: &str,
        id: &str,
        state: Value,
        visible: Option<bool>,
    ) {
        let host = self.lock().host.clone();
        let Some(host) = host else {
            self.lock()
                .buffered_widget_updates
                .push(BufferedWidgetUpdate {
                    plugin: plugin.to_string(),
                    id: id.to_string(),
                    state,
                    visible,
                });
            return;
        };
        let (updated, events) = {
            let mut host_guard = host.lock().await;
            let updated = host_guard
                .extensions
                .apply_widget_update_remote(plugin, id, state, visible);
            (updated, host_guard.events.clone())
        };
        match updated {
            Some(widget) => {
                let _ = events.send(ServerEvent::ExtWidgetUpdate { widget });
            }
            None => {
                tracing::warn!("widgets/update for unknown widget {plugin}:{id}; ignored");
            }
        }
    }

    /// A plugin died: its widgets vanish with it (no UI residue).
    pub async fn remove_plugin_widgets(&self, plugin: &str) {
        let host = self.lock().host.clone();
        let Some(host) = host else {
            return;
        };
        let (removed, events) = {
            let mut host_guard = host.lock().await;
            let removed = host_guard.extensions.remove_plugin_widgets(plugin);
            (removed, host_guard.events.clone())
        };
        if !removed.is_empty() {
            let _ = events.send(ServerEvent::ExtWidgetsRemoved {
                plugin: plugin.to_string(),
                keys: removed,
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::remote::testutil::*;

    fn spec() -> ExtDialogSpec {
        ExtDialogSpec {
            source: "plug".into(),
            kind: ExtDialogKind::Confirm,
            title: "Sure?".into(),
            message: Some("do the thing".into()),
            options: Vec::new(),
            placeholder: None,
            fields: Vec::new(),
        }
    }

    /// No dialog-capable client: dialogs fail fast instead of parking
    /// (plain-headless behavior is preserved for that case).
    #[tokio::test]
    async fn dialog_fails_fast_without_answerers() {
        let bridge = RemoteExtBridge::new();
        let err = bridge.ask_dialog(spec()).await.unwrap_err();
        assert!(err.contains("no dialog-capable"), "{err}");
    }

    /// The park/answer round trip: a capable client is connected, the
    /// request broadcasts, and `answer_dialog` resolves it (first answer
    /// wins; the second gets `false`).
    #[tokio::test]
    async fn dialog_answer_round_trip() {
        let bridge = RemoteExtBridge::new();
        let host = test_host(vec![]);
        bridge.attach(&host).await;
        let mut events = host.lock().await.events.subscribe();
        bridge.client_connected(true);

        let ask = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.ask_dialog(spec()).await })
        };
        let ServerEvent::ExtDialogRequest {
            request_id, kind, ..
        } = events.recv().await.unwrap()
        else {
            panic!("expected dialog request event")
        };
        assert_eq!(kind, ExtDialogKind::Confirm);
        assert!(bridge.answer_dialog(&request_id, false, Some(Value::Bool(true))));
        let answer = ask.await.unwrap().unwrap();
        assert!(!answer.cancelled);
        assert_eq!(answer.value, Some(Value::Bool(true)));
        // The winner resolved it; late answers find nothing.
        assert!(!bridge.answer_dialog(&request_id, false, None));
        // Other clients were told to dismiss the prompt.
        let mut saw_closed = false;
        while let Ok(event) = events.try_recv() {
            if let ServerEvent::ExtDialogClosed { request_id: id } = event {
                saw_closed = id == request_id;
            }
        }
        assert!(
            saw_closed,
            "ext_dialog_closed must follow the winning answer"
        );
    }

    /// Losing the last dialog-capable client fails every parked dialog.
    #[tokio::test]
    async fn last_capable_disconnect_fails_pending_dialogs() {
        let bridge = RemoteExtBridge::new();
        let host = test_host(vec![]);
        bridge.attach(&host).await;
        bridge.client_connected(true);
        bridge.client_connected(true);
        let ask = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.ask_dialog(spec()).await })
        };
        // Let ask_dialog park.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bridge.client_disconnected(true);
        assert!(
            bridge.lock().dialog_capable_connections == 1,
            "one capable client left: dialog still parked"
        );
        bridge.client_disconnected(true);
        let err = ask.await.unwrap().unwrap_err();
        assert!(err.contains("closed"), "{err}");
    }

    /// A dialog nobody answers times out and broadcasts the dismissal.
    #[tokio::test]
    async fn unanswered_dialog_times_out() {
        let bridge = RemoteExtBridge::new();
        bridge
            .dialog_timeout
            .store(50, std::sync::atomic::Ordering::Relaxed);
        let host = test_host(vec![]);
        bridge.attach(&host).await;
        let mut events = host.lock().await.events.subscribe();
        bridge.client_connected(true);
        let err = bridge.ask_dialog(spec()).await.unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        // Request + closed were both broadcast.
        let mut kinds = Vec::new();
        while let Ok(event) = events.try_recv() {
            kinds.push(match event {
                ServerEvent::ExtDialogRequest { .. } => "request",
                ServerEvent::ExtDialogClosed { .. } => "closed",
                _ => "other",
            });
        }
        assert_eq!(kinds, vec!["request", "closed"], "{kinds:?}");
    }

    /// Widget updates arriving before `attach` are buffered, then flushed:
    /// they apply to the manager and broadcast the full new state.
    #[cfg(feature = "ext")]
    #[tokio::test]
    async fn widget_updates_buffer_before_attach_then_flush() {
        let bridge = RemoteExtBridge::new();
        bridge
            .apply_widget_update("plug", "w1", serde_json::json!({"n": 1}), None)
            .await;
        assert_eq!(bridge.lock().buffered_widget_updates.len(), 1);

        let host = test_host(vec![]);
        host.lock().await.extensions.test_insert_widget(
            "plug",
            tack_ext::rpc3::WidgetSpec {
                id: "w1".to_string(),
                r#type: tack_ext::rpc3::WidgetKind::MarkdownPanel,
                title: Some("panel".to_string()),
                ..Default::default()
            },
        );
        let mut events = host.lock().await.events.subscribe();
        bridge.attach(&host).await;
        let ServerEvent::ExtWidgetUpdate { widget } = events.recv().await.unwrap() else {
            panic!("expected widget update event")
        };
        assert_eq!(widget.key, "plug:w1");
        assert_eq!(widget.state, Some(serde_json::json!({"n": 1})));
        assert_eq!(widget.kind, "markdownPanel");
        // The manager holds the new state (list_ext_widgets reads it).
        let widgets = host.lock().await.extensions.widgets().to_vec();
        assert_eq!(widgets[0].state, Some(serde_json::json!({"n": 1})));
    }

    /// Unknown widget ids warn and drop; known removals broadcast.
    #[cfg(feature = "ext")]
    #[tokio::test]
    async fn widget_update_unknown_id_and_removal() {
        let bridge = RemoteExtBridge::new();
        let host = test_host(vec![]);
        bridge.attach(&host).await;
        let mut events = host.lock().await.events.subscribe();
        bridge
            .apply_widget_update("ghost", "nope", serde_json::json!(1), None)
            .await;
        assert!(events.try_recv().is_err(), "unknown widget: no event");

        host.lock().await.extensions.test_insert_widget(
            "plug",
            tack_ext::rpc3::WidgetSpec {
                id: "w1".to_string(),
                r#type: tack_ext::rpc3::WidgetKind::StatusLineSegment,
                ..Default::default()
            },
        );
        bridge.remove_plugin_widgets("plug").await;
        let ServerEvent::ExtWidgetsRemoved { plugin, keys } = events.recv().await.unwrap() else {
            panic!("expected widgets removed event")
        };
        assert_eq!(plugin, "plug");
        assert_eq!(keys, vec!["plug:w1".to_string()]);
    }
}
