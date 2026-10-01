//! Remote session client CLI: `tack client <addr>` — connect to a `tack
//! serve` endpoint, create or attach a session, and drive it interactively
//! (TS experimental `pi client` equivalent).

use std::sync::Arc;

use anyhow::{Context as _, Result};
use tack_protocol::RemoteClient;
use tack_protocol::schemas::{
    Command, CommandResult, ExtDialogKind, ServerEvent, TranscriptItem, TranscriptProgress,
};

/// TLS options for `tack client`.
#[derive(Clone, Debug, Default)]
pub struct TlsOptions {
    pub tls: bool,
    pub ca: Option<std::path::PathBuf>,
    pub insecure: bool,
}

/// The extension surfaces `tack client` opts into (it can answer dialogs
/// and prints widget updates).
fn ext_capabilities() -> Vec<String> {
    vec![
        tack_protocol::schemas::CAP_EXT_WIDGETS.to_string(),
        tack_protocol::schemas::CAP_EXT_DIALOGS.to_string(),
    ]
}

/// Plugin dialogs this client is being asked: request id → (kind, select
/// options). Shared between the event pump (registers/dismisses) and the
/// input loop's /answer.
type PendingDialogs = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, (ExtDialogKind, Vec<String>)>>,
>;

/// `/answer <id> <value>`: parse the value per dialog kind and send the
/// `ext_dialog_response`. Parse errors keep the dialog pending.
async fn answer_dialog(
    client: &RemoteClient,
    pending: &PendingDialogs,
    request_id: &str,
    value: &str,
) {
    let kind = {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(request_id)
            .cloned()
    };
    let Some((kind, options)) = kind else {
        eprintln!("[no pending dialog {request_id} (already answered or closed?)]");
        return;
    };
    let parsed: Option<serde_json::Value> = match kind {
        ExtDialogKind::Confirm => match value.to_ascii_lowercase().as_str() {
            "yes" | "y" | "true" | "1" => Some(serde_json::Value::Bool(true)),
            "no" | "n" | "false" | "0" => Some(serde_json::Value::Bool(false)),
            _ => {
                eprintln!("[confirm dialog: answer yes or no]");
                None
            }
        },
        ExtDialogKind::Select => {
            if options.iter().any(|o| o == value) {
                Some(serde_json::Value::String(value.to_string()))
            } else {
                eprintln!("[select dialog: answer one of: {}]", options.join(", "));
                None
            }
        }
        ExtDialogKind::Input => Some(serde_json::Value::String(value.to_string())),
        ExtDialogKind::Elicitation => match serde_json::from_str::<serde_json::Value>(value) {
            Ok(parsed) if parsed.is_object() => Some(parsed),
            _ => {
                eprintln!("[elicitation: answer a JSON object, e.g. {{\"field\":\"value\"}}]");
                None
            }
        },
    };
    let Some(parsed) = parsed else {
        return;
    };
    match client
        .request(Command::ExtDialogResponse {
            request_id: request_id.to_string(),
            cancelled: false,
            value: Some(parsed),
        })
        .await
    {
        Ok(_) => {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(request_id);
            eprintln!("[dialog {request_id} answered]");
        }
        Err(e) => {
            // Unknown/expired id: another client won, or it timed out.
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(request_id);
            eprintln!("[dialog {request_id} no longer pending: {e}]");
        }
    }
}

/// Derive the TLS server name from a "host:port" dial address. Handles
/// bracketed IPv6 (`[::1]:7749`) and bare hosts without a port.
fn tls_server_name(addr: &str) -> Result<tokio_rustls::rustls::pki_types::ServerName<'static>> {
    let host = match addr.rsplit_once(':') {
        // `host:port` — but only split when what follows is a bare port, so
        // an unbracketed IPv6 address keeps its last segment intact.
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => addr,
    };
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let host = if host.is_empty() { "localhost" } else { host };
    tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
        .with_context(|| format!("invalid TLS server name {host:?}"))
}

/// Adapter exposing a WebSocket as a framed-byte stream: each binary
/// message becomes one length-prefixed frame on the read side, and each
/// complete length-prefixed frame written (and flushed) becomes one binary
/// message. Lets [`RemoteClient`] (which speaks length-prefixed CBOR) run
/// unchanged over the ws transport (bare CBOR payloads per message).
struct WsFrameStream<S> {
    ws: tokio_tungstenite::WebSocketStream<S>,
    /// Framed bytes (4-byte length prefix + payload) ready for reads.
    read_buf: std::collections::VecDeque<u8>,
    /// Partial frame bytes written but not yet flushed.
    write_buf: std::collections::VecDeque<u8>,
    closed: bool,
}

impl<S> WsFrameStream<S> {
    fn new(ws: tokio_tungstenite::WebSocketStream<S>) -> Self {
        Self {
            ws,
            read_buf: Default::default(),
            write_buf: Default::default(),
            closed: false,
        }
    }
}

impl<S> tokio::io::AsyncRead for WsFrameStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use futures_util::StreamExt as _;
        use tokio_tungstenite::tungstenite::Message;
        loop {
            if !self.read_buf.is_empty() {
                let n = buf.remaining().min(self.read_buf.len());
                let chunk: Vec<u8> = self.read_buf.drain(..n).collect();
                buf.put_slice(&chunk);
                return std::task::Poll::Ready(Ok(()));
            }
            if self.closed {
                return std::task::Poll::Ready(Ok(())); // EOF
            }
            let next = std::task::ready!(self.ws.poll_next_unpin(cx));
            match next {
                Some(Ok(Message::Binary(bytes))) => {
                    let length = u32::try_from(bytes.len()).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "ws message too large")
                    })?;
                    self.read_buf.extend(length.to_be_bytes());
                    self.read_buf.extend(bytes.iter().copied());
                }
                Some(Ok(Message::Close(_))) | None => self.closed = true,
                // tungstenite answers pings automatically; flush the pong.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    use futures_util::Sink as _;
                    let _ = std::pin::Pin::new(&mut self.ws).poll_flush(cx);
                }
                Some(Ok(other)) => {
                    return std::task::Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unexpected non-binary websocket message: {other:?}"),
                    )));
                }
                Some(Err(e)) => {
                    return std::task::Poll::Ready(Err(std::io::Error::other(e)));
                }
            }
        }
    }
}

impl<S> tokio::io::AsyncWrite for WsFrameStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.write_buf.extend(buf.iter().copied());
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use futures_util::Sink as _;
        use tokio_tungstenite::tungstenite::Message;
        // Emit every complete frame in the buffer as one binary message.
        loop {
            if self.write_buf.len() < 4 {
                break;
            }
            let length = u32::from_be_bytes([
                self.write_buf[0],
                self.write_buf[1],
                self.write_buf[2],
                self.write_buf[3],
            ]) as usize;
            if self.write_buf.len() < 4 + length {
                break; // partial frame: wait for more bytes
            }
            self.write_buf.drain(..4);
            let payload: Vec<u8> = self.write_buf.drain(..length).collect();
            std::task::ready!(std::pin::Pin::new(&mut self.ws).poll_ready(cx))
                .map_err(std::io::Error::other)?;
            std::pin::Pin::new(&mut self.ws)
                .start_send(Message::Binary(payload.into()))
                .map_err(std::io::Error::other)?;
        }
        std::task::Poll::Ready(
            std::task::ready!(std::pin::Pin::new(&mut self.ws).poll_flush(cx))
                .map_err(std::io::Error::other),
        )
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use futures_util::Sink as _;
        std::task::ready!(self.as_mut().poll_flush(cx))?;
        std::task::Poll::Ready(
            std::task::ready!(std::pin::Pin::new(&mut self.ws).poll_close(cx))
                .map_err(std::io::Error::other),
        )
    }
}

/// Connect over WebSocket (`ws:host:port`, or `wss:host:port` /
/// `ws:... --tls` for TLS). TLS verification honors the same
/// --tls-ca/--tls-insecure options as the TCP transport.
async fn connect_ws(
    addr: &str,
    token: Option<String>,
    tls: &TlsOptions,
) -> Result<Arc<RemoteClient>> {
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let url = format!("ws://{addr}/");
    if tls.tls {
        let connector = crate::remote_tls::connector(tls.ca.as_deref(), tls.insecure)?;
        let name = tls_server_name(addr)?;
        let tls_stream = connector.connect(name, stream).await.with_context(|| {
            format!("TLS handshake with {addr} failed (wrong cert? try --tls-ca or --tls-insecure)")
        })?;
        let (ws, _) = tokio_tungstenite::client_async(&url, tls_stream)
            .await
            .context("websocket handshake failed")?;
        return RemoteClient::connect(WsFrameStream::new(ws), token, ext_capabilities())
            .await
            .map_err(anyhow::Error::from);
    }
    let (ws, _) = tokio_tungstenite::client_async(&url, stream)
        .await
        .context("websocket handshake failed")?;
    RemoteClient::connect(WsFrameStream::new(ws), token, ext_capabilities())
        .await
        .map_err(anyhow::Error::from)
}

/// Connect to tcp:host:port (default tcp:127.0.0.1:7749), ws:host:port /
/// wss:host:port (WebSocket transport of `tack serve --listen ws:...`),
/// or unix:/path.
async fn connect_addr(
    addr: &str,
    token: Option<String>,
    tls: &TlsOptions,
) -> Result<Arc<RemoteClient>> {
    if let Some(rest) = addr.strip_prefix("wss:") {
        let tls = TlsOptions {
            tls: true,
            ..tls.clone()
        };
        return connect_ws(rest, token, &tls).await;
    }
    if let Some(rest) = addr.strip_prefix("ws:") {
        return connect_ws(rest, token, tls).await;
    }
    if let Some(path) = addr.strip_prefix("unix:") {
        #[cfg(unix)]
        {
            let stream = tokio::net::UnixStream::connect(path).await?;
            return RemoteClient::connect(stream, token, ext_capabilities())
                .await
                .map_err(anyhow::Error::from);
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            anyhow::bail!(
                "unix sockets are not supported on this platform; use tcp:127.0.0.1:7749"
            );
        }
    }
    let addr = addr.strip_prefix("tcp:").unwrap_or(addr);
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    if tls.tls {
        let connector = crate::remote_tls::connector(tls.ca.as_deref(), tls.insecure)?;
        let name = tls_server_name(addr)?;
        let tls_stream = connector.connect(name, stream).await.with_context(|| {
            format!("TLS handshake with {addr} failed (wrong cert? try --tls-ca or --tls-insecure)")
        })?;
        return RemoteClient::connect(tls_stream, token, ext_capabilities())
            .await
            .map_err(anyhow::Error::from);
    }
    RemoteClient::connect(stream, token, ext_capabilities())
        .await
        .map_err(anyhow::Error::from)
}

pub async fn run_client(
    addr: &str,
    model: Option<String>,
    thinking: Option<String>,
    token: Option<String>,
    tls: TlsOptions,
) -> Result<()> {
    let client = connect_addr(addr, token, &tls).await?;
    eprintln!(
        "connected ({} sessions on server)",
        client.snapshot.sessions.len()
    );
    let ext_surfaces = !client.server_capabilities.is_empty();
    if ext_surfaces {
        eprintln!(
            "server extension surfaces: {}",
            client.server_capabilities.join(", ")
        );
    }
    // A plugin command invocation may park on a dialog THIS client
    // answers — the 30s default request timeout would fire first.
    client.set_request_timeout(std::time::Duration::from_secs(10 * 60));

    let cwd = std::env::current_dir()?;
    let result = client
        .request(Command::Create {
            cwd: Some(cwd.to_string_lossy().to_string()),
            name: None,
            model: model.map(|m| {
                let (provider, id) = m.split_once('/').unwrap_or(("anthropic", m.as_str()));
                tack_protocol::schemas::ModelRef {
                    provider: provider.to_string(),
                    id: id.to_string(),
                }
            }),
            thinking_level: thinking.and_then(|t| {
                crate::print_mode::parse_thinking_level(&t)
                    .ok()
                    .flatten()
                    .map(|level| match level {
                        tack_ai::ThinkingLevel::Minimal => {
                            tack_protocol::schemas::ThinkingLevel::Minimal
                        }
                        tack_ai::ThinkingLevel::Low => tack_protocol::schemas::ThinkingLevel::Low,
                        tack_ai::ThinkingLevel::Medium => {
                            tack_protocol::schemas::ThinkingLevel::Medium
                        }
                        tack_ai::ThinkingLevel::High => tack_protocol::schemas::ThinkingLevel::High,
                        tack_ai::ThinkingLevel::Xhigh => {
                            tack_protocol::schemas::ThinkingLevel::Xhigh
                        }
                        tack_ai::ThinkingLevel::Max => tack_protocol::schemas::ThinkingLevel::Max,
                    })
            }),
        })
        .await
        .map_err(anyhow::Error::msg)?;
    let session = match result {
        CommandResult::Create { session } => session,
        other => anyhow::bail!("unexpected create result: {other:?}"),
    };
    let session_id = session.id.clone();
    eprintln!(
        "session {session_id} ({}:{})",
        session.model.provider, session.model.id
    );
    if ext_surfaces {
        eprintln!("type a prompt and hit enter; /quit to exit, /abort to cancel a run");
        eprintln!(
            "ext: /ext [name args] · /widgets · /complete [key query] · /answer <id> <value> · /cancel <id>"
        );
    } else {
        eprintln!("type a prompt and hit enter; /quit to exit, /abort to cancel a run");
    }

    // Plugin dialogs this client is being asked, keyed by request id
    // (shared between the event pump, which registers/dismisses them,
    // and the input loop's /answer).
    let pending_dialogs: PendingDialogs = Default::default();

    // Event pump: assistant text to stdout, tool activity + plugin UI
    // (dialogs, widgets) to stderr.
    let mut events = client.subscribe();
    let pump_session = session_id.clone();
    let pump_dialogs = pending_dialogs.clone();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ServerEvent::ExtDialogRequest {
                    request_id,
                    source,
                    kind,
                    title,
                    message,
                    options,
                    placeholder,
                    fields,
                }) => {
                    eprintln!("\n[dialog {request_id}] {source} asks: {title}");
                    if let Some(message) = &message {
                        eprintln!("  {message}");
                    }
                    match kind {
                        ExtDialogKind::Select => {
                            for option in &options {
                                eprintln!("  - {option}");
                            }
                            eprintln!("  answer with: /answer {request_id} <option>");
                        }
                        ExtDialogKind::Confirm => {
                            eprintln!("  answer with: /answer {request_id} yes|no");
                        }
                        ExtDialogKind::Input => {
                            if let Some(placeholder) = &placeholder {
                                eprintln!("  ({placeholder})");
                            }
                            eprintln!("  answer with: /answer {request_id} <text>");
                        }
                        ExtDialogKind::Elicitation => {
                            for field in &fields {
                                eprintln!(
                                    "  - {} ({}{})",
                                    field.name,
                                    field.kind,
                                    if field.required { ", required" } else { "" }
                                );
                            }
                            eprintln!(
                                "  answer with: /answer {request_id} {{\"field\":\"value\",…}} (JSON object)"
                            );
                        }
                    }
                    eprintln!("  or dismiss with: /cancel {request_id}");
                    pump_dialogs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(request_id, (kind, options));
                }
                Ok(ServerEvent::ExtDialogClosed { request_id }) => {
                    if pump_dialogs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&request_id)
                        .is_some()
                    {
                        eprintln!("[dialog {request_id}] closed");
                    }
                }
                Ok(ServerEvent::ExtWidgetUpdate { widget }) => {
                    eprintln!(
                        "[widget {} updated ({}{}); /widgets to inspect]",
                        widget.key,
                        widget.kind,
                        if widget.visible { "" } else { ", hidden" }
                    );
                }
                Ok(ServerEvent::ExtWidgetsRemoved { plugin, keys }) => {
                    eprintln!("[plugin {plugin} widgets removed: {}]", keys.join(", "));
                }
                Ok(ServerEvent::SessionProgress {
                    session_id,
                    progress,
                }) => {
                    if session_id != pump_session {
                        continue;
                    }
                    match progress {
                        TranscriptProgress::AssistantDelta { kind, delta, .. } => {
                            if kind == "text" {
                                crate::cli_output::print_out(&delta);
                            }
                        }
                        TranscriptProgress::ItemStarted { item }
                        | TranscriptProgress::ItemUpdated { item } => {
                            if let TranscriptItem::Tool { tool_name, .. } = item {
                                eprintln!("\n[tool] {tool_name}");
                            }
                        }
                        TranscriptProgress::ItemFinished { item } => match item {
                            TranscriptItem::Tool { is_error: true, .. } => {
                                eprintln!("[tool] failed");
                            }
                            TranscriptItem::Assistant { .. } => {
                                crate::cli_output::print_out("\n");
                            }
                            _ => {}
                        },
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[events lagged by {n}]");
                }
                Err(_) => break,
            }
        }
    });

    // Input loop (blocking stdin on a task).
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if line_tx.send(Some(line)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = line_tx.send(None);
                    return;
                }
            }
        }
        let _ = line_tx.send(None);
    });

    while let Some(line) = line_rx.recv().await {
        let Some(line) = line else { break };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        // Ext-surface commands (`/ext`, `/widgets`, `/complete`,
        // `/answer`, `/cancel`) — handled first; they parse their own
        // arguments. Unknown slash commands fall through to a prompt,
        // matching the previous behavior for any other text.
        if let Some(command) = text.strip_prefix('/') {
            let (verb, args) = match command.split_once(' ') {
                Some((verb, args)) => (verb, args.trim()),
                None => (command, ""),
            };
            match verb {
                "quit" | "exit" => break,
                "abort" => {
                    let _ = client
                        .request(Command::Abort {
                            session_id: session_id.clone(),
                        })
                        .await;
                    eprintln!("[aborted]");
                    continue;
                }
                "ext" if ext_surfaces => {
                    if args.is_empty() {
                        match client.request(Command::ListExtCommands).await {
                            Ok(CommandResult::ListExtCommands { commands }) => {
                                if commands.is_empty() {
                                    eprintln!("[no plugin commands on this server]");
                                }
                                for spec in commands {
                                    match spec.description {
                                        Some(description) => {
                                            eprintln!("  {} — {}", spec.name, description)
                                        }
                                        None => eprintln!("  {}", spec.name),
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => eprintln!("[list failed: {e}]"),
                        }
                    } else {
                        let (name, rest) = match args.split_once(' ') {
                            Some((name, rest)) => (name, rest.trim()),
                            None => (args, ""),
                        };
                        match client
                            .request(Command::InvokeExtCommand {
                                name: name.to_string(),
                                args: if rest.is_empty() {
                                    None
                                } else {
                                    Some(rest.to_string())
                                },
                            })
                            .await
                        {
                            Ok(CommandResult::InvokeExtCommand { result }) => {
                                let pretty = serde_json::to_string_pretty(&result)
                                    .unwrap_or_else(|_| result.to_string());
                                crate::cli_output::print_out(&format!("{pretty}\n"));
                            }
                            Ok(_) => {}
                            Err(e) => eprintln!("[invoke failed: {e}]"),
                        }
                    }
                    continue;
                }
                "widgets" if ext_surfaces => {
                    match client.request(Command::ListExtWidgets).await {
                        Ok(CommandResult::ListExtWidgets { widgets }) => {
                            if widgets.is_empty() {
                                eprintln!("[no plugin widgets on this server]");
                            }
                            for widget in widgets {
                                eprintln!(
                                    "  {} ({}, rev {}{}){}",
                                    widget.key,
                                    widget.kind,
                                    widget.rev,
                                    if widget.visible { "" } else { ", hidden" },
                                    widget.title.map(|t| format!(" — {t}")).unwrap_or_default()
                                );
                                if let Some(state) = &widget.state {
                                    let pretty = serde_json::to_string_pretty(state)
                                        .unwrap_or_else(|_| state.to_string());
                                    eprintln!("    {pretty}");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("[list failed: {e}]"),
                    }
                    continue;
                }
                "complete" if ext_surfaces => {
                    if args.is_empty() {
                        match client.request(Command::ListExtAutocomplete).await {
                            Ok(CommandResult::ListExtAutocomplete { providers }) => {
                                if providers.is_empty() {
                                    eprintln!("[no autocomplete providers on this server]");
                                }
                                for provider in providers {
                                    eprintln!(
                                        "  {} (trigger {:?}){}",
                                        provider.key,
                                        provider.trigger,
                                        provider
                                            .description
                                            .map(|d| format!(" — {d}"))
                                            .unwrap_or_default()
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(e) => eprintln!("[list failed: {e}]"),
                        }
                    } else {
                        let (key, query) = match args.split_once(' ') {
                            Some((key, query)) => (key, query),
                            None => {
                                eprintln!("usage: /complete <providerKey> <query>");
                                continue;
                            }
                        };
                        match client
                            .request(Command::ExtAutocomplete {
                                provider_key: key.to_string(),
                                query: query.to_string(),
                                cursor_offset: query.len() as u32,
                            })
                            .await
                        {
                            Ok(CommandResult::ExtAutocomplete { suggestions }) => {
                                if suggestions.is_empty() {
                                    eprintln!("[no suggestions]");
                                }
                                for suggestion in suggestions {
                                    eprintln!(
                                        "  {}{}",
                                        suggestion.label,
                                        suggestion
                                            .detail
                                            .map(|d| format!(" — {d}"))
                                            .unwrap_or_default()
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(e) => eprintln!("[complete failed: {e}]"),
                        }
                    }
                    continue;
                }
                "answer" if ext_surfaces => {
                    let Some((request_id, value)) = args.split_once(' ') else {
                        eprintln!("usage: /answer <requestId> <value>");
                        continue;
                    };
                    answer_dialog(&client, &pending_dialogs, request_id, value.trim()).await;
                    continue;
                }
                "cancel" if ext_surfaces => {
                    if args.is_empty() {
                        eprintln!("usage: /cancel <requestId>");
                        continue;
                    }
                    let _ = client
                        .request(Command::ExtDialogResponse {
                            request_id: args.to_string(),
                            cancelled: true,
                            value: None,
                        })
                        .await;
                    pending_dialogs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(args);
                    eprintln!("[dialog {args} cancelled]");
                    continue;
                }
                _ => {}
            }
        }
        if let Err(e) = client
            .request(Command::Prompt {
                session_id: session_id.clone(),
                text: text.to_string(),
            })
            .await
        {
            eprintln!("[prompt failed: {e}]");
        }
    }

    let _ = client
        .request(Command::Detach {
            session_id: session_id.clone(),
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn tls_server_name_from_dial_addresses() {
        use tokio_rustls::rustls::pki_types::ServerName;
        assert!(matches!(
            tls_server_name("example.com:7749").unwrap(),
            ServerName::DnsName(_)
        ));
        assert!(matches!(
            tls_server_name("127.0.0.1:7749").unwrap(),
            ServerName::IpAddress(_)
        ));
        // Bracketed IPv6 with a port must not split on the first ':'.
        match tls_server_name("[::1]:7749").unwrap() {
            ServerName::IpAddress(ip) => {
                assert_eq!(
                    std::net::IpAddr::from(ip),
                    std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                )
            }
            other => panic!("expected IPv6 server name, got {other:?}"),
        }
        // Bare host without a port.
        assert!(matches!(
            tls_server_name("example.com").unwrap(),
            ServerName::DnsName(_)
        ));
        // Garbage still errors instead of silently connecting.
        assert!(tls_server_name("not a host:7749").is_err());
    }

    /// `tack client --addr ws:... --tls`: full RemoteClient flow
    /// (hello + create + prompt) over wss against a self-signed server,
    /// trusting the generated cert via --tls-ca semantics.
    #[tokio::test]
    async fn client_over_wss_hello_create_prompt() {
        use crate::remote::testutil::*;
        use tack_protocol::schemas::{Command, CommandResult, ServerEvent, SessionPhase};

        let host = test_host(vec![assistant_text("wss client ok")]);
        let cert_dir = tempfile::tempdir().unwrap();
        let (cert, key) = crate::remote_tls::serve_cert_paths(cert_dir.path()).unwrap();
        let acceptor = crate::remote_tls::acceptor(&cert, &key).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(crate::remote_ws::serve_ws_listener(
            listener,
            host,
            Some(acceptor),
        ));

        let tls = TlsOptions {
            tls: true,
            ca: Some(cert),
            insecure: false,
        };
        let client = connect_addr(&format!("wss:{addr}"), None, &tls)
            .await
            .expect("wss connect");

        let cwd = tempfile::tempdir().unwrap();
        let result = client
            .request(Command::Create {
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                name: None,
                model: None,
                thinking_level: None,
            })
            .await
            .expect("create");
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        let session_id = session.id.clone();

        let mut events = client.subscribe();
        client
            .request(Command::Prompt {
                session_id: session_id.clone(),
                text: "hi".into(),
            })
            .await
            .expect("prompt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut done = false;
        while std::time::Instant::now() < deadline && !done {
            let event = tokio::time::timeout(std::time::Duration::from_secs(15), events.recv())
                .await
                .expect("event timeout")
                .expect("events open");
            if let ServerEvent::SessionSnapshot { snapshot } = event {
                done = snapshot.id == session_id && snapshot.phase == SessionPhase::Idle;
            }
        }
        assert!(done, "turn never finished over wss");
    }

    /// Plain ws (no TLS) client connect also works through the adapter.
    #[tokio::test]
    async fn client_over_plain_ws_list() {
        use crate::remote::testutil::*;
        use tack_protocol::schemas::{Command, CommandResult};

        let host = test_host(vec![]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(crate::remote_ws::serve_ws_listener(listener, host, None));

        let client = connect_addr(&format!("ws:{addr}"), None, &TlsOptions::default())
            .await
            .expect("ws connect");
        let result = client.request(Command::List).await.expect("list");
        assert!(matches!(result, CommandResult::List { .. }));
    }
}
