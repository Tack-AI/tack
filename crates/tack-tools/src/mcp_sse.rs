//! Legacy SSE transport (MCP spec 2024-11-05): the client opens a GET
//! event-stream; the server sends an `endpoint` event with the POST URL,
//! then delivers JSON-RPC messages as `message` events. Client messages go
//! out as HTTP POSTs (202 Accepted). rmcp 3.x dropped this transport, so we
//! implement rmcp's `Transport` trait by hand (SSE parsing included — the
//! format is line-based and trivial).

use std::collections::HashMap;
use std::fmt;

use futures_util::StreamExt;
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::{mpsc, watch};

/// Error type for the legacy SSE transport.
#[derive(Debug, Clone)]
pub struct SseTransportError(pub String);

impl fmt::Display for SseTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SseTransportError {}

fn err(message: impl Into<String>) -> SseTransportError {
    SseTransportError(message.into())
}

/// How long send() waits for the server's `endpoint` event before failing.
const ENDPOINT_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A runaway/broken server must not grow the line buffer without bound:
/// SSE messages are JSON-RPC frames (KBs at most), so 4MiB of undelivered
/// stream means the peer never terminates lines — disconnect instead.
const MAX_SSE_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Inbound JSON-RPC messages are buffered for the consumer; beyond this the
/// read loop applies backpressure via send().await instead of growing
/// memory without bound.
const MESSAGE_CHANNEL_CAPACITY: usize = 256;

/// Legacy SSE client transport (spec 2024-11-05).
#[derive(Debug)]
pub struct SseClientTransport {
    client: reqwest::Client,
    endpoint_rx: watch::Receiver<Option<String>>,
    message_rx: mpsc::Receiver<RxJsonRpcMessage<RoleClient>>,
    task: tokio::task::JoinHandle<()>,
}

impl SseClientTransport {
    /// Connect: start the GET stream in the background. The server's
    /// `endpoint` event supplies the POST URL; `send()` waits for it.
    pub fn connect(
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self, SseTransportError> {
        let mut default_headers = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name: reqwest::header::HeaderName = k
                .parse()
                .map_err(|e| err(format!("bad header name {k:?}: {e}")))?;
            let value: reqwest::header::HeaderValue = v
                .parse()
                .map_err(|e| err(format!("bad header value for {k:?}: {e}")))?;
            default_headers.insert(name, value);
        }
        tack_ai::tls::ensure_ring_provider();
        let client = reqwest::Client::builder()
            .default_headers(default_headers)
            .build()
            .map_err(|e| err(format!("http client: {e}")))?;
        let (endpoint_tx, endpoint_rx) = watch::channel(None);
        // Bounded: the read loop applies backpressure (send().await) instead
        // of buffering unboundedly when the consumer falls behind.
        let (message_tx, message_rx) = mpsc::channel(MESSAGE_CHANNEL_CAPACITY);
        let task = tokio::spawn(read_loop(
            client.clone(),
            url.to_string(),
            endpoint_tx,
            message_tx,
        ));
        Ok(SseClientTransport {
            client,
            endpoint_rx,
            message_rx,
            task,
        })
    }
}

async fn read_loop(
    client: reqwest::Client,
    url: String,
    endpoint_tx: watch::Sender<Option<String>>,
    message_tx: mpsc::Sender<RxJsonRpcMessage<RoleClient>>,
) {
    let result = async {
        let response = client
            .get(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| format!("SSE connect: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("SSE connect rejected: {}", response.status()));
        }
        let base = reqwest::Url::parse(&url).map_err(|e| format!("bad SSE url: {e}"))?;
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut parser = SseEventParser::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("SSE stream: {e}"))?;
            push_capped(&mut buffer, &String::from_utf8_lossy(&chunk))?;
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if let Some((event, data)) = parser.feed_line(&line) {
                    dispatch(&event, &data, &base, &endpoint_tx, &message_tx).await;
                }
            }
        }
        Ok::<(), String>(())
    }
    .await;
    match result {
        Ok(()) => tracing::debug!("MCP SSE stream ended"),
        Err(e) => tracing::warn!("MCP SSE stream failed: {e}"),
    }
}

/// Append a stream chunk to the line buffer, enforcing the byte cap. The
/// buffer only drains at '\n', so a peer that streams without newlines
/// would grow it forever — overflow is a fatal protocol error.
fn push_capped(buffer: &mut String, chunk: &str) -> Result<(), String> {
    buffer.push_str(chunk);
    if buffer.len() > MAX_SSE_BUFFER_BYTES {
        return Err(format!(
            "SSE stream exceeded the {}MiB line buffer without a newline; disconnecting",
            MAX_SSE_BUFFER_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// Line-based SSE event parser (spec: fields are `name:value` or bare
/// `name`; a blank line dispatches the event). Feed one line at a time;
/// returns Some((event, data)) when an event completes.
#[derive(Debug, Default)]
struct SseEventParser {
    event: String,
    data: String,
}

impl SseEventParser {
    fn feed_line(&mut self, line: &str) -> Option<(String, String)> {
        if line.is_empty() {
            return Some((
                std::mem::take(&mut self.event),
                std::mem::take(&mut self.data),
            ));
        }
        if line.starts_with(':') {
            return None; // comment / heartbeat
        }
        // Split at the FIRST colon; the field name is everything before it.
        // (Matching on strip_prefix("event")/strip_prefix("data") instead
        // would misparse field names like "eventual" or "database".)
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // Bare field name (e.g. "data") => empty value, per spec.
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            _ => {} // id:/retry:/unknown fields ignored
        }
        None
    }
}

async fn dispatch(
    event: &str,
    data: &str,
    base: &reqwest::Url,
    endpoint_tx: &watch::Sender<Option<String>>,
    message_tx: &mpsc::Sender<RxJsonRpcMessage<RoleClient>>,
) {
    match event {
        // Default event type per the legacy spec is "message".
        "" | "message" => {
            if let Ok(message) = serde_json::from_str::<RxJsonRpcMessage<RoleClient>>(data) {
                // Backpressure: wait for channel capacity. Err = consumer
                // gone (transport closed) — nothing more to do.
                let _ = message_tx.send(message).await;
            } else {
                tracing::debug!("MCP SSE: ignoring unparseable message frame");
            }
        }
        "endpoint" => {
            let data = data.trim();
            let resolved = base
                .join(data)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| data.to_string());
            let _ = endpoint_tx.send(Some(resolved));
        }
        _ => {}
    }
}

impl Transport<RoleClient> for SseClientTransport {
    type Error = SseTransportError;

    fn name() -> std::borrow::Cow<'static, str> {
        "tack-tools-legacy-sse".into()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let client = self.client.clone();
        let mut endpoint_rx = self.endpoint_rx.clone();
        async move {
            // Wait for the server's endpoint event before the first POST —
            // but not forever: a server that accepts the GET stream yet
            // never sends `endpoint` would otherwise wedge every send.
            let url = tokio::time::timeout(ENDPOINT_WAIT_TIMEOUT, async {
                loop {
                    if let Some(url) = endpoint_rx.borrow().clone() {
                        break Ok(url);
                    }
                    if endpoint_rx.changed().await.is_err() {
                        break Err(err("SSE stream closed before the endpoint event arrived"));
                    }
                }
            })
            .await
            .map_err(|_| {
                err(format!(
                    "timed out ({}s) waiting for the SSE endpoint event",
                    ENDPOINT_WAIT_TIMEOUT.as_secs()
                ))
            })??;
            let response = client
                .post(&url)
                .json(&item)
                .send()
                .await
                .map_err(|e| err(format!("SSE POST: {e}")))?;
            if !response.status().is_success() {
                return Err(err(format!("SSE POST rejected: {}", response.status())));
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.message_rx.recv().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.task.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_event_and_data() {
        let mut p = SseEventParser::default();
        assert!(p.feed_line("event: message").is_none());
        assert!(p.feed_line("data: {\"a\":1}").is_none());
        let (event, data) = p.feed_line("").unwrap();
        assert_eq!(event, "message");
        assert_eq!(data, "{\"a\":1}");
    }

    /// Regression: field names must be matched exactly up to the colon —
    /// "eventual: x" is NOT an event field, "database: y" is NOT data.
    #[test]
    fn lookalike_field_names_are_ignored() {
        let mut p = SseEventParser::default();
        assert!(p.feed_line("eventual: endpoint").is_none());
        assert!(p.feed_line("database: leak").is_none());
        assert!(p.feed_line("data: real").is_none());
        let (event, data) = p.feed_line("").unwrap();
        assert_eq!(event, "");
        assert_eq!(data, "real");
    }

    /// Regression: a bare `data` line (no colon) appends an EMPTY string per
    /// the SSE spec — previously the literal "data" was appended.
    #[test]
    fn bare_data_line_appends_empty() {
        let mut p = SseEventParser::default();
        assert!(p.feed_line("data").is_none());
        let (_, data) = p.feed_line("").unwrap();
        assert_eq!(data, "");
    }

    #[test]
    fn multi_line_data_joined_with_newline() {
        let mut p = SseEventParser::default();
        assert!(p.feed_line("data: one").is_none());
        assert!(p.feed_line("data:two").is_none()); // no space after colon
        assert!(p.feed_line(": heartbeat").is_none());
        let (event, data) = p.feed_line("").unwrap();
        assert_eq!(event, ""); // default event type
        assert_eq!(data, "one\ntwo");
    }

    #[test]
    fn state_resets_after_dispatch() {
        let mut p = SseEventParser::default();
        assert!(p.feed_line("event: endpoint").is_none());
        assert!(p.feed_line("data: /messages").is_none());
        let _ = p.feed_line("").unwrap();
        let (event, data) = p.feed_line("").unwrap();
        assert_eq!((event.as_str(), data.as_str()), ("", ""));
    }

    /// The line buffer must be capped: a peer streaming without '\n' gets
    /// a clear error at the limit instead of unbounded memory growth.
    #[test]
    fn line_buffer_cap_errors_instead_of_growing() {
        let mut buffer = String::new();
        // Well under the cap: fine, even in chunks.
        push_capped(&mut buffer, &"x".repeat(1024)).unwrap();
        assert_eq!(buffer.len(), 1024);
        // A chunk that pushes past the cap errors and names the limit.
        let err = push_capped(&mut buffer, &"y".repeat(MAX_SSE_BUFFER_BYTES)).unwrap_err();
        assert!(err.contains("line buffer"), "{err}");
        assert!(err.contains("disconnecting"), "{err}");
        // Exactly at the cap is still accepted (the next byte would trip it).
        let mut at_cap = "z".repeat(MAX_SSE_BUFFER_BYTES);
        push_capped(&mut at_cap, "").unwrap();
    }

    /// The message channel is bounded: once full, sending applies
    /// backpressure (pends) rather than buffering without limit.
    #[tokio::test]
    async fn message_channel_is_bounded() {
        let (tx, mut rx) = mpsc::channel::<String>(MESSAGE_CHANNEL_CAPACITY);
        for i in 0..MESSAGE_CHANNEL_CAPACITY {
            tx.try_send(format!("{i}")).unwrap();
        }
        assert!(
            tx.try_send("overflow".to_string()).is_err(),
            "channel must reject sends past its capacity"
        );
        // Draining one message frees exactly one slot.
        rx.recv().await.unwrap();
        tx.try_send("fits now".to_string()).unwrap();
    }
}
