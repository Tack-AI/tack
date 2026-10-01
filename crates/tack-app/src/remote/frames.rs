//! Frame-level transport: `FrameIo`/`StreamIo`, the per-connection protocol
//! loop, and the bounded per-connection event pump.

use std::sync::Arc;

use anyhow::Result;
use tack_protocol::schemas::*;
use tack_protocol::{read_frame, write_frame};
use tokio::sync::{Mutex, broadcast};

use super::host::{SessionHost, SharedHost};
use super::{MAX_CONNECTIONS, token_matches};

/// Depth of each connection's event queue. A slow consumer that falls
/// more than this many events behind is disconnected (it can reconnect
/// and resync from the hello snapshot) instead of backlogging unbounded
/// memory on the server.
const EVENT_QUEUE_DEPTH: usize = 256;

/// Forward broadcast events into a per-connection BOUNDED channel.
/// Broadcast lag only means some events were dropped; the channel is
/// still live — keep forwarding (the client sees a gap, not silence).
/// A persistently slow consumer (queue full) is cut off: the pump task
/// ends, the connection's event receiver closes, and `handle_frames`
/// tears the connection down. Only a closed channel ends the pump
/// quietly.
fn spawn_event_pump(
    mut events: broadcast::Receiver<ServerEvent>,
) -> tokio::sync::mpsc::Receiver<ServerEvent> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<ServerEvent>(EVENT_QUEUE_DEPTH);
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => match event_tx.try_send(event) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(
                            "remote client fell {EVENT_QUEUE_DEPTH} events behind; disconnecting it"
                        );
                        break;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    event_rx
}

/// Transport-agnostic frame IO: one `read_message`/`write_message` pair =
/// one protocol frame. TCP/Unix/TLS wrap `read_frame`/`write_frame`
/// (4-byte length prefix); WebSocket sends bare CBOR payloads (one binary
/// message per frame). Implemented in this crate for both.
pub(crate) trait FrameIo {
    fn read_message<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> impl Future<Output = Result<Option<T>>> + Send;
    fn write_message<T: serde::Serialize + Sync>(
        &mut self,
        value: &T,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// Byte-stream frames (TCP / unix socket / TLS): length-prefixed CBOR.
struct StreamIo<S> {
    reader: tokio::io::ReadHalf<S>,
    writer: tokio::io::WriteHalf<S>,
}

impl<S> FrameIo for StreamIo<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    async fn read_message<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        Ok(read_frame(&mut self.reader).await?)
    }
    async fn write_message<T: serde::Serialize + Sync>(&mut self, value: &T) -> Result<()> {
        write_frame(&mut self.writer, value).await?;
        Ok(())
    }
}

/// Per-connection handler (byte streams).
pub(crate) async fn handle_connection<S>(stream: S, host: SharedHost) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    // Connection cap: refuse at the door (drop) rather than backlogging
    // unbounded per-connection tasks/channels.
    let permits = host.lock().await.conn_permits.clone();
    let Ok(_permit) = permits.try_acquire_owned() else {
        tracing::warn!("connection limit ({MAX_CONNECTIONS}) reached; dropping connection");
        return Ok(());
    };
    let (reader, writer) = tokio::io::split(stream);
    let mut io = StreamIo { reader, writer };
    handle_frames(&mut io, &host).await
}

/// Max time a connection may take to send its first (hello) frame.
/// Without this a client that connects and goes silent holds a connection
/// slot forever.
const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Unique id per connection: millis alone collide for two accepts in the
/// same millisecond (same scheme as next_permission_request_id).
fn next_connection_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "conn-{}-{}",
        tack_ai::now_millis(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Per-connection extension-surface opt-ins, negotiated in the hello.
/// Pre-extension clients (no capabilities field) get `false`/`false` and
/// see exactly the v1 event stream.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ConnectionCaps {
    pub widgets: bool,
    pub dialogs: bool,
}

impl ConnectionCaps {
    fn from_hello(capabilities: &[String]) -> Self {
        ConnectionCaps {
            widgets: capabilities.iter().any(|c| c == CAP_EXT_WIDGETS),
            dialogs: capabilities.iter().any(|c| c == CAP_EXT_DIALOGS),
        }
    }

    /// Event gating: ext-surface events are only written to connections
    /// that opted into their capability in the hello.
    fn allows(&self, event: &ServerEvent) -> bool {
        match event {
            ServerEvent::ExtWidgetUpdate { .. } | ServerEvent::ExtWidgetsRemoved { .. } => {
                self.widgets
            }
            ServerEvent::ExtDialogRequest { .. } | ServerEvent::ExtDialogClosed { .. } => {
                self.dialogs
            }
            _ => true,
        }
    }
}

/// Per-connection protocol loop, shared by all transports (TCP/unix/TLS via
/// `StreamIo`, WebSocket via `remote_ws::WsIo`).
pub(crate) async fn handle_frames(
    io: &mut impl FrameIo,
    host: &Arc<Mutex<SessionHost>>,
) -> Result<()> {
    // Handshake (time-boxed).
    let first: Option<ClientMessage> =
        match tokio::time::timeout(HELLO_TIMEOUT, io.read_message()).await {
            Ok(result) => result?,
            Err(_) => {
                io.write_message(&ServerMessage::HelloError {
                    error: ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: "handshake timeout: no hello frame".to_string(),
                        details: None,
                    },
                })
                .await?;
                return Ok(());
            }
        };
    let caps = match first {
        Some(ClientMessage::Hello {
            version,
            token,
            capabilities,
        }) if version == PROTOCOL_VERSION => {
            // Shared-token auth (TS pi's --auth-token semantics).
            let expected = host.lock().await.auth_token.clone();
            if let Some(expected) = expected
                && !token_matches(&expected, token.as_deref())
            {
                io.write_message(&ServerMessage::HelloError {
                    error: ProtocolError {
                        code: ProtocolErrorCode::Auth,
                        message: "invalid or missing auth token".to_string(),
                        details: None,
                    },
                })
                .await?;
                return Ok(());
            }
            ConnectionCaps::from_hello(&capabilities)
        }
        Some(ClientMessage::Hello { .. }) => {
            io.write_message(&ServerMessage::HelloError {
                error: ProtocolError {
                    code: ProtocolErrorCode::Version,
                    message: format!("unsupported protocol version (server: {PROTOCOL_VERSION})"),
                    details: None,
                },
            })
            .await?;
            return Ok(());
        }
        _ => {
            io.write_message(&ServerMessage::HelloError {
                error: ProtocolError {
                    code: ProtocolErrorCode::InvalidRequest,
                    message: "first frame must be hello".to_string(),
                    details: None,
                },
            })
            .await?;
            return Ok(());
        }
    };

    let connection_id = next_connection_id();
    let snapshot = host.lock().await.server_snapshot();
    io.write_message(&ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        connection_id,
        snapshot,
        capabilities: SERVER_CAPABILITIES.iter().map(|s| s.to_string()).collect(),
    })
    .await?;

    // Registered only after a successful handshake (failed handshakes
    // never count): active_connections drives permission-prompt liveness.
    // The ext bridge tracks dialog-capable connections the same way (a
    // plugin dialog with zero answerers fails fast instead of parking).
    {
        let mut host_guard = host.lock().await;
        host_guard.active_connections += 1;
        host_guard.ext_bridge.client_connected(caps.dialogs);
    }
    // This connection's session attachments, refcounted (attach +1 /
    // detach -1); whatever is still held when the loop ends is released
    // by connection_closed.
    let mut attached: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let loop_result = connection_loop(io, host, &mut attached, caps).await;
    // Teardown on EVERY exit path — clean EOF, io error and event-pump
    // shutdown alike (F07: release attachments; F17: last client gone
    // denies parked permission prompts and plugin dialogs).
    SessionHost::connection_closed(host, &attached, caps.dialogs).await;
    loop_result
}

/// The framed request/event loop for one handshaken connection. Every
/// early return (io error via `?`, EOF, event-pump close) is a connection
/// teardown: the caller releases this connection's attachments.
async fn connection_loop(
    io: &mut impl FrameIo,
    host: &Arc<Mutex<SessionHost>>,
    attached: &mut std::collections::HashMap<String, u32>,
    caps: ConnectionCaps,
) -> Result<()> {
    // Events → this connection.
    let mut event_rx = spawn_event_pump(host.lock().await.events.subscribe());
    // Responses of off-loop requests (see below): merged back into the
    // single writer.
    let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

    loop {
        tokio::select! {
            frame = io.read_message::<ClientMessage>() => {
                let Some(message) = frame? else { break };
                match message {
                    ClientMessage::Request { id, request } => {
                        // Potentially long, RE-ENTRANT requests run
                        // off-loop: a plugin command/autocomplete handler
                        // may call back into the host (ui/select →
                        // ext_dialog_request) that only THIS connection
                        // can answer — awaiting it inline would deadlock
                        // the dialog until the timeout. The response
                        // re-enters via response_rx; ordering vs other
                        // responses is irrelevant (ids correlate).
                        if matches!(
                            &request,
                            Command::InvokeExtCommand { .. } | Command::ExtAutocomplete { .. }
                        ) {
                            let host = host.clone();
                            let tx = response_tx.clone();
                            tokio::spawn(async move {
                                let response = match SessionHost::handle_command(&host, request).await
                                {
                                    Ok(result) => ServerMessage::ok(id, result),
                                    Err(error) => ServerMessage::Response {
                                        id,
                                        ok: false,
                                        result: None,
                                        error: Some(error),
                                    },
                                };
                                let _ = tx.send(response);
                            });
                            continue;
                        }
                        // Per-connection attachment tracking (F07): only
                        // SUCCESSFUL attach/detach commands move the
                        // refcount, mirroring the host's `attached` count.
                        let attachment: Option<(String, bool)> = match &request {
                            Command::Attach { session_id } => Some((session_id.clone(), true)),
                            Command::Detach { session_id } => Some((session_id.clone(), false)),
                            _ => None,
                        };
                        // The host lock is acquired per critical section
                        // inside handle_command — never across the whole
                        // command (model resolution and session file IO
                        // run outside it).
                        let result = SessionHost::handle_command(host, request).await;
                        if let (Some((session_id, is_attach)), Ok(_)) = (attachment, &result) {
                            let count = attached.entry(session_id).or_insert(0);
                            *count = if is_attach {
                                count.saturating_add(1)
                            } else {
                                count.saturating_sub(1)
                            };
                        }
                        let response = match result {
                            Ok(result) => ServerMessage::ok(id, result),
                            Err(error) => ServerMessage::Response {
                                id,
                                ok: false,
                                result: None,
                                error: Some(error),
                            },
                        };
                        io.write_message( &response).await?;
                    }
                    ClientMessage::Hello { .. } => {
                        // Duplicate hello: ignore.
                    }
                    ClientMessage::Unknown => {
                        // Message kind from a newer peer: ignore (additive
                        // extension tolerance).
                    }
                }
            }
            Some(response) = response_rx.recv() => {
                io.write_message(&response).await?;
            }
            event = event_rx.recv() => {
                match event {
                    Some(event) => {
                        // Ext-surface events only reach connections that
                        // opted into their capability in the hello.
                        if caps.allows(&event) {
                            io.write_message( &ServerMessage::Event { event }).await?;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod caps_tests {
    use super::*;

    fn widget_event() -> ServerEvent {
        ServerEvent::ExtWidgetsRemoved {
            plugin: "p".into(),
            keys: vec!["p:w".into()],
        }
    }

    fn dialog_event() -> ServerEvent {
        ServerEvent::ExtDialogClosed {
            request_id: "dlg-1".into(),
        }
    }

    /// Ext-surface events only pass with their capability; permission
    /// prompts stay mode-driven (never capability-gated) and regular
    /// events always pass.
    #[test]
    fn ext_events_require_their_capability() {
        // Pre-extension client (no capabilities field): nothing gated passes.
        let none = ConnectionCaps::from_hello(&[]);
        assert!(!none.allows(&widget_event()));
        assert!(!none.allows(&dialog_event()));
        // Regular events are never gated.
        assert!(none.allows(&ServerEvent::SessionRemoved {
            session_id: "s".into()
        }));
        assert!(none.allows(&ServerEvent::PermissionRequest {
            session_id: "s".into(),
            request_id: "r".into(),
            tool_call_id: "t".into(),
            tool_name: "bash".into(),
            title: "bash: ls".into(),
            input: serde_json::json!({}),
        }));

        // Per-surface granularity: widgets ≠ dialogs.
        let widgets_only = ConnectionCaps::from_hello(&[CAP_EXT_WIDGETS.to_string()]);
        assert!(widgets_only.allows(&widget_event()));
        assert!(!widgets_only.allows(&dialog_event()));
        let dialogs_only = ConnectionCaps::from_hello(&[CAP_EXT_DIALOGS.to_string()]);
        assert!(!dialogs_only.allows(&widget_event()));
        assert!(dialogs_only.allows(&dialog_event()));
        // Unknown capability strings are ignored; both bits work together.
        let both = ConnectionCaps::from_hello(&[
            CAP_EXT_WIDGETS.to_string(),
            CAP_EXT_DIALOGS.to_string(),
            "future_thing".to_string(),
        ]);
        assert!(both.allows(&widget_event()));
        assert!(both.allows(&dialog_event()));
    }
}

#[cfg(test)]
pub(crate) struct MockIo {
    incoming: std::collections::VecDeque<ClientMessage>,
    written: std::sync::Arc<std::sync::Mutex<Vec<ServerMessage>>>,
    /// Signalled on every write (tests awaiting off-loop responses).
    written_notify: std::sync::Arc<tokio::sync::Notify>,
    /// When the script runs out: park instead of EOF, so the test (not
    /// the script length) controls the connection lifetime — required
    /// when off-loop responses are still in flight at script end.
    park_on_empty: bool,
}

#[cfg(test)]
impl MockIo {
    /// A scripted connection: the queued messages are delivered in order,
    /// then reads hit EOF (client disconnected).
    pub(crate) fn new(messages: Vec<ClientMessage>) -> Self {
        Self {
            incoming: messages.into_iter().collect(),
            written: Default::default(),
            written_notify: Default::default(),
            park_on_empty: false,
        }
    }

    /// Like [`Self::new`], but reads PARK once the script runs out (the
    /// test ends the connection by aborting the handle_frames task).
    pub(crate) fn open_ended(messages: Vec<ClientMessage>) -> Self {
        Self {
            park_on_empty: true,
            ..Self::new(messages)
        }
    }

    /// Snapshot of everything written so far.
    pub(crate) fn written(&self) -> Vec<ServerMessage> {
        self.written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Shared handle to the written log (open-ended tests outlive the
    /// `MockIo`, which moves into the connection task).
    pub(crate) fn written_shared(&self) -> std::sync::Arc<std::sync::Mutex<Vec<ServerMessage>>> {
        self.written.clone()
    }

    /// Fires on every written message (re-check `written` on wake).
    pub(crate) fn written_notify(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.written_notify.clone()
    }
}

#[cfg(test)]
impl FrameIo for MockIo {
    async fn read_message<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let Some(message) = self.incoming.pop_front() else {
            if self.park_on_empty {
                std::future::pending::<()>().await;
            }
            return Ok(None);
        };
        let value = serde_json::to_value(message)?;
        Ok(Some(serde_json::from_value(value)?))
    }
    async fn write_message<T: serde::Serialize + Sync>(&mut self, value: &T) -> Result<()> {
        let value = serde_json::to_value(value)?;
        self.written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(serde_json::from_value(value)?);
        self.written_notify.notify_waiters();
        Ok(())
    }
}

#[cfg(test)]
#[allow(unsafe_code, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Regression: connection ids used to be `conn-<millis>` — two accepts
    /// in the same millisecond collided. The sequence suffix must make
    /// them unique.
    #[test]
    fn connection_ids_are_unique_within_a_millisecond() {
        let ids: std::collections::HashSet<String> =
            (0..1000).map(|_| next_connection_id()).collect();
        assert_eq!(ids.len(), 1000);
    }

    /// Regression: a lagging broadcast receiver killed the per-connection
    /// event pump permanently (while-let broke on RecvError::Lagged), so the
    /// client silently stopped receiving events. The pump must skip lagged
    /// gaps and keep delivering.
    // NB: #[tokio::test] uses a current-thread runtime, so the pump task
    // cannot poll until the first await below — the overflow is
    // deterministic without start_paused (no test-util feature needed).
    #[tokio::test]
    async fn event_pump_survives_broadcast_lag() {
        let (tx, _) = broadcast::channel(1);
        let mut rx = spawn_event_pump(tx.subscribe());
        let event = |delta: &str| ServerEvent::SessionProgress {
            session_id: "s".into(),
            progress: TranscriptProgress::AssistantDelta {
                message_id: "m".into(),
                content_index: 0,
                kind: "text".into(),
                delta: delta.into(),
            },
        };
        // Overflow the capacity-1 channel before the pump task ever polls:
        // the first recv() will be Lagged.
        for i in 0..5 {
            tx.send(event(&format!("e{i}"))).unwrap();
        }
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("pump must not die on lag");
        assert!(got.is_some(), "pump closed the channel after lagging");
        // And later events still flow.
        tx.send(event("e5")).unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("pump must keep delivering after lag");
        assert!(matches!(got, Some(ServerEvent::SessionProgress { .. })));
    }

    /// Slow consumer guard: the per-connection event queue is bounded; a
    /// client that never drains is disconnected (pump ends → receiver
    /// closes) instead of backlogging unbounded memory on the server.
    #[tokio::test]
    async fn event_pump_disconnects_slow_consumer() {
        let (tx, _) = broadcast::channel(4096);
        let mut rx = spawn_event_pump(tx.subscribe());
        let event = |delta: &str| ServerEvent::SessionProgress {
            session_id: "s".into(),
            progress: TranscriptProgress::AssistantDelta {
                message_id: "m".into(),
                content_index: 0,
                kind: "text".into(),
                delta: delta.into(),
            },
        };
        for i in 0..(EVENT_QUEUE_DEPTH + 50) {
            tx.send(event(&format!("e{i}"))).unwrap();
        }
        // Let the pump run while the consumer is NOT draining: it fills the
        // bounded queue, hits Full, and cuts the connection.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let mut received = 0usize;
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Some(_)) => received += 1,
                Ok(None) => break,
                Err(_) => panic!("pump hung instead of cutting off the slow consumer"),
            }
        }
        assert_eq!(
            received, EVENT_QUEUE_DEPTH,
            "exactly the queue depth is delivered before the pump cuts the connection"
        );
    }
}
