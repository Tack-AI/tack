use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::client::{AnalysisState, Diag, LspClient, verifier_hint};
use super::convert::{
    CallSite, Location, SymbolInfo, WorkspaceSymbol, diagnostics_containing, exact_name_matches,
    flatten_document_symbols, format_hover, normalize_uri, parse_call_sites, parse_locations,
    parse_workspace_symbols, path_to_uri, to_lsp_position, uri_to_path,
};
use super::tool::apply_workspace_edit;

/// Hard cap on rendered hover text (rust-analyzer hovers can be huge).
const MAX_HOVER_CHARS: usize = 2000;

/// Files larger than this are never read whole for LSP sync (same budget as
/// grep's GREP_MAX_FILE_BYTES): didOpen would hand the full text to the
/// server, and position math needs the text resident — a 500MB minified
/// bundle must fail fast instead of being slurped into memory.
const MAX_LSP_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Whole-file read with a size guard. Every LSP path that slurps a file
/// goes through this so oversized (usually generated) files produce a
/// clear error instead of an unbounded allocation.
pub(crate) fn read_lsp_file(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
        .len();
    if size > MAX_LSP_FILE_BYTES {
        return Err(format!(
            "{} is too large for the lsp tool ({} > {} limit); use read/grep on it instead",
            path.display(),
            size,
            MAX_LSP_FILE_BYTES
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// `read_lsp_file` for the Option-style internals (unreadable/oversized =
/// server path unavailable for this file).
fn read_lsp_file_opt(path: &Path) -> Option<String> {
    match read_lsp_file(path) {
        Ok(text) => Some(text),
        Err(e) => {
            tracing::debug!("lsp: {e}");
            None
        }
    }
}

/// How to launch a language server.
#[derive(Clone, Debug)]
pub struct ServerSpec {
    pub command: String,
    pub args: Vec<String>,
}

/// Default extension → server table. Only used when the binary spawns.
fn default_servers() -> HashMap<String, ServerSpec> {
    let mut map = HashMap::new();
    let mut add = |exts: &[&str], command: &str, args: &[&str]| {
        for ext in exts {
            map.insert(
                ext.to_string(),
                ServerSpec {
                    command: command.to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                },
            );
        }
    };
    add(&["rs"], "rust-analyzer", &[]);
    add(
        &["ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx"],
        "typescript-language-server",
        &["--stdio"],
    );
    add(&["py", "pyi"], "pyright-langserver", &["--stdio"]);
    add(&["go"], "gopls", &[]);
    add(&["c", "h", "cpp", "cc", "cxx", "hpp", "hh"], "clangd", &[]);
    map
}

/// LSP languageId for didOpen (defaults to the extension).
pub(crate) fn language_id(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        other => other,
    }
}

/// One live language-server process.
/// Cap on automatic restarts per extension after a mid-session crash
/// (spawn failures are a separate `failed` set). Beyond it the extension
/// is marked failed — a crash loop should not spam processes forever.
const MAX_LSP_RESTARTS: u32 = 3;

// ---------------------------------------------------------------------
// Manager (shared via ToolServices)
// ---------------------------------------------------------------------

struct ManagerInner {
    clients: HashMap<String, Arc<LspClient>>,
    /// Extensions whose server failed to spawn — don't retry every edit.
    failed: HashSet<String>,
    /// Mid-session crash restarts per extension (capped at
    /// MAX_LSP_RESTARTS, then the extension joins `failed`).
    restarts: HashMap<String, u32>,
    servers: HashMap<String, ServerSpec>,
    /// Set when the host disables LSP entirely.
    disabled: bool,
    /// Append diagnostics to edit/write results (settings lspEditFeedback).
    edit_feedback: bool,
}

/// Marker → probe-extension table for [`LspManager::warmup`]. The probe
/// file need not exist — `client_for` only reads the extension. Markers
/// are chosen to be strong language signals (a bare Makefile is NOT one:
/// Rust/Go projects have those too, and warming clangd there is waste).
const WARMUP_MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "rs"),
    ("package.json", "ts"),
    ("go.mod", "go"),
    ("pyproject.toml", "py"),
    ("compile_commands.json", "cpp"),
    ("CMakeLists.txt", "cpp"),
];

/// Probe files to warm, one per server family, for the given roots.
/// Pure (no spawning) so the marker logic is testable without starting
/// real servers.
fn warmup_probes(roots: &[PathBuf], servers: &HashMap<String, ServerSpec>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut probes = Vec::new();
    for root in roots {
        for (marker, ext) in WARMUP_MARKERS {
            // One probe per extension across all roots: the spawned
            // client receives every root as a workspaceFolder. Mark seen
            // ONLY on an actual probe — a family with several markers
            // (clangd) must not be consumed by a marker that isn't there.
            if servers.contains_key(*ext) && root.join(marker).exists() && seen.insert(*ext) {
                probes.push(root.join(format!("__warmup__.{ext}")));
            }
        }
    }
    probes
}

/// Crash bookkeeping for `client_for`: drop a dead cached client and
/// count the restart. Returns false when the extension must not be
/// (re)spawned — already failed, or the restart budget is exhausted.
fn note_crash(inner: &mut ManagerInner, ext: &str) -> bool {
    if inner.clients.remove(ext).is_some() {
        let restarts = inner.restarts.entry(ext.to_string()).or_insert(0);
        *restarts += 1;
        if *restarts > MAX_LSP_RESTARTS {
            tracing::warn!(
                "LSP server for .{ext} keeps crashing ({MAX_LSP_RESTARTS} restarts); giving up"
            );
            inner.failed.insert(ext.to_string());
            return false;
        }
        tracing::warn!("LSP server for .{ext} exited; restarting ({restarts}/{MAX_LSP_RESTARTS})");
    }
    !inner.failed.contains(ext)
}

/// Language-server registry keyed by file extension. Clone is cheap (Arc).
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Mutex<ManagerInner>>,
    /// Workspace roots: primary cwd first, then additional dirs. All are
    /// sent as workspaceFolders at initialize so servers resolve cross-root
    /// dependencies (multi-root sessions).
    roots: Vec<PathBuf>,
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspManager").finish_non_exhaustive()
    }
}

impl LspManager {
    pub fn new(root: PathBuf) -> Self {
        LspManager::with_roots(vec![root])
    }

    /// Multi-root: primary root first, extra working directories after.
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        let mut seen = HashSet::new();
        let mut roots: Vec<PathBuf> = roots
            .into_iter()
            .filter(|r| seen.insert(r.clone()))
            .collect();
        if roots.is_empty() {
            roots.push(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        }
        LspManager {
            inner: Arc::new(Mutex::new(ManagerInner {
                clients: HashMap::new(),
                failed: HashSet::new(),
                restarts: HashMap::new(),
                servers: default_servers(),
                disabled: false,
                edit_feedback: true,
            })),
            roots,
        }
    }

    /// Override/extend the extension → server table (settings `lspServers`).
    pub fn configure(&self, servers: HashMap<String, ServerSpec>, disabled: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (ext, spec) in servers {
            let ext = ext.trim_start_matches('.').to_lowercase();
            inner.servers.insert(ext, spec);
        }
        inner.disabled = disabled;
    }

    /// Toggle the edit/write diagnostics feedback loop.
    pub fn set_edit_feedback(&self, enabled: bool) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .edit_feedback = enabled;
    }

    /// Pre-start the workspace's primary language server(s) in the
    /// background so the first lsp call isn't charged the spawn,
    /// initialize and index cost (seconds for rust-analyzer).
    /// Fire-and-forget: a spawn failure lands in `failed`, exactly as if
    /// the first tool call discovered it. No-op when disabled, outside a
    /// tokio runtime, or when no marker matches. Marker mapping:
    ///
    /// - Cargo.toml → rust-analyzer
    /// - package.json → typescript-language-server
    /// - go.mod → gopls
    /// - pyproject.toml → pyright
    /// - compile_commands.json / CMakeLists.txt → clangd
    ///
    /// All roots are scanned (multi-root sessions); each server family
    /// warms once since the client gets every root as a workspaceFolder.
    pub fn warmup(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let probes = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.disabled {
                return;
            }
            warmup_probes(&self.roots, &inner.servers)
        };
        for probe in probes {
            let this = self.clone();
            tokio::spawn(async move {
                this.client_for(&probe).await;
            });
        }
    }

    pub(crate) fn spec_for(&self, path: &Path) -> Option<ServerSpec> {
        let ext = path.extension()?.to_string_lossy().to_lowercase();
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.disabled {
            return None;
        }
        inner.servers.get(&ext).cloned()
    }

    /// Get (or lazily spawn) the server for this file. None when no server
    /// is configured for the extension or spawning previously failed. A
    /// cached client whose process died is transparently respawned, up to
    /// MAX_LSP_RESTARTS per extension (crash loops join `failed`).
    pub async fn client_for(&self, path: &Path) -> Option<Arc<LspClient>> {
        let ext = path.extension()?.to_string_lossy().to_lowercase();
        let spec = self.spec_for(path)?;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(client) = inner.clients.get(&ext)
                && client.is_alive()
            {
                return Some(client.clone());
            }
            if !note_crash(&mut inner, &ext) {
                return None;
            }
        }
        match LspClient::spawn(&spec, &self.roots).await {
            Ok(client) => {
                self.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clients
                    .insert(ext, client.clone());
                Some(client)
            }
            Err(e) => {
                tracing::debug!("LSP server for .{ext} unavailable: {e}");
                self.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failed
                    .insert(ext);
                None
            }
        }
    }

    /// Sync `path` and wait for the diagnostics burst to settle. Returns
    /// the live client.
    async fn sync_settled(&self, path: &Path, wait: std::time::Duration) -> Option<Arc<LspClient>> {
        let client = self.client_for(path).await?;
        let text = read_lsp_file_opt(path)?;
        let (uri, before) = client.sync_document(path, &text);
        client.wait_fresh_uri(&uri, before, wait).await;
        // Servers may publish in bursts (rust-analyzer first clears, then
        // reports after analysis). Keep collecting while updates arrive,
        // with a short settle window between bursts.
        let mut last = client.uri_version(&uri);
        for _ in 0..3 {
            if !client
                .wait_fresh_uri(&uri, last, std::time::Duration::from_millis(600))
                .await
            {
                break;
            }
            last = client.uri_version(&uri);
        }
        Some(client)
    }

    /// Sync `path` and return its current diagnostics after settling, plus
    /// the server's analysis state (rust-analyzer flycheck may still be
    /// running when the syntax-level diagnostics arrived).
    pub async fn diagnostics_with_state(
        &self,
        path: &Path,
        wait: std::time::Duration,
    ) -> Option<(Vec<Diag>, AnalysisState)> {
        let client = self.sync_settled(path, wait).await?;
        // The syntax pass reports fast; give background analysis (flycheck)
        // one more bounded window so type errors make it into the answer.
        if client.analysis_state().pending {
            client.wait_quiescent(wait).await;
        }
        let uri = path_to_uri(path);
        Some((client.diagnostics_for(&uri), client.analysis_state()))
    }

    /// Sync `path` and return its current diagnostics after settling.
    pub async fn diagnostics(&self, path: &Path, wait: std::time::Duration) -> Option<Vec<Diag>> {
        let client = self.sync_settled(path, wait).await?;
        let uri = path_to_uri(path);
        Some(client.diagnostics_for(&uri))
    }

    /// All diagnostics across open documents of every live server.
    pub fn workspace_diagnostics(&self) -> Vec<(PathBuf, Vec<Diag>)> {
        let clients: Vec<Arc<LspClient>> = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clients
            .values()
            .cloned()
            .collect();
        let mut out = Vec::new();
        for client in clients {
            for (uri, diags) in client.all_diagnostics() {
                if diags.is_empty() {
                    continue;
                }
                if let Some(path) = uri_to_path(&uri) {
                    out.push((path, diags));
                }
            }
        }
        out
    }
}

/// Format diagnostics compactly for the model: `line:col severity message`.
pub fn format_diagnostics(path: &Path, diags: &[Diag], max: usize) -> String {
    let mut lines = Vec::new();
    for d in diags.iter().take(max) {
        let tag = match (&d.source, &d.code) {
            (Some(s), Some(c)) => format!(" [{s} {c}]"),
            (Some(s), None) => format!(" [{s}]"),
            (None, Some(c)) => format!(" [{c}]"),
            (None, None) => String::new(),
        };
        lines.push(format!(
            "  {}:{} {}: {}{}",
            d.line + 1,
            d.character + 1,
            d.severity_label(),
            d.message.lines().next().unwrap_or_default(),
            tag
        ));
    }
    if diags.len() > max {
        lines.push(format!("  … and {} more", diags.len() - max));
    }
    format!("Diagnostics for {}:\n{}", path.display(), lines.join("\n"))
}

/// Post-edit feedback: diagnostics summary for `path`, or None when LSP is
/// unavailable for this file (fast path — no server configured/known-dead).
/// The wording distinguishes a fully analyzed clean file from a clean
/// syntax pass whose type check (rust-analyzer flycheck) is still running,
/// and appends error counts from OTHER files so cross-file breakage from
/// the edit is visible without a build.
pub async fn post_edit_summary(manager: &LspManager, path: &Path) -> Option<String> {
    // Cheap pre-check without spawning: skip when disabled or the extension
    // has no server.
    {
        let inner = manager.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.edit_feedback || inner.disabled {
            return None;
        }
    }
    let spec = manager.spec_for(path)?;
    let client = manager
        .sync_settled(path, std::time::Duration::from_millis(2500))
        .await?;
    // Background analysis (flycheck) lags the syntax pass; give it a short
    // bounded grace so type errors usually make it into the feedback.
    if client.analysis_state().pending {
        client
            .wait_quiescent(std::time::Duration::from_millis(4000))
            .await;
    }
    let verifier = verifier_hint(&spec.command);
    let uri = path_to_uri(path);
    let diags = client.diagnostics_for(&uri);
    let state = client.analysis_state();
    let mut text = if diags.is_empty() {
        match state.clean_confidence_note(verifier) {
            Some(note) => format!("Diagnostics: none yet — {note}."),
            None => "Diagnostics: no problems found.".to_string(),
        }
    } else {
        let mut text = format_diagnostics(path, &diags, 10);
        if let Some(note) = state.more_may_follow_note() {
            text.push_str(&format!("\n({note})"));
        }
        text
    };
    if let Some(note) = cross_file_error_note(client.as_ref(), &uri) {
        text.push('\n');
        text.push_str(&note);
    }
    Some(text)
}

/// "N error(s) in M other file(s): a.rs (2), b.rs (1)" — cross-file errors
/// cached on the same server, excluding `exclude_uri`. None when clean.
fn cross_file_error_note(client: &LspClient, exclude_uri: &str) -> Option<String> {
    let mut files: Vec<(PathBuf, usize)> = Vec::new();
    for (uri, diags) in client.all_diagnostics() {
        if uri == normalize_uri(exclude_uri) {
            continue;
        }
        let errors = diags.iter().filter(|d| d.severity == 1).count();
        if errors > 0
            && let Some(path) = uri_to_path(&uri)
        {
            files.push((path, errors));
        }
    }
    if files.is_empty() {
        return None;
    }
    files.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let total: usize = files.iter().map(|(_, n)| n).sum();
    let short = |path: &Path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        match path.parent().and_then(|p| p.file_name()) {
            Some(dir) => format!("{}/{name}", dir.to_string_lossy()),
            None => name.to_string(),
        }
    };
    let shown: Vec<String> = files
        .iter()
        .take(3)
        .map(|(path, n)| format!("{} ({n})", short(path)))
        .collect();
    let more = if files.len() > 3 { ", …" } else { "" };
    Some(format!(
        "{total} error(s) in {} other file(s): {}{}",
        files.len(),
        shown.join(", "),
        more
    ))
}

impl LspManager {
    /// Sync the document and return the live client + URI. Position requests
    /// are ordered after didOpen/didChange on the same channel, so no settle
    /// wait is needed — except right after server startup: rust-analyzer
    /// returns empty definition/references until its first analysis pass
    /// completes, which coincides with the first diagnostics burst.
    async fn synced(&self, path: &Path) -> Option<(Arc<LspClient>, String)> {
        let client = self.client_for(path).await?;
        let text = read_lsp_file_opt(path)?;
        let (uri, before) = client.sync_document(path, &text);
        if !client.warmed.load(Ordering::Relaxed) {
            client
                .wait_fresh(before, std::time::Duration::from_secs(15))
                .await;
            client.warmed.store(true, Ordering::Relaxed);
        }
        Some((client, uri))
    }

    fn position_params(text: &str, uri: &str, line1: u32, col1: u32) -> Value {
        let (line, character) = to_lsp_position(text, line1, col1);
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        })
    }

    /// textDocument/definition.
    pub async fn definition(&self, path: &Path, line1: u32, col1: u32) -> Option<Vec<Location>> {
        let (client, uri) = self.synced(path).await?;
        let text = read_lsp_file_opt(path)?;
        let params = Self::position_params(&text, &uri, line1, col1);
        let result = client
            .request("textDocument/definition", params)
            .await
            .ok()?;
        Some(parse_locations(&result))
    }

    /// textDocument/implementation (trait impls, interface impls, overrides).
    pub async fn implementation(
        &self,
        path: &Path,
        line1: u32,
        col1: u32,
    ) -> Option<Vec<Location>> {
        let (client, uri) = self.synced(path).await?;
        let text = read_lsp_file_opt(path)?;
        let params = Self::position_params(&text, &uri, line1, col1);
        let result = client
            .request("textDocument/implementation", params)
            .await
            .ok()?;
        Some(parse_locations(&result))
    }

    /// Call hierarchy: incoming = callers of the symbol at the position,
    /// outgoing = functions it calls. Empty when unsupported/no symbol.
    pub async fn call_hierarchy(
        &self,
        path: &Path,
        line1: u32,
        col1: u32,
        incoming: bool,
    ) -> Option<Vec<CallSite>> {
        let (client, uri) = self.synced(path).await?;
        let text = read_lsp_file_opt(path)?;
        let params = Self::position_params(&text, &uri, line1, col1);
        let items = client
            .request("textDocument/prepareCallHierarchy", params)
            .await
            .ok()?;
        let mut out = Vec::new();
        // A position can yield several items (e.g. trait + impl); bound the
        // fan-out so one query can't explode the context.
        for item in items.as_array().into_iter().flatten().take(3) {
            let method = if incoming {
                "callHierarchy/incomingCalls"
            } else {
                "callHierarchy/outgoingCalls"
            };
            let calls = client.request(method, json!({ "item": item })).await.ok()?;
            out.extend(parse_call_sites(&calls, incoming));
        }
        Some(out)
    }

    /// Resolve a bare symbol name to candidate positions: exact
    /// (case-sensitive) `workspace/symbol` matches first, then
    /// case-insensitive. Uses `path_hint`'s server when given, otherwise
    /// every live server (multi-language sessions).
    pub async fn resolve_symbol(
        &self,
        path_hint: Option<&Path>,
        name: &str,
    ) -> Option<Vec<WorkspaceSymbol>> {
        let clients: Vec<Arc<LspClient>> = match path_hint {
            Some(path) => vec![self.client_for(path).await?],
            None => self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clients
                .values()
                .filter(|c| c.is_alive())
                .cloned()
                .collect(),
        };
        if clients.is_empty() {
            return None;
        }
        let mut all = Vec::new();
        for client in clients {
            if let Ok(result) = client
                .request("workspace/symbol", json!({ "query": name }))
                .await
            {
                all.extend(parse_workspace_symbols(&result));
            }
        }
        Some(exact_name_matches(all, name))
    }

    /// textDocument/references (includes the declaration).
    pub async fn references(&self, path: &Path, line1: u32, col1: u32) -> Option<Vec<Location>> {
        let (client, uri) = self.synced(path).await?;
        let text = read_lsp_file_opt(path)?;
        let mut params = Self::position_params(&text, &uri, line1, col1);
        params["context"] = json!({ "includeDeclaration": true });
        let result = client
            .request("textDocument/references", params)
            .await
            .ok()?;
        Some(parse_locations(&result))
    }

    /// textDocument/documentSymbol, flattened with depth.
    pub async fn document_symbols(&self, path: &Path) -> Option<Vec<SymbolInfo>> {
        let (client, uri) = self.synced(path).await?;
        let result = client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await
            .ok()?;
        let mut out = Vec::new();
        if let Some(symbols) = result.as_array() {
            flatten_document_symbols(symbols, 0, &mut out);
        }
        Some(out)
    }

    /// textDocument/rename: compute the WorkspaceEdit and apply it to disk.
    /// Returns per-file edit counts. Checkpoints each touched file first.
    pub async fn rename(
        &self,
        path: &Path,
        line1: u32,
        col1: u32,
        new_name: &str,
        checkpoints: &crate::checkpoint::CheckpointManager,
    ) -> Option<Result<Vec<(PathBuf, usize)>, String>> {
        // Servers (rust-analyzer) reject renames with ContentModified while a
        // re-analysis is in flight after a sync — retry a few times.
        let mut last_err = String::new();
        for attempt in 0..4 {
            let (client, uri) = self.synced(path).await?;
            let text = read_lsp_file_opt(path)?;
            let mut params = Self::position_params(&text, &uri, line1, col1);
            params["newName"] = Value::String(new_name.to_string());
            match client.request("textDocument/rename", params).await {
                Ok(result) => return Some(apply_workspace_edit(&result, checkpoints, &self.roots)),
                Err(e) if e.contains("content modified") || e.contains("-32801") => {
                    last_err = e;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * (attempt + 1) as u64,
                    ))
                    .await;
                }
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Err(last_err))
    }

    /// textDocument/hover. Outer None = server unavailable or request
    /// failed; inner None = no hover information at this position.
    pub async fn hover(&self, path: &Path, line1: u32, col1: u32) -> Option<Option<String>> {
        let (client, uri) = self.synced(path).await?;
        let text = read_lsp_file_opt(path)?;
        let params = Self::position_params(&text, &uri, line1, col1);
        let result = client.request("textDocument/hover", params).await.ok()?;
        Some(format_hover(&result, MAX_HOVER_CHARS))
    }

    /// workspace/symbol — project-wide symbol search in the server for
    /// `path`'s language. Empty query returns a bounded all-symbols list.
    pub async fn workspace_symbols(
        &self,
        path: &Path,
        query: &str,
    ) -> Option<Vec<WorkspaceSymbol>> {
        let (client, _) = self.synced(path).await?;
        let result = client
            .request("workspace/symbol", json!({ "query": query }))
            .await
            .ok()?;
        Some(parse_workspace_symbols(&result))
    }

    /// textDocument/codeAction. Raw CodeAction/Command values in server
    /// order. When `focus` is given, context diagnostics are scoped to
    /// those containing the position (falling back to all current
    /// diagnostics when none contain it); otherwise all current
    /// diagnostics for the file form the context.
    pub async fn code_actions(
        &self,
        path: &Path,
        focus: Option<(u32, u32)>,
        wait: std::time::Duration,
    ) -> Option<Vec<Value>> {
        let client = self.sync_settled(path, wait).await?;
        let uri = path_to_uri(path);
        let text = read_lsp_file_opt(path)?;
        let raw = client.diagnostics_raw_for(&uri);
        let context_diags = match focus {
            Some((line1, col1)) => {
                let (line, character) = to_lsp_position(&text, line1, col1);
                let containing = diagnostics_containing(&raw, line, character);
                if containing.is_empty() {
                    raw
                } else {
                    containing
                }
            }
            None => raw,
        };
        // Request position: the focus point, else the first context
        // diagnostic's start, else the file origin.
        let (line, character) = match focus {
            Some((line1, col1)) => to_lsp_position(&text, line1, col1),
            None => context_diags
                .first()
                .map(|d| {
                    (
                        d["range"]["start"]["line"].as_u64().unwrap_or(0) as u32,
                        d["range"]["start"]["character"].as_u64().unwrap_or(0) as u32,
                    )
                })
                .unwrap_or((0, 0)),
        };
        let result = client
            .request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": line, "character": character },
                        "end": { "line": line, "character": character }
                    },
                    "context": {
                        "diagnostics": context_diags,
                        "triggerKind": 1
                    }
                }),
            )
            .await
            .ok()?;
        Some(result.as_array().cloned().unwrap_or_default())
    }

    /// Apply the code action at `index` from a previous `code_actions`
    /// listing. Actions are re-requested (deterministic for the same file
    /// state), so no state is kept between calls. Only literal
    /// WorkspaceEdits are applied — server commands are reported as
    /// unsupported. Checkpoints each touched file first.
    pub async fn apply_code_action(
        &self,
        path: &Path,
        line1: u32,
        col1: u32,
        index: usize,
        checkpoints: &crate::checkpoint::CheckpointManager,
    ) -> Option<Result<String, String>> {
        let actions = self
            .code_actions(path, Some((line1, col1)), std::time::Duration::from_secs(5))
            .await?;
        let Some(action) = actions.get(index) else {
            return Some(Err(format!(
                "code action index {index} out of range (server offered {})",
                actions.len()
            )));
        };
        let title = action["title"].as_str().unwrap_or("untitled").to_string();
        let Some(edit) = action.get("edit") else {
            let hint = if action.get("command").is_some() {
                "it is a server command, which tack cannot execute; only actions carrying a WorkspaceEdit are supported"
            } else {
                "it carries no edits"
            };
            return Some(Err(format!(
                "code action {index:?} ({title}) cannot be applied: {hint}"
            )));
        };
        match apply_workspace_edit(edit, checkpoints, &self.roots) {
            Ok(summary) => {
                let total: usize = summary.iter().map(|(_, n)| n).sum();
                Some(Ok(format!(
                    "Applied code action {index:?} ({title}): {total} edit(s) across {} file(s)",
                    summary.len()
                )))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    // Only the unix-gated crash-budget test uses these (its fake client
    // spawns `sleep`); importing unconditionally breaks -D warnings on
    // Windows.
    #[cfg(unix)]
    use crate::lsp::client::ReaderShared;
    #[cfg(unix)]
    use std::sync::atomic::AtomicU64;

    /// Warmup is a no-op for a workspace without marker files — nothing
    /// spawns, nothing fails.
    #[tokio::test]
    async fn warmup_without_markers_is_a_noop() {
        let dir = std::env::temp_dir().join(format!("tack-lsp-warmup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manager = LspManager::new(dir.clone());
        manager.warmup();
        // Give any (erroneously) spawned task a moment to land.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let inner = manager.inner.lock().unwrap();
        assert!(inner.clients.is_empty());
        assert!(inner.failed.is_empty());
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A disabled manager must not warm up (settings features.lsp=false).
    #[tokio::test]
    async fn warmup_respects_disabled() {
        let dir =
            std::env::temp_dir().join(format!("tack-lsp-warmup-off-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let manager = LspManager::new(dir.clone());
        manager.configure(HashMap::new(), true);
        manager.warmup();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let inner = manager.inner.lock().unwrap();
        assert!(inner.clients.is_empty());
        assert!(inner.failed.is_empty());
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn marker_dir(name: &str, files: &[&str]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tack-lsp-probes-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for file in files {
            std::fs::write(dir.join(file), "x").unwrap();
        }
        dir
    }

    /// The marker table drives warmup per LANGUAGE — a TypeScript project
    /// must warm tsserver, never rust-analyzer (and vice versa).
    #[test]
    fn warmup_probes_follow_project_markers() {
        let servers = default_servers();
        let rust_dir = marker_dir("rust", &["Cargo.toml"]);
        let probes = warmup_probes(std::slice::from_ref(&rust_dir), &servers);
        assert_eq!(probes, vec![rust_dir.join("__warmup__.rs")]);

        let ts_dir = marker_dir("ts", &["package.json"]);
        let probes = warmup_probes(std::slice::from_ref(&ts_dir), &servers);
        assert_eq!(probes, vec![ts_dir.join("__warmup__.ts")]);

        // C/C++ markers warm clangd; a bare Makefile must NOT (too
        // generic — Rust/Go projects have them too).
        let cpp_dir = marker_dir("cpp", &["CMakeLists.txt", "Makefile"]);
        let probes = warmup_probes(std::slice::from_ref(&cpp_dir), &servers);
        assert_eq!(probes, vec![cpp_dir.join("__warmup__.cpp")]);
        let mk_dir = marker_dir("mk", &["Makefile"]);
        assert!(warmup_probes(std::slice::from_ref(&mk_dir), &servers).is_empty());

        // Mixed monorepo: every matching family warms, nothing else.
        let mixed = marker_dir("mixed", &["Cargo.toml", "package.json", "go.mod"]);
        let probes = warmup_probes(std::slice::from_ref(&mixed), &servers);
        assert_eq!(probes.len(), 3, "{probes:?}");
        assert!(probes.contains(&mixed.join("__warmup__.rs")));
        assert!(probes.contains(&mixed.join("__warmup__.ts")));
        assert!(probes.contains(&mixed.join("__warmup__.go")));

        for dir in [rust_dir, ts_dir, cpp_dir, mk_dir, mixed] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Custom server tables gate warmup; multi-root scans every root but
    /// warms each family once (the client gets all roots as folders).
    #[test]
    fn warmup_probes_respect_custom_servers_and_multi_root() {
        // A user table with only rust: markers for other languages are
        // ignored.
        let dir = marker_dir("custom", &["Cargo.toml", "package.json"]);
        let mut servers = HashMap::new();
        servers.insert(
            "rs".to_string(),
            ServerSpec {
                command: "rust-analyzer".to_string(),
                args: Vec::new(),
            },
        );
        let probes = warmup_probes(std::slice::from_ref(&dir), &servers);
        assert_eq!(probes, vec![dir.join("__warmup__.rs")]);

        // Multi-root: the marker lives in the SECOND root; ts still
        // warms, and only once even if both roots have package.json.
        let servers = default_servers();
        let root_a = marker_dir("roota", &["package.json"]);
        let root_b = marker_dir("rootb", &["package.json"]);
        let probes = warmup_probes(&[root_a.clone(), root_b], &servers);
        assert_eq!(probes, vec![root_a.join("__warmup__.ts")]);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root_a);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn crash_restart_budget_allows_then_gives_up() {
        let mut inner = ManagerInner {
            clients: HashMap::new(),
            failed: HashSet::new(),
            restarts: HashMap::new(),
            servers: HashMap::new(),
            disabled: false,
            edit_feedback: true,
        };
        // No cached client: fresh spawn is allowed (no restart counted).
        assert!(note_crash(&mut inner, "rs"));
        assert!(inner.restarts.is_empty());

        // Simulate a dead cached client: restart budget allows
        // MAX_LSP_RESTARTS respawns, then the extension is failed.
        fn fake_client() -> Arc<LspClient> {
            let (writer, _sink) = tokio::sync::mpsc::unbounded_channel();
            let child = tokio::process::Command::new("sleep")
                .arg("30")
                .kill_on_drop(true)
                .spawn()
                .expect("sleep spawns on unix");
            Arc::new(LspClient {
                shared: Arc::new(ReaderShared::new(writer)),
                next_id: AtomicU64::new(1),
                open_docs: Mutex::new(HashMap::new()),
                sync_seq: AtomicU64::new(1),
                warmed: std::sync::atomic::AtomicBool::new(false),
                alive: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                child: Mutex::new(child),
            })
        }
        inner.clients.insert("rs".to_string(), fake_client());
        for expected in 1..=MAX_LSP_RESTARTS {
            assert!(note_crash(&mut inner, "rs"), "restart {expected} allowed");
            assert_eq!(inner.restarts["rs"], expected);
            inner.clients.insert("rs".to_string(), fake_client());
        }
        assert!(!note_crash(&mut inner, "rs"), "budget exhausted");
        assert!(inner.failed.contains("rs"));
        assert!(!note_crash(&mut inner, "rs"), "failed stays failed");
    }

    #[tokio::test]
    async fn no_server_for_unknown_extension() {
        let manager = LspManager::new(std::env::current_dir().unwrap());
        assert!(manager.client_for(Path::new("foo.xyz123")).await.is_none());
    }

    /// Oversized files are rejected with a clear error BEFORE being read
    /// into memory (the whole-file LSP sync paths all share this guard).
    #[test]
    fn oversized_file_is_rejected_before_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("big.rs");
        {
            use std::io::{Seek, Write};
            let mut f = std::fs::File::create(&big).unwrap();
            f.seek(std::io::SeekFrom::Start(MAX_LSP_FILE_BYTES + 1))
                .unwrap();
            f.write_all(b"x").unwrap();
        }
        let err = read_lsp_file(&big).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        // Exactly at the cap is still readable.
        let ok = tmp.path().join("ok.rs");
        std::fs::write(&ok, "fn main() {}\n").unwrap();
        assert!(read_lsp_file(&ok).is_ok());
    }

    #[tokio::test]
    async fn missing_binary_marks_extension_failed() {
        let manager = LspManager::new(std::env::current_dir().unwrap());
        manager.configure(
            HashMap::from([(
                "zzz".to_string(),
                ServerSpec {
                    command: "definitely-not-a-real-lsp-server-xyz".into(),
                    args: vec![],
                },
            )]),
            false,
        );
        assert!(manager.client_for(Path::new("foo.zzz")).await.is_none());
        // Second call hits the failed cache (no spawn attempt).
        assert!(manager.client_for(Path::new("foo.zzz")).await.is_none());
    }

    /// Live smoke test against a real rust-analyzer. Run with:
    /// `TACK_LSP_SMOKE_ROOT=<cargo project with a type error> cargo test -p tack-tools --lib lsp -- --ignored`
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH and TACK_LSP_SMOKE_ROOT"]
    async fn rust_analyzer_reports_type_error() {
        let root = PathBuf::from(std::env::var("TACK_LSP_SMOKE_ROOT").unwrap());
        let manager = LspManager::new(root.clone());
        let file = root.join("src").join("main.rs");
        let mut diags = manager
            .diagnostics(&file, std::time::Duration::from_secs(30))
            .await
            .expect("rust-analyzer should spawn");
        if diags.is_empty() {
            // Indexing may lag the first publish; sync once more and wait.
            diags = manager
                .diagnostics(&file, std::time::Duration::from_secs(30))
                .await
                .unwrap();
        }
        assert!(
            diags.iter().any(|d| d.severity == 1),
            "expected an error diagnostic, got: {diags:?}"
        );
    }

    /// Live navigation smoke test against a real rust-analyzer.
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH and TACK_LSP_SMOKE_ROOT"]
    async fn rust_analyzer_definition_and_symbols() {
        let root = PathBuf::from(std::env::var("TACK_LSP_SMOKE_ROOT").unwrap());
        let manager = LspManager::new(root.clone());
        let file = root.join("src").join("main.rs");
        // The smoke project's main.rs: line 2 has `let x: i32 = ...`.
        // Symbols should list `main`.
        let symbols = manager.document_symbols(&file).await.expect("server");
        assert!(symbols.iter().any(|s| s.name == "main"), "{symbols:?}");
        // Definition of the local `x` captured in the format string on
        // line 3 (`    println!("{x}");`) → the let binding on line 2.
        let defs = manager.definition(&file, 3, 16).await.expect("server");
        assert!(
            defs.iter().any(|d| d.line == 2),
            "no definition for x on line 2: {defs:?}"
        );
    }

    /// Live rename smoke test (mutates the smoke file, then restores it).
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH and TACK_LSP_SMOKE_ROOT"]
    async fn rust_analyzer_rename_applies_to_disk() {
        let root = PathBuf::from(std::env::var("TACK_LSP_SMOKE_ROOT").unwrap());
        let manager = LspManager::new(root.clone());
        let file = root.join("src").join("main.rs");
        let original = std::fs::read_to_string(&file).unwrap();
        let checkpoints = crate::checkpoint::CheckpointManager::new(); // disabled = no-op
        let result = manager
            .rename(&file, 2, 9, "renamed_x", &checkpoints)
            .await
            .expect("server")
            .expect("rename edits");
        assert!(!result.is_empty());
        let updated = std::fs::read_to_string(&file).unwrap();
        assert!(updated.contains("renamed_x"), "{updated}");
        assert!(!updated.contains("let x"), "{updated}");
        // Restore for other tests.
        std::fs::write(&file, original).unwrap();
    }

    /// Live smoke test for the hover / workspace_symbols / code_actions
    /// additions. Run with the same TACK_LSP_SMOKE_ROOT as above.
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH and TACK_LSP_SMOKE_ROOT"]
    async fn rust_analyzer_hover_workspace_symbols_code_actions() {
        let root = PathBuf::from(std::env::var("TACK_LSP_SMOKE_ROOT").unwrap());
        let manager = LspManager::new(root.clone());
        let file = root.join("src").join("main.rs");

        // hover on the `let x` binding (line 2).
        let hover = manager.hover(&file, 2, 5).await.expect("server");
        let hover = hover.expect("hover on a binding should resolve");
        assert!(hover.contains("i32"), "{hover}");

        // workspace/symbol finds `main`.
        let symbols = manager
            .workspace_symbols(&file, "main")
            .await
            .expect("server");
        assert!(symbols.iter().any(|s| s.name == "main"), "{symbols:?}");

        // code_actions: the smoke project has a type error, so a fix may or
        // may not be offered — the request itself must succeed and the
        // apply path must report out-of-range deterministically.
        let actions = manager
            .code_actions(&file, Some((3, 16)), std::time::Duration::from_secs(10))
            .await
            .expect("server");
        let applied = manager
            .apply_code_action(
                &file,
                3,
                16,
                actions.len() + 5,
                &crate::checkpoint::CheckpointManager::new(),
            )
            .await
            .expect("server");
        assert!(applied.is_err(), "out-of-range apply must fail");
    }

    /// Live smoke test: multi-root initialize (primary + extra dir) must
    /// complete and navigation must work in the primary root.
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH and TACK_LSP_SMOKE_ROOT"]
    async fn rust_analyzer_multi_root_initialize() {
        let root = PathBuf::from(std::env::var("TACK_LSP_SMOKE_ROOT").unwrap());
        let manager = LspManager::with_roots(vec![root.clone(), PathBuf::from("/tmp")]);
        let file = root.join("src").join("main.rs");
        let symbols = manager.document_symbols(&file).await.expect("server");
        assert!(symbols.iter().any(|s| s.name == "main"), "{symbols:?}");
    }

    /// Live smoke test for analysis-status reporting, call hierarchy,
    /// implementation, and name-based symbol resolution. The smoke
    /// project's main.rs has `helper` called from `main` and a `Greet`
    /// trait implemented by `S` (see the TACK_LSP_SMOKE_ROOT setup).
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH and TACK_LSP_SMOKE_ROOT"]
    async fn rust_analyzer_status_call_hierarchy_implementation_resolve() {
        let root = PathBuf::from(std::env::var("TACK_LSP_SMOKE_ROOT").unwrap());
        let manager = LspManager::new(root.clone());
        let file = root.join("src").join("main.rs");

        // rust-analyzer reports serverStatus/progress: `known` must flip.
        let (_, state) = manager
            .diagnostics_with_state(&file, std::time::Duration::from_secs(60))
            .await
            .expect("server");
        assert!(
            state.known,
            "rust-analyzer should report analysis status: {state:?}"
        );

        // incoming_calls on `helper` (fn helper() {}, line 7) ← main.
        let calls = manager
            .call_hierarchy(&file, 7, 4, true)
            .await
            .expect("server");
        assert!(
            calls.iter().any(|c| c.name == "main"),
            "expected main among callers: {calls:?}"
        );

        // implementation of `Greet::greet` (line 10, `fn greet(&self);`).
        let impls = manager.implementation(&file, 10, 8).await.expect("server");
        assert!(!impls.is_empty(), "expected an impl for Greet::greet");

        // Name-based resolution: exactly one `helper`.
        let matches = manager
            .resolve_symbol(Some(&file), "helper")
            .await
            .expect("server");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert!(matches[0].path.ends_with("main.rs"));
    }
}
