//! Per-API provider adapters.

pub mod anthropic_messages;
pub mod bedrock;
pub mod bedrock_converse_stream;
pub mod codex_ws;
pub mod google_generative_ai;
pub mod google_vertex;
pub mod mistral_conversations;
pub mod openai_completions;
pub mod openai_responses;
pub mod tack_messages;
pub mod vertex_adc;

pub use anthropic_messages::AnthropicMessagesProvider;
pub use bedrock_converse_stream::BedrockConverseStreamProvider;
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
pub use google_generative_ai::GoogleGenerativeAiProvider;
pub use google_vertex::GoogleVertexProvider;
pub use mistral_conversations::MistralConversationsProvider;
pub use openai_completions::OpenAiCompletionsProvider;
pub use openai_responses::{OpenAiResponsesProvider, ResponsesFlavor};
pub use tack_messages::TackMessagesProvider;

/// Structured error of the shared request/SSE helpers below.
///
/// The `Display` strings are wire-visible: the adapters encode them in-band
/// in terminal Error events, and `crate::retry` classifies retryability by
/// substring-matching those messages — keep the formats stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// Cancelled via the request's `CancellationToken`.
    Aborted,
    /// Any other failure, pre-formatted for in-band delivery.
    Failed(String),
}

impl ApiError {
    pub(crate) fn is_aborted(&self) -> bool {
        matches!(self, ApiError::Aborted)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Aborted => f.write_str("Request was aborted"),
            ApiError::Failed(message) => f.write_str(message),
        }
    }
}

/// Shared terminal-failure helper for the streaming `run` functions: marks
/// the partial `output` Aborted/Error, attaches the message, finishes the
/// stream with an in-band Error event, and returns from `run`.
///
/// (Streaming scratch state lives outside content blocks in this port, so
/// there is nothing to strip from `output` here.)
macro_rules! fail {
    ($output:expr, $sender:expr, $message:expr, $aborted:expr $(,)?) => {{
        $output.stop_reason = if $aborted {
            $crate::types::StopReason::Aborted
        } else {
            $crate::types::StopReason::Error
        };
        $output.error_message = Some($message);
        $sender.finish($crate::stream::AssistantMessageEvent::Error {
            reason: $output.stop_reason,
            error: $output,
        });
        return;
    }};
}

pub(crate) use fail;

/// Shared block-close helper for adapters that track one open text/thinking
/// block in a `current_block: Option<(usize, bool)>` slot (index +
/// is_thinking): emits TextEnd/ThinkingEnd for the open block — flushing
/// the delta window first via [`DeltaCoalescer::push`] — and clears the
/// slot. The block kind is decided by the content variant, so the
/// is_thinking flag is unused here. The state is passed in explicitly
/// (macro_rules hygiene keeps free locals from resolving at the call
/// site).
macro_rules! close_current_block {
    ($current_block:ident, $output:ident, $sender:ident, $coalescer:ident $(,)?) => {
        if let Some((idx, _)) = $current_block.take() {
            match $output.content.get(idx) {
                Some($crate::types::ContentBlock::Text { text, .. }) => {
                    let content = text.clone();
                    $coalescer.push(
                        &$sender,
                        &$output,
                        $crate::stream::AssistantMessageEvent::TextEnd {
                            content_index: idx,
                            content,
                            partial: $output.clone(),
                        },
                    );
                }
                Some($crate::types::ContentBlock::Thinking { thinking, .. }) => {
                    let content = thinking.clone();
                    $coalescer.push(
                        &$sender,
                        &$output,
                        $crate::stream::AssistantMessageEvent::ThinkingEnd {
                            content_index: idx,
                            content,
                            partial: $output.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
    };
}

pub(crate) use close_current_block;

/// Which content-block kind a coalesced delta window belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeltaKind {
    Text,
    Thinking,
    ToolCall,
}

/// A buffered provider delta window: consecutive deltas of one block are
/// concatenated and emitted as a single equivalent delta event.
struct PendingDelta {
    kind: DeltaKind,
    content_index: usize,
    delta: String,
}

impl PendingDelta {
    fn into_event(
        self,
        partial: crate::types::AssistantMessage,
    ) -> crate::stream::AssistantMessageEvent {
        use crate::stream::AssistantMessageEvent as E;
        match self.kind {
            DeltaKind::Text => E::TextDelta {
                content_index: self.content_index,
                delta: self.delta,
                partial,
            },
            DeltaKind::Thinking => E::ThinkingDelta {
                content_index: self.content_index,
                delta: self.delta,
                partial,
            },
            DeltaKind::ToolCall => E::ToolCallDelta {
                content_index: self.content_index,
                delta: self.delta,
                partial,
            },
        }
    }
}

/// Coalesces token-at-a-time provider deltas so adapters don't clone the
/// full partial message per SSE chunk (O(deltas x message) copy churn, and
/// every clone used to occupy the unbounded event channel until drained).
///
/// Semantics are preserved exactly: consecutive deltas of the same block are
/// concatenated (delta-accumulating consumers see identical text), the
/// merged event's `partial` is a fresh snapshot at emit time, and structural
/// events (block start/end, terminal) always flush the window first so
/// ordering is unchanged. Each provider byte is copied once into the window
/// instead of once per event.
///
/// Emit triggers: window reaches [`MIN_WINDOW_BYTES`], or
/// [`MIN_WINDOW_INTERVAL`] elapsed since the last emit (keeps slow streams
/// live). 50ms matches the downstream coalescing window in tack-agent-core.
pub(crate) struct DeltaCoalescer {
    pending: Option<PendingDelta>,
    last_emit: std::time::Instant,
    min_interval: std::time::Duration,
    min_bytes: usize,
}

/// See [`DeltaCoalescer`].
const MIN_WINDOW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// See [`DeltaCoalescer`].
const MIN_WINDOW_BYTES: usize = 4096;

impl DeltaCoalescer {
    pub(crate) fn new() -> Self {
        Self {
            pending: None,
            last_emit: std::time::Instant::now(),
            min_interval: MIN_WINDOW_INTERVAL,
            min_bytes: MIN_WINDOW_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(min_interval: std::time::Duration, min_bytes: usize) -> Self {
        Self {
            pending: None,
            last_emit: std::time::Instant::now(),
            min_interval,
            min_bytes,
        }
    }

    /// Buffer one provider delta; returns the merged event when the window
    /// fills (callers forward it). `output` is cloned only when emitting.
    pub(crate) fn offer(
        &mut self,
        kind: DeltaKind,
        content_index: usize,
        delta: String,
        output: &crate::types::AssistantMessage,
    ) -> Option<crate::stream::AssistantMessageEvent> {
        match &mut self.pending {
            Some(p) if p.kind == kind && p.content_index == content_index => {
                p.delta.push_str(&delta);
            }
            _ => {
                // Block switch: the stale window must flush first, but the
                // caller can only forward one event per offer — return it and
                // start the new window. (Ordering preserved.)
                let flushed = self.pending.take().map(|p| p.into_event(output.clone()));
                self.pending = Some(PendingDelta {
                    kind,
                    content_index,
                    delta,
                });
                if flushed.is_some() {
                    self.last_emit = std::time::Instant::now();
                    return flushed;
                }
            }
        }
        let full = self
            .pending
            .as_ref()
            .is_some_and(|p| p.delta.len() >= self.min_bytes);
        if full || self.last_emit.elapsed() >= self.min_interval {
            self.flush(output)
        } else {
            None
        }
    }

    /// Would `offer(kind, content_index, delta)` emit an event right now?
    /// Must mirror `offer`'s flush triggers exactly: a block switch always
    /// flushes the stale window; otherwise the window emits when the merged
    /// delta reaches `min_bytes` or `min_interval` has elapsed. Adapters
    /// use this to defer expensive per-delta work (e.g. re-parsing
    /// accumulated tool-call JSON) to actual emit points.
    pub(crate) fn would_flush(
        &self,
        kind: DeltaKind,
        content_index: usize,
        additional_bytes: usize,
    ) -> bool {
        match &self.pending {
            Some(p) if p.kind == kind && p.content_index == content_index => {
                p.delta.len() + additional_bytes >= self.min_bytes
                    || self.last_emit.elapsed() >= self.min_interval
            }
            // Block switch: `offer` flushes the stale window first.
            Some(_) => true,
            None => {
                additional_bytes >= self.min_bytes || self.last_emit.elapsed() >= self.min_interval
            }
        }
    }

    /// Force-emit the buffered window, if any. Call before forwarding any
    /// structural (block start/end) or terminal event, and on error paths.
    pub(crate) fn flush(
        &mut self,
        output: &crate::types::AssistantMessage,
    ) -> Option<crate::stream::AssistantMessageEvent> {
        let p = self.pending.take()?;
        self.last_emit = std::time::Instant::now();
        Some(p.into_event(output.clone()))
    }

    /// Flush the window (if any) into `sender`, then forward `event`.
    /// Adapters route every non-delta `sender.push` through this so block
    /// boundaries never overtake a buffered delta.
    pub(crate) fn push(
        &mut self,
        sender: &crate::stream::AssistantMessageEventSender,
        output: &crate::types::AssistantMessage,
        event: crate::stream::AssistantMessageEvent,
    ) {
        self.flush_into(sender, output);
        let _ = sender.push(event);
    }

    /// Flush the window (if any) into `sender`. Call before `fail!` /
    /// `sender.finish` (terminal events don't go through [`Self::push`]).
    pub(crate) fn flush_into(
        &mut self,
        sender: &crate::stream::AssistantMessageEventSender,
        output: &crate::types::AssistantMessage,
    ) {
        if let Some(ev) = self.flush(output) {
            let _ = sender.push(ev);
        }
    }
}

/// Shared `map_stop_reason` fallback: an unrecognized provider stop reason
/// maps to an in-band error. The provider-specific tables stay in the
/// adapters — their reason vocabularies and error messages differ too much
/// to share (OpenAI reports "Provider finish_reason: ...", Google's table
/// maps to bare `StopReason`s, Anthropic's returns `Result`).
pub(crate) fn unknown_stop_reason(reason: &str) -> (crate::types::StopReason, Option<String>) {
    (
        crate::types::StopReason::Error,
        Some(format!("Provider stopped with: {reason}")),
    )
}

/// Shared SSE consumption primitive for the streaming adapters: wraps a
/// response byte stream with `eventsource-stream` plus a cancellation token.
///
/// Each `next_event` yields the next SSE event; a literal `[DONE]` data
/// payload and stream end both yield `Ok(None)`. Cancellation yields
/// `Err(ApiError::Aborted)`; transport errors are pre-formatted as
/// "SSE stream error: ..." (a string `crate::retry` pattern-matches).
pub(crate) struct SseStream<S> {
    inner: eventsource_stream::EventStream<S>,
    cancel: tokio_util::sync::CancellationToken,
}

impl<S, B, E> SseStream<S>
where
    S: futures_core::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    pub(crate) fn new(byte_stream: S, cancel: tokio_util::sync::CancellationToken) -> Self {
        SseStream {
            inner: byte_stream.eventsource(),
            cancel,
        }
    }

    /// Next SSE event, `Ok(None)` on stream end or a `[DONE]` marker.
    /// `[DONE]` terminates OpenAI-style streams; ignoring it would spin
    /// until the server closes the connection, which some never do promptly.
    pub(crate) async fn next_event(
        &mut self,
    ) -> Result<Option<eventsource_stream::Event>, ApiError> {
        let next = tokio::select! {
            _ = self.cancel.cancelled() => return Err(ApiError::Aborted),
            item = self.inner.next() => item,
        };
        let Some(item) = next else { return Ok(None) };
        match item {
            Ok(event) => {
                if event.data.trim() == "[DONE]" {
                    return Ok(None);
                }
                Ok(Some(event))
            }
            Err(e) => Err(ApiError::Failed(format!("SSE stream error: {e}"))),
        }
    }

    /// `next_event` + JSON parse, for adapters that only consume JSON data
    /// events. Empty data payloads are skipped; parse failures terminate the
    /// stream with a message prefixed by `context`
    /// ("Could not parse {context}: ...").
    pub(crate) async fn next_json(
        &mut self,
        context: &str,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        loop {
            let Some(event) = self.next_event().await? else {
                return Ok(None);
            };
            let data = event.data.trim();
            if data.is_empty() {
                continue;
            }
            return parse_sse_json(context, data).map(Some);
        }
    }
}

/// Truncate a provider payload for an error message like TS pi
/// (MAX_PROVIDER_ERROR_BODY_CHARS). Slice at a char boundary: a naive
/// `&s[..4000]` panics when the cut falls inside a multi-byte UTF-8 char.
pub(crate) fn truncate_for_error(body: &str) -> String {
    match body.char_indices().nth(4000) {
        Some((idx, _)) => format!(
            "{}... [truncated {} chars]",
            &body[..idx],
            body[idx..].chars().count()
        ),
        None => body.to_string(),
    }
}

/// reqwest's error Display embeds the full request URL — query string
/// included. Strip the query so a key can never leak into logs or in-band
/// error events if a URL ever carries one (auth goes in headers instead).
fn reqwest_error_message(e: &reqwest::Error) -> String {
    match e.url() {
        Some(url) if url.query().is_some() => {
            let mut sanitized = url.clone();
            sanitized.set_query(None);
            e.to_string().replace(url.as_str(), sanitized.as_str())
        }
        _ => e.to_string(),
    }
}

/// Parse one SSE data payload as JSON; failures are formatted with the
/// adapter-provided `context` label (wire-visible in Error events — keep
/// the format stable). The offending payload is truncated like any other
/// provider error body.
pub(crate) fn parse_sse_json(context: &str, data: &str) -> Result<serde_json::Value, ApiError> {
    serde_json::from_str(data).map_err(|e| {
        ApiError::Failed(format!(
            "Could not parse {context}: {e}; data={}",
            truncate_for_error(data)
        ))
    })
}

/// Shared HTTP client (connection pooling across requests).
pub(crate) fn http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            crate::tls::ensure_ring_provider();
            let mut builder = reqwest::Client::builder();
            // Connect-phase bound: without it a half-open connection attempt
            // can hang the request indefinitely (read_timeout below only
            // applies once bytes flow).
            builder = builder.connect_timeout(std::time::Duration::from_secs(30));
            let idle = HTTP_IDLE_TIMEOUT_MS.load(std::sync::atomic::Ordering::SeqCst);
            if idle > 0 {
                // Per-read (idle) timeout, NOT a total request timeout —
                // long SSE streams stay alive while bytes flow (TS
                // httpIdleTimeoutMs semantics).
                builder = builder.read_timeout(std::time::Duration::from_millis(idle));
            }
            builder.build().unwrap_or_else(|e| {
                // A misconfigured builder (e.g. TLS backend init failure)
                // must not take down the process — fall back to the default
                // client rather than panicking on first use.
                tracing::warn!("failed to build tuned HTTP client ({e}); using default");
                reqwest::Client::new()
            })
        })
        .clone()
}

static HTTP_IDLE_TIMEOUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// settings httpIdleTimeoutMs: set before the first request is made
/// (the shared client is initialized lazily on first use).
pub fn set_http_idle_timeout_ms(ms: u64) {
    HTTP_IDLE_TIMEOUT_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
}

/// Retry policy for the initial request (port of pi's retryProviderRequest
/// defaults): transient connection errors and 429/5xx are retried with
/// exponential backoff, honoring `retry-after`. Once a 2xx stream starts,
/// errors are in-band and never retried.
const MAX_RETRIES: u32 = 3;
const MAX_RETRY_DELAY_MS: u64 = 60_000;

fn retry_delay(response: &reqwest::Response, attempt: u32) -> std::time::Duration {
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| s.saturating_mul(1000));
    let backoff = 500u64 * (1 << attempt);
    std::time::Duration::from_millis(
        retry_after
            .unwrap_or(backoff)
            .min(MAX_RETRY_DELAY_MS)
            .max(backoff.min(1000)),
    )
}

/// Classify transport errors for retry: only genuinely transient
/// conditions (connect-phase TCP failures, timeouts) merit another
/// attempt. TLS certificate/handshake failures and DNS resolution
/// failures are deterministic — retrying just delays the real error by
/// seconds; request-build/decode errors fail immediately too.
fn is_retryable_transport_error(e: &reqwest::Error) -> bool {
    if e.is_timeout() {
        return true;
    }
    if e.is_request() {
        // Mid-send transport failures (e.g. hyper's IncompleteMessage from
        // a stale keep-alive connection or an LB draining before the
        // response headers) are transient in practice and were retried by
        // the pre-classification baseline — keep retrying them.
        return true;
    }
    if !e.is_connect() {
        // is_builder / is_decode / is_redirect: not transient.
        return false;
    }
    // is_connect() lumps TCP, DNS and TLS together — walk the cause chain
    // and exclude the deterministic ones.
    let mut source = std::error::Error::source(e);
    while let Some(err) = source {
        // TLS handshake / certificate validation (rustls; hyper-rustls may
        // wrap it in an io::Error, so check every level of the chain).
        if err.downcast_ref::<rustls::Error>().is_some() {
            return false;
        }
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            // getaddrinfo failures surface as io::Error from the resolver.
            let msg = io.to_string();
            if msg.contains("failed to lookup address information")
                || msg.contains("Temporary failure in name resolution")
                || msg.contains("Name or service not known")
                || msg.contains("nodename nor servname")
            {
                return false;
            }
        }
        source = err.source();
    }
    true
}

/// Send a streaming POST with retries. `build` constructs the request fresh
/// per attempt. Returns the response on 2xx; on final failure returns Err
/// with a human-readable message (the adapters encode it in-band).
///
/// With `TACK_DEBUG=1`, the failing request body and the error response are
/// dumped to `<temp>/tack-last-error.json` for inspection.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    cancel: &tokio_util::sync::CancellationToken,
    body_for_debug: &serde_json::Value,
) -> Result<reqwest::Response, ApiError> {
    let mut last_error = String::new();
    for attempt in 0..=MAX_RETRIES {
        if cancel.is_cancelled() {
            return Err(ApiError::Aborted);
        }
        let result = tokio::select! {
            // Race the send against cancellation: a hung connect (or a
            // server that stalls before headers) must not outlive the token.
            _ = cancel.cancelled() => return Err(ApiError::Aborted),
            result = build().send() => result,
        };
        match result {
            Ok(response) => {
                let status = response.status();
                let retryable = status.as_u16() == 429 || status.is_server_error();
                if status.is_success() {
                    return Ok(response);
                }
                if retryable && attempt < MAX_RETRIES {
                    let delay = retry_delay(&response, attempt);
                    tracing::debug!(%status, ?delay, "retrying provider request");
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(ApiError::Aborted),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
                let body = response.text().await.unwrap_or_default();
                let body = truncate_for_error(&body);
                if std::env::var("TACK_DEBUG").is_ok_and(|v| v == "1") {
                    let dump = serde_json::json!({
                        "status": status.as_u16(),
                        "request": body_for_debug,
                        "response": body,
                    });
                    let path = std::env::temp_dir().join("tack-last-error.json");
                    write_debug_dump(
                        &path,
                        serde_json::to_string_pretty(&dump).unwrap_or_default(),
                    );
                    tracing::warn!("TACK_DEBUG: wrote failing request to {}", path.display());
                }
                return Err(ApiError::Failed(format!("{status}: {body}")));
            }
            Err(e) => {
                let retryable = is_retryable_transport_error(&e);
                last_error = reqwest_error_message(&e);
                if cancel.is_cancelled() {
                    return Err(ApiError::Aborted);
                }
                if !retryable {
                    // Deterministic failure (TLS cert, DNS, request build):
                    // surface it immediately instead of burning retries.
                    return Err(ApiError::Failed(format!(
                        "HTTP request failed: {last_error}"
                    )));
                }
                if attempt < MAX_RETRIES {
                    let delay = std::time::Duration::from_millis(500 * (1 << attempt));
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(ApiError::Aborted),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            }
        }
    }
    Err(ApiError::Failed(format!(
        "HTTP request failed after retries: {last_error}"
    )))
}

/// Write the TACK_DEBUG error dump. The dump contains the full request body
/// (auth headers excluded, but prompts may be sensitive), so on Unix it is
/// created owner-only — the shared temp dir default umask is not enough.
fn write_debug_dump(path: &std::path::Path, contents: String) {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut f| f.write_all(contents.as_bytes()));
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(path, contents);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    #[test]
    fn truncate_for_error_respects_char_boundaries() {
        // 4001 four-byte chars: byte 4000 lands inside char 1001 — a byte
        // slice would panic, the char-boundary cut must not.
        let body: String = "é".repeat(4001);
        let truncated = truncate_for_error(&body);
        assert!(truncated.starts_with(&"é".repeat(4000)));
        assert!(truncated.contains("[truncated 1 chars]"));
        assert_eq!(truncate_for_error("short"), "short");
    }

    #[tokio::test]
    async fn send_with_retry_races_cancel_against_hung_send() {
        // Server that accepts and never answers: send() alone would hang
        // until the read timeout; the cancel token must win the race.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                        .await;
                });
            }
        });

        let cancel = tokio_util::sync::CancellationToken::new();
        let client = http_client();
        let url = format!("http://{addr}/");
        let build = move || client.post(&url).body("{}".to_string());
        let body = serde_json::json!({});
        let token = cancel.clone();
        let start = std::time::Instant::now();
        let handle = tokio::spawn(async move { send_with_retry(build, &token, &body).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("send_with_retry must return promptly after cancel")
            .unwrap();
        assert!(matches!(result, Err(ApiError::Aborted)));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        server.abort();
    }

    #[tokio::test]
    async fn send_with_retry_cancel_before_send() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let client = http_client();
        let build = || client.post("http://127.0.0.1:9/").body(String::new());
        let body = serde_json::json!({});
        let result = send_with_retry(build, &cancel, &body).await;
        assert!(matches!(result, Err(ApiError::Aborted)));
    }

    /// Transient connect failures (connection refused) exhaust all retries.
    #[tokio::test]
    async fn send_with_retry_retries_connect_errors() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let client = http_client();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = attempts.clone();
        let build = move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            client.post("http://127.0.0.1:9/").body(String::new())
        };
        let body = serde_json::json!({});
        let result = send_with_retry(build, &cancel, &body).await;
        assert!(matches!(result, Err(ApiError::Failed(_))));
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            MAX_RETRIES + 1
        );
    }

    /// Deterministic failures (here: request build / URL parse) return
    /// immediately — one attempt, no backoff delay.
    #[tokio::test]
    async fn send_with_retry_does_not_retry_build_errors() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let client = http_client();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = attempts.clone();
        let build = move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            client.post("http://[::1").body(String::new())
        };
        let body = serde_json::json!({});
        let start = std::time::Instant::now();
        let result = send_with_retry(build, &cancel, &body).await;
        assert!(matches!(result, Err(ApiError::Failed(_))));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    // --- DeltaCoalescer ---

    fn test_message() -> crate::types::AssistantMessage {
        crate::types::AssistantMessage::pending(&crate::types::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://x".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            context_window: 1,
            max_tokens: 1,
            sampling_params: None,
            headers: None,
            compat: None,
        })
    }

    /// Consecutive same-block deltas merge into one event whose delta is the
    /// exact concatenation — no text is lost or reordered.
    #[test]
    fn coalescer_merges_same_block_deltas() {
        let msg = test_message();
        let mut c = DeltaCoalescer::with_limits(std::time::Duration::from_secs(60), 1 << 20);
        assert!(c.offer(DeltaKind::Text, 0, "Hello ".into(), &msg).is_none());
        assert!(c.offer(DeltaKind::Text, 0, "world".into(), &msg).is_none());
        let Some(crate::stream::AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            ..
        }) = c.flush(&msg)
        else {
            panic!("expected text delta");
        };
        assert_eq!((content_index, delta.as_str()), (0, "Hello world"));
        assert!(c.flush(&msg).is_none());
    }

    /// Byte threshold emits a merged window without an explicit flush.
    #[test]
    fn coalescer_emits_on_byte_threshold() {
        let msg = test_message();
        let mut c = DeltaCoalescer::with_limits(std::time::Duration::from_secs(60), 4);
        assert!(c.offer(DeltaKind::Text, 0, "ab".into(), &msg).is_none());
        let ev = c.offer(DeltaKind::Text, 0, "cd".into(), &msg);
        let Some(crate::stream::AssistantMessageEvent::TextDelta { delta, .. }) = ev else {
            panic!("expected emit at threshold");
        };
        assert_eq!(delta, "abcd");
    }

    /// A block switch flushes the stale window before buffering the new one.
    #[test]
    fn coalescer_block_switch_flushes_in_order() {
        let msg = test_message();
        let mut c = DeltaCoalescer::with_limits(std::time::Duration::from_secs(60), 1 << 20);
        assert!(c.offer(DeltaKind::Text, 0, "a".into(), &msg).is_none());
        let ev = c.offer(DeltaKind::Thinking, 1, "t".into(), &msg);
        let Some(crate::stream::AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            ..
        }) = ev
        else {
            panic!("expected flushed text window");
        };
        assert_eq!((content_index, delta.as_str()), (0, "a"));
        let Some(crate::stream::AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            ..
        }) = c.flush(&msg)
        else {
            panic!("expected thinking window");
        };
        assert_eq!((content_index, delta.as_str()), (1, "t"));
    }

    /// The time-based trigger keeps slow streams live even below the byte
    /// threshold.
    #[test]
    fn coalescer_emits_after_interval() {
        let msg = test_message();
        let mut c = DeltaCoalescer::with_limits(std::time::Duration::ZERO, 1 << 20);
        let ev = c.offer(DeltaKind::ToolCall, 2, "{}".into(), &msg);
        let Some(crate::stream::AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        }) = ev
        else {
            panic!("expected immediate emit with zero interval");
        };
        assert_eq!((content_index, delta.as_str()), (2, "{}"));
    }

    /// would_flush must agree with offer's emit decision in every case —
    /// adapters gate expensive per-delta work (streaming JSON re-parse) on
    /// it, so a disagreement would emit events with stale partials.
    #[test]
    fn coalescer_would_flush_matches_offer() {
        let msg = test_message();
        // Fresh window, small delta, long interval: no flush.
        let mut c = DeltaCoalescer::with_limits(std::time::Duration::from_secs(60), 1 << 20);
        assert!(!c.would_flush(DeltaKind::Text, 0, 1));
        assert!(c.offer(DeltaKind::Text, 0, "x".into(), &msg).is_none());
        // Same-block window crossing the byte threshold flushes.
        let mut c = DeltaCoalescer::with_limits(std::time::Duration::from_secs(60), 4);
        assert!(!c.would_flush(DeltaKind::Text, 0, 3));
        assert!(c.offer(DeltaKind::Text, 0, "abc".into(), &msg).is_none());
        assert!(c.would_flush(DeltaKind::Text, 0, 1));
        assert!(c.offer(DeltaKind::Text, 0, "d".into(), &msg).is_some());
        // A block switch flushes the stale window.
        let mut c = DeltaCoalescer::with_limits(std::time::Duration::from_secs(60), 1 << 20);
        assert!(c.offer(DeltaKind::Text, 0, "a".into(), &msg).is_none());
        assert!(c.would_flush(DeltaKind::Thinking, 1, 0));
        // The interval trigger fires with zero interval.
        let c = DeltaCoalescer::with_limits(std::time::Duration::ZERO, 1 << 20);
        assert!(c.would_flush(DeltaKind::Text, 0, 1));
    }
}
