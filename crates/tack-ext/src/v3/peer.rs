//! Transport-agnostic JSON-RPC 2.0 peer for tack-RPC v3.
//!
//! One peer per plugin, over any `AsyncRead`/`AsyncWrite` pair (process
//! stdio, in-memory duplex in tests, later the WIT debug carrier). Both
//! sides may issue requests concurrently; ids are per-sender. Incoming
//! requests are dispatched to a [`PeerHandler`]; outgoing requests
//! correlate responses through a pending map with cancel-safe cleanup
//! (same pattern as the v1 `PluginPeer`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use crate::process::{OverCap, read_line_bounded};
use crate::rpc3::{
    ERR_INTERNAL, ERR_METHOD_NOT_FOUND, ERR_PARSE, ERR_PLUGIN_UNAVAILABLE, ERR_REQUEST_TIMEOUT,
    ErrorObject, Id, Notification, Request, Response,
};

/// Default bound on one outgoing request (mirrors the v1 protocol).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on a single write (a stalled reader must fail calls, not hang).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// `$/cancelRequest` notification (LSP convention): aborts the in-flight
/// handler task for the given id; the aborted side sends no response.
pub const CANCEL_METHOD: &str = "$/cancelRequest";

type PendingMap = Arc<Mutex<HashMap<Id, oneshot::Sender<PendingOutcome>>>>;

/// What resolves a pending outgoing call: the remote's response, or a
/// local cancellation (distinct so callers can tell "cancelled here"
/// apart from "the remote answered with an error").
enum PendingOutcome {
    Response(Result<Value, ErrorObject>),
    Cancelled,
}

/// Removes the pending entry on drop: cancel-safe cleanup for callers
/// that drop a `call` future mid-flight.
struct PendingCleanup {
    pending: PendingMap,
    id: Id,
}

impl Drop for PendingCleanup {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

/// Incoming-request/notification dispatch. Implementations must be cheap
/// to clone (the peer spawns one task per request).
#[async_trait::async_trait]
pub trait PeerHandler: Send + Sync {
    /// Handle an incoming request; the returned value becomes the JSON-RPC
    /// result. Default: method-not-found.
    async fn handle_request(&self, method: &str, _params: Value) -> Result<Value, ErrorObject> {
        Err(ErrorObject {
            code: ERR_METHOD_NOT_FOUND,
            message: format!("unknown method {method}"),
            data: None,
        })
    }

    /// Handle an incoming notification. Default: ignore (protocol
    /// tolerance rule — unknown notifications are not errors).
    async fn handle_notification(&self, _method: &str, _params: Value) {}
}

/// Why an outgoing call failed.
#[derive(Debug)]
pub enum PeerError {
    /// The peer is dead (EOF, trap, over-cap line) or died mid-call.
    Dead,
    /// The call exceeded its timeout; a best-effort `$/cancelRequest`
    /// was sent.
    Timeout,
    /// The call was cancelled locally via [`JsonRpcPeer::cancel`].
    Cancelled,
    /// Transport/serialization failure.
    Transport(String),
    /// The remote answered with a JSON-RPC error (code preserved — domain
    /// codes like `ERR_POLICY_DENIED` survive the trip).
    Remote(ErrorObject),
}

impl PeerError {
    /// The JSON-RPC error code, when the failure has one.
    pub fn code(&self) -> i64 {
        match self {
            PeerError::Dead => ERR_PLUGIN_UNAVAILABLE,
            PeerError::Timeout => ERR_REQUEST_TIMEOUT,
            PeerError::Cancelled => ERR_REQUEST_TIMEOUT,
            PeerError::Transport(_) => ERR_INTERNAL,
            PeerError::Remote(error) => error.code,
        }
    }
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::Dead => write!(f, "peer is unavailable"),
            PeerError::Timeout => write!(f, "request timed out"),
            PeerError::Cancelled => write!(f, "request cancelled"),
            PeerError::Transport(e) => write!(f, "transport error: {e}"),
            PeerError::Remote(e) => write!(f, "remote error {}: {}", e.code, e.message),
        }
    }
}

impl std::error::Error for PeerError {}

/// A live JSON-RPC 2.0 connection. Cheap to clone (Arc inside); all
/// clones share the transport and pending map.
pub struct JsonRpcPeer {
    writer: AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: PendingMap,
    /// In-flight INCOMING request handler tasks, for `$/cancelRequest`.
    inflight: Arc<Mutex<HashMap<Id, tokio::task::JoinHandle<()>>>>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    write_timeout_ms: AtomicU64,
    /// Read pump; awaited by `wait_dead` so liveness is deterministic.
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for JsonRpcPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonRpcPeer")
            .field("alive", &self.alive.load(Ordering::SeqCst))
            .finish()
    }
}

impl JsonRpcPeer {
    /// Wire a peer over a raw transport and spawn the read pump.
    pub fn new<R, W>(reader: R, writer: W, handler: Arc<dyn PeerHandler>) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let peer = Arc::new(JsonRpcPeer {
            writer: AsyncMutex::new(Box::new(writer)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            alive: Arc::new(AtomicBool::new(true)),
            write_timeout_ms: AtomicU64::new(WRITE_TIMEOUT.as_millis() as u64),
            pump: Mutex::new(None),
        });
        let pump_peer = peer.clone();
        let handle = tokio::spawn(async move { pump_peer.read_pump(reader, handler).await });
        *peer.pump.lock().expect("pump mutex") = Some(handle);
        peer
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Wait until the read pump observes EOF. Idempotent.
    pub async fn wait_dead(&self) {
        let handle = self.pump.lock().expect("pump mutex").take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Call a method and wait for the result (default timeout).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, PeerError> {
        self.call_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    /// Call a method with an explicit timeout.
    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, PeerError> {
        if !self.is_alive() {
            return Err(PeerError::Dead);
        }
        let id = Id::Num(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending mutex")
            .insert(id.clone(), tx);
        let _cleanup = PendingCleanup {
            pending: self.pending.clone(),
            id: id.clone(),
        };
        let request = Request::new(id.clone(), method, params);
        let line =
            serde_json::to_string(&request).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.write_line(&line).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(PendingOutcome::Response(Ok(result)))) => Ok(result),
            Ok(Ok(PendingOutcome::Response(Err(error)))) => Err(PeerError::Remote(error)),
            Ok(Ok(PendingOutcome::Cancelled)) => Err(PeerError::Cancelled),
            // Sender dropped without answering: the pump died.
            Ok(Err(_)) => Err(PeerError::Dead),
            Err(_) => {
                self.send_cancel(&id).await;
                Err(PeerError::Timeout)
            }
        }
    }

    /// Send a notification (fire-and-forget; a dead peer fails the write).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), PeerError> {
        if !self.is_alive() {
            return Err(PeerError::Dead);
        }
        let notification = Notification::new(method, params);
        let line = serde_json::to_string(&notification)
            .map_err(|e| PeerError::Transport(e.to_string()))?;
        self.write_line(&line).await
    }

    /// Cancel an outgoing call: the waiter fails with
    /// [`PeerError::Cancelled`] and the remote is asked (best-effort) to
    /// abort the corresponding handler.
    pub async fn cancel(&self, id: &Id) {
        if let Some(tx) = self.pending.lock().expect("pending mutex").remove(id) {
            let _ = tx.send(PendingOutcome::Cancelled);
        }
        let _ = self
            .notify(CANCEL_METHOD, serde_json::json!({ "id": id }))
            .await;
    }

    async fn send_cancel(&self, id: &Id) {
        let _ = self
            .notify(CANCEL_METHOD, serde_json::json!({ "id": id }))
            .await;
    }

    async fn write_line(&self, line: &str) -> Result<(), PeerError> {
        let timeout = Duration::from_millis(self.write_timeout_ms.load(Ordering::Relaxed));
        let mut writer = self.writer.lock().await;
        let write = async {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await
        };
        tokio::time::timeout(timeout, write)
            .await
            .map_err(|_| PeerError::Timeout)?
            .map_err(|e| {
                if self.is_alive() {
                    PeerError::Transport(e.to_string())
                } else {
                    PeerError::Dead
                }
            })
    }

    fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        if let Ok(mut inflight) = self.inflight.lock() {
            for (_, handle) in inflight.drain() {
                handle.abort();
            }
        }
    }

    async fn read_pump<R>(self: Arc<Self>, mut reader: R, handler: Arc<dyn PeerHandler>)
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let mut reader = tokio::io::BufReader::new(&mut reader);
        let mut buf = Vec::new();
        loop {
            let line = match read_line_bounded(&mut reader, &mut buf, OverCap::Fail).await {
                Ok(Some(line)) => line,
                Ok(None) => break, // clean EOF
                Err(_) => break,   // IO error or over-cap line
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                // Not even JSON: protocol parse error, answered per spec.
                let response = Response::error(None, ERR_PARSE, "parse error");
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = self.write_line(&line).await;
                }
                continue;
            };
            self.dispatch(message, &handler);
        }
        self.mark_dead();
    }

    fn dispatch(self: &Arc<Self>, message: Value, handler: &Arc<dyn PeerHandler>) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message
            .get("id")
            .and_then(|v| serde_json::from_value::<Id>(v.clone()).ok());
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            // Incoming request: answer in a spawned task (tracked for
            // cancellation).
            (Some(method), Some(id)) => {
                let peer = self.clone();
                let handler = handler.clone();
                let method = method.to_string();
                let task_id = id.clone();
                let task = tokio::spawn(async move {
                    let outcome = handler.handle_request(&method, params).await;
                    let response = match outcome {
                        Ok(result) => Response::result(Some(id.clone()), result),
                        Err(error) => Response {
                            jsonrpc: crate::rpc3::JSONRPC_VERSION.to_string(),
                            id: Some(id.clone()),
                            result: None,
                            error: Some(error),
                        },
                    };
                    if let Ok(line) = serde_json::to_string(&response) {
                        let _ = peer.write_line(&line).await;
                    }
                    peer.inflight.lock().expect("inflight mutex").remove(&id);
                });
                self.inflight
                    .lock()
                    .expect("inflight mutex")
                    .insert(task_id, task);
                // The spawned task removes itself from `inflight` on
                // completion — but on a multi-threaded runtime a fast
                // handler can finish (and run that removal) BEFORE the
                // insert above lands, leaving a completed handle parked
                // in the map forever. Reap finished handles on every
                // insert so the map stays bounded by live work.
                self.inflight
                    .lock()
                    .expect("inflight mutex")
                    .retain(|_, handle| !handle.is_finished());
            }
            // Incoming notification.
            (Some(method), None) => {
                if method == CANCEL_METHOD {
                    let cancel_id = message
                        .get("params")
                        .and_then(|p| p.get("id"))
                        .and_then(|v| serde_json::from_value::<Id>(v.clone()).ok());
                    if let Some(cancel_id) = cancel_id
                        && let Some(task) = self
                            .inflight
                            .lock()
                            .expect("inflight mutex")
                            .remove(&cancel_id)
                    {
                        task.abort();
                    }
                    return;
                }
                let handler = handler.clone();
                let method = method.to_string();
                tokio::spawn(async move {
                    handler.handle_notification(&method, params).await;
                });
            }
            // Incoming response: complete the pending call.
            (None, Some(id)) => {
                let outcome = if let Some(error) = message.get("error") {
                    match serde_json::from_value::<ErrorObject>(error.clone()) {
                        Ok(error) => Err(error),
                        Err(_) => Err(ErrorObject {
                            code: ERR_INTERNAL,
                            message: "malformed error object".to_string(),
                            data: None,
                        }),
                    }
                } else {
                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                };
                if let Some(tx) = self.pending.lock().expect("pending mutex").remove(&id) {
                    let _ = tx.send(PendingOutcome::Response(outcome));
                }
            }
            // Neither request, notification, nor response: ignore.
            (None, None) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Echo;

    #[async_trait::async_trait]
    impl PeerHandler for Echo {
        async fn handle_request(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
            match method {
                "echo" => Ok(params),
                "fail" => Err(ErrorObject {
                    code: -32001,
                    message: "denied by policy".to_string(),
                    data: None,
                }),
                "sleep" => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(Value::Null)
                }
                _ => Err(ErrorObject {
                    code: ERR_METHOD_NOT_FOUND,
                    message: format!("unknown method {method}"),
                    data: None,
                }),
            }
        }
    }

    struct EchoCounting(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl PeerHandler for EchoCounting {
        async fn handle_request(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
            Echo.handle_request(method, params).await
        }
        async fn handle_notification(&self, _method: &str, _params: Value) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A connected pair over one in-memory duplex. Both peers echo and
    /// count notifications.
    fn connected() -> (
        Arc<JsonRpcPeer>,
        Arc<JsonRpcPeer>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let (s1, s2) = tokio::io::duplex(64 * 1024);
        let (r1, w1) = tokio::io::split(s1);
        let (r2, w2) = tokio::io::split(s2);
        let notifications_a = Arc::new(AtomicUsize::new(0));
        let notifications_b = Arc::new(AtomicUsize::new(0));
        let a = JsonRpcPeer::new(r1, w1, Arc::new(EchoCounting(notifications_a.clone())));
        let b = JsonRpcPeer::new(r2, w2, Arc::new(EchoCounting(notifications_b.clone())));
        (a, b, notifications_a, notifications_b)
    }

    #[tokio::test]
    async fn request_response_roundtrip_both_directions() {
        let (a, b, _, _) = connected();
        let echoed = a.call("echo", serde_json::json!({"x": 1})).await.unwrap();
        assert_eq!(echoed, serde_json::json!({"x": 1}));
        let echoed = b.call("echo", serde_json::json!({"y": 2})).await.unwrap();
        assert_eq!(echoed, serde_json::json!({"y": 2}));
    }

    #[tokio::test]
    async fn remote_error_code_preserved() {
        let (a, _b, _, _) = connected();
        let err = a.call("fail", Value::Null).await.unwrap_err();
        let PeerError::Remote(error) = err else {
            panic!("expected remote error, got {err:?}")
        };
        assert_eq!(error.code, -32001);
        assert_eq!(error.message, "denied by policy");
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let (a, _b, _, _) = connected();
        let err = a.call("nope", Value::Null).await.unwrap_err();
        assert_eq!(err.code(), ERR_METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notification_reaches_handler() {
        // A's notification travels to B's handler.
        let (a, _b, _, notifications_b) = connected();
        a.notify("ping", serde_json::json!({})).await.unwrap();
        for _ in 0..50 {
            if notifications_b.load(Ordering::SeqCst) > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("notification not delivered");
    }

    #[tokio::test]
    async fn timeout_fails_call() {
        let (a, _b, _, _) = connected();
        let err = a
            .call_with_timeout("sleep", Value::Null, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(err, PeerError::Timeout));
    }

    #[tokio::test]
    async fn cancel_aborts_incoming_handler() {
        let cancelled = Arc::new(AtomicBool::new(false));
        struct SlowGuard(Arc<AtomicBool>);
        impl Drop for SlowGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct Slow(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl PeerHandler for Slow {
            async fn handle_request(
                &self,
                _method: &str,
                _params: Value,
            ) -> Result<Value, ErrorObject> {
                let _guard = SlowGuard(self.0.clone());
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(Value::Null)
            }
        }
        let (s1, s2) = tokio::io::duplex(64 * 1024);
        let (r1, w1) = tokio::io::split(s1);
        let (r2, w2) = tokio::io::split(s2);
        let a = JsonRpcPeer::new(r1, w1, Arc::new(Echo));
        let _b = JsonRpcPeer::new(r2, w2, Arc::new(Slow(cancelled.clone())));
        // A calls B's slow handler, then cancels the first-issued id.
        let call = tokio::spawn({
            let a = a.clone();
            async move { a.call("slow", Value::Null).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        a.cancel(&Id::Num(1)).await;
        let err = call.await.unwrap().unwrap_err();
        assert!(
            matches!(err, PeerError::Cancelled),
            "cancel surfaces the local cancellation: {err:?}"
        );
        for _ in 0..50 {
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("incoming handler was not aborted");
    }

    #[tokio::test]
    async fn dead_peer_fails_calls() {
        let (s1, s2) = tokio::io::duplex(1024);
        let (r1, w1) = tokio::io::split(s1);
        let a = JsonRpcPeer::new(r1, w1, Arc::new(Echo));
        drop(s2); // the remote end vanishes: EOF + EPIPE for A
        a.wait_dead().await;
        assert!(!a.is_alive());
        let err = a.call("echo", Value::Null).await.unwrap_err();
        assert!(matches!(err, PeerError::Dead));
    }

    #[tokio::test]
    async fn malformed_json_gets_parse_error_response() {
        let (s1, s2) = tokio::io::duplex(64 * 1024);
        let (r1, w1) = tokio::io::split(s1);
        let (mut r2, mut w2) = tokio::io::split(s2);
        let _a = JsonRpcPeer::new(r1, w1, Arc::new(Echo));
        w2.write_all(b"not json\n").await.unwrap();
        w2.flush().await.unwrap();
        let mut reader = tokio::io::BufReader::new(&mut r2);
        let mut buf = Vec::new();
        let line = read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
            .await
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["error"]["code"], ERR_PARSE);
        assert!(response["id"].is_null());
    }
}
