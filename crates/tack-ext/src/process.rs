//! Plugin subprocess host: spawn, NDJSON framing, request/response matching
//! with per-request timeouts, dead-process fail-fast, and a graceful
//! shutdown handshake. Transport-agnostic core (`PluginPeer`) works over any
//! AsyncRead+AsyncWrite so tests can use in-memory duplexes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

use crate::protocol::{Envelope, InitializePayload, RegisterPayload};

/// Default per-request timeout (intercepts, tool executes, ui dialogs get
/// longer where noted).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Dialogs wait on humans; cap at 10 minutes.
pub const DIALOG_TIMEOUT: Duration = Duration::from_secs(600);
/// Default bound on one write to the plugin's stdin. A plugin that stops
/// reading fills the OS pipe (~64 KiB) and then pends `write_all` forever;
/// without a bound every call, event, and `shutdown` queues behind the same
/// writer mutex and hangs. On expiry the peer is failed like a dead process.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Host-side handler for plugin→host requests (`ui.*`, `session.*`, `exec`)
/// and events (`register`, `log`). Implemented by the app (TUI) and by test
/// fakes.
#[async_trait::async_trait]
pub trait HostServices: Send + Sync {
    /// Handle a plugin request; the return value becomes the response result.
    /// Returning Err becomes a protocol error response.
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, String>;

    /// Handle a plugin event (fire-and-forget).
    async fn handle_event(&self, event: &str, payload: Value);
}

type PendingMap = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Removes the pending entry on drop: cancel-safe cleanup for callers that
/// race `call` against a cancellation token and drop the future mid-flight.
struct PendingCleanup {
    pending: PendingMap,
    id: u64,
}

impl Drop for PendingCleanup {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

/// Hard cap on one NDJSON line. A broken or hostile plugin that streams
/// without ever emitting '\n' would otherwise grow the read buffer without
/// bound (lines() has no limit); past the cap the peer is declared dead.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// What [`read_line_bounded`] does once a line exceeds [`MAX_LINE_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverCap {
    /// Return Err immediately (the protocol read pump: the peer is
    /// declared dead, so there is no point draining the rest of the
    /// hostile line).
    Fail,
    /// Consume and discard bytes until the terminating '\n', THEN return
    /// Err (stderr forwarders: the reader must keep making progress so
    /// the writer never blocks on a full pipe, and must not spin on the
    /// same unconsumed chunk).
    Discard,
}

/// Read one '\n'-terminated line with a hard size cap. Returns Ok(None) on
/// clean EOF (no bytes), Ok(Some(line)) for a line (terminator and a
/// trailing '\r' stripped), Err on IO error or an over-cap line.
///
/// Shared by the v1 subprocess stderr forwarder and the WASM carrier's
/// guest-stderr forwarder (`tack-ext-wasm`): a guest writing stderr without
/// newlines must not grow the host buffer without bound either.
pub async fn read_line_bounded<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    on_over_cap: OverCap,
) -> std::io::Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    fn over_cap_err() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("plugin line exceeds {} byte cap", MAX_LINE_BYTES),
        )
    }
    buf.clear();
    let mut over_cap = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // EOF: a partial line without terminator is still delivered
            // (matches Lines::next_line); an over-cap partial is an error.
            if over_cap {
                return Err(over_cap_err());
            }
            return if buf.is_empty() {
                Ok(None)
            } else {
                let line = String::from_utf8_lossy(buf).into_owned();
                Ok(Some(line))
            };
        }
        let (take, found) = match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (chunk.len(), false),
        };
        let content = &chunk[..take - usize::from(found)];
        if !over_cap && buf.len() + content.len() > MAX_LINE_BYTES {
            if on_over_cap == OverCap::Fail {
                return Err(over_cap_err());
            }
            over_cap = true;
            buf.clear();
        }
        if !over_cap {
            buf.extend_from_slice(content);
        }
        reader.consume(take);
        if found {
            if over_cap {
                return Err(over_cap_err());
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let line = String::from_utf8_lossy(buf).into_owned();
            return Ok(Some(line));
        }
    }
}

/// Write bound for the shutdown event specifically. The general
/// [`WRITE_TIMEOUT`] (30s) exists so a stalled plugin fails calls, but a
/// plugin that stopped reading stdin would make `shutdown` wait the full
/// 30s before the 2s grace/force-kill even starts — shutdown is already
/// terminal, so a short bound gets the teardown underway immediately.
pub const SHUTDOWN_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// A live connection to one plugin process (or in-memory peer in tests).
pub struct PluginPeer {
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    /// Write timeout in milliseconds (atomic so it can be tuned on a shared
    /// `Arc<PluginPeer>`).
    write_timeout_ms: AtomicU64,
    alive: Arc<AtomicBool>,
    /// Registration data from the handshake (filled by `initialize`).
    register: Mutex<Option<RegisterPayload>>,
    /// Read-pump task; awaited by `wait_dead` so liveness becomes
    /// deterministic after shutdown (the child having exited does not
    /// mean the pump has observed the EOF yet).
    pump: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for PluginPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginPeer")
            .field("alive", &self.alive.load(Ordering::SeqCst))
            .finish()
    }
}

impl PluginPeer {
    /// Wire a peer over a raw transport and spawn the read pump. Requests
    /// arriving from the plugin are dispatched to `services`.
    pub fn new<R, W>(reader: R, writer: W, services: Arc<dyn HostServices>) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let peer = Arc::new(PluginPeer {
            writer: Mutex::new(Box::new(writer)),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            write_timeout_ms: AtomicU64::new(WRITE_TIMEOUT.as_millis() as u64),
            alive: Arc::new(AtomicBool::new(true)),
            register: Mutex::new(None),
            pump: std::sync::Mutex::new(None),
        });
        let pump = peer.clone();
        let handle = tokio::spawn(async move { pump.read_pump(reader, services).await });
        *peer.pump.lock().expect("pump mutex") = Some(handle);
        peer
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Override the write timeout (default [`WRITE_TIMEOUT`]).
    pub fn set_write_timeout(&self, timeout: Duration) {
        self.write_timeout_ms.store(
            u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Wait until the read pump observes EOF (after which `is_alive()` is
    /// false). Idempotent; returns immediately if already awaited.
    pub async fn wait_dead(&self) {
        let handle = self.pump.lock().expect("pump mutex").take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Send the `initialize` handshake and wait for the plugin's `register`
    /// event (also the liveness proof). Validates the plugin's protocol
    /// version: a plugin built against a NEWER protocol is rejected (it may
    /// rely on behavior this host does not implement); older or unreported
    /// versions load with a warning.
    pub async fn initialize(&self, payload: InitializePayload) -> Result<RegisterPayload, String> {
        let host_protocol = payload.protocol;
        let payload = serde_json::to_value(payload).map_err(|e| e.to_string())?;
        if let Err(e) = self.send_event("initialize", payload).await {
            // A dead peer surfaces as EPIPE on the write when the child
            // exits before the handshake write lands — same semantics as
            // "exited during handshake" (EOF path below), just a race on
            // which side notices first. Report it uniformly; a raw
            // "Broken pipe" tells the user nothing.
            if !self.is_alive() {
                return Err(format!("plugin exited during handshake ({e})"));
            }
            return Err(e);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let register = loop {
            if let Some(register) = self.register.lock().await.clone() {
                break register;
            }
            if !self.is_alive() {
                return Err("plugin exited during handshake".to_string());
            }
            if std::time::Instant::now() > deadline {
                return Err("plugin handshake timed out (no register event)".to_string());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        match register.protocol {
            Some(plugin_protocol) if plugin_protocol > host_protocol => {
                return Err(format!(
                    "plugin {:?} requires protocol {plugin_protocol}, but this host speaks {host_protocol} — upgrade the host",
                    register.name
                ));
            }
            Some(plugin_protocol) if plugin_protocol < host_protocol => {
                tracing::warn!(
                    "plugin {:?} speaks protocol {plugin_protocol}, host speaks {host_protocol} — loading anyway",
                    register.name
                );
            }
            None => {
                tracing::warn!(
                    "plugin {:?} did not report a protocol version; assuming compatibility with {host_protocol}",
                    register.name
                );
            }
            _ => {}
        }
        Ok(register)
    }

    /// Send a request and await the matched response with a timeout.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if !self.is_alive() {
            return Err(format!("plugin is dead (call {method})"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending mutex").insert(id, tx);
        // Drops (e.g. the caller cancelled the call future) remove the entry.
        let _cleanup = PendingCleanup {
            pending: self.pending.clone(),
            id,
        };
        let send_result = self.send(&Envelope::request(id, method, params)).await;
        send_result?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_closed)) => Err(format!("plugin closed during {method}")),
            Err(_timeout) => Err(format!("plugin request {method} timed out")),
        }
    }

    /// Fire-and-forget event.
    pub async fn send_event(&self, event: &str, payload: Value) -> Result<(), String> {
        if !self.is_alive() {
            return Ok(()); // dead plugins silently drop notifications
        }
        self.send(&Envelope::event(event, payload)).await
    }

    async fn send(&self, envelope: &Envelope) -> Result<(), String> {
        let mut line = serde_json::to_string(envelope).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut writer = self.writer.lock().await;
        // Bound the write: a plugin that stops reading its stdin fills the
        // pipe and would otherwise pend `write_all` forever, stranding this
        // caller AND every later sender queued on the writer mutex.
        let write_timeout = Duration::from_millis(self.write_timeout_ms.load(Ordering::Relaxed));
        let result = tokio::time::timeout(write_timeout, async {
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await
        })
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "plugin stdin write stalled for {}s",
                    write_timeout.as_secs()
                ),
            )),
        };
        if let Err(e) = result {
            self.alive.store(false, Ordering::SeqCst);
            drop(writer);
            // The peer is unwritable: fail every in-flight request NOW.
            // The read pump only drains pending on EOF — with a half-broken
            // pipe (write side dead, read side open) callers would
            // otherwise hang until their full per-request timeout.
            self.fail_pending("plugin write failed");
            return Err(format!("plugin write failed: {e}"));
        }
        Ok(())
    }

    /// Fail every in-flight request (plugin death or unwritable transport).
    fn fail_pending(&self, reason: &str) {
        let mut pending = self.pending.lock().expect("pending mutex");
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(reason.to_string()));
        }
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending mutex").len()
    }

    async fn read_pump<R>(self: Arc<Self>, reader: R, services: Arc<dyn HostServices>)
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let mut reader = BufReader::new(reader);
        let mut line_buf = Vec::new();
        loop {
            let line = match read_line_bounded(&mut reader, &mut line_buf, OverCap::Fail).await {
                Ok(Some(line)) => line,
                Ok(None) => break, // EOF: plugin exited
                Err(e) => {
                    tracing::warn!("plugin read error: {e}");
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let envelope: Envelope = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("plugin sent invalid JSON: {e}");
                    continue;
                }
            };
            match envelope {
                Envelope::Response { id, result, error } => {
                    let tx = self.pending.lock().expect("pending mutex").remove(&id);
                    if let Some(tx) = tx {
                        let _ = tx.send(match (result, error) {
                            (Some(result), None) => Ok(result),
                            (None, Some(error)) => Err(error),
                            // A response carrying BOTH is malformed: letting
                            // the result silently win would hide the error.
                            (Some(_), Some(error)) => Err(format!(
                                "malformed response: both result and error ({error})"
                            )),
                            (None, None) => Err("malformed response".to_string()),
                        });
                    }
                }
                Envelope::Request { id, method, params } => {
                    let services = services.clone();
                    let peer = self.clone();
                    // Concurrent handling: plugins may issue several requests.
                    tokio::spawn(async move {
                        let response = match services.handle_request(&method, params).await {
                            Ok(result) => Envelope::result(id, result),
                            Err(e) => Envelope::error(id, e),
                        };
                        let _ = peer.send(&response).await;
                    });
                }
                Envelope::Event { event, payload } => {
                    if event == "register" {
                        match serde_json::from_value::<RegisterPayload>(payload.clone()) {
                            Ok(register) => {
                                *self.register.lock().await = Some(register);
                            }
                            Err(e) => {
                                tracing::warn!("bad register payload: {e}");
                            }
                        }
                    }
                    services.handle_event(&event, payload).await;
                }
            }
        }
        // Connection closed: fail every pending request.
        self.alive.store(false, Ordering::SeqCst);
        self.fail_pending("plugin exited");
    }
}

/// True for environment variable names that typically carry credentials
/// (`OPENAI_API_KEY`, `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, ...).
/// Plugin child processes must not inherit these by default — a plugin is
/// third-party code and the host's API keys are not its business.
fn is_sensitive_env_key(key: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_ACCESS_KEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_CREDENTIALS",
        "_PRIVATE_KEY",
    ];
    let upper = key.to_ascii_uppercase();
    SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
        || matches!(
            upper.as_str(),
            "API_KEY" | "TOKEN" | "SECRET" | "PASSWORD" | "CREDENTIALS"
        )
}

/// Inherited-env vars to strip from a plugin child: sensitive-looking keys
/// that the plugin manifest did NOT explicitly re-declare. (We
/// `env_remove` individual vars instead of `env_clear`ing: clearing breaks
/// process startup on Windows, where e.g. `SystemRoot` is required.)
pub(crate) fn env_vars_to_strip(
    parent: &[(String, String)],
    declared: &[(String, String)],
) -> Vec<String> {
    parent
        .iter()
        .map(|(k, _)| k)
        .filter(|k| is_sensitive_env_key(k))
        .filter(|k| !declared.iter().any(|(dk, _)| dk == *k))
        .cloned()
        .collect()
}

/// A plugin child process plus its peer. Dropping kills the child.
pub struct PluginProcess {
    pub peer: Arc<PluginPeer>,
    child: tokio::process::Child,
}

impl std::fmt::Debug for PluginProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginProcess")
            .field("alive", &self.peer.is_alive())
            .finish()
    }
}

impl PluginProcess {
    /// Spawn `program` with piped stdio; stderr is forwarded to tracing.
    pub async fn spawn(
        program: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &std::path::Path,
        services: Arc<dyn HostServices>,
    ) -> Result<Self, String> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        for (k, v) in env {
            command.env(k, v);
        }
        // Strip credentials from the inherited environment AFTER applying
        // the manifest's explicit `env` (env_remove would otherwise also
        // drop a key the manifest deliberately declared). vars_os: a
        // non-UTF-8 var must not panic the spawn path.
        let parent_env: Vec<(String, String)> = std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for key in env_vars_to_strip(&parent_env, env) {
            command.env_remove(&key);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn plugin {program}: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                match read_line_bounded(&mut reader, &mut buf, OverCap::Discard).await {
                    Ok(Some(line)) => tracing::info!(target: "tack_ext::plugin_stderr", "{line}"),
                    Ok(None) => break,
                    // Over-cap line (InvalidData): keep draining so the
                    // plugin never blocks on a full stderr pipe.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        tracing::warn!(target: "tack_ext::plugin_stderr", "oversized stderr line dropped");
                    }
                    // Genuine IO error: continuing would spin on a
                    // persistently failing reader.
                    Err(e) => {
                        tracing::warn!(target: "tack_ext::plugin_stderr", "stderr read failed: {e}");
                        break;
                    }
                }
            }
        });
        let peer = PluginPeer::new(stdout, stdin, services);
        Ok(PluginProcess { peer, child })
    }

    /// Graceful shutdown: `shutdown` event, brief wait, then force kill.
    pub async fn shutdown(&mut self) {
        // A plugin that stopped reading stdin must not stall shutdown
        // behind the 30s general write bound (see SHUTDOWN_WRITE_TIMEOUT).
        self.peer.set_write_timeout(SHUTDOWN_WRITE_TIMEOUT);
        let _ = self.peer.send_event("shutdown", Value::Null).await;
        let wait = tokio::time::timeout(Duration::from_secs(2), self.child.wait());
        if wait.await.is_err() {
            let _ = self.child.kill().await;
        }
        // Wait for the read pump to observe the EOF so `is_alive()` is
        // deterministic once shutdown returns (previously racy: the child
        // had exited but the pump task had not been scheduled yet).
        let _ = tokio::time::timeout(Duration::from_secs(2), self.peer.wait_dead()).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct EchoServices;

    #[async_trait::async_trait]
    impl HostServices for EchoServices {
        async fn handle_request(&self, method: &str, params: Value) -> Result<Value, String> {
            Ok(serde_json::json!({ "method": method, "echo": params }))
        }
        async fn handle_event(&self, _event: &str, _payload: Value) {}
    }

    /// Drive a fake plugin over the other end of the duplex.
    async fn fake_plugin(
        mut reader: impl AsyncRead + Send + Unpin,
        mut writer: impl AsyncWrite + Send + Unpin,
    ) {
        // Read initialize, answer register.
        let mut lines = BufReader::new(&mut reader).lines();
        let init = lines.next_line().await.unwrap().unwrap();
        assert!(init.contains("initialize"));
        let register = Envelope::event(
            "register",
            serde_json::to_value(RegisterPayload {
                protocol: Some(crate::protocol::PROTOCOL_VERSION),
                name: Some("fake".to_string()),
                tools: vec![],
                commands: vec![],
                shortcuts: vec![],
                subscriptions: vec![],
                ..Default::default()
            })
            .unwrap(),
        );
        writer
            .write_all(format!("{}\n", serde_json::to_string(&register).unwrap()).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();
        // Answer requests with echo results.
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(Envelope::Request { id, method, params }) =
                serde_json::from_str::<Envelope>(&line)
            {
                let response = Envelope::result(
                    id,
                    serde_json::json!({ "method": method, "params": params }),
                );
                writer
                    .write_all(
                        format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes(),
                    )
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn handshake_and_call_over_duplex() {
        let (host_reader, plugin_writer) = tokio::io::duplex(8192);
        let (plugin_reader, host_writer) = tokio::io::duplex(8192);
        tokio::spawn(fake_plugin(plugin_reader, plugin_writer));

        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));
        let register = peer
            .initialize(InitializePayload {
                protocol: 1,
                mode: "tui".to_string(),
                cwd: "/tmp".to_string(),
                trusted: true,
                host: "tack-test".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(register.name.as_deref(), Some("fake"));

        let result = peer
            .call(
                "tool.execute",
                serde_json::json!({ "name": "t", "arguments": {"x": 1} }),
            )
            .await
            .unwrap();
        assert_eq!(result["method"], "tool.execute");
        assert_eq!(result["params"]["arguments"]["x"], 1);
    }

    /// When the write side of the transport breaks, every in-flight request
    /// must fail immediately — not after its full timeout. (The read pump
    /// only drains pending on EOF; a half-broken pipe must not strand
    /// callers for 30s.)
    #[tokio::test]
    async fn write_failure_fails_pending_requests_fast() {
        let (host_reader, plugin_writer) = tokio::io::duplex(8192);
        let (mut plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));

        // In-flight request A: written successfully, never answered.
        let a = {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.call_with_timeout(
                    "tool.execute",
                    serde_json::json!({}),
                    Duration::from_secs(30),
                )
                .await
            })
        };
        // Read A's request so we know it was sent, then break the write side
        // by dropping the plugin's read half (plugin_writer stays open, so
        // the host's read pump keeps blocking and does NOT drain pending).
        let mut lines = BufReader::new(&mut plugin_reader).lines();
        let _req = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(lines);
        drop(plugin_reader);
        let _keep_open = plugin_writer;

        // The next call fails on write and marks the peer dead.
        let err = peer
            .call("tool.execute", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.contains("write failed") || err.contains("dead"),
            "{err}"
        );

        // A must resolve NOW, not after its 30s timeout.
        let a_result = tokio::time::timeout(std::time::Duration::from_secs(5), a)
            .await
            .expect("pending request stranded after write failure")
            .unwrap();
        assert!(a_result.is_err());
    }

    /// A plugin that stops reading its stdin must not hang the host: once
    /// the pipe buffer is full the write is bounded by the write timeout,
    /// after which the peer is failed and every caller unblocks — instead
    /// of queueing on the writer mutex behind a write that can never
    /// complete (which also hung `shutdown`).
    #[tokio::test]
    async fn stalled_stdin_write_times_out_and_fails_peer() {
        let (host_reader, _plugin_writer) = tokio::io::duplex(8192);
        // The plugin's read half stays OPEN but is never read: writes fill
        // the 8 KiB buffer and then pend (a dropped half would instead
        // fail the write instantly with EPIPE — a different path).
        let (_plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));
        peer.set_write_timeout(Duration::from_millis(200));

        // An in-flight request that never gets answered: it must be failed
        // by the stalled write, not by its own 30s timeout.
        let call = {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.call_with_timeout(
                    "tool.execute",
                    serde_json::json!({}),
                    Duration::from_secs(30),
                )
                .await
            })
        };
        // Wait until the request went out (8 KiB buffer absorbs it).
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(peer.pending_count(), 1);

        // A payload far larger than the pipe buffer stalls the write.
        let big = serde_json::json!({ "blob": "x".repeat(1024 * 1024) });
        let err = tokio::time::timeout(Duration::from_secs(10), peer.send_event("data", big))
            .await
            .expect("stalled write was not bounded")
            .unwrap_err();
        assert!(err.contains("stalled"), "{err}");
        assert!(!peer.is_alive());

        // The in-flight request resolves NOW (failed by the write stall),
        // long before its 30s timeout.
        let err = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("pending request stranded behind a stalled write")
            .unwrap()
            .unwrap_err();
        assert!(err.contains("write failed"), "{err}");

        // Later calls fail fast instead of queueing behind the dead writer.
        let err = peer
            .call("tool.execute", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("dead"), "{err}");
    }

    /// A cancelled call must not leak its pending-map entry: ExtTool races
    /// `call` against the cancellation token, and on cancel the call future
    /// is dropped — bypassing both the timeout-removal and the
    /// response-removal paths.
    #[tokio::test]
    async fn cancelled_call_does_not_leak_pending_entry() {
        let (host_reader, _plugin_writer) = tokio::io::duplex(8192);
        let (mut plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));

        let task = {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.call_with_timeout(
                    "tool.execute",
                    serde_json::json!({}),
                    Duration::from_secs(60),
                )
                .await
            })
        };
        // Wait until the request actually went out.
        let mut lines = BufReader::new(&mut plugin_reader).lines();
        let _req = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(peer.pending_count(), 1);

        task.abort();
        let _ = task.await;
        // Give the drop a moment (abort is not synchronous with Drop).
        for _ in 0..50 {
            if peer.pending_count() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("cancelled call leaked its pending entry");
    }

    /// A plugin that streams an unterminated mega-line must not grow the
    /// host's read buffer without bound: the peer is declared dead and
    /// in-flight requests fail fast.
    #[tokio::test]
    async fn unterminated_huge_line_kills_peer() {
        let (host_reader, mut plugin_writer) = tokio::io::duplex(8192);
        let (_plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));

        let call = {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.call_with_timeout(
                    "tool.execute",
                    serde_json::json!({}),
                    Duration::from_secs(60),
                )
                .await
            })
        };
        // Flood without a newline: well past the per-line cap.
        let flood = tokio::spawn(async move {
            let chunk = vec![b'a'; 64 * 1024];
            for _ in 0..400 {
                // 25MB total, no '\n'
                if plugin_writer.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            // Keep the writer OPEN: the peer must die from the oversized
            // line, not from a clean EOF after buffering everything.
            std::future::pending::<()>().await;
        });
        let err = tokio::time::timeout(std::time::Duration::from_secs(15), call)
            .await
            .expect("peer did not reject the huge line")
            .unwrap()
            .unwrap_err();
        assert!(err.contains("exited") || err.contains("cap"), "{err}");
        assert!(!peer.is_alive());
        flood.abort();
    }

    #[tokio::test]
    async fn read_line_bounded_edges() {
        let data: &[u8] = b"{\"a\":1}\r\nunterminated tail";
        let mut reader = BufReader::new(data);
        let mut buf = Vec::new();
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap()
                .as_deref(),
            Some("{\"a\":1}")
        );
        // Unterminated final line at EOF is still delivered.
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap()
                .as_deref(),
            Some("unterminated tail")
        );
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap(),
            None
        );
    }

    /// Discard mode: the over-cap line is consumed (the reader keeps
    /// making progress — no spin on the same chunk), Err fires once at
    /// its terminator, and the following line reads normally.
    #[tokio::test]
    async fn read_line_bounded_discard_mode_drains_oversized_line() {
        let oversized = "x".repeat(MAX_LINE_BYTES + 10);
        let data = format!("{oversized}\nnext line\n");
        let mut reader = BufReader::new(data.as_bytes());
        let mut buf = Vec::new();
        let err = read_line_bounded(&mut reader, &mut buf, OverCap::Discard)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cap"), "{err}");
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Discard)
                .await
                .unwrap()
                .as_deref(),
            Some("next line")
        );
    }

    /// Drive a fake plugin whose `register` event carries `protocol`.
    async fn handshake_with_plugin_protocol(
        plugin_protocol: Option<u32>,
    ) -> Result<RegisterPayload, String> {
        let (host_reader, mut plugin_writer) = tokio::io::duplex(8192);
        let (mut plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));
        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut plugin_reader).lines();
            let _init = lines.next_line().await;
            let register = Envelope::event(
                "register",
                serde_json::to_value(RegisterPayload {
                    protocol: plugin_protocol,
                    name: Some("fake".to_string()),
                    ..Default::default()
                })
                .unwrap(),
            );
            let mut text = serde_json::to_string(&register).unwrap();
            text.push('\n');
            let _ = plugin_writer.write_all(text.as_bytes()).await;
            let _ = plugin_writer.flush().await;
            // Keep the writer open until the host is done.
            std::future::pending::<()>().await;
        });
        peer.initialize(InitializePayload {
            protocol: crate::protocol::PROTOCOL_VERSION,
            mode: "tui".to_string(),
            cwd: "/tmp".to_string(),
            trusted: true,
            host: "tack-test".to_string(),
        })
        .await
    }

    /// A plugin built against a NEWER protocol than the host must be
    /// rejected at the handshake — it may rely on behavior this host does
    /// not implement.
    #[tokio::test]
    async fn handshake_rejects_newer_plugin_protocol() {
        let err = handshake_with_plugin_protocol(Some(crate::protocol::PROTOCOL_VERSION + 1))
            .await
            .unwrap_err();
        assert!(err.contains("requires protocol"), "{err}");
    }

    /// Older (or unreported, pre-versioning) plugin protocols still load
    /// (with a warning) — breaking every existing plugin on a host bump is
    /// not an option.
    #[tokio::test]
    async fn handshake_accepts_older_or_unreported_protocol() {
        assert!(handshake_with_plugin_protocol(Some(0)).await.is_ok());
        assert!(handshake_with_plugin_protocol(None).await.is_ok());
    }

    /// A response carrying BOTH result and error is malformed: the result
    /// must not silently win and hide the error.
    #[tokio::test]
    async fn response_with_result_and_error_is_malformed() {
        let (host_reader, mut plugin_writer) = tokio::io::duplex(8192);
        let (mut plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));
        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut plugin_reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(Envelope::Request { id, .. }) = serde_json::from_str::<Envelope>(&line) {
                    let raw = serde_json::json!({
                        "type": "response",
                        "id": id,
                        "result": {"content": "looks fine"},
                        "error": "but actually failed",
                    });
                    let mut text = serde_json::to_string(&raw).unwrap();
                    text.push('\n');
                    if plugin_writer.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = plugin_writer.flush().await;
                }
            }
        });
        let err = peer
            .call("tool.execute", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("malformed response"), "{err}");
    }

    /// Credential-shaped env vars are stripped from plugin children;
    /// manifest-declared vars survive even when they look sensitive.
    #[test]
    fn sensitive_env_vars_are_stripped_unless_declared() {
        let parent = vec![
            ("OPENAI_API_KEY".to_string(), "sk-1".to_string()),
            ("GITHUB_TOKEN".to_string(), "tok".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "sec".to_string()),
            ("DB_PASSWORD".to_string(), "pw".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/u".to_string()),
            ("MY_PLUGIN_TOKEN".to_string(), "needed".to_string()),
        ];
        let declared = vec![("MY_PLUGIN_TOKEN".to_string(), "needed".to_string())];
        let stripped = env_vars_to_strip(&parent, &declared);
        assert!(stripped.contains(&"OPENAI_API_KEY".to_string()));
        assert!(stripped.contains(&"GITHUB_TOKEN".to_string()));
        assert!(stripped.contains(&"AWS_SECRET_ACCESS_KEY".to_string()));
        assert!(stripped.contains(&"DB_PASSWORD".to_string()));
        assert!(!stripped.contains(&"PATH".to_string()));
        assert!(!stripped.contains(&"HOME".to_string()));
        assert!(
            !stripped.contains(&"MY_PLUGIN_TOKEN".to_string()),
            "manifest-declared vars must survive"
        );
    }

    #[test]
    fn sensitive_env_key_detection() {
        assert!(is_sensitive_env_key("ANTHROPIC_API_KEY"));
        assert!(is_sensitive_env_key("NPM_TOKEN"));
        assert!(is_sensitive_env_key("APP_SECRET"));
        assert!(!is_sensitive_env_key("PATH"));
        assert!(!is_sensitive_env_key("TOKENIZER_THREADS")); // no _TOKEN suffix
        assert!(!is_sensitive_env_key("SECRETARY_NAME"));
    }

    #[tokio::test]
    async fn plugin_requests_reach_services() {
        let (host_reader, plugin_writer) = tokio::io::duplex(8192);
        let (plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(EchoServices));

        // Plugin issues a request; EchoServices answers. The response must
        // be read from the plugin's READ half.
        let (mut plugin_reader, mut plugin_writer) = (plugin_reader, plugin_writer);
        plugin_writer
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&Envelope::request(
                        1,
                        "ui.notify",
                        serde_json::json!({"message": "hi"})
                    ))
                    .unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        plugin_writer.flush().await.unwrap();
        let mut lines = BufReader::new(&mut plugin_reader).lines();
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(response.contains("ui.notify"), "{response}");
        drop(peer);
    }
}
