use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::convert::{normalize_uri, path_to_uri};
use super::manager::{ServerSpec, language_id};

/// Cap on concurrently open documents per server. Syncing a new document
/// beyond the cap `didClose`s the least-recently-synced one (and drops its
/// cached diagnostics) so server memory stays bounded in long sessions.
const MAX_OPEN_DOCS: usize = 64;

#[derive(Clone, Debug)]
pub struct Diag {
    pub line: u32,
    pub character: u32,
    pub severity: u8, // 1 error, 2 warning, 3 info, 4 hint
    pub message: String,
    pub source: Option<String>,
    /// Diagnostic code (LSP `code`: string or integer), e.g. `E0308`.
    pub code: Option<String>,
}

impl Diag {
    pub(crate) fn severity_label(&self) -> &'static str {
        match self.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        }
    }
}

/// Hard cap on any single LSP request (a wedged server must not hang a
/// tool call forever; the reader failing pending requests only covers
/// requests issued BEFORE the crash).
const REQUEST_TIMEOUT_SECS: u64 = 60;

/// Server-reported analysis status: rust-analyzer's
/// `experimental/serverStatus` (quiescent = indexing + flycheck done) and
/// `$/progress` work-done tokens. Drives the "analysis still running"
/// wording so a clean syntax pass is never reported as fully type-checked.
#[derive(Default, Debug)]
struct ClientStatus {
    /// `experimental/serverStatus` quiescent flag; None until reported.
    quiescent: Option<bool>,
    /// Optional server status message (e.g. "cargo check failed").
    message: Option<String>,
    /// In-flight `$/progress` tokens (begin minus end).
    progress_tokens: HashSet<String>,
    /// True once any serverStatus/$/progress was observed — the server
    /// reports analysis state at all, so `pending` is meaningful.
    reports_status: bool,
}

/// Snapshot of `ClientStatus` for display/decisions.
#[derive(Clone, Debug)]
pub struct AnalysisState {
    /// The server reports analysis status (rust-analyzer does).
    pub known: bool,
    /// Background work (indexing / flycheck) is still running — cached
    /// diagnostics may only cover the syntax pass.
    pub pending: bool,
    /// Optional server status message.
    pub message: Option<String>,
}

impl AnalysisState {
    fn activity(&self) -> String {
        self.message
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default()
    }

    /// Confidence note for EMPTY diagnostics output, or None for a
    /// trustworthy all-clear (settled, status-reporting server). The
    /// empty+pending case is the false-trust trap — "no problems found"
    /// while the type check is still catching up must NOT read as an
    /// all-clear — so the note says what is running and that the result
    /// is incomplete, in so many words. `verifier` names the
    /// ground-truth checker for this language (see `verifier_hint`):
    /// some error classes never reach server diagnostics at all (e.g.
    /// Rust macro expansion only shows via cargo).
    pub fn clean_confidence_note(&self, verifier: &str) -> Option<String> {
        if self.pending {
            Some(format!(
                "analysis in progress{} — results are INCOMPLETE, not a clean bill of health; some error classes only show via {verifier}",
                self.activity()
            ))
        } else if !self.known {
            Some(
                "this server does not report analysis progress — freshness not guaranteed"
                    .to_string(),
            )
        } else {
            None
        }
    }

    /// Shorter suffix for NON-empty diagnostics output (the reader
    /// already has problems to work on; just flag that the list may
    /// grow).
    pub fn more_may_follow_note(&self) -> Option<String> {
        self.pending.then(|| {
            format!(
                "analysis in progress{} — more diagnostics may follow",
                self.activity()
            )
        })
    }
}

/// The ground-truth checker for a language server, named in the
/// confidence note so the agent verifies with the RIGHT tool instead of
/// trusting incomplete diagnostics. Keyed by the server binary name
/// (default_servers uses fixed names; custom servers fall back generic).
pub(crate) fn verifier_hint(command: &str) -> &'static str {
    // Basename by hand: Path::file_name only honors the HOST separator,
    // but a configured command may be a Windows path on any host config.
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    match name {
        // Macro expansion and the full type check only exist in cargo's
        // view; the server covers the syntax pass + earlier batches.
        "rust-analyzer" => "cargo check / clippy",
        // tsserver is per-file; project-wide/reference-graph errors need
        // the compiler proper.
        "typescript-language-server" => "tsc",
        "gopls" => "go build / go vet",
        "pyright-langserver" => "mypy / the test suite",
        "clangd" => "a full build",
        _ => "a full build or type-check",
    }
}

/// State shared between the client handle and its reader task.
pub(crate) struct ReaderShared {
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>,
    /// uri → latest published diagnostics (parsed for display).
    diagnostics: Mutex<HashMap<String, Vec<Diag>>>,
    /// uri → latest published diagnostics (raw, for codeAction context).
    diagnostics_raw: Mutex<HashMap<String, Vec<Value>>>,
    /// uri → global diag-version of that uri's last publishDiagnostics.
    /// Waits for a specific file must not be satisfied by an unrelated
    /// file's publish (the global counter alone allowed that).
    uri_versions: Mutex<HashMap<String, u64>>,
    /// Bumped on every publishDiagnostics; receivers wait on it.
    diag_version: tokio::sync::watch::Sender<u64>,
    status: Mutex<ClientStatus>,
    /// Bumped on every status change (serverStatus/progress).
    status_watch: tokio::sync::watch::Sender<u64>,
    writer: tokio::sync::mpsc::UnboundedSender<Value>,
}

impl ReaderShared {
    pub(crate) fn new(writer: tokio::sync::mpsc::UnboundedSender<Value>) -> Self {
        ReaderShared {
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            diagnostics_raw: Mutex::new(HashMap::new()),
            uri_versions: Mutex::new(HashMap::new()),
            diag_version: tokio::sync::watch::channel(0u64).0,
            status: Mutex::new(ClientStatus::default()),
            status_watch: tokio::sync::watch::channel(0u64).0,
            writer,
        }
    }

    fn note_status_change(&self, f: impl FnOnce(&mut ClientStatus)) {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
        let next = self.status_watch.borrow().wrapping_add(1);
        // send_replace: stores even with zero receivers (watch::send would
        // silently drop the bump, breaking later waiters).
        self.status_watch.send_replace(next);
    }
}

pub struct LspClient {
    pub(crate) shared: Arc<ReaderShared>,
    pub(crate) next_id: AtomicU64,
    /// uri → (LSP document version, last-sync sequence for LRU eviction).
    pub(crate) open_docs: Mutex<HashMap<String, (u64, u64)>>,
    /// Global monotonic counter backing the LRU order in `open_docs`.
    pub(crate) sync_seq: AtomicU64,
    /// Set once the first publishDiagnostics burst arrived (server has
    /// finished initial analysis; navigation requests become reliable).
    pub(crate) warmed: std::sync::atomic::AtomicBool,
    /// False once the server's stdout reached EOF (process exited).
    pub(crate) alive: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) child: Mutex<tokio::process::Child>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient").finish_non_exhaustive()
    }
}

impl LspClient {
    /// Spawn + initialize a server. `roots[0]` becomes rootUri; all roots go
    /// out as workspaceFolders (multi-root sessions).
    pub async fn spawn(spec: &ServerSpec, roots: &[PathBuf]) -> Result<Arc<LspClient>, String> {
        let root = roots.first().ok_or("no workspace roots")?;
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Capture stderr (was Stdio::null): server crash root causes
            // were undiagnosable with the output discarded.
            .stderr(Stdio::piped())
            .current_dir(root)
            // A dropped Child must not leave a server process behind (e.g.
            // when initialize fails after a successful spawn).
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", spec.command))?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        {
            let server = spec.command.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim_end();
                    if !line.is_empty() {
                        tracing::debug!(%server, "LSP stderr: {line}");
                    }
                }
            });
        }

        let (writer, mut writer_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = writer_rx.recv().await {
                let body = message.to_string();
                let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
                if stdin.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let shared = Arc::new(ReaderShared::new(writer.clone()));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        {
            let shared = shared.clone();
            let alive = alive.clone();
            tokio::spawn(async move {
                read_frames(stdout, shared).await;
                // EOF: the server process exited. Flip liveness so the
                // manager can respawn instead of reusing a corpse.
                alive.store(false, Ordering::SeqCst);
            });
        }

        let client = Arc::new(LspClient {
            shared,
            next_id: AtomicU64::new(1),
            open_docs: Mutex::new(HashMap::new()),
            sync_seq: AtomicU64::new(1),
            warmed: std::sync::atomic::AtomicBool::new(false),
            alive,
            child: Mutex::new(child),
        });

        // initialize handshake (generous timeout: first spawn may index).
        let root_uri = path_to_uri(root);
        let folders: Vec<Value> = roots
            .iter()
            .enumerate()
            .map(|(i, r)| {
                json!({
                    "uri": path_to_uri(r),
                    "name": r
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("root-{i}")),
                })
            })
            .collect();
        let init = client.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": false },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "codeAction": {
                            "codeActionLiteralSupport": {
                                "codeActionKind": {
                                    "valueSet": ["quickfix", "refactor", "source"]
                                }
                            }
                        },
                        "implementation": { "dynamicRegistration": false },
                        "callHierarchy": { "dynamicRegistration": false }
                    },
                    "workspace": { "configuration": false },
                    // rust-analyzer gates experimental/serverStatus behind
                    // this capability; it drives the "analysis still
                    // running" wording in diagnostics feedback.
                    "experimental": { "serverStatusNotification": true }
                },
                "workspaceFolders": folders,
                "clientInfo": { "name": "tack" }
            }),
        );
        match tokio::time::timeout(std::time::Duration::from_secs(60), init).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("initialize failed: {e}")),
            Err(_) => return Err("initialize timed out (60s)".to_string()),
        }
        client.notify("initialized", json!({}));
        Ok(client)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        // Fast-fail on a dead server: the writer task may still accept
        // frames (stdin write not yet failed), which would strand the
        // request in `pending` forever now that the reader is gone.
        if !self.is_alive() {
            return Err("language server process exited".to_string());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        self.shared
            .writer
            .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .map_err(|_| "server writer closed".to_string())?;
        match tokio::time::timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS), rx).await {
            Ok(Ok(value)) => match value.get("__lsp_error") {
                Some(error) => Err(format!("LSP error: {error}")),
                None => Ok(value),
            },
            Ok(Err(_)) => Err("server dropped the response".to_string()),
            Err(_) => {
                self.shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(format!("{method} timed out ({REQUEST_TIMEOUT_SECS}s)"))
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self
            .shared
            .writer
            .send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// False once the server process exited (reader hit EOF).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Sync a file's on-disk content to the server (didOpen on first touch,
    /// full-content didChange afterwards). Evicts the least-recently-synced
    /// document (didClose + local cache drop) when the open-doc cap is hit.
    /// Returns the diagnostics-watch version observed *before* syncing, for
    /// change detection.
    pub fn sync_document(&self, path: &Path, text: &str) -> (String, u64) {
        let uri = path_to_uri(path);
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let before = *self.shared.diag_version.borrow();
        let mut docs = self.open_docs.lock().unwrap_or_else(|e| e.into_inner());
        match docs.get_mut(&uri) {
            None => {
                // LRU cap: close the least-recently-synced documents until
                // there is room, dropping their cached diagnostics too.
                while docs.len() >= MAX_OPEN_DOCS {
                    let Some(evict) = evict_oldest_open_doc(&docs) else {
                        break;
                    };
                    docs.remove(&evict);
                    self.notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": evict } }),
                    );
                    self.shared
                        .diagnostics
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&evict);
                    self.shared
                        .diagnostics_raw
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&evict);
                    self.shared
                        .uri_versions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&evict);
                }
                let seq = self.sync_seq.fetch_add(1, Ordering::Relaxed);
                docs.insert(uri.clone(), (1, seq));
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(&ext),
                            "version": 1,
                            "text": text,
                        }
                    }),
                );
            }
            Some(entry) => {
                entry.0 += 1;
                entry.1 = self.sync_seq.fetch_add(1, Ordering::Relaxed);
                let version = entry.0;
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    }),
                );
            }
        }
        (uri, before)
    }

    /// Wait for a publishDiagnostics newer than `since` (or any, when
    /// `since_any`), up to `timeout`. Returns true when fresh diagnostics
    /// arrived.
    pub async fn wait_fresh(&self, since: u64, timeout: std::time::Duration) -> bool {
        let mut rx = self.shared.diag_version.subscribe();
        if *rx.borrow() > since {
            return true;
        }
        tokio::time::timeout(timeout, async {
            while *rx.borrow_and_update() <= since {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
    }

    /// Global diag-version of `uri`'s last publish (0 = never).
    pub fn uri_version(&self, uri: &str) -> u64 {
        self.shared
            .uri_versions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&normalize_uri(uri))
            .copied()
            .unwrap_or(0)
    }

    /// Like [`Self::wait_fresh`], but only satisfied by a publish for
    /// `uri` itself — an unrelated file's diagnostics must not unblock a
    /// waiter on this file.
    pub async fn wait_fresh_uri(
        &self,
        uri: &str,
        since: u64,
        timeout: std::time::Duration,
    ) -> bool {
        if self.uri_version(uri) > since {
            return true;
        }
        let mut rx = self.shared.diag_version.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                rx.borrow_and_update();
                if self.uri_version(uri) > since {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    /// Server-reported analysis status (rust-analyzer serverStatus /
    /// work-done progress). `known=false` means the server never reports
    /// status and `pending` is meaningless.
    pub fn analysis_state(&self) -> AnalysisState {
        let status = self.shared.status.lock().unwrap_or_else(|e| e.into_inner());
        AnalysisState {
            known: status.reports_status,
            pending: status.quiescent == Some(false) || !status.progress_tokens.is_empty(),
            message: status.message.clone(),
        }
    }

    /// Wait until background analysis (indexing + flycheck) is done, up to
    /// `timeout`. Returns true when quiescent (or the server never reports
    /// status — nothing to wait for).
    pub async fn wait_quiescent(&self, timeout: std::time::Duration) -> bool {
        if !self.analysis_state().pending {
            return true;
        }
        let mut rx = self.shared.status_watch.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                rx.borrow_and_update();
                if !self.analysis_state().pending {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    pub fn diagnostics_for(&self, uri: &str) -> Vec<Diag> {
        self.shared
            .diagnostics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&normalize_uri(uri))
            .cloned()
            .unwrap_or_default()
    }

    /// Raw published diagnostics for a uri (codeAction context).
    pub fn diagnostics_raw_for(&self, uri: &str) -> Vec<Value> {
        self.shared
            .diagnostics_raw
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&normalize_uri(uri))
            .cloned()
            .unwrap_or_default()
    }

    pub fn all_diagnostics(&self) -> Vec<(String, Vec<Diag>)> {
        self.shared
            .diagnostics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(u, d)| (u.clone(), d.clone()))
            .collect()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }
}

/// Frame reader: decode LSP frames from `stdout` until EOF, dispatching
/// messages. On exit (server died), every still-pending request is failed —
/// otherwise callers would hang forever waiting on a response that will
/// never arrive.
async fn read_frames<R>(mut stdout: R, shared: Arc<ReaderShared>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        loop {
            match take_frame(&buf) {
                FrameRead::Incomplete => break,
                FrameRead::Invalid(rest) => {
                    tracing::debug!("LSP: skipping unparseable frame");
                    buf = rest;
                }
                FrameRead::Message(message, rest) => {
                    buf = rest;
                    if std::env::var_os("TACK_LSP_TRACE").is_some() {
                        // chars().take() instead of a byte slice: the server
                        // controls this JSON and a byte cut could split a
                        // multi-byte UTF-8 char and panic.
                        let msg = message.to_string();
                        let preview: String = msg.chars().take(400).collect();
                        eprintln!("[lsp <-] {preview}");
                    }
                    dispatch(&message, &shared);
                }
            }
        }
    }
    // Fail pending requests: dropping the senders resolves each awaiting
    // `request()` with "server dropped the response".
    shared
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Outcome of trying to extract one LSP frame from the buffer head.
#[derive(Debug)]
enum FrameRead {
    /// Not enough bytes yet (or an unrecoverable header).
    Incomplete,
    /// A complete, parseable message + the remaining bytes.
    Message(Value, Vec<u8>),
    /// A complete frame whose body was not valid JSON — consumed and
    /// skipped so one bad frame cannot wedge the client permanently.
    Invalid(Vec<u8>),
}

/// Extract one LSP frame from the buffer head.
fn take_frame(buf: &[u8]) -> FrameRead {
    let Some(header_end) = find_subslice(buf, b"\r\n\r\n") else {
        return FrameRead::Incomplete;
    };
    let Ok(header) = std::str::from_utf8(&buf[..header_end]) else {
        return FrameRead::Incomplete;
    };
    let mut length = None;
    for line in header.split("\r\n") {
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else {
        return FrameRead::Incomplete;
    };
    // Guard against hostile/buggy servers: cap the frame size and avoid
    // overflow in `start + length` (a huge Content-Length would otherwise
    // panic the reader task, bypassing the crash-restart logic since
    // `alive` is never cleared).
    const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
    if length > MAX_FRAME_BYTES {
        // Consume the header and skip the frame; the stream is likely
        // desynced anyway, but this keeps the reader alive.
        return FrameRead::Invalid(buf[header_end + 4..].to_vec());
    }
    let start = header_end + 4;
    let Some(end) = start.checked_add(length) else {
        return FrameRead::Invalid(buf[start..].to_vec());
    };
    if buf.len() < end {
        return FrameRead::Incomplete;
    }
    let rest = buf[end..].to_vec();
    match serde_json::from_slice(&buf[start..end]) {
        Ok(body) => FrameRead::Message(body, rest),
        Err(_) => FrameRead::Invalid(rest),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn dispatch(message: &Value, shared: &Arc<ReaderShared>) {
    let method = message.get("method").and_then(Value::as_str);
    match method {
        Some("textDocument/publishDiagnostics") => {
            let params = &message["params"];
            let uri = normalize_uri(params["uri"].as_str().unwrap_or_default());
            let raw_diags: Vec<Value> = params["diagnostics"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let diags = raw_diags
                .iter()
                .map(|d| Diag {
                    line: d["range"]["start"]["line"].as_u64().unwrap_or(0) as u32,
                    character: d["range"]["start"]["character"].as_u64().unwrap_or(0) as u32,
                    severity: d["severity"].as_u64().unwrap_or(1) as u8,
                    message: d["message"].as_str().unwrap_or_default().to_string(),
                    source: d["source"].as_str().map(str::to_string),
                    code: parse_diag_code(d),
                })
                .collect::<Vec<_>>();
            shared
                .diagnostics_raw
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(uri.clone(), raw_diags);
            shared
                .diagnostics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(uri.clone(), diags);
            let next = shared.diag_version.borrow().wrapping_add(1);
            shared
                .uri_versions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(uri, next);
            // send_replace: must bump even with zero active receivers.
            shared.diag_version.send_replace(next);
        }
        // rust-analyzer: quiescent = indexing + flycheck finished.
        Some("experimental/serverStatus") => {
            let params = &message["params"];
            let quiescent = params["quiescent"].as_bool();
            let message = params["message"].as_str().map(str::to_string);
            shared.note_status_change(|status| {
                status.reports_status = true;
                if let Some(q) = quiescent {
                    status.quiescent = Some(q);
                }
                status.message = message;
            });
        }
        // Work-done progress (flycheck shows up as begin/end tokens).
        Some("$/progress") => {
            let params = &message["params"];
            let token = params["token"].to_string();
            let kind = params["value"]["kind"].as_str().unwrap_or_default();
            shared.note_status_change(|status| {
                status.reports_status = true;
                match kind {
                    "begin" => {
                        status.progress_tokens.insert(token);
                    }
                    "end" => {
                        status.progress_tokens.remove(&token);
                    }
                    _ => {}
                }
            });
        }
        // Server → client request: answer null so servers don't stall.
        Some(_) => {
            if let Some(id) = message.get("id").cloned() {
                let _ = shared
                    .writer
                    .send(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
            }
        }
        // Response to one of our requests.
        None => {
            if let Some(id) = message.get("id").and_then(Value::as_u64)
                && let Some(tx) = shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id)
            {
                let result = match message.get("error") {
                    Some(error) => json!({ "__lsp_error": error }),
                    None => message.get("result").cloned().unwrap_or(Value::Null),
                };
                let _ = tx.send(result);
            }
        }
    }
}

/// LSP diagnostic `code` may be a string or an integer.
fn parse_diag_code(diag: &Value) -> Option<String> {
    let code = &diag["code"];
    if let Some(s) = code.as_str() {
        return Some(s.to_string()).filter(|s| !s.is_empty());
    }
    code.as_i64().map(|n| n.to_string())
}

/// Pick the uri to evict when the open-document cap is hit: the entry with
/// the smallest sync sequence (least recently synced — sequences are global
/// and monotonic per client).
fn evict_oldest_open_doc(open_docs: &HashMap<String, (u64, u64)>) -> Option<String> {
    open_docs
        .iter()
        .min_by_key(|(_, (_, seq))| *seq)
        .map(|(uri, _)| uri.clone())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn frame_parsing_split_delivery() {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "result": null }).to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let bytes = frame.as_bytes();
        // Not complete yet.
        assert!(matches!(
            take_frame(&bytes[..bytes.len() - 2]),
            FrameRead::Incomplete
        ));
        match take_frame(bytes) {
            FrameRead::Message(message, rest) => {
                assert_eq!(message["id"], 1);
                assert!(rest.is_empty());
            }
            other => panic!(
                "expected a complete frame, got {}",
                match other {
                    FrameRead::Incomplete => "Incomplete",
                    FrameRead::Invalid(_) => "Invalid",
                    FrameRead::Message(..) => "Message",
                }
            ),
        }
    }

    /// Regression: a complete frame with an unparseable JSON body must be
    /// consumed and skipped — previously take_frame returned None without
    /// consuming, wedging the reader on the bad frame forever.
    #[test]
    fn invalid_frame_body_is_skipped() {
        let bad = "not json";
        let good = json!({ "jsonrpc": "2.0", "id": 7, "result": 42 }).to_string();
        let mut bytes = format!("Content-Length: {}\r\n\r\n{bad}", bad.len()).into_bytes();
        bytes.extend_from_slice(format!("Content-Length: {}\r\n\r\n{good}", good.len()).as_bytes());
        match take_frame(&bytes) {
            FrameRead::Invalid(rest) => match take_frame(&rest) {
                FrameRead::Message(message, rest) => {
                    assert_eq!(message["id"], 7);
                    assert!(rest.is_empty());
                }
                _ => panic!("expected the good frame after the bad one"),
            },
            _ => panic!("expected the bad frame to be consumed as invalid"),
        }
    }

    /// Regression: a hostile Content-Length (huge / overflow-inducing)
    /// must not panic the reader task — previously `start + length` could
    /// overflow and slice-index panics killed the reader with `alive`
    /// still true, wedging the client until every request timed out.
    #[test]
    fn hostile_content_length_does_not_panic() {
        let body = json!({ "jsonrpc": "2.0", "id": 9, "result": 1 }).to_string();
        for evil in [usize::MAX.to_string(), (64 * 1024 * 1024 + 1).to_string()] {
            let mut bytes = format!("Content-Length: {evil}\r\n\r\n").into_bytes();
            bytes.extend_from_slice(
                format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes(),
            );
            match take_frame(&bytes) {
                FrameRead::Invalid(rest) => match take_frame(&rest) {
                    FrameRead::Message(message, rest) => {
                        assert_eq!(message["id"], 9);
                        assert!(rest.is_empty());
                    }
                    _ => panic!("expected the good frame after the hostile header"),
                },
                _ => panic!("expected the hostile header to be consumed as invalid"),
            }
        }
    }

    /// Regression: when the server dies, pending requests must fail instead
    /// of hanging forever.
    #[tokio::test]
    async fn pending_requests_fail_when_stream_ends() {
        let (writer, _sink) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(ReaderShared::new(writer));
        let (tx, rx) = tokio::sync::oneshot::channel();
        shared.pending.lock().unwrap().insert(1, tx);
        // Empty stream => immediate EOF.
        read_frames(&b""[..], shared).await;
        assert!(rx.await.is_err(), "pending request must fail at EOF");
    }

    /// An unparseable frame followed by a valid one: the reader skips the
    /// garbage and still dispatches the valid message.
    #[tokio::test]
    async fn reader_recovers_after_garbage_frame() {
        let (writer, _sink) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(ReaderShared::new(writer));
        let mut stream = b"Content-Length: 8\r\n\r\nGARBAGE!".to_vec();
        let diag = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": "file:///tmp/a.rs", "diagnostics": [] }
        })
        .to_string();
        stream
            .extend_from_slice(format!("Content-Length: {}\r\n\r\n{diag}", diag.len()).as_bytes());
        read_frames(&stream[..], shared.clone()).await;
        assert!(
            shared
                .diagnostics
                .lock()
                .unwrap()
                .contains_key("file:///tmp/a.rs"),
            "valid frame after garbage must still be dispatched"
        );
    }

    #[test]
    fn server_status_drives_analysis_state() {
        let (writer, _sink) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(ReaderShared::new(writer));
        // Before any status notification: unknown, never "pending".
        dispatch(
            &json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                     "params": { "uri": "file:///tmp/a.rs", "diagnostics": [] } }),
            &shared,
        );
        {
            let status = shared.status.lock().unwrap();
            assert!(!status.reports_status);
            assert!(status.quiescent.is_none());
        }
        // Indexing: quiescent=false → pending.
        dispatch(
            &json!({ "jsonrpc": "2.0", "method": "experimental/serverStatus",
                     "params": { "health": "ok", "quiescent": false } }),
            &shared,
        );
        {
            let status = shared.status.lock().unwrap();
            assert!(status.reports_status);
            assert_eq!(status.quiescent, Some(false));
        }
        // Flycheck done: quiescent=true → not pending.
        dispatch(
            &json!({ "jsonrpc": "2.0", "method": "experimental/serverStatus",
                     "params": { "health": "ok", "quiescent": true } }),
            &shared,
        );
        let status = shared.status.lock().unwrap();
        assert_eq!(status.quiescent, Some(true));
    }

    #[test]
    fn progress_tokens_drive_analysis_state() {
        let (writer, _sink) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(ReaderShared::new(writer));
        let progress = |kind: &str| {
            json!({ "jsonrpc": "2.0", "method": "$/progress",
                    "params": { "token": "rustAnalyzer/check", "value": { "kind": kind } } })
        };
        dispatch(&progress("begin"), &shared);
        assert_eq!(shared.status.lock().unwrap().progress_tokens.len(), 1);
        // Duplicate begin must not double-count.
        dispatch(&progress("begin"), &shared);
        assert_eq!(shared.status.lock().unwrap().progress_tokens.len(), 1);
        dispatch(&progress("end"), &shared);
        assert!(shared.status.lock().unwrap().progress_tokens.is_empty());
    }

    /// The confidence wording is the guard against the false-trust trap:
    /// an empty-diagnostic result while analysis runs must read as
    /// INCOMPLETE, never as an all-clear.
    #[test]
    fn clean_confidence_note_distinguishes_states() {
        let settled = AnalysisState {
            known: true,
            pending: false,
            message: None,
        };
        assert!(
            settled
                .clean_confidence_note("cargo check / clippy")
                .is_none()
        );

        let pending = AnalysisState {
            known: true,
            pending: true,
            message: None,
        };
        let note = pending
            .clean_confidence_note("cargo check / clippy")
            .unwrap();
        assert!(note.contains("INCOMPLETE"), "{note}");
        assert!(note.contains("not a clean bill of health"), "{note}");
        assert!(note.contains("cargo check / clippy"), "{note}");

        let with_msg = AnalysisState {
            known: true,
            pending: true,
            message: Some("flycheck: 3 crates".to_string()),
        };
        assert!(
            with_msg
                .clean_confidence_note("cargo check / clippy")
                .unwrap()
                .contains("flycheck: 3 crates")
        );

        // A server that never reports status: no false freshness claim.
        let unknown = AnalysisState {
            known: false,
            pending: false,
            message: None,
        };
        assert!(
            unknown
                .clean_confidence_note("tsc")
                .unwrap()
                .contains("does not report")
        );
    }

    #[test]
    fn more_may_follow_note_only_when_pending() {
        let pending = AnalysisState {
            known: true,
            pending: true,
            message: Some("indexing".to_string()),
        };
        assert_eq!(
            pending.more_may_follow_note().unwrap(),
            "analysis in progress (indexing) — more diagnostics may follow"
        );
        let settled = AnalysisState {
            known: true,
            pending: false,
            message: None,
        };
        assert!(settled.more_may_follow_note().is_none());
    }

    /// The ground-truth hint must name the language's actual checker —
    /// "cargo/clippy" in a TypeScript project would be nonsense.
    #[test]
    fn verifier_hint_names_the_languages_checker() {
        assert_eq!(verifier_hint("rust-analyzer"), "cargo check / clippy");
        assert_eq!(verifier_hint("typescript-language-server"), "tsc");
        assert_eq!(verifier_hint("gopls"), "go build / go vet");
        assert_eq!(verifier_hint("pyright-langserver"), "mypy / the test suite");
        assert_eq!(verifier_hint("clangd"), "a full build");
        // Full paths and .exe suffixes resolve to the same server.
        assert_eq!(
            verifier_hint("/usr/local/bin/rust-analyzer"),
            "cargo check / clippy"
        );
        assert_eq!(verifier_hint("C:\\tools\\gopls.exe"), "go build / go vet");
        // Unknown/custom servers get a generic hint.
        assert_eq!(
            verifier_hint("my-custom-server"),
            "a full build or type-check"
        );
    }

    #[tokio::test]
    async fn per_uri_versions_gate_waits() {
        let (writer, _sink) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(ReaderShared::new(writer));
        let publish = |uri: &str| {
            json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri, "diagnostics": [] } })
        };
        dispatch(&publish("file:///tmp/a.rs"), &shared);
        let v_a = *shared.diag_version.borrow();
        dispatch(&publish("file:///tmp/b.rs"), &shared);
        let v_b = *shared.diag_version.borrow();
        assert!(v_b > v_a);
        assert_eq!(shared.uri_versions.lock().unwrap()["file:///tmp/a.rs"], v_a);
        assert_eq!(shared.uri_versions.lock().unwrap()["file:///tmp/b.rs"], v_b);
        // A waiter on a.rs since before v_a: an unrelated b.rs publish
        // (v_b > v_a) must NOT satisfy it — only another a.rs publish does.
        // (Direct wait_fresh_uri coverage needs a client; the version map
        // semantics above are what it keys on.)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_fast_fails_on_dead_server() {
        // A dead client must reject requests immediately — otherwise the
        // write goes to a writer task whose stdin is gone and the request
        // would sit in `pending` until the timeout.
        let (writer, _sink) = tokio::sync::mpsc::unbounded_channel();
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = LspClient {
            shared: Arc::new(ReaderShared::new(writer)),
            next_id: AtomicU64::new(1),
            open_docs: Mutex::new(HashMap::new()),
            sync_seq: AtomicU64::new(1),
            warmed: std::sync::atomic::AtomicBool::new(false),
            alive: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            child: Mutex::new(child),
        };
        let err = client
            .request("textDocument/hover", json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("exited"), "unexpected error: {err}");
    }

    #[test]
    fn diag_code_parses_string_and_number() {
        assert_eq!(
            parse_diag_code(&json!({ "code": "E0308" })).as_deref(),
            Some("E0308")
        );
        assert_eq!(
            parse_diag_code(&json!({ "code": 2315 })).as_deref(),
            Some("2315")
        );
        assert_eq!(parse_diag_code(&json!({})), None);
        assert_eq!(parse_diag_code(&json!({ "code": "" })), None);
    }

    #[test]
    fn open_doc_eviction_picks_least_recently_synced() {
        let mut docs = HashMap::new();
        docs.insert("a".to_string(), (5u64, 30u64));
        docs.insert("b".to_string(), (2u64, 10u64));
        docs.insert("c".to_string(), (9u64, 99u64));
        assert_eq!(evict_oldest_open_doc(&docs).as_deref(), Some("b"));
        // LSP document version (the first tuple element) must not drive
        // eviction — only the sync sequence does.
        docs.insert("d".to_string(), (1u64, 200u64));
        assert_eq!(evict_oldest_open_doc(&docs).as_deref(), Some("b"));
        assert_eq!(evict_oldest_open_doc(&HashMap::new()), None);
    }
}

#[cfg(test)]
mod proptests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Arbitrary bytes must never panic the frame parser.
        #[test]
        fn take_frame_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = take_frame(&bytes);
        }

        /// Valid frames (any JSON body, split at any point) parse completely
        /// and consume exactly the frame.
        #[test]
        fn take_frame_roundtrip(body in ".*", split in any::<proptest::sample::Index>()) {
            let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
            // JSON parse must succeed for the round-trip assertion.
            if serde_json::from_str::<Value>(&body).is_err() {
                return Ok(());
            }
            let bytes = frame.as_bytes();
            let cut = split.index(bytes.len());
            if cut < bytes.len() {
                prop_assert!(matches!(take_frame(&bytes[..cut]), FrameRead::Incomplete));
            }
            match take_frame(bytes) {
                FrameRead::Message(message, rest) => {
                    let expected = serde_json::from_str::<Value>(&body).unwrap();
                    prop_assert_eq!(message, expected);
                    prop_assert!(rest.is_empty());
                }
                _ => prop_assert!(false, "expected a complete frame"),
            }
        }
    }
}
