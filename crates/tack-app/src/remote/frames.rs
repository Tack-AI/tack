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
    match first {
        Some(ClientMessage::Hello { version, token }) if version == PROTOCOL_VERSION => {
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
    }

    let connection_id = next_connection_id();
    let snapshot = host.lock().await.server_snapshot();
    io.write_message(&ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        connection_id,
        snapshot,
    })
    .await?;

    // Registered only after a successful handshake (failed handshakes
    // never count): active_connections drives permission-prompt liveness.
    host.lock().await.active_connections += 1;
    // This connection's session attachments, refcounted (attach +1 /
    // detach -1); whatever is still held when the loop ends is released
    // by connection_closed.
    let mut attached: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let loop_result = connection_loop(io, host, &mut attached).await;
    // Teardown on EVERY exit path — clean EOF, io error and event-pump
    // shutdown alike (F07: release attachments; F17: last client gone
    // denies parked permission prompts).
    SessionHost::connection_closed(host, &attached).await;
    loop_result
}

/// The framed request/event loop for one handshaken connection. Every
/// early return (io error via `?`, EOF, event-pump close) is a connection
/// teardown: the caller releases this connection's attachments.
async fn connection_loop(
    io: &mut impl FrameIo,
    host: &Arc<Mutex<SessionHost>>,
    attached: &mut std::collections::HashMap<String, u32>,
) -> Result<()> {
    // Events → this connection.
    let mut event_rx = spawn_event_pump(host.lock().await.events.subscribe());

    loop {
        tokio::select! {
            frame = io.read_message::<ClientMessage>() => {
                let Some(message) = frame? else { break };
                match message {
                    ClientMessage::Request { id, request } => {
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
            event = event_rx.recv() => {
                match event {
                    Some(event) => {
                        io.write_message( &ServerMessage::Event { event }).await?;
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) struct MockIo {
    incoming: std::collections::VecDeque<ClientMessage>,
    pub(crate) written: Vec<ServerMessage>,
}

#[cfg(test)]
impl MockIo {
    /// A scripted connection: the queued messages are delivered in order,
    /// then reads hit EOF (client disconnected).
    pub(crate) fn new(messages: Vec<ClientMessage>) -> Self {
        Self {
            incoming: messages.into_iter().collect(),
            written: Vec::new(),
        }
    }
}

#[cfg(test)]
impl FrameIo for MockIo {
    async fn read_message<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let Some(message) = self.incoming.pop_front() else {
            return Ok(None);
        };
        let value = serde_json::to_value(message)?;
        Ok(Some(serde_json::from_value(value)?))
    }
    async fn write_message<T: serde::Serialize + Sync>(&mut self, value: &T) -> Result<()> {
        let value = serde_json::to_value(value)?;
        self.written.push(serde_json::from_value(value)?);
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
