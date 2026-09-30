//! Codex Responses over WebSocket (port of the transport in
//! `api/openai-codex-responses.ts`): the same Responses API streamed over a
//! WS connection instead of HTTP SSE, with a per-session fallback record so a
//! host that can't do WS permanently falls back to SSE.
//!
//! Connection pooling / cached continuation mirror the TS implementation:
//! - One pooled connection per (session_id, base_url, auth fingerprint), kept
//!   idle for up to `IDLE_TTL_DEFAULT_MS` (TS `SESSION_WEBSOCKET_CACHE_TTL_MS`,
//!   5 min) and never reused past `MAX_CONNECTION_AGE` (TS
//!   `SESSION_WEBSOCKET_MAX_AGE_MS`, 55 min). Without a session id the
//!   connection is not pooled (TS: no `cacheSessionId` → no cache entry).
//! - A pooled connection is single-flight: while a request is in flight the
//!   entry is `busy` and concurrent requests open their own non-pooled
//!   connection (TS `cached.busy` branch).
//! - Cached continuation (TS `buildCachedWebSocketRequestBody`): after a
//!   successful response the connection remembers
//!   (last request body, response id, response output items). If the next
//!   request is identical except for appended input items, only the delta is
//!   sent with `previous_response_id`, reusing the server-side cached
//!   context. Applied for transport "auto" (TS also gates plain "websocket"
//!   out of cached context).
//! - Transparent recovery: a dead pooled connection is detected while idle
//!   (peer close) or on send (one reconnect attempt); a
//!   `previous_response_not_found` / `websocket_connection_limit_reached`
//!   error before any event was forwarded clears the continuation and
//!   retries once on a fresh connection with the full body (TS outer retry
//!   loop). If recovery fails the caller falls back to SSE as before.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_util::sync::CancellationToken;

/// Lock a global static `Mutex`, recovering from poisoning instead of
/// panicking. These statics (TRANSPORT / WS_FALLBACK_SESSIONS / POOL) live
/// for the whole process: if a request handler panicked while holding one,
/// every later `.lock().expect(...)` would panic too, cascading a single
/// failure into a permanently unusable connection pool. The guarded data
/// contains no invariants a panic can leave violated (a transport string,
/// a set of session ids, and pool entries that are fully re-validated by id
/// on every access), so taking the inner value is safe and keeps the pool
/// serviceable.
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Transport selection: "auto" (WS first, SSE fallback), "sse", "websocket".
static TRANSPORT: Mutex<String> = Mutex::new(String::new());

/// settings.transport applied process-wide (adapter reads it per call).
pub fn set_transport(mode: &str) {
    *lock_or_recover(&TRANSPORT) = mode.to_string();
}

fn transport_mode() -> String {
    let mode = lock_or_recover(&TRANSPORT).clone();
    if mode.is_empty() {
        "auto".to_string()
    } else {
        mode
    }
}

static WS_FALLBACK_SESSIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Cap on remembered WS-incapable sessions: this set lives for the whole
/// process and session ids are caller-controlled, so an unbounded set would
/// be a slow memory leak. When full, new ids simply aren't recorded (those
/// sessions retry WS each call — harmless).
const MAX_WS_FALLBACK_SESSIONS: usize = 1024;

/// Whether the Codex WS path should be attempted for this call.
pub fn wants_websocket(
    flavor: super::openai_responses::ResponsesFlavor,
    session_id: Option<&str>,
) -> bool {
    if flavor != super::openai_responses::ResponsesFlavor::Codex {
        return false;
    }
    if transport_mode() == "sse" {
        return false;
    }
    !session_id.is_some_and(|id| lock_or_recover(&WS_FALLBACK_SESSIONS).contains(id))
}

/// Mark this session as WS-incapable (subsequent calls go straight to SSE).
pub fn record_ws_fallback(session_id: Option<&str>) {
    if let Some(id) = session_id {
        let mut sessions = lock_or_recover(&WS_FALLBACK_SESSIONS);
        if sessions.len() < MAX_WS_FALLBACK_SESSIONS || sessions.contains(id) {
            sessions.insert(id.to_string());
        }
    }
}

// ============================================================================
// Connection pool (port of the TS websocketSessionCache)
// ============================================================================

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// TS `SESSION_WEBSOCKET_CACHE_TTL_MS`: close a pooled connection after this
/// much idle time.
const IDLE_TTL_DEFAULT_MS: u64 = 5 * 60 * 1000;
/// TS `SESSION_WEBSOCKET_MAX_AGE_MS`: never reuse a connection older than this.
const MAX_CONNECTION_AGE: Duration = Duration::from_secs(55 * 60);

/// Idle TTL in ms (atomic so tests can shorten it).
static IDLE_TTL_MS: AtomicU64 = AtomicU64::new(IDLE_TTL_DEFAULT_MS);

fn idle_ttl() -> Duration {
    Duration::from_millis(IDLE_TTL_MS.load(Ordering::Relaxed))
}

/// Pool key: TS keys sessions as sessionId → accountId; the auth fingerprint
/// (SHA-256 of the API key) stands in for the JWT account id, and base_url
/// scopes connections to a host.
#[derive(Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    session_id: String,
    base_url: String,
    auth: String,
}

/// Port of TS `CachedWebSocketContinuationState`.
struct Continuation {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

/// Port of TS `CachedWebSocketConnection`; the socket itself lives in the
/// driver task, reachable through `cmd`.
struct PoolEntry {
    id: u64,
    cmd: mpsc::UnboundedSender<DriverCmd>,
    busy: bool,
    created_at: Instant,
    continuation: Option<Continuation>,
}

static POOL: LazyLock<Mutex<HashMap<PoolKey, PoolEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_DRIVER_ID: AtomicU64 = AtomicU64::new(1);

fn auth_fingerprint(api_key: Option<&str>) -> String {
    use sha2::Digest;
    match api_key {
        Some(key) => {
            let digest = sha2::Sha256::digest(key.as_bytes());
            digest[..16].iter().map(|b| format!("{b:02x}")).collect()
        }
        None => String::new(),
    }
}

/// A single request handed to a connection driver.
struct Request {
    /// Body to send (possibly continuation-collapsed).
    send_body: String,
    /// Full request body; resent verbatim when a retry must not use the
    /// cached continuation, and remembered as the continuation baseline.
    full_body: Value,
    /// Transport allows storing the next continuation (TS useCachedContext).
    use_cached_context: bool,
    events: mpsc::UnboundedSender<Result<String, String>>,
    cancel: CancellationToken,
    /// Reports the outcome of the first send so `spawn_producer` can still
    /// fall back to SSE when the WS path never came up.
    sent_ack: oneshot::Sender<Result<(), String>>,
}

enum DriverCmd {
    Run(Box<Request>),
    /// Close promptly (connection aged out while idle).
    Shutdown,
}

enum RequestOutcome {
    /// Terminal event seen; connection released back to the pool.
    Completed,
    /// Connection unusable; deregister and close.
    Dead,
}

/// Everything needed to (re)establish the WS connection.
struct ConnectParams {
    url: String,
    headers: Vec<(String, String)>,
}

impl ConnectParams {
    async fn connect(
        &self,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        String,
    > {
        let mut request = self
            .url
            .clone()
            .into_client_request()
            .map_err(|e| e.to_string())?;
        let headers = request.headers_mut();
        for (name, value) in &self.headers {
            let name: tokio_tungstenite::tungstenite::http::header::HeaderName = name
                .parse()
                .map_err(|e| format!("invalid header {name}: {e}"))?;
            let value: tokio_tungstenite::tungstenite::http::header::HeaderValue = value
                .parse()
                .map_err(|e| format!("invalid value for {name}: {e}"))?;
            headers.insert(name, value);
        }
        let connect = tokio_tungstenite::connect_async(request);
        let (socket, _response) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| "websocket connect timed out".to_string())?
            .map_err(|e| e.to_string())?;
        Ok(socket)
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Owns one WS connection for its whole lifetime: serves one request at a
/// time, watches for peer closure while idle, and closes after the idle TTL.
struct Driver {
    socket: Ws,
    connect: ConnectParams,
    key: Option<PoolKey>,
    id: u64,
    cmd_rx: mpsc::UnboundedReceiver<DriverCmd>,
}

/// Driver state minus the socket (kept separate so the socket can be borrowed
/// mutably while the rest is shared).
struct DriverCore {
    connect: ConnectParams,
    key: Option<PoolKey>,
    id: u64,
    cmd_rx: mpsc::UnboundedReceiver<DriverCmd>,
}

impl Driver {
    async fn run(self) {
        let Driver {
            mut socket,
            connect,
            key,
            id,
            cmd_rx,
        } = self;
        let core = DriverCore {
            connect,
            key,
            id,
            cmd_rx,
        };
        core.run_loop(&mut socket).await;
    }
}

impl DriverCore {
    async fn run_loop(mut self, socket: &mut Ws) {
        loop {
            // ---- idle phase: wait for the next request, peer close, or TTL ----
            let ttl = idle_ttl();
            let sleep = tokio::time::sleep(ttl);
            tokio::pin!(sleep);
            let cmd = loop {
                tokio::select! {
                    cmd = self.cmd_rx.recv() => break cmd,
                    frame = socket.next() => match frame {
                        // Answer pings while idle so the peer keeps us alive.
                        Some(Ok(Message::Ping(_))) => {
                            let _ = socket.flush().await;
                        }
                        // Peer closed (or errored) while idle: deregister so
                        // the next acquire opens a fresh connection.
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                            self.deregister();
                            return;
                        }
                        Some(Ok(_)) => {}
                    },
                    _ = &mut sleep => {
                        self.deregister();
                        let _ = socket.close(None).await;
                        return;
                    }
                }
            };
            match cmd {
                None => {
                    self.deregister();
                    return;
                }
                Some(DriverCmd::Shutdown) => {
                    let _ = socket.close(None).await;
                    return;
                }
                Some(DriverCmd::Run(request)) => {
                    if matches!(
                        self.run_request(socket, *request).await,
                        RequestOutcome::Dead
                    ) {
                        self.deregister();
                        let _ = socket.close(None).await;
                        return;
                    }
                }
            }
        }
    }

    fn deregister(&self) {
        if let Some(key) = &self.key {
            let mut pool = lock_or_recover(&POOL);
            if pool.get(key).is_some_and(|e| e.id == self.id) {
                pool.remove(key);
            }
        }
    }

    fn clear_continuation(&self) {
        if let Some(key) = &self.key {
            let mut pool = lock_or_recover(&POOL);
            if let Some(entry) = pool.get_mut(key)
                && entry.id == self.id
            {
                entry.continuation = None;
            }
        }
    }

    /// Release the connection back to the pool after a terminal event.
    /// Returns false (connection must be closed) when the entry is gone —
    /// e.g. aged out — or the connection was never pooled.
    fn release(&self, continuation: Option<Continuation>) -> bool {
        if let Some(key) = &self.key {
            let mut pool = lock_or_recover(&POOL);
            if let Some(entry) = pool.get_mut(key)
                && entry.id == self.id
            {
                entry.busy = false;
                entry.continuation = continuation;
                return true;
            }
        }
        false
    }

    async fn run_request(&self, socket: &mut Ws, request: Request) -> RequestOutcome {
        let Request {
            send_body: mut body,
            full_body,
            use_cached_context,
            events,
            cancel,
            sent_ack,
        } = request;
        let mut sent_ack = Some(sent_ack);
        let mut retried = false;

        'attempt: loop {
            if let Err(e) = socket
                .send(Message::Text(Utf8Bytes::from(body.clone())))
                .await
            {
                // Dead pooled connection: one transparent reconnect with the
                // full body (the continuation lived on the dead connection).
                if !retried {
                    retried = true;
                    self.clear_continuation();
                    match self.connect.connect().await {
                        Ok(fresh) => {
                            *socket = fresh;
                            body = full_body.to_string();
                            continue 'attempt;
                        }
                        Err(e2) => {
                            if let Some(ack) = sent_ack.take() {
                                let _ = ack.send(Err(format!("websocket reconnect failed: {e2}")));
                            }
                            let _ = events.send(Err(format!("websocket error: {e2}")));
                            return RequestOutcome::Dead;
                        }
                    }
                }
                if let Some(ack) = sent_ack.take() {
                    let _ = ack.send(Err(format!("websocket send failed: {e}")));
                }
                let _ = events.send(Err(format!("websocket error: {e}")));
                return RequestOutcome::Dead;
            }
            if let Some(ack) = sent_ack.take() {
                let _ = ack.send(Ok(()));
            }

            let mut forwarded_any = false;
            loop {
                let frame = tokio::select! {
                    _ = cancel.cancelled() => return RequestOutcome::Dead,
                    frame = socket.next() => frame,
                };
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_))) => {
                        let _ = socket.flush().await;
                        continue;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = events
                            .send(Err("websocket closed before response.completed".to_string()));
                        return RequestOutcome::Dead;
                    }
                    Some(Err(e)) => {
                        let _ = events.send(Err(format!("websocket error: {e}")));
                        return RequestOutcome::Dead;
                    }
                    Some(Ok(_)) => continue, // pong/binary ignored
                };
                // Codex may batch multiple events into one frame (one JSON per
                // line); split defensively.
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    let parsed: Value = serde_json::from_str(line).unwrap_or(Value::Null);
                    let event_type = parsed.get("type").and_then(Value::as_str).unwrap_or("");
                    match event_type {
                        "response.completed" | "response.done" | "response.incomplete" => {
                            // Terminal event. Capture the continuation, release
                            // the connection (before forwarding, so a
                            // back-to-back next request never sees `busy`),
                            // then hand the event on and end the request by
                            // dropping `events`.
                            let continuation = if use_cached_context && self.key.is_some() {
                                capture_continuation(&full_body, &parsed)
                            } else {
                                None
                            };
                            let kept = self.release(continuation);
                            let _ = events.send(Ok(line.to_string()));
                            return if kept {
                                RequestOutcome::Completed
                            } else {
                                RequestOutcome::Dead
                            };
                        }
                        "error" | "response.failed" => {
                            let code = extract_event_error_code(&parsed);
                            // TS outer retry loop: missing cached continuation
                            // or connection-limit before any event → clear the
                            // continuation and retry once on a fresh connection
                            // with the full body.
                            let retryable = matches!(
                                code.as_deref(),
                                Some("previous_response_not_found")
                                    | Some("websocket_connection_limit_reached")
                            );
                            if !forwarded_any && !retried && retryable {
                                retried = true;
                                self.clear_continuation();
                                match self.connect.connect().await {
                                    Ok(fresh) => {
                                        *socket = fresh;
                                        body = full_body.to_string();
                                        continue 'attempt;
                                    }
                                    Err(e) => {
                                        let _ = events.send(Err(format!("websocket error: {e}")));
                                        return RequestOutcome::Dead;
                                    }
                                }
                            }
                            // Any other protocol error: forward it and discard
                            // the connection (TS keepConnection = false).
                            if events.send(Ok(line.to_string())).is_err() {
                                return RequestOutcome::Dead;
                            }
                            return RequestOutcome::Dead;
                        }
                        _ => {
                            if events.send(Ok(line.to_string())).is_err() {
                                return RequestOutcome::Dead;
                            }
                            forwarded_any = true;
                        }
                    }
                }
            }
        }
    }
}

/// TS extractCodexEventError: `event.code` / `event.error.code` for "error"
/// events, `response.error.code` for "response.failed".
fn extract_event_error_code(event: &Value) -> Option<String> {
    if event.get("type").and_then(Value::as_str) == Some("response.failed") {
        return event
            .pointer("/response/error/code")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    event
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/error/code").and_then(Value::as_str))
        .map(str::to_string)
}

/// Build the continuation state from a terminal event's response object.
/// `response.output` holds exactly the items that must be appended to the
/// next request's input baseline (TS converts the assistant message back to
/// input items, excluding tool outputs, which never appear in output anyway).
fn capture_continuation(full_body: &Value, terminal_event: &Value) -> Option<Continuation> {
    let response = terminal_event.get("response")?;
    let id = response.get("id").and_then(Value::as_str)?;
    let items = response.get("output").and_then(Value::as_array)?;
    if id.is_empty() {
        return None;
    }
    Some(Continuation {
        last_request_body: full_body.clone(),
        last_response_id: id.to_string(),
        last_response_items: items.clone(),
    })
}

/// Port of TS `buildCachedWebSocketRequestBody` /
/// `getCachedWebSocketInputDelta`: when the request matches the continuation
/// baseline (everything but `input`/`previous_response_id` equal, current
/// input prefixed by last input + last response items), collapse the body to
/// `previous_response_id` + delta input. A mismatch clears the continuation
/// (TS sets `entry.continuation = undefined`).
fn apply_cached_continuation(slot: &mut Option<Continuation>, full_body: &Value) -> String {
    let Some(cont) = slot.take() else {
        return full_body.to_string();
    };
    let body = match input_delta(full_body, &cont) {
        Some(delta) if !cont.last_response_id.is_empty() => {
            let mut body = full_body.clone();
            body["previous_response_id"] = Value::String(cont.last_response_id.clone());
            body["input"] = Value::Array(delta);
            body
        }
        _ => full_body.clone(),
    };
    body.to_string()
}

fn input_delta(body: &Value, cont: &Continuation) -> Option<Vec<Value>> {
    fn stripped(value: &Value) -> Value {
        let mut value = value.clone();
        if let Value::Object(map) = &mut value {
            map.remove("input");
            map.remove("previous_response_id");
        }
        value
    }
    if stripped(body) != stripped(&cont.last_request_body) {
        return None;
    }
    let empty = Vec::new();
    let current = body
        .get("input")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut baseline = cont
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    baseline.extend(cont.last_response_items.iter().cloned());
    if current.len() < baseline.len() || current[..baseline.len()] != baseline[..] {
        return None;
    }
    Some(current[baseline.len()..].to_vec())
}

/// Connect (or reuse) the WS and hand the request to the connection driver,
/// which feeds JSON event strings into the channel (same shape as SSE `data:`
/// lines).
pub async fn spawn_producer(
    base_url: &str,
    api_key: Option<&str>,
    model_headers: Option<&std::collections::BTreeMap<String, String>>,
    session_id: Option<&str>,
    params: Value,
    cancel: CancellationToken,
) -> Result<mpsc::UnboundedReceiver<Result<String, String>>, String> {
    // Same endpoint resolution as the SSE path (TS
    // `resolveCodexWebSocketUrl`), then swap the scheme.
    let http_url = super::openai_responses::resolve_codex_url(base_url);
    let url = if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http_url
    };

    let mut headers = vec![(
        "OpenAI-Beta".to_string(),
        "responses=experimental".to_string(),
    )];
    if let Some(key) = api_key {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    if let Some(session_id) = session_id {
        headers.push(("session_id".to_string(), session_id.to_string()));
    }
    if let Some(model_headers) = model_headers {
        for (k, v) in model_headers {
            headers.push((k.clone(), v.clone()));
        }
    }
    let connect = ConnectParams { url, headers };

    // response.create: the whole Responses payload as one WS message (TS).
    let mut full_body = serde_json::Map::new();
    full_body.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    if let Value::Object(params) = params {
        full_body.extend(params);
    }
    let full_body = Value::Object(full_body);

    // TS useCachedContext: transport "auto" (and "websocket-cached", which
    // tack does not have); plain "websocket" pools but sends full context.
    let use_cached_context = transport_mode() != "websocket";
    // TS: pooling is session-scoped (no cacheSessionId → fresh connection,
    // closed on release).
    let key = session_id.map(|sid| PoolKey {
        session_id: sid.to_string(),
        base_url: base_url.to_string(),
        auth: auth_fingerprint(api_key),
    });

    // ---- try the pool ----
    if let Some(key) = &key {
        let mut acquired = None;
        {
            let mut pool = lock_or_recover(&POOL);
            if let Some(entry) = pool.get_mut(key) {
                if entry.busy {
                    // TS: single-flight per cached connection — concurrent
                    // requests open their own non-pooled connection.
                } else if entry.created_at.elapsed() >= MAX_CONNECTION_AGE {
                    // Invariant: `get_mut` above proved the entry exists and
                    // this guard is still held, so removal cannot fail.
                    let entry = pool
                        .remove(key)
                        .expect("entry present: checked above under the same lock");
                    let _ = entry.cmd.send(DriverCmd::Shutdown);
                } else {
                    let send_body = if use_cached_context {
                        apply_cached_continuation(&mut entry.continuation, &full_body)
                    } else {
                        full_body.to_string()
                    };
                    entry.busy = true;
                    acquired = Some((entry.id, entry.cmd.clone(), send_body));
                }
            }
        }
        if let Some((id, cmd, send_body)) = acquired {
            let (tx, rx) = mpsc::unbounded_channel();
            let (ack_tx, ack_rx) = oneshot::channel();
            let request = Request {
                send_body,
                full_body: full_body.clone(),
                use_cached_context,
                events: tx,
                cancel: cancel.clone(),
                sent_ack: ack_tx,
            };
            if cmd.send(DriverCmd::Run(Box::new(request))).is_ok() {
                // Wait for the first send so a dead connection still falls
                // back to SSE instead of failing the request mid-flight.
                match tokio::time::timeout(CONNECT_TIMEOUT, ack_rx).await {
                    Ok(Ok(Ok(()))) => return Ok(rx),
                    Ok(Ok(Err(e))) => return Err(e),
                    Ok(Err(_)) | Err(_) => {
                        // Driver died: drop the stale entry and connect fresh.
                        let mut pool = lock_or_recover(&POOL);
                        if pool.get(key).is_some_and(|e| e.id == id) {
                            pool.remove(key);
                        }
                    }
                }
            } else {
                let mut pool = lock_or_recover(&POOL);
                if pool.get(key).is_some_and(|e| e.id == id) {
                    pool.remove(key);
                }
            }
        }
    }

    // ---- fresh connection ----
    let socket = connect.connect().await?;
    let pooled = key.is_some()
        && lock_or_recover(&POOL)
            // Invariant: `key.is_some()` is checked by the `&&` above.
            .get(key.as_ref().expect("key checked Some above"))
            .is_none_or(|e| !e.busy);
    // If a pooled entry is busy we land here per TS: fresh connection that is
    // NOT inserted into the pool and closed on release.
    let driver_key = if pooled { key.clone() } else { None };
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let id = NEXT_DRIVER_ID.fetch_add(1, Ordering::Relaxed);
    let driver = Driver {
        socket,
        connect,
        key: driver_key,
        id,
        cmd_rx,
    };
    tokio::spawn(driver.run());
    if let Some(key) = &key
        && pooled
    {
        lock_or_recover(&POOL).insert(
            key.clone(),
            PoolEntry {
                id,
                cmd: cmd_tx.clone(),
                busy: true,
                created_at: Instant::now(),
                continuation: None,
            },
        );
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let (ack_tx, ack_rx) = oneshot::channel();
    let request = Request {
        send_body: full_body.to_string(),
        full_body,
        use_cached_context,
        events: tx,
        cancel,
        sent_ack: ack_tx,
    };
    if cmd_tx.send(DriverCmd::Run(Box::new(request))).is_err() {
        if let Some(key) = &key {
            let mut pool = lock_or_recover(&POOL);
            if pool.get(key).is_some_and(|e| e.id == id) {
                pool.remove(key);
            }
        }
        return Err("websocket driver failed to start".to_string());
    }
    match tokio::time::timeout(CONNECT_TIMEOUT, ack_rx).await {
        Ok(Ok(Ok(()))) => Ok(rx),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(_)) | Err(_) => Err("websocket driver unavailable".to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use tokio::net::TcpListener;

    /// Serializes tests that mutate the process-global transport / TTL.
    static TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    fn pool_contains(base_url: &str, session: &str) -> bool {
        let key = PoolKey {
            session_id: session.to_string(),
            base_url: base_url.to_string(),
            auth: auth_fingerprint(Some("key-a")),
        };
        POOL.lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&key)
    }

    #[test]
    fn ws_fallback_sessions_are_bounded() {
        // Fill past the cap: the set must stop growing rather than leak
        // memory on caller-controlled session ids.
        for i in 0..(MAX_WS_FALLBACK_SESSIONS + 100) {
            record_ws_fallback(Some(&format!("cap-test-{i}")));
        }
        let sessions = WS_FALLBACK_SESSIONS.lock().unwrap();
        assert!(sessions.len() <= MAX_WS_FALLBACK_SESSIONS);
        // Re-recording an already-known id still works when full.
        drop(sessions);
        record_ws_fallback(Some("cap-test-0"));
        assert!(WS_FALLBACK_SESSIONS.lock().unwrap().contains("cap-test-0"));
        // Unknown ids past the cap are not recorded.
        assert!(
            !WS_FALLBACK_SESSIONS
                .lock()
                .unwrap()
                .contains("cap-test-not-recorded")
        );
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Behavior {
        /// Complete every request; `resp_N` ids from a global counter.
        Normal,
        /// Complete the first request, then close the connection.
        CloseAfterFirst,
        /// Error `previous_response_not_found` when the body carries
        /// `previous_response_id`; complete otherwise.
        RejectContinuation,
        /// Hold every response back for a beat (forces request overlap).
        Delayed,
    }

    struct Server {
        base_url: String,
        handshakes: Arc<AtomicUsize>,
        bodies: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    async fn start_server(behavior: Behavior) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handshakes = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let response_ids = Arc::new(AtomicUsize::new(0));
        let server = Server {
            base_url: format!("http://{addr}"),
            handshakes: handshakes.clone(),
            bodies: bodies.clone(),
        };
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                handshakes.fetch_add(1, Ordering::Relaxed);
                let bodies = bodies.clone();
                let response_ids = response_ids.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    let mut first = true;
                    while let Some(Ok(msg)) = ws.next().await {
                        let Message::Text(text) = msg else { continue };
                        let body: Value = serde_json::from_str(&text).unwrap();
                        let has_prev = body.get("previous_response_id").is_some();
                        bodies.lock().unwrap().push(body);
                        if behavior == Behavior::RejectContinuation && has_prev {
                            let err = serde_json::json!({
                                "type": "error",
                                "code": "previous_response_not_found",
                                "message": "Previous response not found",
                            });
                            ws.send(Message::Text(err.to_string().into()))
                                .await
                                .unwrap();
                            continue;
                        }
                        if behavior == Behavior::Delayed {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        let n = response_ids.fetch_add(1, Ordering::Relaxed) + 1;
                        let done = serde_json::json!({
                            "type": "response.completed",
                            "response": {
                                "id": format!("resp_{n}"),
                                "status": "completed",
                                "output": [{
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{"type": "output_text", "text": "hi"}],
                                }],
                            },
                        });
                        ws.send(Message::Text(done.to_string().into()))
                            .await
                            .unwrap();
                        if behavior == Behavior::CloseAfterFirst && first {
                            ws.close(None).await.unwrap();
                        }
                        first = false;
                    }
                });
            }
        });
        server
    }

    fn params(turns: usize) -> Value {
        let input: Vec<Value> = (0..turns)
            .map(|i| {
                serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": format!("turn {i}")}],
                })
            })
            .collect();
        serde_json::json!({ "model": "gpt-test", "input": input })
    }

    /// Second-turn params: first turn input + first response output + new item,
    /// which is exactly the continuation baseline the pool expects.
    fn continuation_params(first: &Value, server: &Server) -> Value {
        let first_body = &server.bodies.lock().unwrap()[0];
        let mut next = first.clone();
        let mut input = first_body
            .get("input")
            .and_then(Value::as_array)
            .unwrap()
            .clone();
        input.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "hi"}],
        }));
        input.push(serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "turn 1"}],
        }));
        next["input"] = Value::Array(input);
        next
    }

    async fn drain(rx: &mut mpsc::UnboundedReceiver<Result<String, String>>) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn reuses_connection_and_applies_cached_continuation() {
        let _guard = TEST_LOCK.lock().await;
        set_transport("auto");
        let server = start_server(Behavior::Normal).await;
        let session = "t-reuse";

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let events = drain(&mut rx).await;
        assert!(events.iter().any(|e| e.contains("response.completed")));

        let second = continuation_params(&params(1), &server);
        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            second,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;

        assert_eq!(
            server.handshakes.load(Ordering::Relaxed),
            1,
            "one handshake"
        );
        let bodies = server.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0].get("previous_response_id"), None);
        assert_eq!(
            bodies[1]
                .get("previous_response_id")
                .and_then(Value::as_str),
            Some("resp_1")
        );
        let delta = bodies[1].get("input").and_then(Value::as_array).unwrap();
        assert_eq!(delta.len(), 1, "only the new turn is sent");
        assert_eq!(delta[0]["content"][0]["text"], "turn 1");
    }

    #[tokio::test]
    async fn no_pooling_without_session_id() {
        let _guard = TEST_LOCK.lock().await;
        set_transport("auto");
        let server = start_server(Behavior::Normal).await;
        for _ in 0..2 {
            let mut rx = spawn_producer(
                &server.base_url,
                Some("key-a"),
                None,
                None,
                params(1),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            drain(&mut rx).await;
        }
        assert_eq!(server.handshakes.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn websocket_transport_pools_without_cached_continuation() {
        let _guard = TEST_LOCK.lock().await;
        set_transport("websocket");
        let server = start_server(Behavior::Normal).await;
        let session = "t-plain-ws";

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(2),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;
        set_transport("auto");

        assert_eq!(server.handshakes.load(Ordering::Relaxed), 1, "still pooled");
        let bodies = server.bodies.lock().unwrap();
        assert_eq!(bodies[1].get("previous_response_id"), None);
        assert_eq!(
            bodies[1]
                .get("input")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            2,
            "full context sent"
        );
    }

    #[tokio::test]
    async fn idle_timeout_closes_pooled_connection() {
        let _guard = TEST_LOCK.lock().await;
        set_transport("auto");
        IDLE_TTL_MS.store(150, Ordering::Relaxed);
        let server = start_server(Behavior::Normal).await;
        let session = "t-idle";

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;
        assert_eq!(server.handshakes.load(Ordering::Relaxed), 1);

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !pool_contains(&server.base_url, session),
            "idle entry expired"
        );

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;
        IDLE_TTL_MS.store(IDLE_TTL_DEFAULT_MS, Ordering::Relaxed);
        assert_eq!(server.handshakes.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn peer_close_while_idle_reconnects_transparently() {
        let _guard = TEST_LOCK.lock().await;
        set_transport("auto");
        let server = start_server(Behavior::CloseAfterFirst).await;
        let session = "t-close";

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;
        // Give the driver a moment to observe the server-initiated close.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let events = drain(&mut rx).await;
        assert!(events.iter().any(|e| e.contains("response.completed")));
        assert_eq!(server.handshakes.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn concurrent_requests_open_separate_connections() {
        let _guard = TEST_LOCK.lock().await;
        set_transport("auto");
        let server = start_server(Behavior::Delayed).await;
        let session = "t-concurrent";

        let (first, second) = tokio::join!(
            async {
                let mut rx = spawn_producer(
                    &server.base_url,
                    Some("key-a"),
                    None,
                    Some(session),
                    params(1),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                drain(&mut rx).await
            },
            async {
                // Ensure the first request is in flight (entry busy) before
                // the second one arrives.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut rx = spawn_producer(
                    &server.base_url,
                    Some("key-a"),
                    None,
                    Some(session),
                    params(1),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                drain(&mut rx).await
            }
        );
        assert!(first.iter().any(|e| e.contains("response.completed")));
        assert!(second.iter().any(|e| e.contains("response.completed")));
        assert_eq!(server.handshakes.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn missing_cached_continuation_retries_on_fresh_connection() {
        let _guard = TEST_LOCK.lock().await;
        set_transport("auto");
        let server = start_server(Behavior::RejectContinuation).await;
        let session = "t-prev-missing";

        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;

        let second = continuation_params(&params(1), &server);
        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            second,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let events = drain(&mut rx).await;

        assert!(
            events.iter().any(|e| e.contains("response.completed")),
            "retry completed: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.contains("previous_response_not_found")),
            "error swallowed by transparent retry: {events:?}"
        );
        // initial + one reconnect for the retry
        assert_eq!(server.handshakes.load(Ordering::Relaxed), 2);
        {
            let bodies = server.bodies.lock().unwrap();
            assert_eq!(bodies.len(), 3);
            assert!(bodies[1].get("previous_response_id").is_some());
            assert_eq!(bodies[2].get("previous_response_id"), None);
            assert_eq!(
                bodies[2]
                    .get("input")
                    .and_then(Value::as_array)
                    .unwrap()
                    .len(),
                3,
                "retry sends the full input"
            );
        }
        // Continuation was cleared: a third request must send full context.
        let mut rx = spawn_producer(
            &server.base_url,
            Some("key-a"),
            None,
            Some(session),
            params(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain(&mut rx).await;
        let bodies = server.bodies.lock().unwrap();
        assert_eq!(bodies[3].get("previous_response_id"), None);
    }
}
