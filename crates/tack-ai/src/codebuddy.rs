//! CodeBuddy native provider (`codebuddy-stream`): drives the `codebuddy`
//! CLI as a long-lived **stream-json** session and surfaces it as a tack-ai
//! provider — no Node SDK, no HTTP/OpenAI translation shim.
//!
//! ```text
//! tack agent loop ──stream()──► CodeBuddySession (CLI, stdin NDJSON)
//!                              ◄── assistant/result events (stdout NDJSON)
//!   tools ◄── SDK MCP (mcp_message control frames) ── CLI's MCP client
//! ```
//!
//! Control-flow model (same as TS pi's pi-codebuddy-sdk): the CLI owns the
//! model conversation; tack owns tool execution. tack's tools are exposed
//! to the CLI as an **SDK MCP server**: the `initialize` control request
//! declares `sdkMcpServers: ["tack"]` and the CLI then speaks MCP JSON-RPC
//! over `control_request` frames (`subtype: "mcp_message"`, initialize /
//! tools/list / tools/call), answered with `control_response` frames whose
//! `response.response.mcp_response` carries the JSON-RPC reply. SDK MCP
//! tools are native to the CLI (not deferred like HTTP MCP servers), so
//! the model sees their schemas even with `--tools ""` keeping the CLI's
//! builtin tools off. The model's tool_use ends the tack-ai stream
//! (stop_reason ToolUse) while the CLI's tools/call stays PARKED; tack's
//! agent loop executes the tool with its own permissions/UI; the next
//! `stream()` call answers the parked tools/call with the result and the
//! CLI turn continues. Session continuity is native (`session_id`), so
//! CLI-side caching survives.
//!
//! Protocol sources: CodeBuddy docs (`docs/cli/headless`, `docs/cli/sdk*`)
//! — `--input-format stream-json --output-format stream-json` long-running
//! mode, `control_request`/`control_response` (subtype `initialize`),
//! system/assistant/user/result messages — plus the @tencent-ai/agent-sdk
//! transport for the sdkMcpServers/mcp_message wire shapes.
//!
//! History divergence: tack-ai contexts are full-history; the CLI session is
//! append-only. The provider fingerprints synced messages; on mismatch
//! (compaction, tree navigation) it respawns the CLI and replays a
//! flattened transcript — correct but loses CLI-side cache (documented).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::Mutex;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::stream::event_stream;
use crate::types::{
    AssistantMessage, ContentBlock, Context, InputContentBlock, Message, Model, StopReason,
    ThinkingLevel, ToolResultMessage, Usage, UserContent,
};
use crate::{
    AssistantMessageEvent, AssistantMessageEventStream, ModelCost, Provider, StreamOptions,
};

/// Wire-api kind for CodeBuddy models (models.json `api` field).
pub const CODEBUDDY_API: &str = "codebuddy-stream";
/// Built-in provider id.
pub const PROVIDER_ID: &str = "codebuddy";
/// MCP server name as seen by the CLI (tools appear as `mcp__tack__<name>`).
/// Self-declared wire identity, not a protocol requirement: the CLI registers
/// SDK MCP servers under whatever name the client sends — verified generic in
/// codebuddy-code 2.156.0 (`registerSdkMcpServers`/`applySdkMcpServers` and
/// the `mcp__${name}__` tool matcher carry no `pi` special-casing).
const MCP_SERVER_NAME: &str = "tack";

/// Windows command-line limits make argv a dangerous channel for tack's
/// assembled system prompt: CreateProcessW caps the command line at 32767
/// chars and `cmd /c` (npm `.cmd` shims) at 8191 — the prompt regularly
/// exceeds both (observed 34498), and an over-limit command line crashed
/// the spawn path with an SEH on affected machines ("Rust cannot catch
/// foreign exceptions"). Past this threshold the prompt is delivered
/// through the first stream-json user message instead of
/// `--append-system-prompt`. Unix ARG_MAX (~2MB) needs no such guard.
#[cfg(windows)]
const MAX_ARGV_SYSTEM_PROMPT_CHARS: usize = 6000;
#[cfg(not(windows))]
const MAX_ARGV_SYSTEM_PROMPT_CHARS: usize = usize::MAX;

/// Split the system prompt into the argv part (fits the platform's
/// command-line budget) and the stdin-injection part (prepended to the
/// first user message). Full prompt is kept either way for the
/// staleness detector.
// On non-Windows the budget is usize::MAX and the guard arm is unreachable
// by design — the cfg'd constant documents that, so allow the lint.
#[cfg_attr(not(windows), allow(clippy::absurd_extreme_comparisons))]
fn split_system_prompt(prompt: Option<&str>) -> (Option<&str>, Option<String>) {
    match prompt {
        Some(p) if p.len() > MAX_ARGV_SYSTEM_PROMPT_CHARS => (None, Some(p.to_string())),
        other => (other, None),
    }
}

// ===========================================================================
// CLI discovery + model catalog
// ===========================================================================

/// Whether the CodeBuddy CLI is installed (`CODEBUDDY_PATH` or PATH).
/// Auth lives entirely in the CLI (`codebuddy login`), so CLI presence is
/// tack's whole credential check for the provider.
pub fn cli_available() -> bool {
    cli_path().is_some()
}

/// Resolve the codebuddy binary: `CODEBUDDY_PATH` or PATH lookup.
pub fn cli_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CODEBUDDY_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    for name in ["codebuddy", "cbc"] {
        if let Some(path) = which(name) {
            return Some(path);
        }
    }
    None
}

/// PATH lookup via `which`.
#[cfg(unix)]
fn which(name: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// PATH lookup via `where` (Windows has no `which`). `where` matches every
/// PATHEXT variant npm installs (`codebuddy`, `codebuddy.cmd`,
/// `codebuddy.ps1`, …); only some are spawnable without a shell.
#[cfg(windows)]
fn which(name: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("where")
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    pick_windows_shim(stdout.lines().map(str::trim).filter(|l| !l.is_empty())).map(PathBuf::from)
}

/// Choose the spawnable shim from `where` output: prefer `.exe` (direct
/// CreateProcess) then `.cmd`/`.bat` (spawned via `cmd /c` in `spawn_cli`);
/// extension-less git-bash scripts and `.ps1` need a shell we don't have.
#[cfg(any(windows, test))]
fn pick_windows_shim<'a>(lines: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let candidates: Vec<&str> = lines.collect();
    for ext in ["exe", "cmd", "bat"] {
        if let Some(hit) = candidates.iter().find(|l| {
            std::path::Path::new(l)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        }) {
            return Some(hit);
        }
    }
    None
}

/// Cached model catalog (process-wide; discovery spawns the CLI once).
static MODELS: OnceLock<RwLock<Option<Vec<Model>>>> = OnceLock::new();

/// Discovered models, if a probe already ran.
pub fn models() -> Vec<Model> {
    MODELS
        .get_or_init(|| RwLock::new(None))
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

/// Last-known-good model catalog on disk (reference: models-cache.ts):
/// installed synchronously at startup so /model lists codebuddy models even
/// while discovery is in flight or failing, and served context limits
/// learned from result.modelUsage survive restarts.
fn models_cache_path() -> PathBuf {
    let agent = std::env::var_os("TACK_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".tack")
                .join("agent")
        });
    agent.join("codebuddy-models.json")
}

fn read_models_cache() -> Vec<Model> {
    let Ok(text) = std::fs::read_to_string(models_cache_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_models_cache(models: &[Model]) {
    let path = models_cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(models) {
        let _ = std::fs::write(path, text);
    }
}

/// Install a model list process-wide (static cache + provider registry).
fn install_models(models: Vec<Model>) {
    if let Ok(mut guard) = MODELS.get_or_init(|| RwLock::new(None)).write() {
        *guard = Some(models.clone());
    }
    if !models.is_empty() {
        crate::providers::install_local_models(PROVIDER_ID, models);
    }
}

/// Discover CodeBuddy models and install them into the built-in catalog
/// (same pattern as local providers). The disk cache is installed first so
/// a fresh process registers the last-known-good list synchronously;
/// discovery then refreshes it. Silent on any failure: no CLI or an old
/// CLI just means the cache (or nothing) stays.
pub async fn refresh() {
    let cached = read_models_cache();
    if !cached.is_empty() {
        install_models(cached);
    }
    if crate::local_providers::offline_mode() {
        return;
    }
    let discovered = tokio::time::timeout(std::time::Duration::from_secs(20), discover_models())
        .await
        .ok()
        .flatten();
    match discovered {
        Some(mut list) if !list.is_empty() => {
            // Discovery re-estimates metadata from ids; carry over served
            // limits learned from result.modelUsage so a refresh can't
            // revert them to estimates.
            let previous = models();
            for m in &mut list {
                if let Some(prev) = previous.iter().find(|p| p.id == m.id) {
                    if prev.context_window != m.context_window {
                        m.context_window = prev.context_window;
                    }
                    if prev.max_tokens != m.max_tokens {
                        m.max_tokens = prev.max_tokens;
                    }
                }
            }
            write_models_cache(&list);
            install_models(list);
        }
        _ => {
            // CLI present but discovery failed and no cache: expose a
            // pass-through "default" model so /model still offers CodeBuddy.
            if models().is_empty() && cli_path().is_some() {
                install_models(vec![codebuddy_model("default", "CodeBuddy (CLI default)")]);
            }
        }
    }
}

/// Patch registered metadata with the CLI-reported SERVED limits
/// (result.modelUsage; reference: logServedContextWindow, issue #18). The
/// id-based estimate can be wildly wrong — e.g. a hy3 model served at 1M
/// ctx while estimated 128K. Returns true when anything changed. Pure so
/// tests don't touch the process-global registry / disk cache.
fn apply_served_limits(models: &mut [Model], model_usage: &Value) -> bool {
    let Some(served) = model_usage.as_object() else {
        return false;
    };
    let mut changed = false;
    for (served_id, limits) in served {
        let context_window = limits
            .get("contextWindow")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        let max_tokens = limits
            .get("maxOutputTokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        if context_window.is_none() && max_tokens.is_none() {
            continue;
        }
        // The served id can differ from the registered id — exact match
        // first, else the longest registered id that is a substring of the
        // served id (or vice versa).
        let mut best: Option<usize> = None;
        for (i, m) in models.iter().enumerate() {
            if m.id == *served_id {
                best = Some(i);
                break;
            }
            if (served_id.contains(&m.id) || m.id.contains(served_id.as_str()))
                && best.is_none_or(|b| models[b].id.len() < m.id.len())
            {
                best = Some(i);
            }
        }
        let Some(i) = best else { continue };
        let model = &mut models[i];
        if let Some(cw) = context_window
            && cw != model.context_window
        {
            tracing::info!(
                "codebuddy: served contextWindow for {} is {cw} (registered {}) — adopting",
                model.id,
                model.context_window
            );
            model.context_window = cw;
            changed = true;
        }
        if let Some(mt) = max_tokens
            && mt != model.max_tokens
        {
            tracing::info!(
                "codebuddy: served maxOutputTokens for {} is {mt} (registered {}) — adopting",
                model.id,
                model.max_tokens
            );
            model.max_tokens = mt;
            changed = true;
        }
    }
    changed
}

/// Global glue over apply_served_limits: update the registry and persist.
fn learn_served_limits(model_usage: &Value) {
    let lock = MODELS.get_or_init(|| RwLock::new(None));
    let updated = {
        let Ok(mut guard) = lock.write() else { return };
        let Some(models) = guard.as_mut() else { return };
        if !apply_served_limits(models, model_usage) {
            return;
        }
        models.clone()
    };
    write_models_cache(&updated);
    crate::providers::install_local_models(PROVIDER_ID, updated);
}

/// Model metadata is estimated from the id (the CLI's model list carries no
/// capabilities), mirroring the reference plugin's detectors:
/// reasoning for /claude|gemini|gpt-5|hy3|deepseek|glm/i, images for
/// /claude|gemini|gpt/i, gemini→1M ctx, claude/gpt→200K, else 128K; max
/// output 16K for gpt, else 8K.
fn detect_reasoning(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    ["claude", "gemini", "gpt-5", "hy3", "deepseek", "glm"]
        .iter()
        .any(|needle| id.contains(needle))
}

fn detect_images(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    ["claude", "gemini", "gpt"].iter().any(|n| id.contains(n))
}

fn estimate_context_window(id: &str) -> u32 {
    let id = id.to_ascii_lowercase();
    if id.contains("gemini") {
        1_048_576
    } else if id.contains("claude") || id.contains("gpt") {
        200_000
    } else {
        131_072
    }
}

fn estimate_max_tokens(id: &str) -> u32 {
    if id.to_ascii_lowercase().contains("gpt") {
        16_384
    } else {
        8_192
    }
}

fn codebuddy_model(id: &str, name: &str) -> Model {
    Model {
        provider: PROVIDER_ID.to_string(),
        id: id.to_string(),
        name: name.to_string(),
        api: CODEBUDDY_API.to_string(),
        base_url: String::new(),
        reasoning: detect_reasoning(id),
        thinking_level_map: None,
        input: if detect_images(id) {
            vec![crate::InputKind::Text, crate::InputKind::Image]
        } else {
            vec![crate::InputKind::Text]
        },
        cost: ModelCost::default(),
        context_window: estimate_context_window(id),
        max_tokens: estimate_max_tokens(id),
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// Map pi's reasoning level to the CLI's `--effort` value (reference:
/// REASONING_TO_EFFORT). The model's own `thinking_level_map` wins when it
/// has an entry for the level (an explicit `null` there disables effort);
/// otherwise minimal/low→low, medium→medium, high→high, xhigh/max→xhigh.
fn effort_for(model: &Model, reasoning: Option<ThinkingLevel>) -> Option<String> {
    let level = reasoning?;
    if let Some(mapped) = model.thinking_level_value(level) {
        return mapped.clone();
    }
    effort_for_level(level)
}

/// Generic reasoning → `--effort` mapping (no model-specific overrides),
/// also used by the one-shot delegation path (AskCodebuddy parity).
pub fn effort_for_level(level: ThinkingLevel) -> Option<String> {
    let effort = match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh | ThinkingLevel::Max => "xhigh",
    };
    Some(effort.to_string())
}

/// Spawn the CLI, run the initialize handshake, return the models array.
async fn discover_models() -> Option<Vec<Model>> {
    let cli = cli_path()?;
    let mut child = spawn_cli(&cli, "default", None, None, None).ok()?;
    let mut session = CliIo::new(&mut child).ok()?;
    let response = session.initialize(false).await.ok()?;
    kill_tree(&child);
    let _ = child.kill().await;
    let models = response
        .get("response")
        .and_then(|r| r.get("models"))
        .and_then(Value::as_array)?;
    let out: Vec<Model> = models
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
            Some(codebuddy_model(id, name))
        })
        .collect();
    Some(out)
}

// ===========================================================================
// CLI process plumbing
// ===========================================================================

/// Spawn the CLI in long-running stream-json mode.
///
/// Flags/env mirror the TS reference (pi-codebuddy-sdk → @tencent-ai/agent-sdk
/// ProcessTransport::buildArgs):
/// - `--tools ""` disables ALL built-in CLI tools — tack tools arrive over
///   the MCP bridge, built-ins would bypass tack's permissions/UI.
/// - `--strict-mcp-config` ignores MCP servers from the user's own CodeBuddy
///   config; only the pi bridge may be present.
/// - `--permission-mode bypassPermissions`: tool gating is tack's job; the
///   CLI must never pause a parked MCP call on its own permission prompt.
/// - `--setting-sources none` isolates the session from user/project/local
///   settings (AGENTS.md etc. already ride tack's system prompt).
/// - `--include-partial-messages` turns on `stream_event` partials for
///   incremental text/thinking/tool-call streaming.
/// - env: no auto-updater (an update would restart the CLI mid-session and
///   kill the transport), no auto-memory (its Write tool_use needs a
///   permission hook we don't provide), no auto-compact (tack owns context
///   management), no background tasks.
fn spawn_cli(
    cli: &PathBuf,
    model: &str,
    system_prompt: Option<&str>,
    effort: Option<&str>,
    resume: Option<&str>,
) -> Result<tokio::process::Child, String> {
    // Script CLIs can't be spawned directly on Windows — CreateProcess
    // needs an executable image, there is no shebang mechanism. Go through
    // the interpreter / command processor instead (std escapes args for
    // cmd since Rust 1.77):
    //   .py        — test mock, or CODEBUDDY_PATH pointing at a script
    //   .cmd/.bat  — npm's Windows shims (`%APPDATA%\npm\codebuddy.cmd`)
    let ext = cli
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let mut command = match ext.as_deref() {
        Some("py") => {
            let mut c = tokio::process::Command::new("python3");
            c.arg(cli);
            c
        }
        Some("cmd" | "bat") => {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/c").arg(cli);
            c
        }
        _ => tokio::process::Command::new(cli),
    };
    command
        .arg("-p")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages")
        .arg("--model")
        .arg(model)
        .arg("--allowedTools")
        .arg(format!("mcp__{MCP_SERVER_NAME}"))
        .arg("--tools")
        .arg("")
        .arg("--strict-mcp-config")
        .arg("--permission-mode")
        .arg("bypassPermissions")
        .arg("--setting-sources")
        .arg("none")
        .env("CODEBUDDY_CODE_DISABLE_BACKGROUND_TASKS", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("CODEBUDDY_DISABLE_AUTO_MEMORY", "1")
        .env("DISABLE_AUTO_COMPACT", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // SDK parity: entrypoint tag, but never override an explicit user value.
    if std::env::var_os("CODEBUDDY_CODE_ENTRYPOINT").is_none() {
        command.env("CODEBUDDY_CODE_ENTRYPOINT", "sdk-rs");
    }
    if let Some(effort) = effort {
        command.arg("--effort").arg(effort);
    }
    if let Some(prompt) = system_prompt {
        // Reference passes --system-prompt (replace CodeBuddy's identity
        // with pi's), not --append-system-prompt.
        command.arg("--system-prompt").arg(prompt);
    }
    if let Some(session_id) = resume {
        // JSONL rebuild path: resume the session file we just rewrote.
        command.arg("--resume").arg(session_id);
    }
    command
        .spawn()
        .map_err(|e| format!("failed to spawn codebuddy CLI ({}): {e}", cli.display()))
}

/// Kill the CLI process. On Windows the CLI is usually a cmd-shimmed node
/// process (`cmd /c codebuddy.cmd …`); plain kill/`kill_on_drop` only reaps
/// cmd.exe and would orphan the node grandchild, so taskkill the whole tree
/// first. No-op elsewhere.
fn kill_tree(child: &tokio::process::Child) {
    #[cfg(windows)]
    if let Some(id) = child.id() {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &id.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    let _ = child;
}

/// stdin writer + stdout line receiver for one CLI process.
struct CliIo {
    stdin: tokio::process::ChildStdin,
    lines: mpsc::UnboundedReceiver<Value>,
    /// Conversation lines that arrived while waiting for a control_response
    /// (set_model) — replayed to the next reader before the channel.
    buffer: std::collections::VecDeque<Value>,
    next_request_id: u64,
}

impl CliIo {
    /// Take the process pipes and spawn the stdout/stderr reader tasks.
    fn new(child: &mut tokio::process::Child) -> Result<Self, String> {
        let stdin = child.stdin.take().ok_or("CLI stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("CLI stdout unavailable")?;
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(&line) {
                            Ok(value) => {
                                if tx.send(value).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::debug!("codebuddy: non-JSON stdout line: {e}");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("codebuddy: stdout read error: {e}");
                        break;
                    }
                }
            }
        });
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "tack_ai::codebuddy::cli", "{line}");
                }
            });
        }
        Ok(CliIo {
            stdin,
            lines: rx,
            buffer: std::collections::VecDeque::new(),
            next_request_id: 1,
        })
    }

    /// Next stdout line: buffered strays first, then the channel.
    async fn next_line(&mut self) -> Option<Value> {
        if let Some(message) = self.buffer.pop_front() {
            Some(message)
        } else {
            self.lines.recv().await
        }
    }

    /// Channel-only read for WAIT loops that buffer non-matching lines:
    /// reading via next_line() there would re-serve the very line the loop
    /// just buffered, ping-ponging it forever at full CPU (livelock).
    async fn recv_channel(&mut self) -> Option<Value> {
        self.lines.recv().await
    }

    /// Bound on a single stdin write: a wedged CLI that stops reading
    /// leaves the pipe full and would otherwise pend `write_all`
    /// forever — the 300s no-event watchdog only covers the read side,
    /// so a stuck write froze the whole run while the UI stayed alive.
    /// Mirrors tack-ext's WRITE_TIMEOUT.
    const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    async fn send(&mut self, value: &Value) -> Result<(), String> {
        let mut text = serde_json::to_string(value).map_err(|e| e.to_string())?;
        text.push('\n');
        let send = async {
            self.stdin
                .write_all(text.as_bytes())
                .await
                .map_err(|e| format!("codebuddy stdin write failed (CLI died?): {e}"))?;
            self.stdin
                .flush()
                .await
                .map_err(|e| format!("codebuddy stdin flush failed: {e}"))
        };
        tokio::time::timeout(Self::SEND_TIMEOUT, send)
            .await
            .map_err(|_| {
                format!(
                    "codebuddy stdin write timed out after {}s (CLI wedged?)",
                    Self::SEND_TIMEOUT.as_secs()
                )
            })?
    }

    /// Send a control_request and wait for its control_response (matched by
    /// request_id), buffering interleaved conversation lines for the next
    /// reader. Used between turns (set_model), when the CLI is idle.
    async fn control_request(&mut self, tag: &str, request: &Value) -> Result<Value, String> {
        let request_id = format!("{tag}-{}", self.next_request_id);
        self.next_request_id += 1;
        self.send(&json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        }))
        .await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            // Channel-only: buffered lines belong to the event loop, and
            // re-serving them here would livelock (see recv_channel).
            let message = match tokio::time::timeout_at(deadline, self.recv_channel()).await {
                Ok(Some(message)) => message,
                Ok(None) => return Err("codebuddy CLI exited during control request".to_string()),
                Err(_) => return Err(format!("codebuddy control request {tag} timed out")),
            };
            let is_response = message.get("type").and_then(Value::as_str)
                == Some("control_response")
                && message
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    == Some(request_id.as_str());
            if !is_response {
                // Not ours: stash for the event loop / next reader.
                self.buffer.push_back(message);
                continue;
            }
            let response = message.get("response").cloned().unwrap_or(Value::Null);
            if response.get("subtype").and_then(Value::as_str) == Some("error") {
                return Err(format!(
                    "codebuddy {tag} rejected: {}",
                    response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ));
            }
            return Ok(response);
        }
    }

    /// Answer an mcp_message control_request with a success
    /// control_response carrying the MCP JSON-RPC reply (wire shape from
    /// the agent-sdk transport: response.response.mcp_response).
    async fn mcp_control_response(
        &mut self,
        request_id: &str,
        mcp_response: Value,
    ) -> Result<(), String> {
        self.send(&json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "mcp_response": mcp_response },
            },
        }))
        .await
    }

    /// Fail an mcp_message control_request (unknown server, abort paths).
    async fn mcp_control_error(&mut self, request_id: &str, error: &str) -> Result<(), String> {
        self.send(&json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": request_id,
                "error": error,
            },
        }))
        .await
    }

    /// Initialize control handshake; returns the control_response payload.
    /// `sdk_mcp` declares tack's tools as an SDK MCP server
    /// (`sdkMcpServers: ["tack"]`) — the CLI then speaks MCP JSON-RPC over
    /// mcp_message control frames (native tools, immune to `--tools ""`).
    /// Delegation (ask_codebuddy) passes false: it keeps the CLI's own
    /// tools and must not attract mcp_message traffic it can't serve.
    async fn initialize(&mut self, sdk_mcp: bool) -> Result<Value, String> {
        // SDK parity: the field is omitted (not null) when empty.
        let mut request = json!({ "subtype": "initialize", "hasPrompt": true });
        if sdk_mcp {
            request["sdkMcpServers"] = json!([MCP_SERVER_NAME]);
        }
        self.send(&json!({
            "type": "control_request",
            "request_id": "init-0",
            "request": request,
        }))
        .await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            // Channel-only: buffered lines belong to the event loop, and
            // re-serving them here would livelock (see recv_channel).
            let message = tokio::time::timeout_at(deadline, self.recv_channel())
                .await
                .map_err(|_| "codebuddy initialize timed out".to_string())?
                .ok_or_else(|| "codebuddy CLI exited during initialize".to_string())?;
            if message.get("type").and_then(Value::as_str) == Some("control_response") {
                let response = message.get("response").cloned().unwrap_or(Value::Null);
                let is_error = response
                    .get("subtype")
                    .and_then(Value::as_str)
                    .map(|s| s == "error")
                    .unwrap_or(false);
                if is_error {
                    return Err(format!(
                        "codebuddy initialize rejected: {}",
                        response
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    ));
                }
                return Ok(response);
            }
            // system(init) or other lines: buffer for the event loop (the
            // init line carries the session_id needed by set_model).
            self.buffer.push_back(message);
        }
    }
}

// ===========================================================================
// Session: one CLI process per tack session, with sync state
// ===========================================================================

/// A tool call the model made (from the assistant message), awaiting its
/// mcp_message counterpart and ultimately a tack tool result.
struct ParkedCall {
    tool_use_id: String,
    name: String,
    arguments: Value,
    /// The CLI tools/call frame paired at the tool boundary
    /// (adopt_mcp_tool_args) — resolve_parked answers exactly this frame,
    /// immune to (name, arguments) mismatches after arg normalization and
    /// to out-of-order results for same-name parallel calls.
    mcp_request_id: Option<String>,
}

/// The CLI's `tools/call` (an mcp_message control_request), stashed until
/// the matching tack tool result answers it. The MCP JSON-RPC `id` rides
/// along — the CLI matches `mcp_response` to its pending call by it.
struct McpToolCall {
    request_id: String,
    mcp_id: Value,
    name: String,
    arguments: Value,
}

struct CodeBuddySession {
    /// Registry key (tack session id) — the cb-session-id mapping is
    /// persisted under it for cross-process resume.
    key: String,
    child: tokio::process::Child,
    io: CliIo,
    /// tack tool definitions served on MCP tools/list (refreshed per
    /// stream call; the CLI re-lists around every tool turn).
    tools: Vec<crate::ToolDefinition>,
    /// tack-ai messages already reflected in the CLI session, as
    /// normalized fingerprints (volatile fields like timestamps stripped).
    synced: Vec<Value>,
    /// System prompt the CLI was spawned with (divergence detector).
    system_prompt: Option<String>,
    /// System prompt deferred from argv (Windows command-line limits):
    /// prepended to the next outgoing user message as a text block.
    pending_system_prompt: Option<String>,
    /// Model id the CLI was spawned with.
    model_id: String,
    /// `--effort` the CLI was spawned with (thinking-level divergence
    /// detector — the flag is spawn-time only, so a change respawns).
    effort: Option<String>,
    /// Set after an abort: the next stream must respawn + replay instead of
    /// syncing on top of a half-finished turn (reference: needsRebuild).
    needs_respawn: bool,
    /// Set after an abort (reference: forceRotate): the killed CLI may
    /// still be flushing orphan records to the old session JSONL, so the
    /// rebuild takes a FRESH session id instead of rewriting in place.
    rotate_on_rebuild: bool,
    /// Last drive() — idle sessions are evicted from the registry.
    last_used: std::time::Instant,
    /// Parked bridge calls from the current tool boundary.
    parked: Vec<ParkedCall>,
    /// CLI tools/call frames received but not yet matched to a tack tool
    /// result (they arrive on the CLI stream after the tool boundary).
    pending_mcp_calls: Vec<McpToolCall>,
    cli_session_id: Option<String>,
    /// Tool-use ids of the last returned tool turn whose completed
    /// `assistant` echo the CLI still has in flight. The CLI yields every
    /// assistant message once as stream_event partials AND once more as
    /// completed message(s) after the tool call resolves (SDK: "always
    /// yields assistant messages after streaming"; the real CLI splits
    /// them per content block); on a tool turn drive() returns AT
    /// message_stop, so the echo is read by the NEXT turn's event loop
    /// (whose saw_stream_event guard is still false) and must be
    /// recognized as a duplicate, not re-parked as a fresh tool call.
    /// Non-empty = skip ALL assistant lines until this turn's first
    /// stream_event.
    tool_echo_pending: Vec<String>,
}

/// Rate-limit surface (reference: piUI.notify on rate_limit_event). The
/// provider layer has no UI channel of its own; events ride the
/// provider-event channel ([`crate::provider::emit_provider_event`]) that
/// tack-app surfaces (TUI inline warning + desktop notification; headless
/// modes log).
fn notify_rate_limit(message: &str) {
    crate::provider::emit_provider_event(crate::provider::ProviderEvent {
        kind: crate::provider::ProviderEventKind::RateLimited,
        provider: "codebuddy".to_string(),
        message: message.to_string(),
    });
}

/// Process-wide session registry (keyed by tack session id).
static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Mutex<CodeBuddySession>>>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, Arc<Mutex<CodeBuddySession>>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Idle sessions are reaped after this long without a stream call
/// (CLI child + MCP bridge are not free; the registry is otherwise
/// append-only).
const SESSION_IDLE_EVICT: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

/// Persisted tack-session → codebuddy-session-id mapping: a NEW tack
/// process rebuilds into the SAME CodeBuddy session file (cross-process
/// resume), keeping the CLI's native history/cache aligned.
fn cb_session_map_path() -> PathBuf {
    let agent = std::env::var_os("TACK_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".tack")
                .join("agent")
        });
    agent.join("codebuddy-sessions.json")
}

/// Serialize access to the codebuddy session map: writers race with each
/// other (read-modify-write) AND with readers (O_TRUNC means a concurrent
/// read can observe an empty/torn file and mint a stray uuid).
static CB_SESSION_MAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn read_cb_session_id(key: &str) -> Option<String> {
    let _guard = CB_SESSION_MAP_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let text = std::fs::read_to_string(cb_session_map_path()).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

fn write_cb_session_id(key: &str, id: &str) {
    let _guard = CB_SESSION_MAP_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = cb_session_map_path();
    let mut map = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if map.get(key).and_then(Value::as_str) == Some(id) {
        return;
    }
    map.insert(key.to_string(), json!(id));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(&Value::Object(map)) {
        // Atomic publish: a plain O_TRUNC write is observable mid-truncate
        // by readers in other processes.
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Close + remove the codebuddy CLI session for a tack session id
/// (session_end / shutdown hook).
pub async fn close_session(key: &str) {
    let session = sessions().lock().await.remove(key);
    if let Some(session) = session {
        let mut session = session.lock().await;
        kill_tree(&session.child);
        let _ = session.child.start_kill();
    }
}

/// Close every codebuddy CLI session (process shutdown).
pub async fn close_all_sessions() {
    let drained: Vec<Arc<Mutex<CodeBuddySession>>> =
        sessions().lock().await.drain().map(|(_, s)| s).collect();
    for session in drained {
        let mut session = session.lock().await;
        kill_tree(&session.child);
        let _ = session.child.start_kill();
    }
}

// ===========================================================================
// Provider
// ===========================================================================

/// CodeBuddy provider: one long-lived CLI session per tack session.
#[derive(Debug)]
pub struct CodeBuddyStreamProvider;

impl Provider for CodeBuddyStreamProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let model = model.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let message = drive(model, context, options, &sender).await;
            sender.end(message);
        });
        stream
    }
}

/// F18 idle watchdog for the CLI event loop: while a turn is generating,
/// the CLI emits stream_event deltas every few seconds; parked tool calls
/// end the loop (message_stop) rather than idle it. Five minutes without
/// ANY line therefore means the CLI process is wedged (deadlocked, or its
/// own HTTP stack hung beyond every provider timeout) — abort the turn
/// instead of hanging the agent forever. Generous on purpose: this is a
/// dead-process detector, not a latency SLO.
const CLI_IDLE_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Error text from a `result` message with `is_error: true`. The CLI
/// mirrors Claude Code here: `errors` is a string array, `error` a
/// string (or `{message}` object) on older builds, and the `result`
/// string covers synthetic failures like interrupts. `errors_info` —
/// read by the first tack implementation — never existed on the wire,
/// which collapsed every real failure into the opaque "codebuddy turn
/// failed". Last resort names the subtype so the failure stays
/// diagnosable.
fn result_error_text(message: &Value) -> String {
    if let Some(errors) = message.get("errors").and_then(Value::as_array) {
        let text = errors
            .iter()
            .map(|e| {
                e.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string())
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return text;
        }
    }
    match message.get("error") {
        Some(Value::String(s)) if !s.is_empty() => return s.clone(),
        Some(obj @ Value::Object(_)) => {
            if let Some(s) = obj
                .get("message")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return s.to_string();
            }
        }
        _ => {}
    }
    if let Some(s) = message
        .get("result")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return s.to_string();
    }
    let subtype = message
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("codebuddy turn failed ({subtype})")
}

/// MCP `inputSchema` sanitizer. schemars 1.x emits draft-2020-12 output
/// — `$schema`, `$defs`/`$ref`, `"type": ["T","null"]` unions, `format`
/// values like "uint"/"double" — that strict function-calling backends
/// reject outright: CodeBuddy's API answers "400 invalid function call
/// parameters", and a CLI retry without functions then surfaces the
/// model's raw `<tool_calls>` TEXT instead of native calls. Rewrite to
/// the conservative draft-07 subset the TS pi tools (TypeBox) emit:
/// meta declarations gone, local refs inlined, null unions collapsed
/// (optionality already lives in `required`), `format`/`title` dropped
/// (informational only; "uint"/"double" are not draft-07 values).
fn sanitize_mcp_schema(schema: &Value) -> Value {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    sanitize_schema_node(schema, &defs, 0)
}

fn sanitize_schema_node(node: &Value, defs: &Value, depth: u32) -> Value {
    // Generous cycle guard: a recursive schema (none today) degrades to
    // the raw node instead of looping forever.
    if depth > 32 {
        return node.clone();
    }
    let Value::Object(map) = node else {
        return match node {
            Value::Array(arr) => Value::Array(
                arr.iter()
                    .map(|v| sanitize_schema_node(v, defs, depth + 1))
                    .collect(),
            ),
            other => other.clone(),
        };
    };
    // Local $ref: inline the target, then apply siblings (schemars puts
    // `description` next to a $ref).
    if let Some(name) = node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        && let Some(target) = defs.get(name)
    {
        let mut resolved = sanitize_schema_node(target, defs, depth + 1);
        if let Value::Object(res_obj) = &mut resolved {
            for (k, v) in map {
                if k == "$ref" {
                    continue;
                }
                res_obj.insert(k.clone(), sanitize_schema_node(v, defs, depth + 1));
            }
        }
        return resolved;
    }
    let mut out = serde_json::Map::new();
    for (k, v) in map {
        match k.as_str() {
            "$schema" | "$id" | "$anchor" | "$dynamicAnchor" | "$vocabulary" | "$comment"
            | "$defs" | "definitions" | "format" | "title" => {}
            "type" => match v {
                Value::Array(types) => {
                    let mut non_null = types.iter().filter(|t| t.as_str() != Some("null"));
                    match (non_null.next(), non_null.next()) {
                        // ["T", "null"] -> "T" (absence already signals
                        // optionality via `required`).
                        (Some(t), None) => {
                            out.insert(k.clone(), t.clone());
                        }
                        _ => {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                }
                _ => {
                    out.insert(k.clone(), v.clone());
                }
            },
            _ => {
                out.insert(k.clone(), sanitize_schema_node(v, defs, depth + 1));
            }
        }
    }
    Value::Object(out)
}

fn emit_error(
    sender: &crate::stream::EventSender<AssistantMessageEvent, AssistantMessage>,
    model: &Model,
    reason: StopReason,
    error: &str,
) -> AssistantMessage {
    let mut message = AssistantMessage::pending(model);
    message.stop_reason = reason;
    message.error_message = Some(error.to_string());
    let _ = sender.push(AssistantMessageEvent::Error {
        reason,
        error: message.clone(),
    });
    message
}

/// The per-stream driver: sync the CLI session, forward events until the
/// next tool boundary or turn end.
async fn drive(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: &crate::stream::EventSender<AssistantMessageEvent, AssistantMessage>,
) -> AssistantMessage {
    let session_key = options
        .session_id
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let effort = effort_for(&model, options.reasoning);
    // Cross-process fast path: a fresh tack process with restored history
    // (assistant messages present) can never sync onto a fresh CLI —
    // rebuild natively RIGHT AWAY (reuses the persisted codebuddy session
    // id) instead of wasting a doomed spawn + initialize first.
    {
        let registered = sessions().lock().await.contains_key(&session_key);
        let has_history = context
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant(_)));
        if !registered && has_history {
            match CodeBuddySession::rebuild_native(
                &session_key,
                &model.id,
                context.system_prompt.as_deref(),
                effort.as_deref(),
                None,
                false,
                &context,
            )
            .await
            {
                Ok(session) => {
                    tracing::debug!(
                        "codebuddy: fresh process with restored history — native rebuild (no doomed spawn)"
                    );
                    sessions()
                        .lock()
                        .await
                        .insert(session_key.clone(), Arc::new(Mutex::new(session)));
                }
                Err(error) => {
                    // Fall through: the normal path respawns via the
                    // transcript fallback.
                    tracing::debug!(
                        "codebuddy: startup native rebuild unavailable ({error}); normal path"
                    );
                }
            }
        }
    }
    let session = match get_or_spawn(
        &session_key,
        &model,
        context.system_prompt.as_deref(),
        effort.as_deref(),
    )
    .await
    {
        Ok(session) => session,
        Err(error) => return emit_error(sender, &model, StopReason::Error, &error),
    };
    let mut session = session.lock().await;
    tracing::debug!("codebuddy: drive start (session {session_key}, effort={effort:?})");

    // Refresh the session's tool list for this call (the CLI re-lists
    // tools around every tool turn).
    session.tools = context.tools.clone();

    // Post-abort rebuild (reference: needsRebuild): never sync on top of a
    // half-finished turn — respawn and replay the flattened transcript.
    if session.needs_respawn {
        session.needs_respawn = false;
        tracing::debug!("codebuddy: post-abort rebuild, respawning session");
        if let Err(error) = session.respawn(&context).await {
            return emit_error(sender, &model, StopReason::Error, &error);
        }
    }

    // Sync: bring the CLI session up to date with tack-ai's context.
    if let Err(error) = session.sync(&context.messages).await {
        tracing::warn!("codebuddy: sync failed ({error}); respawning session");
        if let Err(error) = session.respawn(&context).await {
            return emit_error(sender, &model, StopReason::Error, &error);
        }
        // A native (JSONL) rebuild leaves the unresolved tail unsent —
        // deliver it to the resumed CLI. No-op after a transcript replay
        // (everything already synced there).
        if let Err(error) = session.sync(&context.messages).await {
            return emit_error(sender, &model, StopReason::Error, &error);
        }
    }
    tracing::debug!("codebuddy: sync ok, entering event loop");

    // Event loop.
    let mut partial = AssistantMessage::pending(&model);
    let _ = sender.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });
    let mut content_index = 0usize;
    // stream_event state (--include-partial-messages). `blocks` stays
    // parallel to `partial.content`; a stopped block keeps its slot with
    // api_index=None so positions never shift (reference: index deleted,
    // block kept — deltas reverse-scan, stops first-match).
    let mut blocks: Vec<CliBlock> = Vec::new();
    let mut saw_stream_event = false;
    let mut saw_tool_call = false;
    // Delta window (api::DeltaCoalescer): stream_event deltas arrive
    // token-at-a-time and used to clone the whole partial message into the
    // unbounded event channel per chunk. Merge them like the HTTP
    // adapters; structural events flush the window first (ordering), and
    // terminal paths flush below.
    let mut coalescer = crate::api::DeltaCoalescer::new();
    loop {
        let inbound = {
            let session = &mut *session;
            let io = &mut session.io;
            tokio::select! {
                _ = options.cancel.cancelled() => {
                let _ = io.send(&json!({
                    "type": "control_request",
                    "request_id": "cancel-0",
                    "request": { "subtype": "interrupt" },
                })).await;
                // F18: fail EVERY in-flight tool call — parked tack calls
                // are dropped and stashed CLI tools/calls get an error
                // response, so the CLI's MCP client unwinds instead of
                // parking on a response that will never come.
                let had_parked = !session.parked.is_empty();
                session.parked.clear();
                for call in std::mem::take(&mut session.pending_mcp_calls) {
                    let _ = io.mcp_control_error(&call.request_id, "Operation aborted").await;
                }
                if !had_parked {
                    // Aborted mid-GENERATION: the CLI session is still
                    // consistent — the partial assistant turn never entered
                    // tack's synced history, so the next turn syncs cleanly
                    // onto this session. Drain the interrupted turn's
                    // residual messages (up to its result) and KEEP the
                    // session alive instead of rebuilding.
                    let deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                    loop {
                        match tokio::time::timeout_at(deadline, io.next_line()).await {
                            Ok(Some(message))
                                if message.get("type").and_then(Value::as_str)
                                    == Some("result") =>
                            {
                                tracing::debug!(
                                    "codebuddy: interrupted turn drained; session kept"
                                );
                                break;
                            }
                            // partial assistant / stream_events / control_*: discard
                            Ok(Some(_)) => {
                                continue;
                            }
                            // CLI gone or no result within the budget: state
                            // unknown — rebuild next turn on a FRESH id
                            // (orphan writes may still land on the old one).
                            Ok(None) | Err(_) => {
                                session.needs_respawn = true;
                                session.rotate_on_rebuild = true;
                                break;
                            }
                        }
                    }
                } else {
                    // Aborted with parked tool calls (reference onAbort):
                    // the CLI's in-flight tools/calls were failed above so
                    // its MCP client can unwind; force a rebuild next turn —
                    // the interrupted turn's CLI state is undefined.
                    session.needs_respawn = true;
                    session.rotate_on_rebuild = true;
                }
                coalescer.flush_into(sender, &partial);
                return emit_error(sender, &model, StopReason::Aborted, "Operation aborted");
            }
            message = tokio::time::timeout(CLI_IDLE_WATCHDOG, io.next_line()) => {
                match message {
                    Ok(Some(message)) => message,
                    Ok(None) => {
                        coalescer.flush_into(sender, &partial);
                        return emit_error(
                            sender,
                            &model,
                            StopReason::Error,
                            "codebuddy CLI exited mid-turn",
                        );
                    }
                    // F18 idle watchdog: the CLI produced no event for the
                    // whole window — treat it as wedged, run the same
                    // cleanup as an abort, and fail the turn instead of
                    // hanging the agent forever.
                    Err(_elapsed) => {
                        tracing::warn!(
                            "codebuddy: no CLI event for {CLI_IDLE_WATCHDOG:?}; aborting wedged turn"
                        );
                        session.parked.clear();
                        for call in std::mem::take(&mut session.pending_mcp_calls) {
                            let _ = io
                                .mcp_control_error(&call.request_id, "turn aborted (CLI idle)")
                                .await;
                        }
                        session.needs_respawn = true;
                        session.rotate_on_rebuild = true;
                        coalescer.flush_into(sender, &partial);
                        return emit_error(
                            sender,
                            &model,
                            StopReason::Error,
                            "codebuddy CLI idle: no events for 5 minutes",
                        );
                    }
                }
            }
            }
        };
        {
            let message = inbound;
            // MCP control traffic (mcp_message): initialize / tools/list
            // answered inline; tools/call stashes itself for resolve_parked.
            match session.handle_mcp_control(&message).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    coalescer.flush_into(sender, &partial);
                    return emit_error(sender, &model, StopReason::Error, &error);
                }
            }
            {
                let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    // Raw Anthropic SSE stream (--include-partial-messages):
                    // incremental text/thinking/tool-call streaming. When a
                    // turn saw these, the following `assistant` message
                    // (completed blocks) is skipped (reference:
                    // processStreamEvent / processAssistantMessage).
                    "stream_event" => {
                        saw_stream_event = true;
                        // The real turn has started producing tokens — any
                        // stale echo still queued behind it is covered by
                        // the saw_stream_event guard above.
                        session.tool_echo_pending.clear();
                        let event = message.get("event").cloned().unwrap_or(Value::Null);
                        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                        match event_type {
                            "message_start" => {
                                if let Some(usage) =
                                    event.pointer("/message/usage").and_then(parse_usage)
                                {
                                    apply_request_usage(&mut partial, usage);
                                }
                                // A SECOND message_start mid-turn means the
                                // CLI abandoned the first attempt (API-level
                                // retry) and restarted content indices — the
                                // old blocks will never see their
                                // content_block_stop, and their index is about
                                // to be reused (misrouting every later delta
                                // with that index). Finalize the abandoned
                                // text/thinking blocks so each stays a
                                // coherent, properly-ended prefix. (Tool
                                // blocks are left alone: a retried-away
                                // tool_use never produces a tools/call frame,
                                // and parking it would wedge the boundary.)
                                for (pos, block) in blocks.iter_mut().enumerate() {
                                    if block.api_index.take().is_none() {
                                        continue;
                                    }
                                    match partial.content.get_mut(pos) {
                                        Some(ContentBlock::Text { text, .. }) => {
                                            let content = text.clone();
                                            coalescer.push(
                                                sender,
                                                &partial,
                                                AssistantMessageEvent::TextEnd {
                                                    content_index: pos,
                                                    content,
                                                    partial: partial.clone(),
                                                },
                                            );
                                        }
                                        Some(ContentBlock::Thinking { thinking, .. }) => {
                                            let content = thinking.clone();
                                            coalescer.push(
                                                sender,
                                                &partial,
                                                AssistantMessageEvent::ThinkingEnd {
                                                    content_index: pos,
                                                    content,
                                                    partial: partial.clone(),
                                                },
                                            );
                                        }
                                        // ToolCall: keep the slot but stop
                                        // routing deltas to it (api_index
                                        // already cleared). A retried-away
                                        // tool_use never produces a
                                        // tools/call frame; leaving it
                                        // unparked (no stop will match) is
                                        // what keeps the boundary sane.
                                        _ => {}
                                    }
                                }
                            }
                            "content_block_start" => {
                                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0)
                                    as usize;
                                let block =
                                    event.get("content_block").cloned().unwrap_or(Value::Null);
                                match block.get("type").and_then(Value::as_str) {
                                    Some("text") => {
                                        partial.content.push(ContentBlock::text(""));
                                        blocks.push(CliBlock::new(index));
                                        let content_index = partial.content.len() - 1;
                                        coalescer.push(
                                            sender,
                                            &partial,
                                            AssistantMessageEvent::TextStart {
                                                content_index,
                                                partial: partial.clone(),
                                            },
                                        );
                                    }
                                    Some("thinking") => {
                                        partial.content.push(ContentBlock::Thinking {
                                            thinking: String::new(),
                                            thinking_signature: None,
                                            redacted: None,
                                        });
                                        blocks.push(CliBlock::new(index));
                                        let content_index = partial.content.len() - 1;
                                        coalescer.push(
                                            sender,
                                            &partial,
                                            AssistantMessageEvent::ThinkingStart {
                                                content_index,
                                                partial: partial.clone(),
                                            },
                                        );
                                    }
                                    Some("tool_use") => {
                                        let name =
                                            block.get("name").and_then(Value::as_str).unwrap_or("");
                                        if let Some(short) =
                                            name.strip_prefix(&format!("mcp__{MCP_SERVER_NAME}__"))
                                        {
                                            saw_tool_call = true;
                                            let id = block
                                                .get("id")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_string();
                                            // Seed with content_block.input
                                            // (reference: arguments ?? {} at
                                            // block start): the CLI delivers
                                            // some tool calls with the FULL
                                            // input at start and no
                                            // input_json_delta stream at all.
                                            let seeded = match block.get("input") {
                                                Some(v @ Value::Object(_)) => v.clone(),
                                                _ => json!({}),
                                            };
                                            partial.content.push(ContentBlock::ToolCall {
                                                id,
                                                name: short.to_string(),
                                                arguments: seeded,
                                                thought_signature: None,
                                                namespace: None,
                                            });
                                            blocks.push(CliBlock::new(index));
                                            let content_index = partial.content.len() - 1;
                                            coalescer.push(
                                                sender,
                                                &partial,
                                                AssistantMessageEvent::ToolCallStart {
                                                    content_index,
                                                    partial: partial.clone(),
                                                },
                                            );
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_delta" => {
                                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0)
                                    as usize;
                                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                                // Reverse scan: the CLI can reuse a content
                                // index for a later block; route the delta to
                                // the ACTIVE (last-started) match.
                                let Some(pos) =
                                    blocks.iter().rposition(|b| b.api_index == Some(index))
                                else {
                                    continue;
                                };
                                match delta.get("type").and_then(Value::as_str) {
                                    Some("text_delta") => {
                                        let text =
                                            delta.get("text").and_then(Value::as_str).unwrap_or("");
                                        if let Some(ContentBlock::Text { text: t, .. }) =
                                            partial.content.get_mut(pos)
                                        {
                                            t.push_str(text);
                                        }
                                        if let Some(ev) = coalescer.offer(
                                            crate::api::DeltaKind::Text,
                                            pos,
                                            text.to_string(),
                                            &partial,
                                        ) {
                                            let _ = sender.push(ev);
                                        }
                                    }
                                    Some("thinking_delta") => {
                                        let text = delta
                                            .get("thinking")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        if let Some(ContentBlock::Thinking { thinking, .. }) =
                                            partial.content.get_mut(pos)
                                        {
                                            thinking.push_str(text);
                                        }
                                        if let Some(ev) = coalescer.offer(
                                            crate::api::DeltaKind::Thinking,
                                            pos,
                                            text.to_string(),
                                            &partial,
                                        ) {
                                            let _ = sender.push(ev);
                                        }
                                    }
                                    Some("input_json_delta") => {
                                        let chunk = delta
                                            .get("partial_json")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        blocks[pos].partial_json.push_str(chunk);
                                        // F13: defer the O(accumulated)
                                        // streaming re-parse to the coalescer
                                        // window (final parse at block stop).
                                        if coalescer.would_flush(
                                            crate::api::DeltaKind::ToolCall,
                                            pos,
                                            chunk.len(),
                                        ) {
                                            let parsed = crate::json_repair::parse_streaming_json(
                                                &blocks[pos].partial_json,
                                            );
                                            if let Some(ContentBlock::ToolCall {
                                                arguments, ..
                                            }) = partial.content.get_mut(pos)
                                            {
                                                *arguments = parsed;
                                            }
                                        }
                                        if let Some(ev) = coalescer.offer(
                                            crate::api::DeltaKind::ToolCall,
                                            pos,
                                            chunk.to_string(),
                                            &partial,
                                        ) {
                                            let _ = sender.push(ev);
                                        }
                                    }
                                    Some("signature_delta") => {
                                        let sig = delta
                                            .get("signature")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        if let Some(ContentBlock::Thinking {
                                            thinking_signature,
                                            ..
                                        }) = partial.content.get_mut(pos)
                                        {
                                            thinking_signature
                                                .get_or_insert_with(String::new)
                                                .push_str(sig);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_stop" => {
                                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0)
                                    as usize;
                                // Stop EVERY active block carrying this
                                // index: the CLI reuses ONE content index
                                // for PARALLEL tool_use blocks and emits a
                                // single stop for all of them (observed on
                                // 2.156.0). Sequential index reuse (a later
                                // block re-taking a stopped block's index)
                                // is unaffected — only the active block
                                // matches.
                                let positions: Vec<usize> = blocks
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(pos, b)| {
                                        (b.api_index == Some(index)).then_some(pos)
                                    })
                                    .collect();
                                if positions.is_empty() {
                                    continue;
                                }
                                for pos in positions {
                                    blocks[pos].api_index = None;
                                    let partial_json =
                                        std::mem::take(&mut blocks[pos].partial_json);
                                    match partial.content.get_mut(pos) {
                                        Some(ContentBlock::Text { text, .. }) => {
                                            let content = text.clone();
                                            coalescer.push(
                                                sender,
                                                &partial,
                                                AssistantMessageEvent::TextEnd {
                                                    content_index: pos,
                                                    content,
                                                    partial: partial.clone(),
                                                },
                                            );
                                        }
                                        Some(ContentBlock::Thinking { thinking, .. }) => {
                                            let content = thinking.clone();
                                            coalescer.push(
                                                sender,
                                                &partial,
                                                AssistantMessageEvent::ThinkingEnd {
                                                    content_index: pos,
                                                    content,
                                                    partial: partial.clone(),
                                                },
                                            );
                                        }
                                        Some(ContentBlock::ToolCall { .. }) => {
                                            // Scope the mutable borrow: finalize
                                            // arguments, then clone for the
                                            // event + parked registration.
                                            let (id, name, tool_call) = {
                                                let Some(ContentBlock::ToolCall {
                                                    id,
                                                    name,
                                                    arguments,
                                                    ..
                                                }) = partial.content.get_mut(pos)
                                                else {
                                                    continue;
                                                };
                                                // No input_json_delta stream
                                                // (or an unrecoverable one):
                                                // fall back to the block-start
                                                // seed (reference:
                                                // parsePartialJson(partial, block.arguments)).
                                                *arguments = map_tool_args(
                                                    name,
                                                    parse_tool_json(&partial_json, arguments),
                                                );
                                                (
                                                    id.clone(),
                                                    name.clone(),
                                                    partial.content[pos].clone(),
                                                )
                                            };
                                            coalescer.push(
                                                sender,
                                                &partial,
                                                AssistantMessageEvent::ToolCallEnd {
                                                    content_index: pos,
                                                    tool_call,
                                                    partial: partial.clone(),
                                                },
                                            );
                                            let arguments = match &partial.content[pos] {
                                                ContentBlock::ToolCall { arguments, .. } => {
                                                    arguments.clone()
                                                }
                                                _ => json!({}),
                                            };
                                            session.parked.push(ParkedCall {
                                                tool_use_id: id,
                                                name,
                                                arguments,
                                                mcp_request_id: None,
                                            });
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            "message_delta" => {
                                if let Some(reason) =
                                    event.pointer("/delta/stop_reason").and_then(Value::as_str)
                                {
                                    partial.stop_reason = cli_stop_reason(reason);
                                }
                                if let Some(usage) = event.get("usage").and_then(parse_usage) {
                                    apply_request_usage(&mut partial, usage);
                                }
                            }
                            "message_stop" if saw_tool_call => {
                                // Tool boundary: end this pi stream; the
                                // CLI's MCP call parks until the next
                                // stream() resolves it. The CLI still has
                                // this message's completed `assistant` echo
                                // in flight — remember its tool-use ids so
                                // the next turn's event loop skips it.
                                partial.stop_reason = StopReason::ToolUse;
                                finalize_tool_arguments(&mut partial, &blocks);
                                // The stream_event channel is LOSSY for tool
                                // arguments: parallel tool_use blocks share
                                // one content index, and the CLI may deliver
                                // a call's input only via content_block_start
                                // or interleave/drop its input_json deltas
                                // entirely (observed on 2.156.0 with
                                // deepseek-v4-pro: parallel calls arrived as
                                // `{}` and executed as \"command is a
                                // required property\"). The tools/call MCP
                                // frames the CLI dispatches right after the
                                // boundary carry the COMPLETE arguments —
                                // adopt them before the agent loop executes
                                // anything.
                                session
                                    .adopt_mcp_tool_args(&mut partial, &options.cancel)
                                    .await;
                                session.tool_echo_pending = partial
                                    .content
                                    .iter()
                                    .filter_map(|b| match b {
                                        ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                                        _ => None,
                                    })
                                    .collect();
                                session.synced.push(assistant_fingerprint(&partial));
                                coalescer.push(
                                    sender,
                                    &partial,
                                    AssistantMessageEvent::Done {
                                        reason: StopReason::ToolUse,
                                        message: partial.clone(),
                                    },
                                );
                                return partial;
                            }
                            "message_stop" => {}
                            _ => {} // ping and unknown events
                        }
                    }
                    "assistant" => {
                        if saw_stream_event {
                            // Content already streamed via stream_event; the
                            // assistant message only repeats it.
                            continue;
                        }
                        // Stale-echo skip: the completed `assistant`
                        // message(s) of the PREVIOUS tool turn are still in
                        // the pipe (drive() returned at its message_stop;
                        // the real CLI re-yields them after the tool call
                        // resolves, split per content block). Every
                        // assistant line before this turn's first
                        // stream_event is that duplicate — processing it
                        // would re-park the same tool call(s) to pi and
                        // wedge the turn (no tools/call exists for the
                        // duplicate ids).
                        if !session.tool_echo_pending.is_empty() {
                            tracing::debug!("codebuddy: skipping stale assistant echo");
                            continue;
                        }
                        if let Some(raw) = message.get("message").and_then(|m| m.get("usage")) {
                            tracing::debug!("codebuddy: assistant usage: {raw}");
                        }
                        let blocks = message
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let mut tool_calls = Vec::new();
                        for block in &blocks {
                            match block.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    let text = block
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    push_block(
                                        sender,
                                        &mut partial,
                                        &mut content_index,
                                        ContentBlock::text(text),
                                    );
                                }
                                Some("thinking") => {
                                    let thinking = block
                                        .get("thinking")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    push_block(
                                        sender,
                                        &mut partial,
                                        &mut content_index,
                                        ContentBlock::Thinking {
                                            thinking,
                                            thinking_signature: None,
                                            redacted: None,
                                        },
                                    );
                                }
                                Some("tool_use") => {
                                    let name = block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    if let Some(short) =
                                        name.strip_prefix(&format!("mcp__{MCP_SERVER_NAME}__"))
                                    {
                                        let id = block
                                            .get("id")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string();
                                        let input = map_tool_args(
                                            short,
                                            block.get("input").cloned().unwrap_or(json!({})),
                                        );
                                        tool_calls.push(ContentBlock::ToolCall {
                                            id: id.clone(),
                                            name: short.to_string(),
                                            arguments: input.clone(),
                                            thought_signature: None,
                                            namespace: None,
                                        });
                                        session.parked.push(ParkedCall {
                                            tool_use_id: id,
                                            name: short.to_string(),
                                            arguments: input,
                                            mcp_request_id: None,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !tool_calls.is_empty() {
                            for call in tool_calls {
                                push_block(sender, &mut partial, &mut content_index, call);
                            }
                            if let Some(usage) = message
                                .get("message")
                                .and_then(|m| m.get("usage"))
                                .and_then(parse_usage)
                            {
                                apply_request_usage(&mut partial, usage);
                            }
                            partial.stop_reason = StopReason::ToolUse;
                            session.synced.push(assistant_fingerprint(&partial));
                            // Same stale-echo guard as the message_stop
                            // path: if the CLI re-yields this message after
                            // the tool call resolves, the next turn's event
                            // loop skips it.
                            session.tool_echo_pending = partial
                                .content
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                                    _ => None,
                                })
                                .collect();
                            let _ = sender.push(AssistantMessageEvent::Done {
                                reason: StopReason::ToolUse,
                                message: partial.clone(),
                            });
                            return partial;
                        }
                        if let Some(usage) = message
                            .get("message")
                            .and_then(|m| m.get("usage"))
                            .and_then(parse_usage)
                        {
                            apply_request_usage(&mut partial, usage);
                        }
                    }
                    "result" => {
                        // Any pending stale echo precedes the result line;
                        // if it never arrived, drop the ids here so they
                        // can't linger into a later turn.
                        session.tool_echo_pending.clear();
                        // Zombie tools/calls (stashed but never matched to
                        // a tack tool result — e.g. a duplicate the agent
                        // loop dropped): the turn is over, fail them so
                        // the CLI's MCP client unwinds.
                        if !session.pending_mcp_calls.is_empty() {
                            session
                                .fail_tool_calls("turn ended with the tool call unresolved")
                                .await;
                        }
                        let is_error = message
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        // result.modelUsage carries the SERVED limits — the
                        // id-based estimate can be wrong (issue #18 analog);
                        // adopt the real values (registry + disk cache).
                        if let Some(model_usage) = message.get("modelUsage") {
                            learn_served_limits(model_usage);
                        }
                        if let Some(raw) = message.get("usage") {
                            tracing::debug!("codebuddy: result usage: {raw}");
                        }
                        if let Some(mut usage) = message.get("usage").and_then(parse_usage) {
                            // result.usage is the turn AGGREGATE (sum over every
                            // API call the CLI made, internal aux calls
                            // included) — right for cost totals, wrong for
                            // context size: each call re-sends the full
                            // context, so the aggregate counts it N times.
                            // Keep the last per-request size in total_tokens;
                            // calculate_context_tokens prefers it over the sum.
                            usage.total_tokens = partial.usage.total_tokens;
                            partial.usage = usage;
                        }
                        if is_error {
                            tracing::debug!("codebuddy: error result: {message}");
                            let reason_text = result_error_text(&message);
                            coalescer.flush_into(sender, &partial);
                            return emit_error(sender, &model, StopReason::Error, &reason_text);
                        }
                        // Reference fallback: a turn with no stream events
                        // and no assistant content surfaces the result text.
                        if !saw_stream_event
                            && partial.content.is_empty()
                            && let Some(text) = message
                                .get("result")
                                .and_then(Value::as_str)
                                .filter(|t| !t.is_empty())
                        {
                            push_block(
                                sender,
                                &mut partial,
                                &mut content_index,
                                ContentBlock::text(text),
                            );
                        }
                        finalize_tool_arguments(&mut partial, &blocks);
                        // Keep the stop reason from message_delta (Length
                        // survives); only an unset one defaults to Stop.
                        if matches!(partial.stop_reason, StopReason::Pending) {
                            partial.stop_reason = StopReason::Stop;
                        }
                        session.synced.push(assistant_fingerprint(&partial));
                        coalescer.push(
                            sender,
                            &partial,
                            AssistantMessageEvent::Done {
                                reason: partial.stop_reason,
                                message: partial.clone(),
                            },
                        );
                        return partial;
                    }
                    "system" if message.get("subtype").and_then(Value::as_str) == Some("init") => {
                        let id = message
                            .get("session_id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        if let Some(id) = &id {
                            // Persist for cross-process resume.
                            write_cb_session_id(&session.key, id);
                        }
                        session.cli_session_id = id;
                    }
                    "rate_limit_event" => {
                        let info = message
                            .get("rate_limit_info")
                            .cloned()
                            .unwrap_or(Value::Null);
                        tracing::info!("codebuddy: rate_limit_event {info}");
                        // Reference: piUI.notify on rejected / warning.
                        let status = info.get("status").and_then(Value::as_str).unwrap_or("");
                        match status {
                            "rejected" => {
                                let resets = info
                                    .get("resetsAt")
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|| "unknown".into());
                                let kind = info
                                    .get("rateLimitType")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                notify_rate_limit(&format!(
                                    "CodeBuddy rate limited ({kind}) — resets at {resets}"
                                ));
                            }
                            "allowed_warning" => {
                                let pct = info
                                    .get("utilization")
                                    .and_then(Value::as_f64)
                                    .unwrap_or(0.0)
                                    .round();
                                notify_rate_limit(&format!(
                                    "CodeBuddy rate limit warning: {pct}% used"
                                ));
                            }
                            _ => {}
                        }
                    }
                    // user (tool_result echoes), control_*.
                    _ => {}
                }
            }
        }
    }
}

/// One in-flight content block from the CLI's raw stream (`stream_event`).
/// Stays slot-parallel to `partial.content`; `api_index` is cleared on
/// block stop so a reused CLI index binds to the next block (deltas
/// reverse-scan, stops first-match — reference: processStreamEvent).
struct CliBlock {
    api_index: Option<usize>,
    partial_json: String,
}

impl CliBlock {
    fn new(api_index: usize) -> Self {
        CliBlock {
            api_index: Some(api_index),
            partial_json: String::new(),
        }
    }
}

/// Parse the accumulated input_json_delta stream of a tool call, falling
/// back to the block-start seed (`content_block.input`) when the stream is
/// empty or unrecoverable (reference: parsePartialJson(partialJson,
/// block.arguments) — a failed/empty parse keeps the seeded arguments).
fn parse_tool_json(partial_json: &str, seeded: &Value) -> Value {
    let trimmed = partial_json.trim();
    if trimmed.is_empty() {
        return seeded.clone();
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value;
    }
    let repaired = crate::json_repair::parse_streaming_json(trimmed);
    // parse_streaming_json collapses unrecoverable input to {} — prefer a
    // non-empty seed over that empty husk.
    if repaired.as_object().is_some_and(|o| o.is_empty())
        && seeded.as_object().is_some_and(|o| !o.is_empty())
    {
        return seeded.clone();
    }
    repaired
}

/// Final re-parse of every accumulated tool partial_json into the
/// message's arguments (F13 follow-up): per-delta parsing is throttled
/// to coalescer flushes and the block-stop re-parse, so a CLI/proxy that
/// omits content_block_stop would leave the terminal message short of
/// the trailing deltas. Slot-parallel indexing, same as the delta path.
fn finalize_tool_arguments(partial: &mut AssistantMessage, blocks: &[CliBlock]) {
    for (pos, block) in blocks.iter().enumerate() {
        if let Some(ContentBlock::ToolCall {
            name, arguments, ..
        }) = partial.content.get_mut(pos)
        {
            *arguments = map_tool_args(name, parse_tool_json(&block.partial_json, arguments));
        }
    }
}

/// Map the CLI's message_delta stop_reason (reference: mapStopReason).
fn cli_stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::Length,
        _ => StopReason::Stop,
    }
}

/// Normalize tool-call arguments to pi's parameter names (reference:
/// mapToolArgs / SDK_KEY_RENAMES). The model sometimes emits Claude-Code-
/// shaped keys from training data (`file_path`, `old_string`, …); first
/// alias wins. pi's bash has no default timeout, so a 120s safety default
/// is added when the model omits it.
fn map_tool_args(name: &str, args: Value) -> Value {
    let Value::Object(input) = args else {
        return args;
    };
    let renames: &[(&str, &str)] = match name {
        "read" | "write" => &[("file_path", "path")],
        "edit" => &[
            ("file_path", "path"),
            ("old_string", "oldText"),
            ("new_string", "newText"),
            ("old_text", "oldText"),
            ("new_text", "newText"),
        ],
        _ => &[],
    };
    // Canonical (pi-style) keys win over legacy aliases: insert direct keys
    // first, then aliases only into vacant slots. (The TS reference's
    // "first alias wins" follows document insertion order, which serde_json
    // doesn't preserve; canonical-wins is deterministic.)
    let mut out = serde_json::Map::new();
    for (key, value) in &input {
        if renames.iter().any(|(from, _)| *from == key) {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    for (key, value) in input {
        let Some((_, to)) = renames.iter().find(|(from, _)| *from == key) else {
            continue;
        };
        out.entry(to.to_string()).or_insert(value);
    }
    if name == "bash" && !out.contains_key("timeout") {
        out.insert("timeout".to_string(), json!(120));
    }
    Value::Object(out)
}

/// Append one content block with proper streaming events.
fn push_block(
    sender: &crate::stream::EventSender<AssistantMessageEvent, AssistantMessage>,
    partial: &mut AssistantMessage,
    content_index: &mut usize,
    block: ContentBlock,
) {
    let index = *content_index;
    enum Kind {
        Text(String),
        Thinking(String),
        Tool(Value),
        Other,
    }
    let kind = match &block {
        ContentBlock::Text { text, .. } => Kind::Text(text.clone()),
        ContentBlock::Thinking { thinking, .. } => Kind::Thinking(thinking.clone()),
        ContentBlock::ToolCall { arguments, .. } => Kind::Tool(arguments.clone()),
        _ => Kind::Other,
    };
    match kind {
        Kind::Text(text) => {
            let _ = sender.push(AssistantMessageEvent::TextStart {
                content_index: index,
                partial: partial.clone(),
            });
            if !text.is_empty() {
                let _ = sender.push(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: text.clone(),
                    partial: partial.clone(),
                });
            }
            partial.content.push(block);
            let _ = sender.push(AssistantMessageEvent::TextEnd {
                content_index: index,
                content: text,
                partial: partial.clone(),
            });
        }
        Kind::Thinking(thinking) => {
            let _ = sender.push(AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial: partial.clone(),
            });
            if !thinking.is_empty() {
                let _ = sender.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: thinking.clone(),
                    partial: partial.clone(),
                });
            }
            partial.content.push(block);
            let _ = sender.push(AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: thinking,
                partial: partial.clone(),
            });
        }
        Kind::Tool(arguments) => {
            let _ = sender.push(AssistantMessageEvent::ToolCallStart {
                content_index: index,
                partial: partial.clone(),
            });
            let delta = arguments.to_string();
            if delta != "{}" {
                let _ = sender.push(AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta,
                    partial: partial.clone(),
                });
            }
            partial.content.push(block.clone());
            let _ = sender.push(AssistantMessageEvent::ToolCallEnd {
                content_index: index,
                tool_call: block,
                partial: partial.clone(),
            });
        }
        Kind::Other => {
            partial.content.push(block);
        }
    }
    *content_index += 1;
}

/// Semantic fingerprint of a message for sync comparison: volatile
/// fields (timestamps) stripped so identical content across turns compares
/// equal even though pi re-stamps entries.
fn message_fingerprint(message: &Message) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or(Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("timestamp");
    }
    value
}

fn assistant_fingerprint(message: &AssistantMessage) -> Value {
    message_fingerprint(&Message::Assistant(message.clone()))
}

fn parse_usage(value: &Value) -> Option<Usage> {
    let get = |k: &str| value.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input: get("input_tokens"),
        output: get("output_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
        ..Usage::zero()
    })
}

/// Per-API-request usage from an assistant event (Anthropic semantics:
/// input excludes cached prefixes). Its sum is the actual context size at
/// that point — stash it in total_tokens so a later turn-aggregate
/// result.usage can't inflate the context estimate (it stays authoritative
/// for input/output/cache totals and cost).
fn apply_request_usage(partial: &mut AssistantMessage, mut usage: Usage) {
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    partial.usage = usage;
}

// ===========================================================================
// Session implementation
// ===========================================================================

impl CodeBuddySession {
    async fn spawn_new(
        key: &str,
        model_id: &str,
        system_prompt: Option<&str>,
        effort: Option<&str>,
        resume: Option<&str>,
    ) -> Result<Self, String> {
        let cli = cli_path().ok_or_else(|| {
            "codebuddy CLI not found — install @tencent-ai/codebuddy-code or set CODEBUDDY_PATH"
                .to_string()
        })?;
        let (argv_prompt, pending_system_prompt) = split_system_prompt(system_prompt);
        tracing::debug!(
            "codebuddy: spawning session (cli={}, model={model_id}, system_prompt={} chars{})",
            cli.display(),
            system_prompt.map(str::len).unwrap_or(0),
            if pending_system_prompt.is_some() {
                ", via stdin (over argv limit)"
            } else {
                ""
            }
        );
        if let Some(prompt) = system_prompt {
            tracing::debug!(
                "codebuddy: system prompt ({} chars):\n{prompt}",
                prompt.chars().count()
            );
        }
        let mut child = spawn_cli(&cli, model_id, argv_prompt, effort, resume)?;
        tracing::debug!(
            "codebuddy: CLI spawned (pid={:?}), initializing",
            child.id()
        );
        let mut io = CliIo::new(&mut child)?;
        io.initialize(true).await?;
        tracing::debug!("codebuddy: initialize handshake complete");
        Ok(CodeBuddySession {
            key: key.to_string(),
            child,
            io,
            tools: Vec::new(),
            synced: Vec::new(),
            system_prompt: system_prompt.map(str::to_string),
            pending_system_prompt,
            model_id: model_id.to_string(),
            effort: effort.map(str::to_string),
            needs_respawn: false,
            rotate_on_rebuild: false,
            last_used: std::time::Instant::now(),
            parked: Vec::new(),
            pending_mcp_calls: Vec::new(),
            cli_session_id: None,
            tool_echo_pending: Vec::new(),
        })
    }

    /// Hot-switch the session's model via the set_model control request
    /// (SDK parity: Query.setModel) — keeps the CLI process and its cache,
    /// unlike a respawn. Falls back to respawn on any failure (caller).
    async fn set_model(&mut self, model_id: &str) -> Result<(), String> {
        let Some(session_id) = self.cli_session_id.clone() else {
            return Err("CLI session id unknown (init not seen)".to_string());
        };
        self.io
            .control_request(
                "set-model",
                &json!({
                    "subtype": "set_model",
                    "session_id": session_id,
                    "model": model_id,
                }),
            )
            .await?;
        tracing::debug!("codebuddy: set_model → {model_id} (hot switch, session kept)");
        self.model_id = model_id.to_string();
        Ok(())
    }

    /// Inspect one inbound CLI line for MCP control traffic
    /// (`control_request` with `subtype: "mcp_message"`). initialize /
    /// notifications / ping / tools/list are answered inline; a tools/call
    /// is stashed in `pending_mcp_calls` — it answers only when the
    /// matching tack tool result arrives (resolve_parked). Returns true
    /// when the line was MCP traffic (caller must not process it as a
    /// conversation line).
    async fn handle_mcp_control(&mut self, line: &Value) -> Result<bool, String> {
        if line.get("type").and_then(Value::as_str) != Some("control_request") {
            return Ok(false);
        }
        let request = line.get("request").cloned().unwrap_or(Value::Null);
        if request.get("subtype").and_then(Value::as_str) != Some("mcp_message") {
            return Ok(false);
        }
        let request_id = line
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let server = request
            .get("server_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        if server != MCP_SERVER_NAME {
            self.io
                .mcp_control_error(&request_id, &format!("unknown MCP server: {server}"))
                .await?;
            return Ok(true);
        }
        let message = request.get("message").cloned().unwrap_or(Value::Null);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let mcp_id = message.get("id").cloned().unwrap_or(Value::Null);
        // Request vs notification (agent-sdk: a request has a method AND a
        // non-null id). Notifications get an ID-less ack — an `id` here
        // would collide with the MCP initialize id and wedge the CLI's
        // handshake for 60s.
        let is_request = !method.is_empty() && message.get("id").is_some_and(|id| !id.is_null());
        if !is_request {
            self.io
                .mcp_control_response(&request_id, json!({"jsonrpc": "2.0", "result": {}}))
                .await?;
            return Ok(true);
        }
        match method {
            "initialize" => {
                self.io.mcp_control_response(&request_id, json!({
                    "jsonrpc": "2.0",
                    "id": mcp_id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "tack-codebuddy", "version": env!("CARGO_PKG_VERSION") },
                    },
                }))
                .await?;
            }
            "ping" => {
                self.io
                    .mcp_control_response(
                        &request_id,
                        json!({"jsonrpc": "2.0", "id": mcp_id, "result": {}}),
                    )
                    .await?;
            }
            "tools/list" => {
                let tools: Vec<Value> = self
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": sanitize_mcp_schema(&t.parameters),
                        })
                    })
                    .collect();
                self.io
                    .mcp_control_response(
                        &request_id,
                        json!({
                            "jsonrpc": "2.0",
                            "id": mcp_id,
                            "result": { "tools": tools },
                        }),
                    )
                    .await?;
            }
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                self.pending_mcp_calls.push(McpToolCall {
                    request_id,
                    mcp_id,
                    name,
                    arguments,
                });
            }
            _ => {
                self.io
                    .mcp_control_response(
                        &request_id,
                        json!({
                            "jsonrpc": "2.0",
                            "id": mcp_id,
                            "error": { "code": -32601, "message": "method not found" },
                        }),
                    )
                    .await?;
            }
        }
        Ok(true)
    }

    /// Abort-path cleanup (F18): drop parked tack calls and fail every
    /// stashed CLI tools/call so the CLI's MCP client unwinds instead of
    /// parking on a response that will never come.
    async fn fail_tool_calls(&mut self, error: &str) {
        self.parked.clear();
        for call in std::mem::take(&mut self.pending_mcp_calls) {
            let _ = self.io.mcp_control_error(&call.request_id, error).await;
        }
    }

    /// Bring the CLI session up to date with tack-ai's context. Fails on
    /// divergence (caller respawns + replays).
    async fn sync(&mut self, messages: &[Message]) -> Result<(), String> {
        // Divergence check: synced prefix must match exactly (compared
        // by fingerprint — message timestamps differ across turns).
        let incoming: Vec<Value> = messages.iter().map(message_fingerprint).collect();
        if self.synced.len() > incoming.len() || self.synced[..] != incoming[..self.synced.len()] {
            return Err("context diverged (compaction or history edit)".into());
        }
        let tail = &messages[self.synced.len()..];
        tracing::debug!(
            "codebuddy: sync replaying {} message(s) ({} already synced)",
            tail.len(),
            self.synced.len()
        );
        for message in tail {
            match message {
                Message::ToolResult(result) => {
                    self.resolve_parked(result).await?;
                    self.synced.push(message_fingerprint(message));
                }
                Message::User(user) => {
                    // User messages only arrive at turn starts; none may be
                    // parked then.
                    if !self.parked.is_empty() {
                        return Err("new user input while tool calls are parked".into());
                    }
                    let mut content = Vec::new();
                    // Deferred system prompt (Windows argv limits): folded
                    // into the first user message — no extra turn.
                    if let Some(prompt) = self.pending_system_prompt.take() {
                        content.push(json!({
                            "type": "text",
                            "text": format!(
                                "[session system instructions]\n\n{prompt}\n\n[end of session system instructions]"
                            ),
                        }));
                    }
                    content.extend(user_content_to_cli(&user.content));
                    self.io
                        .send(&json!({
                            "type": "user",
                            "message": { "role": "user", "content": content },
                            "parent_tool_use_id": Value::Null,
                        }))
                        .await?;
                    self.synced.push(message_fingerprint(message));
                }
                Message::Assistant(_) => {
                    // Assistant messages are generated by the CLI itself;
                    // a foreign one means divergence.
                    return Err("unexpected assistant message in tail".into());
                }
                // System messages are transcript state the caller collapses
                // before syncing; skip defensively.
                Message::System(_) => {}
            }
        }
        Ok(())
    }

    /// Adopt the CLI's tools/call arguments as authoritative at a tool
    /// boundary. The stream_event re-stream is lossy for parallel tool
    /// calls (shared content index, dropped/interleaved input_json deltas),
    /// while the tools/call MCP frames — dispatched right after
    /// message_stop from the CLI's COMPLETE assistant message — always
    /// carry the full arguments. Wait (bounded) until every parked call
    /// has its frame, then rewrite any call whose streamed arguments don't
    /// match. Frames stay in pending_mcp_calls; the pairing is recorded on
    /// the parked call so resolve_parked answers exactly that frame —
    /// arg normalization (map_tool_args) would otherwise defeat the
    /// (name, arguments) match.
    async fn adopt_mcp_tool_args(
        &mut self,
        partial: &mut AssistantMessage,
        cancel: &tokio_util::sync::CancellationToken,
    ) {
        if self.parked.is_empty() {
            return;
        }
        // The real CLI dispatches every frame immediately after
        // message_stop; the bound only guards a lazy/lost dispatcher.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.pending_mcp_calls.len() < self.parked.len() {
            let line = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::debug!(
                        "codebuddy: {} parked call(s) but {} tools/call frame(s) at boundary; keeping streamed args",
                        self.parked.len(),
                        self.pending_mcp_calls.len(),
                    );
                    break;
                }
                line = self.io.recv_channel() => match line {
                    Some(line) => line,
                    None => return,
                },
            };
            match self.handle_mcp_control(&line).await {
                Ok(true) => {}
                Ok(false) => self.io.buffer.push_back(line),
                Err(error) => {
                    tracing::debug!("codebuddy: MCP handling failed at boundary: {error}");
                    break;
                }
            }
        }
        // Pair frames to parked calls: exact (name, arguments) first —
        // those streamed fine — then positionally by name for the rest
        // (the CLI dispatches in block order; reference: nextHandlerIdx).
        let mut used = vec![false; self.pending_mcp_calls.len()];
        for i in 0..self.parked.len() {
            let name = self.parked[i].name.clone();
            let exact = self
                .pending_mcp_calls
                .iter()
                .enumerate()
                .find(|(j, f)| {
                    !used[*j] && f.name == name && f.arguments == self.parked[i].arguments
                })
                .map(|(j, _)| j);
            if let Some(j) = exact {
                used[j] = true;
                self.parked[i].mcp_request_id = Some(self.pending_mcp_calls[j].request_id.clone());
                continue;
            }
            let same_name = self
                .pending_mcp_calls
                .iter()
                .enumerate()
                .find(|(j, f)| !used[*j] && f.name == name)
                .map(|(j, _)| j);
            let Some(j) = same_name else {
                continue;
            };
            used[j] = true;
            self.parked[i].mcp_request_id = Some(self.pending_mcp_calls[j].request_id.clone());
            let adopted = map_tool_args(&name, self.pending_mcp_calls[j].arguments.clone());
            if adopted == self.parked[i].arguments {
                continue;
            }
            tracing::info!(
                "codebuddy: adopting tools/call arguments for {name} (streamed args were lost/corrupt)"
            );
            self.parked[i].arguments = adopted.clone();
            let tool_use_id = self.parked[i].tool_use_id.clone();
            if let Some(ContentBlock::ToolCall { arguments, .. }) = partial
                .content
                .iter_mut()
                .find(|b| matches!(b, ContentBlock::ToolCall { id, .. } if *id == tool_use_id))
            {
                *arguments = adopted;
            }
        }
    }

    /// Resolve one parked call with its tool result: find the CLI's
    /// matching tools/call (stashed or still inbound) and answer it with
    /// an mcp_response carrying the result content. The CLI's tools/call
    /// can lag the tool boundary (it is dispatched after message_stop),
    /// so wait briefly for it before failing.
    async fn resolve_parked(&mut self, result: &ToolResultMessage) -> Result<(), String> {
        let position = self
            .parked
            .iter()
            .position(|p| p.tool_use_id == result.tool_call_id)
            .ok_or_else(|| format!("tool result for unknown call {}", result.tool_call_id))?;
        let parked = self.parked.remove(position);
        // Correlate with the CLI's tools/call: the boundary pairing
        // (adopt_mcp_tool_args) is authoritative — arg normalization makes
        // (name, arguments) matching unreliable. Legacy fallback for calls
        // parked before any pairing: exact (name, arguments) first (the
        // model may call the same tool twice in one batch), then first
        // same-name.
        let take_match = |calls: &mut Vec<McpToolCall>| -> Option<McpToolCall> {
            let position = match &parked.mcp_request_id {
                Some(rid) => calls.iter().position(|c| &c.request_id == rid),
                None => calls
                    .iter()
                    .position(|c| c.name == parked.name && c.arguments == parked.arguments)
                    .or_else(|| calls.iter().position(|c| c.name == parked.name)),
            }?;
            Some(calls.remove(position))
        };
        let call = match take_match(&mut self.pending_mcp_calls) {
            Some(call) => call,
            None => {
                // Buffered lines are older than anything on the channel —
                // scan them first (a tools/call can be buffered by a
                // control_request wait), re-queueing conversation lines.
                let buffered: Vec<Value> = self.io.buffer.drain(..).collect();
                for line in buffered {
                    if !self.handle_mcp_control(&line).await? {
                        self.io.buffer.push_back(line);
                    }
                }
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    if let Some(call) = take_match(&mut self.pending_mcp_calls) {
                        break call;
                    }
                    let line = tokio::select! {
                        line = self.io.recv_channel() => line.ok_or_else(|| {
                            "codebuddy CLI exited while a tool call was parked".to_string()
                        })?,
                        _ = tokio::time::sleep_until(deadline) => {
                            return Err(format!(
                                "tool result for {} arrived before the CLI's MCP request",
                                result.tool_call_id
                            ));
                        }
                    };
                    // MCP traffic is served inline (a tools/call stashes
                    // itself); conversation lines (stale echoes) belong to
                    // the event loop.
                    if !self.handle_mcp_control(&line).await? {
                        self.io.buffer.push_back(line);
                    }
                }
            }
        };
        let (content, is_error) = if result.is_error {
            let text = result
                .content
                .iter()
                .filter_map(|b| match b {
                    InputContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (
                vec![
                    json!({"type":"text","text":if text.is_empty() { "tool failed".into() } else { text }}),
                ],
                true,
            )
        } else {
            let content: Vec<Value> = result
                .content
                .iter()
                .map(|block| match block {
                    InputContentBlock::Text { text, .. } => {
                        json!({"type":"text","text":text})
                    }
                    InputContentBlock::Image { data, mime_type } => {
                        json!({"type":"image","data":data,"mimeType":mime_type})
                    }
                })
                .collect();
            (content, false)
        };
        self.io
            .mcp_control_response(
                &call.request_id,
                json!({
                    "jsonrpc": "2.0",
                    "id": call.mcp_id,
                    "result": { "content": content, "isError": is_error },
                }),
            )
            .await
    }

    /// Divergence recovery: rebuild the session from tack-ai's context.
    /// Prefers the native path — rewrite CodeBuddy's own session JSONL and
    /// respawn with `--resume` (multi-turn structure + CLI cache survive,
    /// reference: cb-session-io) — and falls back to the flattened
    /// transcript replay when the JSONL path fails (format drift, io).
    async fn respawn(&mut self, context: &Context) -> Result<(), String> {
        match self.respawn_native(context).await {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::warn!(
                    "codebuddy: JSONL rebuild failed ({error}); falling back to transcript replay"
                );
                self.respawn_transcript(context).await
            }
        }
    }

    /// Native rebuild: write tack-ai's history into CodeBuddy's session
    /// store, kill the CLI, respawn with `--resume <session-id>`.
    async fn respawn_native(&mut self, context: &Context) -> Result<(), String> {
        let rotate = self.rotate_on_rebuild;
        if rotate {
            tracing::debug!("codebuddy: rotating to fresh session id (post-abort)");
        }
        let fresh = CodeBuddySession::rebuild_native(
            &self.key.clone(),
            &self.model_id.clone(),
            self.system_prompt.as_deref(),
            self.effort.as_deref(),
            self.cli_session_id.as_deref(),
            rotate,
            context,
        )
        .await?;
        kill_tree(&self.child);
        let _ = self.child.kill().await;
        *self = fresh;
        Ok(())
    }

    /// Native rebuild: write tack-ai's history into CodeBuddy's session
    /// store, spawn the CLI with `--resume <session-id>`, and return the
    /// fresh session — its synced prefix covers the settled history; the
    /// unresolved tail is delivered by the caller's sync.
    async fn rebuild_native(
        key: &str,
        model_id: &str,
        system_prompt: Option<&str>,
        effort: Option<&str>,
        cli_session_id: Option<&str>,
        rotate: bool,
        context: &Context,
    ) -> Result<CodeBuddySession, String> {
        use crate::codebuddy_jsonl as jsonl;
        let cwd = std::env::current_dir()
            .map_err(|e| format!("cwd unavailable: {e}"))?
            .to_string_lossy()
            .to_string();
        // Reuse the live CLI's session id (its session file is ours to
        // rewrite). After an abort, rotate to a fresh id — the killed CLI
        // may still be flushing orphan records to the old file (reference:
        // forceRotate). With no live id (fresh tack process), reuse the
        // PERSISTED id from the previous process so the rebuild lands on
        // the same CodeBuddy session file (cross-process resume).
        let session_id = match (cli_session_id, rotate) {
            (Some(id), false) => id.to_string(),
            (None, false) => read_cb_session_id(key).unwrap_or_else(jsonl::new_uuid),
            (_, true) => jsonl::new_uuid(),
        };
        // Rebuild history up to the last assistant message; the trailing
        // unresolved turn is delivered NATIVELY by sync after the resume
        // (reference: resume session + fresh prompt). Tool results in the
        // tail can't be delivered — a fresh CLI has no parked calls for
        // them — and an empty tail would leave the CLI with nothing to
        // answer; both go down the transcript fallback.
        let split = context
            .messages
            .iter()
            .rposition(|m| matches!(m, Message::Assistant(_)))
            .map(|i| i + 1)
            .unwrap_or(0);
        let tail = &context.messages[split..];
        if tail.is_empty() || tail.iter().any(|m| matches!(m, Message::ToolResult(_))) {
            return Err("unresolved tail is not a plain user turn; transcript fallback".into());
        }
        let records = jsonl::pi_to_cb_records(&context.messages[..split]);
        let path = jsonl::write_session_jsonl(&session_id, &cwd, &records)?;
        let warnings = jsonl::verify_written_session(&path, &session_id, records.len());
        for warning in &warnings {
            tracing::warn!("codebuddy: session verify: {warning}");
        }
        if warnings
            .iter()
            .any(|w| w.starts_with("file unreadable") || w.starts_with("record count"))
        {
            return Err(format!("session verify failed: {}", warnings.join("; ")));
        }
        let mut fresh =
            CodeBuddySession::spawn_new(key, model_id, system_prompt, effort, Some(&session_id))
                .await?;
        // Only the rebuilt prefix counts as synced — sync delivers the
        // unresolved tail to the resumed CLI as fresh input.
        fresh.synced = context.messages[..split]
            .iter()
            .map(message_fingerprint)
            .collect();
        tracing::debug!(
            "codebuddy: native rebuild complete (session {session_id}, {} records, {} tail)",
            records.len(),
            tail.len()
        );
        Ok(fresh)
    }

    /// Transcript fallback: kill the CLI, respawn fresh, replay a
    /// flattened transcript so the new session starts from tack-ai's
    /// context. Loses CLI-side cache and tool structure but always works.
    async fn respawn_transcript(&mut self, context: &Context) -> Result<(), String> {
        kill_tree(&self.child);
        let _ = self.child.kill().await;
        let effort = self.effort.clone();
        let key = self.key.clone();
        let fresh = CodeBuddySession::spawn_new(
            &key,
            &self.model_id.clone(),
            self.system_prompt.as_deref(),
            effort.as_deref(),
            None,
        )
        .await?;
        *self = fresh;
        let mut transcript = String::from(
            "[conversation history replayed after a context change; continue from here]\n\n",
        );
        // A deferred system prompt (Windows argv limits) rides the replay.
        if let Some(prompt) = self.pending_system_prompt.take() {
            transcript = format!(
                "[session system instructions]\n\n{prompt}\n\n[end of session system instructions]\n\n{transcript}"
            );
        }
        for message in &context.messages {
            match message {
                Message::User(user) => {
                    transcript.push_str("## User\n");
                    transcript.push_str(&user_text(&user.content));
                    transcript.push_str("\n\n");
                }
                Message::Assistant(assistant) => {
                    transcript.push_str("## Assistant\n");
                    for block in &assistant.content {
                        match block {
                            ContentBlock::Text { text, .. } => transcript.push_str(text),
                            ContentBlock::Thinking { thinking, .. } => {
                                transcript.push_str(thinking)
                            }
                            ContentBlock::ToolCall {
                                name, arguments, ..
                            } => {
                                transcript.push_str(&format!(
                                    "\n[called tool {name} with {arguments}]\n"
                                ));
                            }
                            _ => {}
                        }
                    }
                    transcript.push_str("\n\n");
                }
                Message::ToolResult(result) => {
                    transcript.push_str("## Tool result\n");
                    for block in &result.content {
                        if let InputContentBlock::Text { text, .. } = block {
                            transcript.push_str(text);
                        }
                    }
                    transcript.push_str("\n\n");
                }
                Message::System(system) => {
                    let text = match &system.content {
                        UserContent::Text(text) => text.clone(),
                        UserContent::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|b| match b {
                                InputContentBlock::Text { text, .. } => Some(text.as_str()),
                                InputContentBlock::Image { .. } => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    if !text.is_empty() {
                        transcript.push_str("## System\n");
                        transcript.push_str(&text);
                        transcript.push_str("\n\n");
                    }
                }
            }
        }
        self.io
            .send(&json!({
                "type": "user",
                "message": { "role": "user", "content": [{"type":"text","text":transcript}] },
                "parent_tool_use_id": Value::Null,
            }))
            .await?;
        self.synced = context.messages.iter().map(message_fingerprint).collect();
        Ok(())
    }
}

fn user_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                InputContentBlock::Text { text, .. } => text.clone(),
                InputContentBlock::Image { .. } => "[image]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_content_to_cli(content: &UserContent) -> Vec<Value> {
    match content {
        UserContent::Text(text) => vec![json!({"type":"text","text":text})],
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                InputContentBlock::Text { text, .. } => {
                    json!({"type":"text","text":text})
                }
                InputContentBlock::Image { data, mime_type } => {
                    json!({"type":"image","source":{"type":"base64","media_type":mime_type,"data":data}})
                }
            })
            .collect(),
    }
}

/// Get or spawn the session for a key. System-prompt / effort changes
/// respawn (spawn-time-only); a model-only change hot-switches via the
/// set_model control request (respawn as fallback).
async fn get_or_spawn(
    key: &str,
    model: &Model,
    system_prompt: Option<&str>,
    effort: Option<&str>,
) -> Result<Arc<Mutex<CodeBuddySession>>, String> {
    let existing = sessions().lock().await.get(key).cloned();
    // Reap idle sessions while we're here (registry is otherwise
    // append-only; each entry holds a CLI child + MCP bridge).
    {
        let mut registry = sessions().lock().await;
        let idle: Vec<String> = registry
            .iter()
            .filter(|(k, s)| {
                *k != key
                    && s.try_lock()
                        .map(|s| s.last_used.elapsed() > SESSION_IDLE_EVICT)
                        .unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        // Remove under the registry lock, but lock+kill AFTER releasing
        // it: a session busy in `drive` would otherwise stall every
        // sessions() user behind the registry guard for a whole turn.
        let mut evicted = Vec::new();
        for idle_key in idle {
            tracing::debug!("codebuddy: evicting idle session {idle_key}");
            if let Some(session) = registry.remove(&idle_key) {
                evicted.push(session);
            }
        }
        drop(registry);
        for session in evicted {
            let mut session = session.lock().await;
            kill_tree(&session.child);
            let _ = session.child.start_kill();
        }
    }
    if let Some(session) = existing {
        let mut guard = session.lock().await;
        let respawn_stale =
            guard.system_prompt.as_deref() != system_prompt || guard.effort.as_deref() != effort;
        if !respawn_stale {
            if guard.model_id == model.id {
                return Ok(session.clone());
            }
            match guard.set_model(&model.id).await {
                Ok(()) => return Ok(session.clone()),
                Err(error) => {
                    tracing::warn!("codebuddy: set_model failed ({error}); rebuilding session");
                }
            }
        }
        // Rebuild path: kill the CLI, adopt the new spawn parameters, and
        // let drive() rebuild natively (JSONL + --resume) — no wasted
        // fresh spawn on top of a doomed one.
        kill_tree(&guard.child);
        let _ = guard.child.kill().await;
        guard.system_prompt = system_prompt.map(str::to_string);
        guard.effort = effort.map(str::to_string);
        guard.model_id = model.id.to_string();
        guard.needs_respawn = true;
        return Ok(session.clone());
    }
    let session = CodeBuddySession::spawn_new(key, &model.id, system_prompt, effort, None).await?;
    let session = Arc::new(Mutex::new(session));
    sessions()
        .lock()
        .await
        .insert(key.to_string(), session.clone());
    Ok(session)
}

impl Drop for CodeBuddySession {
    fn drop(&mut self) {
        kill_tree(&self.child);
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod mcp_schema_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    /// Real capture shape (CLI 2.156.0): schemars 1.x output loses its
    /// meta declarations, `format`, `title`, and null unions.
    #[test]
    fn strips_meta_and_collapses_null_unions() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "BashParams",
            "type": "object",
            "properties": {
                "command": {"description": "Bash command", "type": "string"},
                "timeout": {
                    "description": "Timeout",
                    "format": "double",
                    "type": ["number", "null"]
                },
                "limit": {"format": "uint", "minimum": 0, "type": ["integer", "null"]},
            },
            "required": ["command"],
        });
        let out = sanitize_mcp_schema(&schema);
        assert_eq!(out.get("$schema"), None);
        assert_eq!(out.get("title"), None);
        assert_eq!(out["type"], "object");
        assert_eq!(out["required"], json!(["command"]));
        assert_eq!(out["properties"]["command"]["type"], "string");
        // Union collapsed, format gone, siblings (minimum) preserved.
        assert_eq!(
            out["properties"]["timeout"],
            json!({"description": "Timeout", "type": "number"})
        );
        assert_eq!(
            out["properties"]["limit"],
            json!({"minimum": 0, "type": "integer"})
        );
    }

    /// The edit tool's shape: `$defs` + `$ref` inlined, siblings merged,
    /// recursion inside the $def sanitized too.
    #[test]
    fn inlines_local_defs_refs() {
        let schema = json!({
            "$defs": {
                "EditEntry": {
                    "type": "object",
                    "properties": {
                        "oldText": {"type": "string"},
                        "flags": {"type": ["string", "null"], "format": "uint"}
                    },
                    "required": ["oldText"]
                }
            },
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "edits": {
                    "description": "edits to apply",
                    "items": {"$ref": "#/$defs/EditEntry", "description": "one edit"},
                    "type": "array"
                }
            },
            "required": ["edits"]
        });
        let out = sanitize_mcp_schema(&schema);
        assert_eq!(out.get("$defs"), None);
        let items = &out["properties"]["edits"]["items"];
        assert_eq!(items["type"], "object");
        assert_eq!(items["description"], "one edit");
        assert_eq!(items["properties"]["oldText"]["type"], "string");
        assert_eq!(items["properties"]["flags"], json!({"type": "string"}));
        assert_eq!(items["required"], json!(["oldText"]));
    }

    /// A multi-type union that is NOT just T|null stays untouched (we
    /// don't guess), and non-local refs pass through.
    #[test]
    fn leaves_real_unions_and_remote_refs_alone() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {"type": ["string", "number"]},
                "ext": {"$ref": "https://example.com/x.json"}
            }
        });
        let out = sanitize_mcp_schema(&schema);
        assert_eq!(
            out["properties"]["value"]["type"],
            json!(["string", "number"])
        );
        assert_eq!(
            out["properties"]["ext"]["$ref"],
            "https://example.com/x.json"
        );
    }
}

#[cfg(test)]
mod alignment_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::collections::BTreeMap;

    /// Reference: REASONING_TO_EFFORT — minimal/low→low, medium→medium,
    /// high→high, xhigh/max→xhigh; a model thinkingLevelMap entry wins.
    #[test]
    fn effort_mapping_matches_reference() {
        let model = codebuddy_model("hy3-preview", "test");
        assert_eq!(effort_for(&model, None), None);
        assert_eq!(
            effort_for(&model, Some(ThinkingLevel::Minimal)).as_deref(),
            Some("low")
        );
        assert_eq!(
            effort_for(&model, Some(ThinkingLevel::Low)).as_deref(),
            Some("low")
        );
        assert_eq!(
            effort_for(&model, Some(ThinkingLevel::Medium)).as_deref(),
            Some("medium")
        );
        assert_eq!(
            effort_for(&model, Some(ThinkingLevel::High)).as_deref(),
            Some("high")
        );
        assert_eq!(
            effort_for(&model, Some(ThinkingLevel::Xhigh)).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            effort_for(&model, Some(ThinkingLevel::Max)).as_deref(),
            Some("xhigh")
        );

        // thinkingLevelMap overrides the generic table; an explicit null
        // disables effort for that level.
        let mut mapped = codebuddy_model("hy3-preview", "test");
        mapped.thinking_level_map = Some(BTreeMap::from([
            ("high".to_string(), Some("max".to_string())),
            ("low".to_string(), None),
        ]));
        assert_eq!(
            effort_for(&mapped, Some(ThinkingLevel::High)).as_deref(),
            Some("max")
        );
        assert_eq!(effort_for(&mapped, Some(ThinkingLevel::Low)), None);
    }

    /// Reference: SDK_KEY_RENAMES — Claude-Code-shaped keys normalize to
    /// pi's parameter names; first alias wins; bash gets a 120s default.
    #[test]
    fn tool_args_are_normalized() {
        assert_eq!(
            map_tool_args("read", json!({"file_path": "/a"})),
            json!({"path": "/a"})
        );
        assert_eq!(
            map_tool_args(
                "edit",
                json!({"file_path": "/a", "old_string": "x", "new_string": "y"})
            ),
            json!({"path": "/a", "oldText": "x", "newText": "y"})
        );
        // First alias wins when both forms are present.
        assert_eq!(
            map_tool_args("read", json!({"path": "/keep", "file_path": "/drop"})),
            json!({"path": "/keep"})
        );
        // bash default timeout; explicit values pass through.
        assert_eq!(
            map_tool_args("bash", json!({"command": "ls"})),
            json!({"command": "ls", "timeout": 120})
        );
        assert_eq!(
            map_tool_args("bash", json!({"command": "ls", "timeout": 5})),
            json!({"command": "ls", "timeout": 5})
        );
        // Unknown tools pass through untouched.
        assert_eq!(
            map_tool_args("echo", json!({"text": "hi"})),
            json!({"text": "hi"})
        );
        // Non-object arguments survive as-is.
        assert_eq!(map_tool_args("bash", Value::Null), Value::Null);
    }

    /// Reference: mapStopReason — tool_use→ToolUse, max_tokens→Length,
    /// everything else→Stop.
    #[test]
    fn cli_stop_reasons_map() {
        assert_eq!(cli_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(cli_stop_reason("max_tokens"), StopReason::Length);
        assert_eq!(cli_stop_reason("end_turn"), StopReason::Stop);
        assert_eq!(cli_stop_reason("whatever"), StopReason::Stop);
    }

    /// Reference: models.ts detectors — reasoning/images/context/maxTokens
    /// are estimated from the model id.
    #[test]
    fn model_metadata_estimates_match_reference() {
        let gemini = codebuddy_model("gemini-2.5-pro", "g");
        assert!(gemini.reasoning);
        assert_eq!(gemini.context_window, 1_048_576);
        assert_eq!(gemini.input.len(), 2);

        let claude = codebuddy_model("claude-opus-4", "c");
        assert!(claude.reasoning);
        assert_eq!(claude.context_window, 200_000);
        assert_eq!(claude.max_tokens, 8_192);

        let gpt = codebuddy_model("gpt-5-codex", "g");
        assert!(gpt.reasoning);
        assert_eq!(gpt.max_tokens, 16_384);

        let hunyuan = codebuddy_model("hy3-preview-agent-ioa", "h");
        assert!(hunyuan.reasoning);
        assert_eq!(hunyuan.context_window, 131_072);
        assert_eq!(hunyuan.input.len(), 1);

        let unknown = codebuddy_model("some-model", "s");
        assert!(!unknown.reasoning);
        assert_eq!(unknown.context_window, 131_072);
        assert_eq!(unknown.max_tokens, 8_192);
    }
    /// result.modelUsage carries the SERVED limits (issue #18 analog):
    /// adopt them over the id-based estimate; exact id match first, then
    /// substring; untouched models keep their values.
    #[test]
    fn served_limits_override_estimates() {
        let mut models = vec![
            codebuddy_model("hy3-preview-agent-ioa", "h"),
            codebuddy_model("gpt-5", "g"),
        ];
        assert_eq!(models[0].context_window, 131_072);
        let changed = apply_served_limits(
            &mut models,
            &json!({"hy3-preview-agent-ioa": {"contextWindow": 1048576, "maxOutputTokens": 32768}}),
        );
        assert!(changed);
        assert_eq!(models[0].context_window, 1_048_576);
        assert_eq!(models[0].max_tokens, 32_768);
        assert_eq!(models[1].context_window, 200_000, "untouched");

        // Same values again → no change.
        assert!(!apply_served_limits(
            &mut models,
            &json!({"hy3-preview-agent-ioa": {"contextWindow": 1048576}}),
        ));
        // Unknown served id → no match, no change.
        assert!(!apply_served_limits(
            &mut models,
            &json!({"totally-different": {"contextWindow": 1}}),
        ));
        // Substring match: a served id containing the registered id.
        assert!(apply_served_limits(
            &mut models,
            &json!({"gpt-5[1m]": {"contextWindow": 1048576}}),
        ));
        assert_eq!(models[1].context_window, 1_048_576);
        // Malformed payloads are ignored.
        assert!(!apply_served_limits(&mut models, &json!({"gpt-5": {}})));
        assert!(!apply_served_limits(&mut models, &json!("nope")));
    }
}

#[cfg(test)]
mod prompt_split_tests {
    use super::*;

    /// Short prompts stay on argv; over-limit prompts defer to stdin
    /// injection (Windows budget only — unix ARG_MAX never defers).
    #[test]
    fn split_system_prompt_respects_argv_budget() {
        assert_eq!(split_system_prompt(None), (None, None));
        assert_eq!(split_system_prompt(Some("short")), (Some("short"), None));
        // Windows has a finite argv budget: an over-limit prompt defers.
        // Unix's budget is usize::MAX (never defers) — a repeated-string
        // test cannot reach it, so assert the boundary directly there.
        #[cfg(windows)]
        {
            let long = "x".repeat(MAX_ARGV_SYSTEM_PROMPT_CHARS + 1);
            let (argv, stdin) = split_system_prompt(Some(&long));
            assert!(argv.is_none());
            assert_eq!(stdin.as_deref(), Some(long.as_str()));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(MAX_ARGV_SYSTEM_PROMPT_CHARS, usize::MAX);
        }
    }
}

#[cfg(test)]
mod cli_spawn_tests {
    #![allow(clippy::unwrap_used, unsafe_code)]
    use super::*;

    /// Probe script path inside the crate's test fixtures.
    #[cfg(windows)]
    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[cfg(windows)]
    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// `where codebuddy` lists every npm shim; the picker must choose a
    /// spawnable one (.exe > .cmd/.bat), never the bare script or .ps1.
    #[test]
    fn windows_shim_picker_prefers_spawnable_candidates() {
        let npm = r"C:\Users\me\AppData\Roaming\npm\codebuddy
C:\Users\me\AppData\Roaming\npm\codebuddy.cmd
C:\Users\me\AppData\Roaming\npm\codebuddy.ps1";
        assert_eq!(
            pick_windows_shim(npm.lines()).unwrap(),
            r"C:\Users\me\AppData\Roaming\npm\codebuddy.cmd"
        );
        let exe = r"C:\tools\codebuddy.exe";
        assert_eq!(pick_windows_shim(exe.lines()).unwrap(), exe);
        assert!(pick_windows_shim(r"C:\npm\codebuddy.ps1".lines()).is_none());
        assert!(pick_windows_shim(r"/usr/local/bin/codebuddy".lines()).is_none());
    }

    /// Windows: npm installs the CLI as a `.cmd` batch shim, which
    /// CreateProcess cannot run directly — spawn_cli must go through
    /// `cmd /c`. Verify with a shim wrapping the mock and a full handshake.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_cli_runs_cmd_shim() {
        if !python3_available() {
            eprintln!("python3 unavailable; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("tack-cmdshim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("mock-codebuddy.cmd");
        std::fs::write(
            &shim,
            format!(
                "@echo off\r\npython3 \"{}\" %*\r\n",
                fixture("mock_codebuddy.py")
            ),
        )
        .unwrap();
        let mut child = spawn_cli(&shim, "default", None, None, None).unwrap();
        let mut io = CliIo::new(&mut child).unwrap();
        let response = io.initialize(true).await.unwrap();
        assert!(response.get("response").is_some(), "{response}");
        kill_tree(&child);
        let _ = child.kill().await;
    }
}

// ===========================================================================
// One-shot delegation (AskCodebuddy parity: promptAndWait)
// ===========================================================================

/// Delegation mode (reference: MODE_DISALLOWED_TOOLS). Controls which of
/// CodeBuddy's OWN built-in tools the delegated call may use — there is no
/// MCP bridge here; tack tools are not involved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskMode {
    /// Questions about the codebase: read/grep/glob allowed, no writes.
    Read,
    /// Full autonomy: writes and bash allowed (careful).
    Full,
    /// General knowledge only: no file access at all.
    None,
}

/// Tools blocked in every mode (reference: ASKCLAUDE_ALWAYS_BLOCKED) —
/// they cannot work without a pi TUI / harness.
const ASK_ALWAYS_BLOCKED: [&str; 5] = [
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "ToolSearch",
    "ScheduleWakeup",
];

/// `--disallowedTools` per mode (reference: MODE_DISALLOWED_TOOLS).
fn disallowed_tools(mode: AskMode) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = ASK_ALWAYS_BLOCKED.to_vec();
    match mode {
        AskMode::Full => {}
        AskMode::Read => out.extend([
            "Write",
            "Edit",
            "Bash",
            "NotebookEdit",
            "EnterWorktree",
            "ExitWorktree",
            "CronCreate",
            "CronDelete",
            "TeamCreate",
            "TeamDelete",
        ]),
        AskMode::None => out.extend([
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "Bash",
            "Agent",
            "NotebookEdit",
            "EnterWorktree",
            "ExitWorktree",
            "CronCreate",
            "CronDelete",
            "TeamCreate",
            "TeamDelete",
            "WebFetch",
            "WebSearch",
        ]),
    }
    out
}

/// Outcome of a one-shot delegation.
#[derive(Clone, Debug)]
pub struct AskOutcome {
    pub text: String,
    /// Tool names the delegated call used (for the actions summary).
    pub tool_uses: Vec<String>,
}

/// Spawn the CLI for a one-shot delegation (reference: promptAndWait's
/// query options): no MCP bridge, CodeBuddy uses its own tools;
/// `settingSources user,project` (unlike the provider session, the
/// delegated call is allowed to see the user's CodeBuddy setup).
fn spawn_ask_cli(
    cli: &PathBuf,
    model: Option<&str>,
    effort: Option<&str>,
    disallowed: &[&str],
) -> Result<tokio::process::Child, String> {
    let ext = cli
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let mut command = match ext.as_deref() {
        Some("py") => {
            let mut c = tokio::process::Command::new("python3");
            c.arg(cli);
            c
        }
        Some("cmd" | "bat") => {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/c").arg(cli);
            c
        }
        _ => tokio::process::Command::new(cli),
    };
    command
        .arg("-p")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages")
        .arg("--permission-mode")
        .arg("bypassPermissions")
        .arg("--strict-mcp-config")
        .arg("--setting-sources")
        .arg("user,project")
        .env("DISABLE_AUTO_COMPACT", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if std::env::var_os("CODEBUDDY_CODE_ENTRYPOINT").is_none() {
        command.env("CODEBUDDY_CODE_ENTRYPOINT", "sdk-rs");
    }
    if !disallowed.is_empty() {
        command.arg("--disallowedTools");
        for tool in disallowed {
            command.arg(tool);
        }
    }
    if let Some(model) = model {
        command.arg("--model").arg(model);
    }
    if let Some(effort) = effort {
        command.arg("--effort").arg(effort);
    }
    command
        .spawn()
        .map_err(|e| format!("failed to spawn codebuddy CLI ({}): {e}", cli.display()))
}

/// One-shot delegation (reference: AskCodebuddy / promptAndWait): spawn a
/// fresh CLI session, send one prompt, collect the answer, kill. The
/// delegated call sees ONLY this prompt (clean session) — make it
/// self-contained. `on_text` receives the accumulated answer as it
/// streams (progress reporting); a hard 10-minute cap bounds the call.
pub async fn ask_codebuddy(
    prompt: &str,
    mode: AskMode,
    model: Option<&str>,
    effort: Option<&str>,
    cancel: tokio_util::sync::CancellationToken,
    on_text: &(dyn Fn(&str) + Send + Sync),
) -> Result<AskOutcome, String> {
    let cli = cli_path().ok_or_else(|| {
        "codebuddy CLI not found — install @tencent-ai/codebuddy-code or set CODEBUDDY_PATH"
            .to_string()
    })?;
    let disallowed = disallowed_tools(mode);
    let mut child = spawn_ask_cli(&cli, model, effort, &disallowed)?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        ask_drive(&mut child, prompt, &cancel, on_text),
    )
    .await;
    kill_tree(&child);
    let _ = child.kill().await;
    match result {
        Ok(outcome) => outcome,
        Err(_) => Err("codebuddy delegation timed out after 600s".to_string()),
    }
}

async fn ask_drive(
    child: &mut tokio::process::Child,
    prompt: &str,
    cancel: &tokio_util::sync::CancellationToken,
    on_text: &(dyn Fn(&str) + Send + Sync),
) -> Result<AskOutcome, String> {
    let mut io = CliIo::new(child)?;
    io.initialize(false).await?;
    io.send(&json!({
        "type": "user",
        "message": { "role": "user", "content": [{"type": "text", "text": prompt}] },
        "parent_tool_use_id": Value::Null,
    }))
    .await?;
    let mut text = String::new();
    let mut tool_uses: Vec<String> = Vec::new();
    let mut saw_stream_event = false;
    loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = io.send(&json!({
                    "type": "control_request",
                    "request_id": "cancel-0",
                    "request": { "subtype": "interrupt" },
                })).await;
                return Err("Operation aborted".to_string());
            }
            message = io.next_line() => {
                message.ok_or_else(|| "codebuddy CLI exited mid-delegation".to_string())?
            }
        };
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
            "stream_event" => {
                saw_stream_event = true;
                let event = message.get("event").cloned().unwrap_or(Value::Null);
                match event.get("type").and_then(Value::as_str) {
                    Some("content_block_delta") => {
                        if event.pointer("/delta/type").and_then(Value::as_str)
                            == Some("text_delta")
                            && let Some(delta) =
                                event.pointer("/delta/text").and_then(Value::as_str)
                        {
                            text.push_str(delta);
                            on_text(&text);
                        }
                    }
                    Some("content_block_start") => {
                        if event.pointer("/content_block/type").and_then(Value::as_str)
                            == Some("tool_use")
                            && let Some(name) =
                                event.pointer("/content_block/name").and_then(Value::as_str)
                        {
                            tool_uses.push(name.to_string());
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                // Fallback when the CLI sends no stream_events; also the
                // source of completed tool_use inputs for the summary.
                let blocks = message
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for block in &blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") if !saw_stream_event => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                                on_text(&text);
                            }
                        }
                        Some("tool_use") if !saw_stream_event => {
                            if let Some(name) = block.get("name").and_then(Value::as_str) {
                                tool_uses.push(name.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                let is_error = message
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    return Err(result_error_text(&message));
                }
                // Last-resort content when nothing streamed.
                if text.is_empty()
                    && let Some(result_text) = message.get("result").and_then(Value::as_str)
                {
                    text = result_text.to_string();
                }
                return Ok(AskOutcome { text, tool_uses });
            }
            _ => {}
        }
    }
}
