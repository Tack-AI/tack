//! Remote session server: hosts live sessions over framed CBOR (pi protocol
//! v1). `tack serve --listen 127.0.0.1:7749` (TCP) or `--listen unix:/path`
//! (unix socket / Windows named pipe via tokio uds).

/// Plugin-initiated UI bridged to remote clients (widgets, dialogs, MCP
/// elicitation). Public for the crate's integration tests; not part of
/// the supported API surface.
#[doc(hidden)]
pub mod ext_bridge;
mod frames;
mod host;

#[cfg(test)]
pub(crate) use frames::MockIo;
pub(crate) use frames::{FrameIo, handle_connection, handle_frames};
pub use host::{SessionHost, build_host, spawn_session_reaper};

use std::sync::Arc;

use anyhow::{Context as _, Result};
use tack_ai::Provider;
use tokio::sync::Mutex;

use crate::settings::Settings;

/// Constant-time token comparison (both sides hashed to fixed length first
/// so length doesn't leak through early-exit timing).
fn token_matches(expected: &str, presented: Option<&str>) -> bool {
    use sha2::Digest as _;
    let hash = |s: &str| sha2::Sha256::digest(s.as_bytes());
    let presented = match presented {
        Some(p) => p,
        None => return false,
    };
    hash(expected) == hash(presented)
}

/// Cap on concurrent client connections (all transports share it).
const MAX_CONNECTIONS: usize = 64;

/// Generate a random shared auth token (ws listener with no configured
/// token: the token is printed to stderr once at startup).
fn generate_auth_token() -> String {
    let mut bytes = [0u8; 24];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// True when the address only accepts local connections.
fn is_loopback_addr(addr: &str) -> bool {
    addr.starts_with("127.")
        || addr == "localhost"
        || addr.starts_with("localhost:")
        || addr == "[::1]"
        || addr.starts_with("[::1]:")
}

/// Serve connections from a pre-bound TCP listener.
pub async fn serve_tcp_listener(
    listener: tokio::net::TcpListener,
    host: Arc<Mutex<SessionHost>>,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_connection(stream, host.clone()));
    }
}

/// Clear the way for `UnixListener::bind`: a crashed/killed server leaves
/// the socket file behind and bind would fail with EADDRINUSE. Only a
/// confirmed socket is removed (anything else is the operator's file —
/// refuse to start rather than delete it). Liveness is not probed: a
/// running peer's socket looks identical on disk, so operators must not
/// point two servers at the same path.
#[cfg(unix)]
fn prepare_unix_socket_path(path: &str) -> Result<()> {
    use std::os::unix::fs::FileTypeExt as _;
    match std::fs::metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove stale unix socket {path}"))?;
        }
        Ok(_) => {
            anyhow::bail!(
                "unix socket path {path} exists and is not a socket; refusing to remove it. \
                 Choose another path or remove the file yourself."
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("failed to stat unix socket path {path}"));
        }
    }
    Ok(())
}

pub async fn serve(
    listen: &str,
    model: tack_ai::Model,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    auth_token: Option<String>,
    tls: Option<(std::path::PathBuf, std::path::PathBuf)>,
    allow_no_auth: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let settings = Settings::load(&cwd, &tack_session::default_agent_dir());
    let provider: Arc<dyn Provider> = tack_ai::provider_for(&model)
        .with_context(|| format!("no adapter for api kind {}", model.api))?;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::retry::RetryingProvider {
        inner: provider,
        policy: settings.retry.policy(),
        on_retry_scheduled: None,
    });

    // tack-ext plugins in headless mode (rpc parity): hook bridges,
    // plugin tools, the approval chain and trust-gated exec stay live.
    // Remote mode additionally routes plugin-initiated UI (widgets,
    // ui/* dialogs, MCP elicitation) to connected clients through the
    // ext bridge (see remote/ext_bridge.rs) instead of degrading.
    // Loaded once for the server and shared by every session in-process.
    let agent_dir = tack_session::default_agent_dir();
    let bridge_state = crate::ext_provider_bridge::ProviderBridgeState::shared();
    let sampling_llm = crate::mcp_sampling::SharedSamplingLlm::default();
    let ext_bridge = ext_bridge::RemoteExtBridge::new();
    let extensions = crate::extension_host::ExtensionManager::load(
        &cwd,
        &agent_dir,
        "remote",
        crate::ext_headless::HeadlessExtServices::new_with_remote(
            "remote",
            crate::project_trust::is_trusted(&cwd, &agent_dir),
            bridge_state.clone(),
            ext_bridge.clone(),
        ),
        settings.extension_lock_required,
        crate::mcp_config::plugin_mcp_callbacks(
            &settings,
            crate::mcp_elicitation::InteractionMode::Remote,
            Some(crate::mcp_elicitation::ElicitationChannel::Remote(
                ext_bridge.clone(),
            )),
            &sampling_llm,
            crate::mcp_sampling::log_usage_sink(),
        ),
        bridge_state,
    )
    .await;
    // A dead plugin's widgets vanish with it (no UI residue): watch each
    // plugin connection for EOF and tell widget-capable clients.
    #[cfg(feature = "ext")]
    for plugin in &extensions.plugins {
        let Some(handle) = &plugin.handle else {
            continue;
        };
        let conn = handle.client();
        let name = plugin.id.to_string();
        let bridge = ext_bridge.clone();
        crate::extension_host::watch_plugin_death(conn, move || {
            tokio::spawn(async move {
                bridge.remove_plugin_widgets(&name).await;
            });
        });
    }
    // Provider-boundary lifecycle events for subscribed plugins.
    let provider: Arc<dyn Provider> = Arc::new(crate::extension_host::ExtNotifyProvider::new(
        provider,
        extensions.clone_sink(),
    ));

    // Security posture: a remote client can run bash on this machine.
    let is_unix = listen.starts_with("unix:");
    let addr = listen
        .strip_prefix("tcp:")
        .or_else(|| listen.strip_prefix("ws:"))
        .unwrap_or(listen);
    let is_ws = listen.starts_with("ws:");
    if !is_unix && !is_loopback_addr(addr) {
        // Non-loopback without a token is unauthenticated remote code
        // execution: refuse to start unless the operator explicitly
        // overrides with --allow-no-auth.
        if auth_token.is_none() && !allow_no_auth {
            anyhow::bail!(
                "refusing to listen on non-loopback address {addr} without an auth token. \
                 Pass --auth-token/--auth-token-file (recommended), or --allow-no-auth to \
                 override (DANGEROUS: anyone who can reach the port can run commands on this machine)."
            );
        }
        if auth_token.is_some() {
            eprintln!(
                "tack serve: WARNING: listening on a non-loopback address ({addr}); token auth is REQUIRED by all clients."
            );
        } else {
            eprintln!(
                "tack serve: WARNING: --allow-no-auth on a NON-LOOPBACK address ({addr}): \
                 anyone who can reach this port can run commands on this machine."
            );
        }
        if tls.is_none() {
            eprintln!(
                "tack serve: WARNING: TLS is off — the token and all session traffic are PLAINTEXT on the network. Use --tls (self-signed) or --tls-cert/--tls-key."
            );
        }
    }
    // The WebSocket listener serves a browser client and is reachable via
    // any web page unless authenticated (DNS rebinding is mitigated by the
    // Origin/Host checks in remote_ws, but token auth is the real
    // boundary): always require a token, generating a random one when the
    // operator didn't configure any.
    let auth_token = if is_ws && auth_token.is_none() {
        let token = generate_auth_token();
        eprintln!("tack serve: ws: no --auth-token configured; generated a random token.");
        eprintln!("tack serve: ws: auth token: {token}");
        eprintln!("tack serve: ws: pass it to clients via --auth-token or TACK_REMOTE_TOKEN.");
        Some(token)
    } else {
        auth_token
    };

    let host = build_host(
        provider,
        model,
        auth,
        settings,
        auth_token,
        extensions,
        sampling_llm,
        ext_bridge.clone(),
    );
    // Widget updates pushed while plugins were loading flush now.
    ext_bridge.attach(&host).await;
    // Idle detached sessions would otherwise accumulate forever.
    let _reaper = spawn_session_reaper(&host);
    let acceptor = match &tls {
        Some((cert, key)) => Some(crate::remote_tls::acceptor(cert, key)?),
        None => None,
    };

    if is_ws {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind {addr}"))?;
        match &acceptor {
            Some(_) => {
                eprintln!("tack serve: listening on wss://{addr} (web client: https://{addr}/)")
            }
            None => {
                eprintln!("tack serve: listening on ws://{addr} (web client: http://{addr}/)")
            }
        }
        return crate::remote_ws::serve_ws_listener(listener, host, acceptor).await;
    }

    #[cfg(unix)]
    if let Some(path) = listen.strip_prefix("unix:") {
        prepare_unix_socket_path(path)?;
        let listener = tokio::net::UnixListener::bind(path)
            .with_context(|| format!("failed to bind unix socket {path}"))?;
        // Local-only does not mean safe: without a configured token the
        // socket is the ONLY access control, and the default umask may
        // leave it reachable by other users on the machine.
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on unix socket {path}"))?;
        eprintln!("tack serve: listening on unix:{path}");
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(handle_connection(stream, host.clone()));
        }
    }
    #[cfg(not(unix))]
    if listen.starts_with("unix:") {
        anyhow::bail!("unix sockets are not supported on this platform; use tcp:127.0.0.1:7749");
    }

    {
        let addr = listen.strip_prefix("tcp:").unwrap_or(listen);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind {addr}"))?;
        eprintln!(
            "tack serve: listening on {addr}{}",
            if acceptor.is_some() { " (TLS)" } else { "" }
        );
        loop {
            let (stream, _) = listener.accept().await?;
            let host = host.clone();
            match &acceptor {
                Some(acceptor) => {
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                if let Err(e) = handle_connection(tls_stream, host).await {
                                    tracing::debug!("tls connection ended: {e}");
                                }
                            }
                            Err(e) => tracing::debug!("TLS handshake failed: {e}"),
                        }
                    });
                }
                None => {
                    tokio::spawn(handle_connection(stream, host));
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(unsafe_code, clippy::unwrap_used)]
pub(crate) mod testutil {
    use std::sync::Arc;

    use tack_ai::Provider;
    use tokio::sync::Mutex;

    use super::{SessionHost, build_host};
    use crate::settings::Settings;

    /// Shared agent dir for this test binary's remote tests (sessions land
    /// under per-cwd subdirectories, so one root is safe).
    pub fn test_agent_dir() -> &'static std::path::Path {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            std::mem::forget(dir); // keep the tempdir alive for the process
            unsafe { std::env::set_var("TACK_AGENT_DIR", &path) };
            path
        })
    }

    #[derive(Debug, Default)]
    pub struct ScriptedProvider {
        pub scripts: std::sync::Mutex<Vec<tack_ai::AssistantMessage>>,
    }

    impl Provider for ScriptedProvider {
        fn stream(
            &self,
            model: &tack_ai::Model,
            _context: &tack_ai::Context,
            _options: tack_ai::StreamOptions,
        ) -> tack_ai::AssistantMessageEventStream {
            let (sender, stream) = tack_ai::event_stream();
            let message = {
                let mut scripts = self.scripts.lock().unwrap();
                if scripts.is_empty() {
                    let mut m = tack_ai::AssistantMessage::pending(model);
                    m.stop_reason = tack_ai::StopReason::Error;
                    m.error_message = Some("no script left".into());
                    m
                } else {
                    scripts.remove(0)
                }
            };
            tokio::spawn(async move {
                let _ = sender.push(tack_ai::AssistantMessageEvent::Start {
                    partial: message.clone(),
                });
                match message.stop_reason {
                    tack_ai::StopReason::Error | tack_ai::StopReason::Aborted => {
                        sender.finish(tack_ai::AssistantMessageEvent::Error {
                            reason: message.stop_reason,
                            error: message,
                        });
                    }
                    reason => {
                        sender.finish(tack_ai::AssistantMessageEvent::Done { reason, message });
                    }
                }
            });
            stream
        }
    }

    pub fn test_model() -> tack_ai::Model {
        tack_ai::Model {
            id: "mock".into(),
            name: "Mock".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 200_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    pub fn assistant_text(text: &str) -> tack_ai::AssistantMessage {
        let mut m = tack_ai::AssistantMessage::pending(&test_model());
        m.stop_reason = tack_ai::StopReason::Stop;
        m.content = vec![tack_ai::ContentBlock::text(text)];
        m
    }

    pub fn test_host(scripts: Vec<tack_ai::AssistantMessage>) -> Arc<Mutex<SessionHost>> {
        test_agent_dir();
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            scripts: std::sync::Mutex::new(scripts),
        });
        build_host(
            provider,
            test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string()))),
            Settings::default(),
            None,
            crate::extension_host::ExtensionManager::default(),
            crate::mcp_sampling::SharedSamplingLlm::default(),
            super::ext_bridge::RemoteExtBridge::new(),
        )
    }
}

#[cfg(test)]
#[allow(unsafe_code, clippy::unwrap_used)]
mod tests {
    use super::testutil::*;
    use super::*;
    use tack_protocol::schemas::*;

    // ------------------------------------------------------------------
    // WebSocket transport (crates/tack-app/src/remote_ws.rs)
    // ------------------------------------------------------------------

    mod ws {
        use super::*;
        use futures_util::{SinkExt, StreamExt};
        use tack_protocol::{decode_payload, encode_payload};
        use tokio_tungstenite::tungstenite::Message;

        async fn start_ws_server(host: Arc<Mutex<SessionHost>>) -> std::net::SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(crate::remote_ws::serve_ws_listener(listener, host, None));
            addr
        }

        pub(super) async fn ws_send<S, T>(ws: &mut tokio_tungstenite::WebSocketStream<S>, value: &T)
        where
            S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
            T: serde::Serialize,
        {
            ws.send(Message::Binary(encode_payload(value).unwrap().into()))
                .await
                .unwrap();
        }

        pub(super) async fn ws_recv<S>(
            ws: &mut tokio_tungstenite::WebSocketStream<S>,
        ) -> ServerMessage
        where
            S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
                .await
                .expect("timed out waiting for server message")
                .expect("stream ended")
                .expect("ws error");
            let Message::Binary(bytes) = msg else {
                panic!("expected binary frame, got {msg:?}")
            };
            decode_payload(&bytes).expect("valid CBOR server message")
        }

        pub(super) async fn ws_hello<S>(
            ws: &mut tokio_tungstenite::WebSocketStream<S>,
            token: Option<&str>,
        ) -> ServerMessage
        where
            S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        {
            ws_send(
                ws,
                &ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    token: token.map(str::to_string),
                    capabilities: Vec::new(),
                },
            )
            .await;
            ws_recv(ws).await
        }

        /// WS handshake + create + one prompt round: the exact same framed
        /// CBOR protocol as TCP, one bare payload per binary message.
        #[tokio::test]
        async fn ws_create_prompt_round_trip() {
            let host = test_host(vec![assistant_text("hello from ws")]);
            let addr = start_ws_server(host).await;
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                .await
                .unwrap();

            // Handshake.
            let ServerMessage::Hello { version, .. } = ws_hello(&mut ws, None).await else {
                panic!("expected hello")
            };
            assert_eq!(version, PROTOCOL_VERSION);

            // Create a session.
            let cwd = tempfile::tempdir().unwrap();
            ws_send(
                &mut ws,
                &ClientMessage::Request {
                    id: "r1".into(),
                    request: Command::Create {
                        cwd: Some(cwd.path().to_string_lossy().to_string()),
                        name: None,
                        model: None,
                        thinking_level: None,
                    },
                },
            )
            .await;
            let ServerMessage::Response {
                ok: true,
                result: Some(CommandResult::Create { session }),
                ..
            } = ws_recv(&mut ws).await
            else {
                panic!("expected create response")
            };
            let session_id = session.id.clone();

            // Prompt, then collect events until the turn ends
            // (session_snapshot back to idle).
            ws_send(
                &mut ws,
                &ClientMessage::Request {
                    id: "r2".into(),
                    request: Command::Prompt {
                        session_id: session_id.clone(),
                        text: "hi".into(),
                    },
                },
            )
            .await;
            let mut saw_prompt_response = false;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let final_snapshot = loop {
                assert!(std::time::Instant::now() < deadline, "turn never finished");
                match ws_recv(&mut ws).await {
                    ServerMessage::Response { id, ok, .. } => {
                        assert_eq!(id, "r2");
                        assert!(ok);
                        saw_prompt_response = true;
                    }
                    ServerMessage::Event {
                        event: ServerEvent::SessionSnapshot { snapshot },
                    } if snapshot.phase == SessionPhase::Idle => break snapshot,
                    _ => {}
                }
            };
            assert!(saw_prompt_response);
            // The finished transcript (broadcast with the idle snapshot)
            // contains the assistant reply.
            let reply = final_snapshot.transcript.iter().find_map(|item| {
                let TranscriptItem::Assistant { content, .. } = item else {
                    return None;
                };
                content.iter().find_map(|c| {
                    let AssistantContent::Text { text } = c else {
                        return None;
                    };
                    text.contains("hello from ws").then(|| text.clone())
                })
            });
            assert_eq!(reply.as_deref(), Some("hello from ws"));
        }

        /// Wrong token over WS → hello_error with code auth, like TCP.
        #[tokio::test]
        async fn ws_rejects_bad_token() {
            let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::default());
            test_agent_dir();
            let host = build_host(
                provider,
                test_model(),
                Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string()))),
                Settings::default(),
                Some("secret".to_string()),
                crate::extension_host::ExtensionManager::default(),
                crate::mcp_sampling::SharedSamplingLlm::default(),
                ext_bridge::RemoteExtBridge::new(),
            );
            let addr = start_ws_server(host).await;
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                .await
                .unwrap();
            let ServerMessage::HelloError { error } = ws_hello(&mut ws, Some("wrong")).await else {
                panic!("expected hello_error")
            };
            assert_eq!(error.code, ProtocolErrorCode::Auth);
        }

        /// Plain HTTP GET / on the same port serves the embedded web client.
        #[tokio::test]
        async fn http_get_root_serves_web_client() {
            let host = test_host(vec![]);
            let addr = start_ws_server(host).await;
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            tokio::io::AsyncWriteExt::write_all(
                &mut stream,
                b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            let mut response = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
                .await
                .unwrap();
            let response = String::from_utf8_lossy(&response);
            assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
            assert!(response.contains("<title>tack"), "web client html");
            assert!(response.contains("cborDecode"), "inline CBOR codec");
        }

        /// Non-upgrade requests to other paths get a 404.
        #[tokio::test]
        async fn http_other_paths_404() {
            let host = test_host(vec![]);
            let addr = start_ws_server(host).await;
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            tokio::io::AsyncWriteExt::write_all(
                &mut stream,
                b"GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            let mut response = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 404"));
        }

        /// Security: a browser on any web page could otherwise open a
        /// WebSocket to the listener (DNS rebinding / drive-by upgrade).
        /// Upgrades with a non-loopback Origin are refused with 403.
        #[tokio::test]
        async fn ws_upgrade_rejects_foreign_origin() {
            let host = test_host(vec![]);
            let addr = start_ws_server(host).await;
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: http://evil.example\r\n\r\n"
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
                .await
                .unwrap();
            let mut response = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
                .await
                .unwrap();
            let response = String::from_utf8_lossy(&response);
            assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        }

        /// Loopback Origin + Host passes the guard and completes the
        /// WebSocket handshake (101).
        #[tokio::test]
        async fn ws_upgrade_allows_loopback_origin() {
            let host = test_host(vec![]);
            let addr = start_ws_server(host).await;
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: http://localhost:3000\r\n\r\n"
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
                .await
                .unwrap();
            let mut response = vec![0u8; 256];
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tokio::io::AsyncReadExt::read(&mut stream, &mut response),
            )
            .await
            .expect("handshake response")
            .unwrap();
            let response = String::from_utf8_lossy(&response[..n]);
            assert!(response.starts_with("HTTP/1.1 101"), "{response}");
        }
    }

    // ------------------------------------------------------------------
    // wss (TLS-terminated WebSocket transport)
    // ------------------------------------------------------------------

    mod wss {
        use super::ws::{ws_hello, ws_recv, ws_send};
        use super::*;

        /// Start a wss listener with a generated self-signed cert;
        /// returns (addr, cert_path) so clients can trust the cert as CA.
        async fn start_wss_server(
            host: Arc<Mutex<SessionHost>>,
            cert_dir: &std::path::Path,
        ) -> (std::net::SocketAddr, std::path::PathBuf) {
            let (cert, key) = crate::remote_tls::serve_cert_paths(cert_dir).unwrap();
            let acceptor = crate::remote_tls::acceptor(&cert, &key).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(crate::remote_ws::serve_ws_listener(
                listener,
                host,
                Some(acceptor),
            ));
            (addr, cert)
        }

        type TlsWs = tokio_tungstenite::WebSocketStream<
            tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
        >;

        /// WebSocket handshake over a rustls connection (the wss client
        /// path: --tls-ca pins the self-signed cert, --tls-insecure skips
        /// verification).
        async fn connect_wss(
            addr: std::net::SocketAddr,
            ca: Option<&std::path::Path>,
            insecure: bool,
        ) -> TlsWs {
            let connector = crate::remote_tls::connector(ca, insecure).unwrap();
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
                .unwrap()
                .to_owned();
            let tls = connector.connect(name, stream).await.unwrap();
            let (ws, _) = tokio_tungstenite::client_async(format!("wss://{addr}/"), tls)
                .await
                .unwrap();
            ws
        }

        /// Full wss flow: hello + create + prompt, trusting the generated
        /// self-signed cert as CA.
        #[tokio::test]
        async fn wss_create_prompt_round_trip() {
            let host = test_host(vec![assistant_text("hello from wss")]);
            let cert_dir = tempfile::tempdir().unwrap();
            let (addr, cert) = start_wss_server(host, cert_dir.path()).await;
            let mut ws = connect_wss(addr, Some(&cert), false).await;

            let ServerMessage::Hello { version, .. } = ws_hello(&mut ws, None).await else {
                panic!("expected hello")
            };
            assert_eq!(version, PROTOCOL_VERSION);

            let cwd = tempfile::tempdir().unwrap();
            ws_send(
                &mut ws,
                &ClientMessage::Request {
                    id: "r1".into(),
                    request: Command::Create {
                        cwd: Some(cwd.path().to_string_lossy().to_string()),
                        name: None,
                        model: None,
                        thinking_level: None,
                    },
                },
            )
            .await;
            let ServerMessage::Response {
                ok: true,
                result: Some(CommandResult::Create { session }),
                ..
            } = ws_recv(&mut ws).await
            else {
                panic!("expected create response")
            };
            let session_id = session.id.clone();

            ws_send(
                &mut ws,
                &ClientMessage::Request {
                    id: "r2".into(),
                    request: Command::Prompt {
                        session_id: session_id.clone(),
                        text: "hi".into(),
                    },
                },
            )
            .await;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let final_snapshot = loop {
                assert!(std::time::Instant::now() < deadline, "turn never finished");
                match ws_recv(&mut ws).await {
                    ServerMessage::Response { id, ok, .. } => {
                        assert_eq!(id, "r2");
                        assert!(ok);
                    }
                    ServerMessage::Event {
                        event: ServerEvent::SessionSnapshot { snapshot },
                    } if snapshot.phase == SessionPhase::Idle => break snapshot,
                    _ => {}
                }
            };
            let reply = final_snapshot.transcript.iter().find_map(|item| {
                let TranscriptItem::Assistant { content, .. } = item else {
                    return None;
                };
                content.iter().find_map(|c| {
                    let AssistantContent::Text { text } = c else {
                        return None;
                    };
                    text.contains("hello from wss").then(|| text.clone())
                })
            });
            assert_eq!(reply.as_deref(), Some("hello from wss"));
        }

        /// --tls-insecure connects without the CA; a WRONG CA (no trust
        /// anchor) must fail the handshake.
        #[tokio::test]
        async fn wss_insecure_connects_but_untrusted_ca_fails() {
            let host = test_host(vec![]);
            let cert_dir = tempfile::tempdir().unwrap();
            let (addr, _cert) = start_wss_server(host, cert_dir.path()).await;

            // Insecure: hello completes.
            let mut ws = connect_wss(addr, None, true).await;
            assert!(matches!(
                ws_hello(&mut ws, None).await,
                ServerMessage::Hello { .. }
            ));

            // Default webpki roots do not trust the self-signed cert.
            let connector = crate::remote_tls::connector(None, false).unwrap();
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
                .unwrap()
                .to_owned();
            assert!(connector.connect(name, stream).await.is_err());
        }

        /// Plaintext ws:// against a wss listener fails the TLS handshake
        /// (the server just drops the connection).
        #[tokio::test]
        async fn wss_rejects_plaintext_ws() {
            let host = test_host(vec![]);
            let cert_dir = tempfile::tempdir().unwrap();
            let (addr, _cert) = start_wss_server(host, cert_dir.path()).await;
            let result = tokio_tungstenite::connect_async(format!("ws://{addr}/")).await;
            assert!(result.is_err(), "plaintext ws must not handshake");
        }

        /// The embedded web client is served over https on the same port.
        #[tokio::test]
        async fn wss_https_get_root_serves_web_client() {
            let host = test_host(vec![]);
            let cert_dir = tempfile::tempdir().unwrap();
            let (addr, cert) = start_wss_server(host, cert_dir.path()).await;
            let connector = crate::remote_tls::connector(Some(&cert), false).unwrap();
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
                .unwrap()
                .to_owned();
            let mut tls = connector.connect(name, stream).await.unwrap();
            tokio::io::AsyncWriteExt::write_all(
                &mut tls,
                b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            let mut response = Vec::new();
            // The server closes with `connection: close` and no TLS
            // close_notify: read until EOF-or-error and keep the bytes.
            let mut chunk = [0u8; 4096];
            loop {
                match tokio::io::AsyncReadExt::read(&mut tls, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => response.extend_from_slice(&chunk[..n]),
                }
            }
            let response = String::from_utf8_lossy(&response);
            assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
            assert!(response.contains("<title>tack"), "web client html");
        }
    }

    /// Security: a non-loopback listener with no auth token is
    /// unauthenticated remote code execution — serve must refuse to start
    /// unless explicitly overridden.
    #[tokio::test]
    async fn serve_refuses_non_loopback_without_token() {
        test_agent_dir();
        let result = serve(
            "tcp:0.0.0.0:0",
            test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string()))),
            None,
            None,
            false,
        )
        .await;
        let err = result.expect_err("must refuse to start");
        assert!(
            err.to_string().contains("without an auth token"),
            "unexpected error: {err}"
        );
    }

    /// A stale socket file from a crashed server must be removed before
    /// bind; a non-socket file at the path must abort startup instead of
    /// being deleted.
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_path_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        // Stale socket: removed, path reusable.
        let stale = tokio::net::UnixListener::bind(&sock).unwrap();
        drop(stale);
        prepare_unix_socket_path(sock.to_str().unwrap()).unwrap();
        tokio::net::UnixListener::bind(&sock).expect("bind after stale cleanup");
        // A regular file at the path: refuse, and the file survives.
        let regular = dir.path().join("not-a-socket");
        std::fs::write(&regular, b"precious").unwrap();
        let err = prepare_unix_socket_path(regular.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("not a socket"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&regular).unwrap(), b"precious");
        // Missing path: fine.
        prepare_unix_socket_path(dir.path().join("fresh.sock").to_str().unwrap()).unwrap();
    }
}
