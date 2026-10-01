//! Client for the remote-session protocol: hello handshake, id-correlated
//! requests, and an event broadcast channel.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::framing::{ProtocolError, read_frame, write_frame};
use crate::schemas::*;

/// Default per-request timeout: a server that never answers must not hang
/// the caller (and leak the pending entry) forever.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct RemoteClient {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ServerMessage>>>>,
    events: broadcast::Sender<ServerEvent>,
    next_id: AtomicU64,
    /// Per-request timeout in milliseconds (atomic so it can be tuned on a
    /// shared `Arc<RemoteClient>`).
    request_timeout_ms: AtomicU64,
    /// Set by the read pump when the connection ends; requests issued after
    /// this point fail fast instead of awaiting a response that can never
    /// arrive.
    closed: Arc<AtomicBool>,
    /// The read-pump task. Dropping the last `Arc<RemoteClient>` aborts it:
    /// the pump only holds the socket's read half, so without an abort it
    /// would block in `read_frame` forever, leaking the task and keeping
    /// the socket alive after the client is gone.
    pump: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub connection_id: String,
    pub snapshot: ServerSnapshot,
    /// Extension-surface capabilities the server advertised in its hello
    /// (empty on pre-extension servers — treat as "no ext surfaces").
    pub server_capabilities: Vec<String>,
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        if let Some(handle) = self.pump.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }
}

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteClient")
            .field("connection_id", &self.connection_id)
            .finish_non_exhaustive()
    }
}

impl RemoteClient {
    /// Handshake + spawn the read pump. `stream` is any connected transport.
    /// `token` is sent in the hello when the server requires auth.
    /// `capabilities` opts the connection into extension-surface events
    /// (`CAP_EXT_*`); pass an empty vec for the pre-extension event stream.
    pub async fn connect<S>(
        stream: S,
        token: Option<String>,
        capabilities: Vec<String>,
    ) -> Result<Arc<Self>, ProtocolError>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, writer) = tokio::io::split(stream);
        let writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>> =
            Arc::new(Mutex::new(Box::new(writer)));

        write_frame(
            &mut *writer.lock().await,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                token,
                capabilities,
            },
        )
        .await?;

        let first: ServerMessage = read_frame(&mut reader)
            .await?
            .ok_or(ProtocolError::ConnectionClosed)?;
        let (connection_id, snapshot, server_capabilities) = match first {
            ServerMessage::Hello {
                version,
                connection_id,
                snapshot,
                capabilities,
            } => {
                // Protocol versions are ordered integers (no semver). A
                // server speaking a NEWER protocol may rely on behavior
                // this client does not implement, so the connection is
                // rejected with the reserved Version error code; an older
                // server is accepted (the wire format is additive and
                // unknown fields are ignored).
                if version > PROTOCOL_VERSION {
                    return Err(ProtocolError::ServerError {
                        code: ProtocolErrorCode::Version,
                        message: format!(
                            "server speaks protocol {version}, but this client speaks {PROTOCOL_VERSION} — upgrade the client"
                        ),
                    });
                }
                (connection_id, snapshot, capabilities)
            }
            ServerMessage::HelloError { error } => {
                return Err(ProtocolError::ServerError {
                    code: error.code,
                    message: error.message,
                });
            }
            _ => return Err(ProtocolError::CborDecode("expected server hello".into())),
        };

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<ServerMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(256);
        let closed = Arc::new(AtomicBool::new(false));

        let client = Arc::new(RemoteClient {
            writer: writer.clone(),
            pending: pending.clone(),
            events: events.clone(),
            next_id: AtomicU64::new(1),
            request_timeout_ms: AtomicU64::new(DEFAULT_REQUEST_TIMEOUT.as_millis() as u64),
            closed: closed.clone(),
            pump: std::sync::Mutex::new(None),
            connection_id,
            snapshot,
            server_capabilities,
        });

        let pump = tokio::spawn(async move {
            loop {
                let message: Option<ServerMessage> = match read_frame(&mut reader).await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let Some(message) = message else { break };
                match &message {
                    ServerMessage::Response { id, .. } => {
                        if let Some(tx) = pending.lock().await.remove(id) {
                            let _ = tx.send(message);
                        }
                    }
                    ServerMessage::Event { event } => {
                        let _ = events.send(event.clone());
                    }
                    _ => {}
                }
            }
            // Connection ended: mark closed, then drop every pending sender
            // so in-flight requests resolve with an error instead of hanging
            // forever. The flag is set BEFORE the drain so a request that
            // inserts after the drain still sees `closed` and fails fast.
            closed.store(true, Ordering::SeqCst);
            pending.lock().await.clear();
        });
        *client.pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(pump);

        Ok(client)
    }

    /// Override the per-request timeout (default
    /// [`DEFAULT_REQUEST_TIMEOUT`]).
    pub fn set_request_timeout(&self, timeout: Duration) {
        self.request_timeout_ms.store(
            u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Send a command and await its result, bounded by the per-request
    /// timeout (default 30s). On timeout the pending entry is removed so a
    /// late response is dropped instead of leaking.
    pub async fn request(&self, command: Command) -> Result<CommandResult, ProtocolError> {
        let id = format!("req-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        if self.closed.load(Ordering::SeqCst) {
            self.pending.lock().await.remove(&id);
            return Err(ProtocolError::ConnectionClosed);
        }
        // Bound the write with the same request timeout: a server that
        // stops reading (or a half-open socket with full buffers) would
        // otherwise pend write_all forever while holding the writer mutex
        // — convoying every later request behind it, and the response
        // timeout below would never fire because it starts after the
        // write.
        let timeout = Duration::from_millis(self.request_timeout_ms.load(Ordering::Relaxed));
        let written = tokio::time::timeout(
            timeout,
            write_frame(
                &mut *self.writer.lock().await,
                &ClientMessage::Request {
                    id: id.clone(),
                    request: command,
                },
            ),
        )
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Don't leak the pending entry on a failed write.
                self.pending.lock().await.remove(&id);
                return Err(e);
            }
            Err(_elapsed) => {
                self.pending.lock().await.remove(&id);
                return Err(ProtocolError::Timeout);
            }
        }
        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(response) => response.map_err(|_| ProtocolError::ConnectionClosed)?,
            Err(_elapsed) => {
                self.pending.lock().await.remove(&id);
                return Err(ProtocolError::Timeout);
            }
        };
        match response {
            ServerMessage::Response {
                ok: true,
                result: Some(result),
                ..
            } => Ok(result),
            ServerMessage::Response {
                error: Some(error), ..
            } => Err(ProtocolError::ServerError {
                code: error.code,
                message: error.message,
            }),
            _ => Err(ProtocolError::CborDecode("malformed response".into())),
        }
    }

    /// Subscribe to server events.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn snapshot() -> ServerSnapshot {
        ServerSnapshot {
            server_id: "test-server".to_string(),
            protocol_version: PROTOCOL_VERSION,
            revision: 0,
            sessions: vec![],
            models: vec![],
        }
    }

    /// Run the server side of the handshake, then handle the connection
    /// according to `behavior` (called with the stream after the hello).
    async fn server_handshake(stream: &mut tokio::io::DuplexStream) {
        server_handshake_with_version(stream, PROTOCOL_VERSION).await;
    }

    /// [`server_handshake`] answering the hello with an explicit protocol
    /// version (for version-negotiation tests).
    async fn server_handshake_with_version(stream: &mut tokio::io::DuplexStream, version: u32) {
        let hello: Option<ClientMessage> = read_frame(stream).await.expect("client hello");
        assert!(matches!(hello, Some(ClientMessage::Hello { .. })));
        write_frame(
            stream,
            &ServerMessage::Hello {
                version,
                connection_id: "conn-1".to_string(),
                snapshot: snapshot(),
                capabilities: SERVER_CAPABILITIES.iter().map(|s| s.to_string()).collect(),
            },
        )
        .await
        .unwrap();
    }

    /// A server speaking a NEWER protocol than the client must be
    /// rejected at the handshake with the reserved Version error code —
    /// it may rely on behavior this client does not implement.
    #[tokio::test]
    async fn connect_rejects_newer_server_protocol() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            server_handshake_with_version(&mut server_stream, PROTOCOL_VERSION + 1).await;
        });

        let err = RemoteClient::connect(client_stream, None, Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ProtocolError::ServerError {
                    code: ProtocolErrorCode::Version,
                    ..
                }
            ),
            "{err:?}"
        );
        server.await.unwrap();
    }

    /// Older (and equal) server protocol versions connect fine: the wire
    /// format is additive and unknown fields are ignored.
    #[tokio::test]
    async fn connect_accepts_older_or_equal_server_protocol() {
        for version in [PROTOCOL_VERSION, 0] {
            let (client_stream, mut server_stream) = tokio::io::duplex(8192);
            let server = tokio::spawn(async move {
                server_handshake_with_version(&mut server_stream, version).await;
            });
            let client = RemoteClient::connect(client_stream, None, Vec::new()).await;
            assert!(client.is_ok(), "version {version} must connect");
            server.await.unwrap();
        }
    }

    /// Dropping the last client Arc must abort the read pump and tear the
    /// connection down: the server observes EOF promptly instead of the
    /// pump blocking in `read_frame` forever on a socket nobody serves.
    #[tokio::test]
    async fn dropping_client_aborts_read_pump_and_closes_connection() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            server_handshake(&mut server_stream).await;
            // With the client dropped, its writer and the aborted pump's
            // reader are gone: reads on the server side hit EOF.
            let mut buf = Vec::new();
            for _ in 0..100 {
                match tokio::io::AsyncReadExt::read(&mut server_stream, &mut [0u8; 64]).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                buf.push(0u8);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            buf.len()
        });

        let client = RemoteClient::connect(client_stream, None, Vec::new())
            .await
            .unwrap();
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("server task hung")
            .expect("server task panicked");
    }

    /// A request whose connection dies before the response must fail fast,
    /// not hang awaiting a response that will never arrive.
    #[tokio::test]
    async fn pending_request_fails_when_connection_drops() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            server_handshake(&mut server_stream).await;
            // Read one request, then drop the connection without answering.
            let _: Option<ClientMessage> = read_frame(&mut server_stream).await.unwrap();
        });

        let client = RemoteClient::connect(client_stream, None, Vec::new())
            .await
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.request(Command::List),
        )
        .await
        .expect("request must not hang after the connection drops");
        assert!(result.is_err(), "dropped connection must surface an error");
        server.await.unwrap();
    }

    /// Requests issued after the connection is already dead must fail fast
    /// (not wait on a receiver no pump will ever resolve).
    #[tokio::test]
    async fn request_after_close_fails_fast() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            server_handshake(&mut server_stream).await;
            // Drop immediately.
        });

        let client = RemoteClient::connect(client_stream, None, Vec::new())
            .await
            .unwrap();
        server.await.unwrap();
        // Give the read pump a moment to observe EOF.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.request(Command::List),
        )
        .await
        .expect("request on a dead connection must not hang");
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ProtocolError::ConnectionClosed),
            "dead connection must report ConnectionClosed"
        );
    }

    /// A server that never answers must not hang the request forever: the
    /// configurable timeout fires, the error is Timeout, and the pending
    /// entry is cleaned up (a subsequent request still works).
    #[tokio::test]
    async fn unanswered_request_times_out_and_cleans_up() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            server_handshake(&mut server_stream).await;
            // Read the first request and never answer it; answer the second.
            let first: Option<ClientMessage> = read_frame(&mut server_stream).await.unwrap();
            let Some(ClientMessage::Request { .. }) = first else {
                panic!("expected first request")
            };
            let second: Option<ClientMessage> = read_frame(&mut server_stream).await.unwrap();
            let Some(ClientMessage::Request { id, .. }) = second else {
                panic!("expected second request")
            };
            write_frame(
                &mut server_stream,
                &ServerMessage::ok(id, CommandResult::List { sessions: vec![] }),
            )
            .await
            .unwrap();
        });

        let client = RemoteClient::connect(client_stream, None, Vec::new())
            .await
            .unwrap();
        client.set_request_timeout(std::time::Duration::from_millis(100));

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.request(Command::List),
        )
        .await
        .expect("request must resolve via its own timeout")
        .unwrap_err();
        assert!(matches!(err, ProtocolError::Timeout), "{err:?}");

        // The timed-out pending entry was removed: the next request is
        // answered normally (its id is not confused with the stale one).
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.request(Command::List),
        )
        .await
        .expect("second request must not hang")
        .unwrap();
        assert!(matches!(result, CommandResult::List { .. }));
        server.await.unwrap();
    }

    /// A business-level error from the server surfaces as ServerError with
    /// the server's code/message, not as a codec failure.
    #[tokio::test]
    async fn server_business_error_has_dedicated_variant() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            server_handshake(&mut server_stream).await;
            let request: Option<ClientMessage> = read_frame(&mut server_stream).await.unwrap();
            let Some(ClientMessage::Request { id, .. }) = request else {
                panic!("expected request")
            };
            write_frame(
                &mut server_stream,
                &ServerMessage::err(id, ProtocolErrorCode::Busy, "session is busy"),
            )
            .await
            .unwrap();
        });

        let client = RemoteClient::connect(client_stream, None, Vec::new())
            .await
            .unwrap();
        let err = client.request(Command::List).await.unwrap_err();
        let ProtocolError::ServerError { code, message } = err else {
            panic!("expected ServerError, got {err:?}")
        };
        assert_eq!(code, ProtocolErrorCode::Busy);
        assert_eq!(message, "session is busy");
        server.await.unwrap();
    }

    /// A hello_error at the handshake is a ServerError too (was CborDecode).
    #[tokio::test]
    async fn hello_error_is_server_error_not_cbor_decode() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            let hello: Option<ClientMessage> = read_frame(&mut server_stream).await.unwrap();
            assert!(matches!(hello, Some(ClientMessage::Hello { .. })));
            write_frame(
                &mut server_stream,
                &ServerMessage::HelloError {
                    error: crate::schemas::ProtocolError {
                        code: ProtocolErrorCode::Auth,
                        message: "bad token".to_string(),
                        details: None,
                    },
                },
            )
            .await
            .unwrap();
        });

        let err = RemoteClient::connect(client_stream, None, Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ProtocolError::ServerError {
                    code: ProtocolErrorCode::Auth,
                    ..
                }
            ),
            "{err:?}"
        );
        server.await.unwrap();
    }
}
