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
use tokio::sync::{Mutex as AsyncMutex, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::process::{OverCap, read_line_bounded};
use crate::rpc3::{
    ERR_INTERNAL, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND, ERR_PARSE, ERR_PLUGIN_UNAVAILABLE,
    ERR_REQUEST_TIMEOUT, ErrorObject, Id, Notification, Request, Response,
};

/// Default bound on one outgoing request (mirrors the v1 protocol).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on a single write (a stalled reader must fail calls, not hang).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// `$/cancelRequest` notification (LSP convention): aborts the in-flight
/// handler task for the given id; the aborted side sends no response.
pub const CANCEL_METHOD: &str = "$/cancelRequest";

/// Bound on concurrent INBOUND request/notification handler tasks. A
/// well-behaved plugin never approaches the low hundreds; beyond that the
/// remote is flooding us, and spawning/queuing unboundedly would be a
/// memory and CPU DoS against the host — exhaustion is treated as
/// protocol abuse and fails closed (the peer is killed, not queued).
const MAX_INFLIGHT_INBOUND: usize = 256;

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
    /// The call was cancelled locally (via [`JsonRpcPeer::cancel`] or a
    /// cancellation token). Note [`PeerError::code`] deliberately aliases
    /// this to `ERR_REQUEST_TIMEOUT`: the rpc3 schema is generated and
    /// defines no distinct cancellation code, so callers must match the
    /// VARIANT — not the code — to tell local cancellation apart from a
    /// real timeout.
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
            // Deliberate alias of Timeout: rpc3.rs is generated (no
            // hand-edits, no new codes) and has no cancellation code.
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
    /// Read pump; aborted by `mark_dead` so a write-side death (timeout,
    /// flood kill) tears down a pump parked in a read.
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Liveness broadcast behind `wait_dead`: flipped by `mark_dead` so
    /// EVERY concurrent waiter observes the real death (a taken
    /// JoinHandle would only ever resolve the first caller).
    dead: watch::Sender<bool>,
    /// Permits for inbound request/notification handler tasks; see
    /// [`MAX_INFLIGHT_INBOUND`].
    inbound_permits: Arc<tokio::sync::Semaphore>,
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
        let (dead, _) = watch::channel(false);
        let peer = Arc::new(JsonRpcPeer {
            writer: AsyncMutex::new(Box::new(writer)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            alive: Arc::new(AtomicBool::new(true)),
            write_timeout_ms: AtomicU64::new(WRITE_TIMEOUT.as_millis() as u64),
            pump: Mutex::new(None),
            dead,
            inbound_permits: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_INBOUND)),
        });
        let pump_peer = peer.clone();
        let handle = tokio::spawn(async move { pump_peer.read_pump(reader, handler).await });
        *peer.pump.lock().expect("pump mutex") = Some(handle);
        peer
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Wait until the peer dies (EOF, IO error, flood kill). Safe for
    /// any number of concurrent callers: liveness rides a watch channel
    /// flipped by `mark_dead`, so every waiter observes the real death —
    /// not just the first caller to take a JoinHandle.
    pub async fn wait_dead(&self) {
        let mut rx = self.dead.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
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
        let (id, rx, _cleanup) = self.begin_call(method, params).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(outcome) => Self::resolve(outcome),
            Err(_) => {
                self.send_cancel(&id).await;
                Err(PeerError::Timeout)
            }
        }
    }

    /// Call with NO wall-clock timeout, bound to a [`CancellationToken`]
    /// instead: on cancel the remote is told to abort via
    /// `$/cancelRequest` carrying the real allocated id, and the waiter
    /// fails with [`PeerError::Cancelled`]. For long-running work (tool
    /// execution — builds, test suites) where `REQUEST_TIMEOUT` would
    /// kill legitimate runs; keep `call`/`call_with_timeout` for
    /// everything else.
    pub async fn call_cancellable(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, PeerError> {
        let (id, rx, _cleanup) = self.begin_call(method, params).await?;
        tokio::select! {
            _ = cancel.cancelled() => {
                self.send_cancel(&id).await;
                Err(PeerError::Cancelled)
            }
            outcome = rx => Self::resolve(outcome),
        }
    }

    /// Shared prologue of an outgoing call: liveness check, id
    /// allocation, pending-map registration (the returned guard removes
    /// the entry if the waiter is dropped mid-flight), request write.
    async fn begin_call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Id, oneshot::Receiver<PendingOutcome>, PendingCleanup), PeerError> {
        if !self.is_alive() {
            return Err(PeerError::Dead);
        }
        let id = Id::Num(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending mutex")
            .insert(id.clone(), tx);
        let cleanup = PendingCleanup {
            pending: self.pending.clone(),
            id: id.clone(),
        };
        let request = Request::new(id.clone(), method, params);
        let line =
            serde_json::to_string(&request).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.write_line(&line).await?;
        Ok((id, rx, cleanup))
    }

    /// Interpret a resolved pending receiver uniformly across the
    /// timeout/cancellation wrappers.
    fn resolve(
        outcome: Result<PendingOutcome, oneshot::error::RecvError>,
    ) -> Result<Value, PeerError> {
        match outcome {
            Ok(PendingOutcome::Response(Ok(result))) => Ok(result),
            Ok(PendingOutcome::Response(Err(error))) => Err(PeerError::Remote(error)),
            Ok(PendingOutcome::Cancelled) => Err(PeerError::Cancelled),
            // Sender dropped without answering: the pump died.
            Err(_) => Err(PeerError::Dead),
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
        match tokio::time::timeout(timeout, write).await {
            Ok(Ok(())) => Ok(()),
            // A timed-out or failed write can leave a PARTIAL NDJSON
            // frame in the pipe: framing is permanently desynchronized,
            // so the peer is dead — every caller must fail fast, not
            // just this one.
            Err(_) => {
                self.mark_dead();
                Err(PeerError::Timeout)
            }
            Ok(Err(e)) => {
                self.mark_dead();
                Err(PeerError::Transport(e.to_string()))
            }
        }
    }

    fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
        // Wake every wait_dead caller FIRST: liveness must land no
        // matter which path detected the death. send_replace (not send)
        // because it stores the value even with zero receivers — a
        // waiter subscribing after death must still observe it.
        self.dead.send_replace(true);
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        if let Ok(mut inflight) = self.inflight.lock() {
            for (_, handle) in inflight.drain() {
                handle.abort();
            }
        }
        // Abort the read pump when death was detected write-side or by
        // the flood guard: a pump parked in a read would otherwise
        // linger forever. On the EOF path the pump calls this itself and
        // aborting its own (finishing) handle is a harmless no-op.
        if let Some(handle) = self.pump.lock().expect("pump mutex").take() {
            handle.abort();
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
        // Distinguish "id absent" (notification) from "id present but not
        // a legal JSON-RPC id" (invalid request): `Id` only accepts a
        // u64 or a string, so negatives/floats/bools/null land in Err.
        let id = message
            .get("id")
            .map(|v| serde_json::from_value::<Id>(v.clone()));
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            // Incoming request: answer in a spawned task (tracked for
            // cancellation).
            (Some(method), Some(Ok(id))) => {
                // Bounded inbound concurrency: a hostile plugin flooding
                // requests must not spawn unbounded tasks. Exhaustion is
                // protocol abuse — fail closed (kill the peer) rather
                // than queue without bound.
                let Ok(permit) = self.inbound_permits.clone().try_acquire_owned() else {
                    tracing::warn!(
                        "inbound request flood exceeded {MAX_INFLIGHT_INBOUND}; killing peer"
                    );
                    self.mark_dead();
                    return;
                };
                {
                    let mut inflight = self.inflight.lock().expect("inflight mutex");
                    // Reap finished handles BEFORE the duplicate check so
                    // a completed-but-unreaped task is not mistaken for
                    // a live collision (see the race note below).
                    inflight.retain(|_, handle| !handle.is_finished());
                    // A duplicate id would overwrite the inflight entry
                    // and corrupt cancellation: reject the newcomer, keep
                    // the original task.
                    if inflight.contains_key(&id) {
                        drop(inflight);
                        let peer = self.clone();
                        tokio::spawn(async move {
                            let response = Response::error(
                                Some(id),
                                ERR_INVALID_REQUEST,
                                "duplicate request id",
                            );
                            if let Ok(line) = serde_json::to_string(&response) {
                                let _ = peer.write_line(&line).await;
                            }
                        });
                        return;
                    }
                }
                let peer = self.clone();
                let handler = handler.clone();
                let method = method.to_string();
                let task_id = id.clone();
                let task = tokio::spawn(async move {
                    // The permit is held for the handler's whole life.
                    let _permit = permit;
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
            // Method with an id that is present but unparseable
            // (negative, float, bool, null): Invalid Request per spec,
            // answered with a null id — the offending id cannot be
            // echoed back because it is not representable.
            (Some(_), Some(Err(_))) => {
                let response = Response::error(None, ERR_INVALID_REQUEST, "invalid request id");
                if let Ok(line) = serde_json::to_string(&response) {
                    let peer = self.clone();
                    tokio::spawn(async move {
                        let _ = peer.write_line(&line).await;
                    });
                }
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
                // Same inbound bound as requests: notifications spawn
                // handler tasks too, so an unbounded flood here is just
                // as much a DoS.
                let Ok(permit) = self.inbound_permits.clone().try_acquire_owned() else {
                    tracing::warn!(
                        "inbound notification flood exceeded {MAX_INFLIGHT_INBOUND}; killing peer"
                    );
                    self.mark_dead();
                    return;
                };
                let handler = handler.clone();
                let method = method.to_string();
                tokio::spawn(async move {
                    let _permit = permit;
                    handler.handle_notification(&method, params).await;
                });
            }
            // Incoming response: complete the pending call.
            (None, Some(Ok(id))) => {
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
            (None, _) => {}
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

    /// A failed write leaves a partial NDJSON frame in the pipe, so the
    /// peer must die: the failing call errors AND every later call fails
    /// fast with Dead instead of writing into a desynchronized stream.
    #[tokio::test]
    async fn failed_write_marks_peer_dead() {
        // Reader side: the far writer is held open, so the pump never
        // observes EOF on its own.
        let (read_half, _dangling_writer) = tokio::io::duplex(1024);
        // Writer side: the far reader is dropped, so every write fails.
        let (dropped_reader, write_half) = tokio::io::duplex(1024);
        drop(dropped_reader);
        let peer = JsonRpcPeer::new(read_half, write_half, Arc::new(Echo));
        let err = peer.call("echo", Value::Null).await.unwrap_err();
        assert!(
            matches!(err, PeerError::Transport(_) | PeerError::Timeout),
            "the failing call surfaces the write failure: {err:?}"
        );
        assert!(!peer.is_alive(), "a failed write must kill the peer");
        let err = peer.call("echo", Value::Null).await.unwrap_err();
        assert!(
            matches!(err, PeerError::Dead),
            "subsequent calls fail fast: {err:?}"
        );
        // wait_dead resolves even though the pump never saw EOF.
        tokio::time::timeout(Duration::from_secs(5), peer.wait_dead())
            .await
            .expect("wait_dead must observe the write-side death");
    }

    /// An id that is present but not a legal JSON-RPC id (negative,
    /// float) is Invalid Request (-32600) with a null id — not a silent
    /// downgrade to a notification.
    #[tokio::test]
    async fn unparseable_id_gets_invalid_request_response() {
        let (s1, s2) = tokio::io::duplex(64 * 1024);
        let (r1, w1) = tokio::io::split(s1);
        let (mut r2, mut w2) = tokio::io::split(s2);
        let _peer = JsonRpcPeer::new(r1, w1, Arc::new(Echo));
        w2.write_all(br#"{"jsonrpc":"2.0","id":-3,"method":"echo"}"#)
            .await
            .unwrap();
        w2.write_all(b"\n").await.unwrap();
        w2.write_all(br#"{"jsonrpc":"2.0","id":1.5,"method":"echo"}"#)
            .await
            .unwrap();
        w2.write_all(b"\n").await.unwrap();
        w2.flush().await.unwrap();
        let mut reader = tokio::io::BufReader::new(&mut r2);
        let mut buf = Vec::new();
        for _ in 0..2 {
            let line = read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap()
                .unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                response["error"]["code"],
                crate::rpc3::ERR_INVALID_REQUEST,
                "{response}"
            );
            // The offending id cannot be echoed back (not representable).
            assert!(response["id"].is_null(), "{response}");
        }
    }

    /// A duplicate request id must be rejected (Invalid Request) without
    /// disturbing the original in-flight task.
    #[tokio::test]
    async fn duplicate_request_id_is_rejected_and_original_survives() {
        /// "hold" blocks until released; anything else echoes.
        struct Gate(Arc<tokio::sync::Notify>);
        #[async_trait::async_trait]
        impl PeerHandler for Gate {
            async fn handle_request(
                &self,
                method: &str,
                params: Value,
            ) -> Result<Value, ErrorObject> {
                match method {
                    "hold" => {
                        self.0.notified().await;
                        Ok(serde_json::json!("held"))
                    }
                    other => Echo.handle_request(other, params).await,
                }
            }
        }
        let (s1, s2) = tokio::io::duplex(64 * 1024);
        let (r1, w1) = tokio::io::split(s1);
        let (mut r2, mut w2) = tokio::io::split(s2);
        let release = Arc::new(tokio::sync::Notify::new());
        let _peer = JsonRpcPeer::new(r1, w1, Arc::new(Gate(release.clone())));
        // The pump dispatches lines in order and registers inflight
        // synchronously, so no sleep is needed between the two sends.
        w2.write_all(br#"{"jsonrpc":"2.0","id":1,"method":"hold"}"#)
            .await
            .unwrap();
        w2.write_all(b"\n").await.unwrap();
        w2.write_all(br#"{"jsonrpc":"2.0","id":1,"method":"echo","params":{"x":1}}"#)
            .await
            .unwrap();
        w2.write_all(b"\n").await.unwrap();
        w2.flush().await.unwrap();
        let mut reader = tokio::io::BufReader::new(&mut r2);
        let mut buf = Vec::new();
        // The duplicate is rejected immediately; the original still runs.
        let line = read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
            .await
            .unwrap()
            .unwrap();
        let rejection: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            rejection["error"]["code"],
            crate::rpc3::ERR_INVALID_REQUEST,
            "{rejection}"
        );
        assert_eq!(rejection["id"], 1);
        // Releasing the gate lets the ORIGINAL request complete normally.
        release.notify_one();
        let line = read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
            .await
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["result"], "held", "{response}");
        assert_eq!(response["id"], 1);
    }

    /// An unbounded inbound notification flood is protocol abuse: the
    /// peer fails closed instead of spawning without bound.
    #[tokio::test]
    async fn notification_flood_kills_peer() {
        struct Sleepy;
        #[async_trait::async_trait]
        impl PeerHandler for Sleepy {
            async fn handle_notification(&self, _method: &str, _params: Value) {
                // Hold the permit long enough for the flood to exhaust it.
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
        let (s1, s2) = tokio::io::duplex(64 * 1024);
        let (r1, w1) = tokio::io::split(s1);
        let (_r2, mut w2) = tokio::io::split(s2);
        let peer = JsonRpcPeer::new(r1, w1, Arc::new(Sleepy));
        let mut payload = String::new();
        for i in 0..(MAX_INFLIGHT_INBOUND + 64) {
            payload.push_str(&format!("{{\"jsonrpc\":\"2.0\",\"method\":\"n{i}\"}}\n"));
        }
        w2.write_all(payload.as_bytes()).await.unwrap();
        w2.flush().await.unwrap();
        for _ in 0..100 {
            if !peer.is_alive() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("inbound flood did not kill the peer");
    }

    /// Every concurrent wait_dead caller must observe the real death,
    /// not just the first to take the pump handle.
    #[tokio::test]
    async fn concurrent_wait_dead_callers_all_observe_death() {
        let (s1, s2) = tokio::io::duplex(1024);
        let (r1, w1) = tokio::io::split(s1);
        let peer = JsonRpcPeer::new(r1, w1, Arc::new(Echo));
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let peer = peer.clone();
            waiters.push(tokio::spawn(async move { peer.wait_dead().await }));
        }
        // Let all waiters subscribe before the death lands.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(s2);
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("a wait_dead caller hung")
                .unwrap();
        }
        assert!(!peer.is_alive());
    }
}
