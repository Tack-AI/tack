//! WebSocket transport for `tack serve --listen ws:ADDR`.
//!
//! Wire format: the exact same protocol v1 messages as the TCP transport,
//! with **one CBOR payload per WebSocket binary message** and NO 4-byte
//! length prefix (WebSocket already provides message boundaries). Auth is
//! unchanged: the `hello` frame carries the shared `--auth-token` token.
//!
//! The same TCP port doubles as a plain HTTP server: `GET /` returns the
//! embedded zero-dependency web client (single self-contained HTML file),
//! any other non-upgrade request gets a 404. Requests with
//! `Upgrade: websocket` (any path) start the protocol session. With
//! `--tls`, the listener terminates TLS first (wss:// / https://), reusing
//! the same self-signed cert machinery as the TCP transport
//! (`remote_tls`).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tack_protocol::{DEFAULT_MAX_FRAME_LENGTH, decode_payload, encode_payload};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::remote::{FrameIo, SessionHost, handle_frames};

/// Embedded web client (single self-contained HTML file, no build step).
const WEB_CLIENT_HTML: &str = include_str!("../assets/web_client.html");

const MAX_HTTP_HEAD: usize = 16 * 1024;

/// Accept loop for a pre-bound WS/HTTP listener. When `acceptor` is set
/// (`--tls`), TLS is terminated before the HTTP/WebSocket handling
/// (wss://); otherwise the listener stays plaintext (ws://).
pub(crate) async fn serve_ws_listener(
    listener: TcpListener,
    host: Arc<Mutex<SessionHost>>,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let host = host.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let result = match acceptor {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(tls_stream) => handle_http_connection(tls_stream, host).await,
                    Err(e) => {
                        tracing::debug!("wss TLS handshake failed: {e}");
                        Ok(())
                    }
                },
                None => handle_http_connection(stream, host).await,
            };
            if let Err(e) = result {
                tracing::debug!("ws/http connection ended: {e:#}");
            }
        });
    }
}

/// Read until the end of the HTTP head (`\r\n\r\n`), returning the head
/// length. `buf` keeps the whole read (head + any over-read bytes).
async fn read_http_head<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut Vec<u8>) -> Result<usize> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
            return Ok(pos + 4);
        }
        if buf.len() >= MAX_HTTP_HEAD {
            bail!("HTTP head exceeds {MAX_HTTP_HEAD} bytes");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed before HTTP head completed");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Header value lookup (case-insensitive name and value fragment).
fn header_contains(head: &str, name: &str, fragment: &str) -> bool {
    head.split("\r\n").skip(1).any(|line| {
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        key.trim().eq_ignore_ascii_case(name) && value.to_ascii_lowercase().contains(fragment)
    })
}

/// Exact header value (case-insensitive name), trimmed.
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Is this host[:port] (or http(s)://host[:port]) a loopback authority?
/// Browsers send `Host` on every request and `Origin` on cross-site
/// WebSocket upgrades; requiring both to be loopback stops DNS-rebinding
/// and drive-by upgrades from arbitrary web pages.
fn is_loopback_authority(value: &str) -> bool {
    // Strip an optional scheme (Origin values carry one, Host doesn't).
    let authority = value
        .split("://")
        .nth(1)
        .unwrap_or(value)
        .split('/')
        .next()
        .unwrap_or("")
        .trim();
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // [v6]:port
        rest.split(']').next().unwrap_or("")
    } else if authority.parse::<std::net::IpAddr>().is_ok() {
        // Bare IP literal (e.g. "::1", "127.0.0.1") with no port.
        authority
    } else {
        authority.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

async fn handle_http_connection<S>(stream: S, host: Arc<Mutex<SessionHost>>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // Connection cap (shared with the byte-stream transports): refuse at
    // the door rather than backlogging unbounded per-connection tasks.
    let permits = host.lock().await.conn_permits.clone();
    let Ok(_permit) = permits.try_acquire_owned() else {
        tracing::warn!("connection limit reached; dropping ws/http connection");
        return Ok(());
    };
    let mut stream = stream;
    let mut buf = Vec::with_capacity(2048);
    let head_len = read_http_head(&mut stream, &mut buf).await?;
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let request_line = head.split("\r\n").next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    if header_contains(&head, "upgrade", "websocket") {
        // Loopback guard for the upgrade: Host is mandatory and must be
        // loopback; Origin, when present (browsers always send it for
        // WebSocket), must be loopback too. Non-browser clients that omit
        // Origin are admitted on the Host check (token auth still applies).
        let host_ok = header_value(&head, "host")
            .map(is_loopback_authority)
            .unwrap_or(false);
        let origin_ok = header_value(&head, "origin")
            .map(is_loopback_authority)
            .unwrap_or(true);
        if !host_ok || !origin_ok {
            tracing::warn!(
                "rejected websocket upgrade: host={:?} origin={:?} is not loopback",
                header_value(&head, "host"),
                header_value(&head, "origin")
            );
            let body = b"forbidden: websocket upgrades are only accepted from loopback origins";
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.flush().await?;
            return Ok(());
        }
        // Replay the bytes we already read (head + over-read) into the WS
        // handshake, which expects to read the request itself.
        let buffered = BufferedStream::new(buf, stream);
        let config = WebSocketConfig::default();
        config
            .max_message_size(Some(DEFAULT_MAX_FRAME_LENGTH))
            .max_frame_size(Some(DEFAULT_MAX_FRAME_LENGTH));
        let ws = tokio_tungstenite::accept_async_with_config(buffered, Some(config))
            .await
            .context("websocket handshake failed")?;
        let mut io = WsIo { ws };
        return handle_frames(&mut io, &host).await;
    }

    // Plain HTTP: the web client at /, 404 elsewhere.
    let (status, content_type, body) =
        if matches!(method, "GET" | "HEAD") && (path == "/" || path == "/index.html") {
            (
                "200 OK",
                "text/html; charset=utf-8",
                WEB_CLIENT_HTML.as_bytes(),
            )
        } else {
            (
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"not found".as_slice(),
            )
        };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    Ok(())
}

/// A stream prefixed with already-read bytes (the HTTP request head), so
/// the tungstenite handshake sees the request as if nothing was consumed.
struct BufferedStream<S> {
    replay: std::io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S> BufferedStream<S> {
    fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            replay: std::io::Cursor::new(prefix),
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for BufferedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let pos = usize::try_from(this.replay.position()).unwrap_or(usize::MAX);
        let remaining = &this.replay.get_ref()[pos.min(this.replay.get_ref().len())..];
        if !remaining.is_empty() {
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.replay.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for BufferedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// WebSocket frame IO: one binary message = one bare CBOR payload.
struct WsIo<S> {
    ws: WebSocketStream<S>,
}

impl<S> FrameIo for WsIo<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn read_message<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        loop {
            match self.ws.next().await {
                None => return Ok(None),
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(Message::Binary(bytes))) => {
                    return Ok(Some(decode_payload(&bytes)?));
                }
                // Clean close → treat as EOF.
                Some(Ok(Message::Close(_))) => return Ok(None),
                // tungstenite answers pings automatically; keep waiting.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(other)) => {
                    bail!("unexpected non-binary websocket message: {other:?}")
                }
            }
        }
    }

    async fn write_message<T: serde::Serialize + Sync>(&mut self, value: &T) -> Result<()> {
        let payload = encode_payload(value)?;
        self.ws.send(Message::Binary(payload.into())).await?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn header_contains_is_case_insensitive() {
        let head = "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: WebSocket\r\nConnection: keep-alive, Upgrade\r\n\r\n";
        assert!(header_contains(head, "upgrade", "websocket"));
        assert!(header_contains(head, "connection", "upgrade"));
        assert!(!header_contains(head, "upgrade", "h2c"));
    }

    #[test]
    fn loopback_authority_parsing() {
        // Host-style values.
        assert!(is_loopback_authority("127.0.0.1:7749"));
        assert!(is_loopback_authority("localhost"));
        assert!(is_loopback_authority("localhost:8080"));
        assert!(is_loopback_authority("[::1]:7749"));
        assert!(is_loopback_authority("::1"));
        // Origin-style values (scheme included).
        assert!(is_loopback_authority("http://localhost:3000"));
        assert!(is_loopback_authority("https://127.0.0.1"));
        // Foreign authorities rejected.
        assert!(!is_loopback_authority("http://evil.example"));
        assert!(!is_loopback_authority("evil.example:80"));
        assert!(!is_loopback_authority("192.168.1.10:7749"));
        // Tricky lookalikes.
        assert!(!is_loopback_authority("localhost.evil.example"));
        assert!(!is_loopback_authority("http://127.0.0.1.evil.example"));
        assert!(!is_loopback_authority(""));
    }

    #[tokio::test]
    async fn buffered_stream_replays_prefix_then_inner() {
        use tokio::io::AsyncReadExt as _;
        let (a, mut b) = tokio::io::duplex(64);
        b.write_all(b"inner").await.unwrap();
        drop(b);
        let mut stream = BufferedStream::new(b"prefix-".to_vec(), a);
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "prefix-inner");
    }
}
