//! RPC mode: JSONL commands on stdin, JSONL responses + events on stdout.
//! Wire-compatible with TS pi's `pi --mode rpc` (see
//! `packages/coding-agent/src/modes/rpc/rpc-types.ts`).

mod events;
mod mcp;
mod permission;
mod prompt;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use tack_agent_core::AgentMessage;
use tack_ai::Provider;
use tack_session::SessionManager;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::settings::Settings;
use events::save_nested_bool;
use permission::{EventSink, RpcPermissionState};
use prompt::{run_bash_command, run_compact_command, spawn_prompt};

pub use events::event_to_json;

/// Shared RPC session state behind a lock (commands arrive while prompts run).
pub(crate) struct RpcState {
    pub(crate) session: SessionManager,
    pub(crate) model: tack_ai::Model,
    pub(crate) thinking: Option<tack_ai::ThinkingLevel>,
    pub(crate) steering: std::collections::VecDeque<String>,
    pub(crate) follow_up: std::collections::VecDeque<String>,
    pub(crate) steering_mode: Option<String>,
    pub(crate) follow_up_mode: Option<String>,
    pub(crate) cancel: tokio_util::sync::CancellationToken,
    /// Cancellation scope for `bash` RPC commands only (TS abortBash
    /// semantics: abort_bash must not kill an in-flight prompt run).
    pub(crate) bash_cancel: tokio_util::sync::CancellationToken,
    /// Retry-scope cancellation (TS abortRetry): aborts an in-progress
    /// provider retry backoff without killing the prompt run.
    pub(crate) retry_cancel: tokio_util::sync::CancellationToken,
    pub(crate) is_streaming: bool,
    /// A compaction LLM call is in flight (started, result not yet persisted
    /// or discarded). Tree navigation is rejected while set — the compaction
    /// entry must land on the leaf it was computed from (TS pi #9178).
    pub(crate) is_compacting: bool,
    /// Run generation: bumped on every prompt spawn and every session swap
    /// (new_session/clone/switch_session). The event-forwarder task only
    /// persists messages / clears is_streaming when its generation is still
    /// current, so a cancelled run's trailing events never land in the
    /// replacement session nor clobber a newer run's streaming flag.
    pub(crate) generation: u64,
    /// Background bash tasks persist across prompts in one RPC session.
    pub(crate) background: tack_tools::background::BackgroundTaskManager,
    /// LSP registry (language servers stay warm across prompts).
    pub(crate) lsp: tack_tools::lsp::LspManager,
    /// tack-ext plugins (process + wasm carriers): one manager per RPC
    /// process, tools/hooks are re-collected per prompt run.
    pub(crate) extensions: Arc<Mutex<crate::extension_host::ExtensionManager>>,
    /// Active provider adapter (startup wrapping: Retrying inside ExtNotify).
    /// Rebound by set_model when the api kind changes — otherwise a
    /// cross-api switch streams the new model through the startup adapter,
    /// which then reports "No API key for provider: <model.provider>".
    pub(crate) provider: Arc<dyn Provider>,
    /// Request-time auth source; re-resolved by set_model when the provider
    /// id changes (kept on same-provider switches so an explicit --api-key
    /// survives).
    pub(crate) auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    /// MCP connection pool reused across prompt runs (F08): previously
    /// every prompt reconnected every configured server and
    /// `mem::forget`'ed the connections, so server child processes
    /// accumulated linearly with the prompt count. Replacing or clearing
    /// the cache drops the connections, which cancels their rmcp
    /// services and kills the server processes.
    pub(crate) mcp_connections: Option<McpConnectionCache>,
    /// Session-scoped MCP servers injected by the host via
    /// `set_mcp_servers` (ZCode createSession.mcpServers passthrough):
    /// merged over file-configured servers by name at pool-build time.
    pub(crate) session_mcp_servers: Vec<tack_tools::mcp::McpServerSpec>,
    /// Session-scoped hooks injected by the host via `set_hooks` (ZCode
    /// workspace-hooks passthrough): merged AFTER settings/bundle hooks at
    /// each prompt run (per-event groups appended).
    pub(crate) session_hooks: crate::shell_hooks::HookConfig,
    /// additionalContext collected from SessionStart hooks; appended to the
    /// system prompt of every subsequent run in this session.
    pub(crate) session_hook_context: Option<String>,
    /// SessionStart hooks fire lazily at the first prompt of a session so a
    /// host-injected `set_hooks` (sent right after spawn) is included.
    pub(crate) session_start_hook_pending: bool,
    /// `source` field for the next SessionStart hook input
    /// (startup|resume|clear, ZCode/Claude activation-source semantics).
    pub(crate) session_start_hook_source: String,
    /// Stop-hook continuation guard (Claude: honored at most once per stop
    /// point). Reset on every fresh user prompt.
    pub(crate) stop_hook_active: bool,
    /// Interactive permission state (set_mode ask|acceptEdits|plan|bypass).
    pub(crate) permission: Arc<Mutex<RpcPermissionState>>,
    /// Out-of-band JSONL event sink (permission_request/resolved frames).
    pub(crate) event_sink: EventSink,
    /// Session-owned sub-agent budget/concurrency coordination (shared by
    /// every prompt's tool set and every background child).
    pub(crate) subagent_limits: Arc<crate::subagent_tool::SubagentLimits>,
}

/// Live MCP server connections plus the fingerprint of the spec set they
/// were built from (see `mcp_cache_fingerprint`); a fingerprint mismatch
/// rebuilds the pool.
pub(crate) struct McpConnectionCache {
    pub(crate) fingerprint: String,
    pub(crate) connections: Vec<Arc<tack_tools::mcp::McpConnection>>,
}

/// Cache key for the MCP connection pool: the connections are reused
/// across prompts as long as the effective server set (config file +
/// extension contributions), the cwd/agent_dir they were resolved from,
/// and the sampling model stay the same. The model is part of the key
/// because the sampling callback captures provider/model/auth at connect
/// time.
pub(crate) fn mcp_cache_fingerprint(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    specs: &[tack_tools::mcp::McpServerSpec],
    model: &tack_ai::Model,
) -> String {
    // McpServerSpec is neither Hash nor Serialize; its Debug covers every
    // field (command/args/env/cwd/url/headers/oauth), which is exactly
    // the connection-affecting state.
    format!(
        "{}|{}|{}/{}|{}|{specs:#?}",
        cwd.display(),
        agent_dir.display(),
        model.provider,
        model.id,
        model.api,
    )
}

impl std::fmt::Debug for RpcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcState").finish_non_exhaustive()
    }
}

fn success(id: Option<&str>, command: &str, data: Option<Value>) -> Value {
    match data {
        Some(data) => {
            json!({ "id": id, "type": "response", "command": command, "success": true, "data": data })
        }
        None => json!({ "id": id, "type": "response", "command": command, "success": true }),
    }
}

fn error(id: Option<&str>, command: &str, message: impl ToString) -> Value {
    json!({ "id": id, "type": "response", "command": command, "success": false, "error": message.to_string() })
}

/// Hard cap on one stdin JSONL line (matches tack-ext's plugin line cap and
/// tack-protocol's 16MiB frame cap). `BufReader::lines()` has no limit: a
/// client that streams without ever emitting '\n' would grow the read
/// buffer in one unbounded allocation until OOM. Over-cap lines are drained
/// through their newline, reported as an error response, and skipped.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

fn line_too_long() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("rpc stdin line exceeds {} byte cap", MAX_LINE_BYTES),
    )
}

/// Read one '\n'-terminated line with a hard size cap (same approach as
/// tack-ext's read_line_bounded, which is crate-private). Returns Ok(None) on
/// clean EOF, Ok(Some(line)) for a line, Err(InvalidData) for an over-cap
/// line (its remainder is drained so the next command starts clean), Err
/// on IO error.
async fn read_line_bounded<R>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    buf.clear();
    let mut over_cap = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // EOF: a partial line without terminator is still delivered
            // (matches Lines::next_line).
            if over_cap {
                return Err(line_too_long());
            }
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(buf).into_owned()))
            };
        }
        let (take, found) = match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (chunk.len(), false),
        };
        let content = &chunk[..take - usize::from(found)];
        if !over_cap && buf.len() + content.len() <= MAX_LINE_BYTES {
            buf.extend_from_slice(content);
        } else {
            // Over the cap: discard the rest of this line (through its
            // newline) so the next read starts on a fresh command.
            over_cap = true;
        }
        reader.consume(take);
        if found {
            if over_cap {
                buf.clear();
                return Err(line_too_long());
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(String::from_utf8_lossy(buf).into_owned()));
        }
    }
}

/// Entry point for `tack rpc`.
pub async fn run_rpc(
    model: tack_ai::Model,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    thinking: Option<tack_ai::ThinkingLevel>,
    cwd: PathBuf,
    continue_session: bool,
) -> Result<()> {
    let agent_dir = tack_session::default_agent_dir();
    let settings = Settings::load(&cwd, &agent_dir);

    let provider: Arc<dyn Provider> = tack_ai::provider_for(&model)
        .with_context(|| format!("no adapter for api kind {}", model.api))?;
    let provider: Arc<dyn Provider> = Arc::new(tack_ai::retry::RetryingProvider {
        inner: provider,
        policy: settings.retry.policy(),
        on_retry_scheduled: None,
    });

    // tack-ext plugins in headless mode: tools, intercepts, lifecycle events
    // and trust-gated exec stay live; UI dialogs degrade (ext_headless).
    let bridge_state = crate::ext_provider_bridge::ProviderBridgeState::shared();
    let extensions = crate::extension_host::ExtensionManager::load(
        &cwd,
        &agent_dir,
        "rpc",
        crate::ext_headless::HeadlessExtServices::new(
            "rpc",
            crate::project_trust::is_trusted(&cwd, &agent_dir),
            bridge_state.clone(),
        ),
        settings.extension_lock_required,
        crate::mcp_config::plugin_mcp_callbacks(
            &settings,
            crate::mcp_elicitation::InteractionMode::Headless,
            None,
        ),
        bridge_state,
    )
    .await;
    // Provider-boundary lifecycle events for subscribed plugins.
    let provider: Arc<dyn Provider> = Arc::new(crate::extension_host::ExtNotifyProvider::new(
        provider,
        extensions.clone_sink(),
    ));

    let mut session = if continue_session {
        SessionManager::continue_recent(&cwd, None).context("failed to continue session")?
    } else {
        SessionManager::create(&cwd, None).context("failed to create session")?
    };
    if continue_session {
        match session.repair_dangling_tool_calls() {
            Ok(0) => {}
            Ok(n) => tracing::info!("repaired {n} dangling tool call(s) from an interrupted run"),
            Err(e) => tracing::warn!("failed to repair dangling tool calls: {e}"),
        }
    }

    // Out-of-band JSONL events (permission prompts): one line per frame,
    // same atomic-write discipline as the response/event writers.
    let (event_sink, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(value) = event_rx.recv().await {
            if out
                .write_all(format!("{value}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
            let _ = out.flush().await;
        }
    });

    let state = Arc::new(Mutex::new(RpcState {
        session,
        model,
        thinking,
        steering: Default::default(),
        follow_up: Default::default(),
        steering_mode: None,
        follow_up_mode: None,
        cancel: tokio_util::sync::CancellationToken::new(),
        bash_cancel: tokio_util::sync::CancellationToken::new(),
        retry_cancel: tokio_util::sync::CancellationToken::new(),
        is_streaming: false,
        is_compacting: false,
        generation: 0,
        background: {
            let manager = tack_tools::background::BackgroundTaskManager::new();
            // Background task completion notifications -> out-of-band JSONL.
            let (notify_tx, mut notify_rx) =
                tokio::sync::mpsc::unbounded_channel::<tack_tools::background::TaskNotification>();
            manager.set_notify(notify_tx);
            let sink = event_sink.clone();
            tokio::spawn(async move {
                while let Some(notification) = notify_rx.recv().await {
                    let _ = sink.send(json!({
                        "type": "background_task",
                        "taskId": notification.task_id,
                        "command": notification.command,
                        "status": notification.status,
                    }));
                }
            });
            manager
        },
        lsp: settings.lsp_manager(&cwd),
        extensions: Arc::new(Mutex::new(extensions)),
        provider: provider.clone(),
        auth: auth.clone(),
        mcp_connections: None,
        session_mcp_servers: Vec::new(),
        session_hooks: crate::shell_hooks::HookConfig::default(),
        session_hook_context: None,
        session_start_hook_pending: true,
        session_start_hook_source: if continue_session {
            "resume".to_string()
        } else {
            "startup".to_string()
        },
        stop_hook_active: false,
        permission: Arc::new(Mutex::new(RpcPermissionState::default())),
        event_sink,
        subagent_limits: crate::subagent_tool::SubagentLimits::shared(
            (settings.subagents_max_concurrent > 0).then_some(settings.subagents_max_concurrent),
            (settings.subagents_budget_tokens > 0).then_some(settings.subagents_budget_tokens),
        ),
    }));
    // tack-ext: session_start lifecycle event.
    {
        let state = state.lock().await;
        let session_id = state.session.session_id().to_string();
        state
            .extensions
            .lock()
            .await
            .notify(
                "session_start",
                json!({
                    "sessionId": session_id,
                    "resumed": continue_session,
                    "cwd": cwd.to_string_lossy(),
                }),
            )
            .await;
    }

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line_buf = Vec::new();

    loop {
        let line = match read_line_bounded(&mut stdin, &mut line_buf).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Over-cap line: report and skip it (already drained); the
                // connection stays usable for subsequent commands.
                let out = error(None, "read", e.to_string());
                stdout.write_all(format!("{out}\n").as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let command: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let out = error(None, "parse", format!("invalid JSON: {e}"));
                // One write per record: concurrent event writes must never
                // interleave between a line and its newline.
                stdout.write_all(format!("{out}\n").as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };

        let _id = command
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let command_type = command.get("type").and_then(Value::as_str).unwrap_or("");
        let response = handle_command(command_type, &command, &state, &settings).await;

        stdout.write_all(format!("{response}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    // tack-ext: session_end lifecycle event, then stop all plugins.
    {
        let mut state = state.lock().await;
        // Drop cached MCP connections first: this cancels their services
        // and kills the server child processes instead of orphaning them.
        state.mcp_connections = None;
        let session_id = state.session.session_id().to_string();
        // Close this session's codebuddy CLI (provider registry is keyed
        // by session id).
        tack_ai::codebuddy::close_session(&session_id).await;
        let mut extensions = state.extensions.lock().await;
        extensions
            .notify("session_end", json!({"sessionId": session_id}))
            .await;
        extensions.shutdown().await;
    }
    Ok(())
}

/// Rebind the provider adapter (+ request auth) after `set_model` changed
/// the active model. The adapter is resolved for the startup model; without
/// this a cross-api switch streams the new model through the old adapter,
/// which then reports "No API key for provider: <model.provider>". Mirrors
/// the TUI's rebind_provider (same startup wrapping: Retrying inside
/// ExtNotify); auth is only re-resolved on provider change so an explicit
/// --api-key survives same-provider switches.
async fn rebind_provider_after_model_change(
    state: &mut RpcState,
    previous: &tack_ai::Model,
    settings: &Settings,
) {
    let api_changed = state.model.api != previous.api;
    let provider_changed = state.model.provider != previous.provider;
    if api_changed {
        match tack_ai::provider_for(&state.model) {
            Some(base) => {
                let provider: Arc<dyn Provider> = Arc::new(tack_ai::retry::RetryingProvider {
                    inner: base,
                    policy: settings.retry.policy(),
                    on_retry_scheduled: None,
                });
                let sink = state.extensions.lock().await.clone_sink();
                state.provider = Arc::new(crate::extension_host::ExtNotifyProvider::new(
                    provider, sink,
                ));
            }
            None => {
                tracing::warn!("no adapter for api kind {}", state.model.api);
            }
        }
    }
    if provider_changed {
        state.auth = crate::model::resolve_auth(
            &state.model.provider,
            None,
            &tack_session::default_agent_dir(),
        );
    }
}

async fn handle_command(
    command_type: &str,
    command: &Value,
    state: &Arc<Mutex<RpcState>>,
    settings: &Settings,
) -> Value {
    let id = command.get("id").and_then(Value::as_str);
    match command_type {
        "prompt" => {
            let message = command
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // A fresh user prompt resets the Stop-hook continuation guard.
            state.lock().await.stop_hook_active = false;
            // /skill:<name> expansion (TS prompt path).
            let message = if message.trim_start().starts_with("/skill:") {
                let (cwd, agent_dir) = {
                    let state = state.lock().await;
                    (
                        state.session.cwd().to_path_buf(),
                        tack_session::default_agent_dir(),
                    )
                };
                let (skills, _) = crate::skills::load_skills(&cwd, &agent_dir);
                crate::skills::expand_skill_command(message.trim_start(), &skills)
                    .unwrap_or(message)
            } else {
                message
            };
            let streaming_behavior = command
                .get("streamingBehavior")
                .and_then(Value::as_str)
                .unwrap_or("steer");
            let (is_streaming, steering) = {
                let state = state.lock().await;
                (state.is_streaming, state.steering.len())
            };
            let _ = steering;
            // Optional attached images: `[{data: base64, mimeType}]`, carried
            // with the first text block. They cannot be queued behind a
            // running turn (steering/follow-up are text-only).
            let images: Vec<(String, String)> = command
                .get("images")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|img| {
                            let data = img.get("data").and_then(Value::as_str)?.to_string();
                            let mime_type = img
                                .get("mimeType")
                                .and_then(Value::as_str)
                                .unwrap_or("image/png")
                                .to_string();
                            Some((data, mime_type))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if is_streaming {
                if !images.is_empty() {
                    return error(id, "prompt", "cannot attach images while streaming");
                }
                // Queue as steering/follow-up instead of starting a new run.
                let mut state = state.lock().await;
                if streaming_behavior == "followUp" {
                    state.follow_up.push_back(message);
                } else {
                    state.steering.push_back(message);
                }
                return success(id, "prompt", None);
            }
            let (provider, auth) = {
                let state = state.lock().await;
                (state.provider.clone(), state.auth.clone())
            };
            spawn_prompt(
                state,
                &provider,
                settings,
                auth,
                vec![message],
                images,
                false,
            )
            .await;
            success(id, "prompt", None)
        }
        "steer" => {
            let message = command
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            state.lock().await.steering.push_back(message);
            success(id, "steer", None)
        }
        "follow_up" => {
            let message = command
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            state.lock().await.follow_up.push_back(message);
            success(id, "follow_up", None)
        }
        "abort" => {
            state.lock().await.cancel.cancel();
            success(id, "abort", None)
        }
        "clear_queue" => {
            // TS session.clearQueue(): drain both queues and return their
            // text so the client can restore it (Esc-then-abort flow).
            let (steering, follow_up) = {
                let mut state = state.lock().await;
                (
                    state.steering.drain(..).collect::<Vec<_>>(),
                    state.follow_up.drain(..).collect::<Vec<_>>(),
                )
            };
            success(
                id,
                "clear_queue",
                Some(json!({ "steering": steering, "followUp": follow_up })),
            )
        }
        "new_session" => {
            let cancelled = {
                let mut state = state.lock().await;
                let was_streaming = state.is_streaming;
                if was_streaming {
                    state.cancel.cancel();
                }
                let cwd = state.session.cwd().to_path_buf();
                state.session = SessionManager::create(&cwd, None)
                    .unwrap_or_else(|_| SessionManager::in_memory(&cwd));
                state.cancel = tokio_util::sync::CancellationToken::new();
                state.session_start_hook_pending = true;
                state.session_start_hook_source = "clear".to_string();
                state.session_hook_context = None;
                // Invalidate the in-flight run's persistence/streaming guard,
                // then clear the flag ourselves: the cancelled run's forwarder
                // will skip it (stale generation).
                state.generation += 1;
                state.is_streaming = false;
                was_streaming
            };
            success(id, "new_session", Some(json!({ "cancelled": cancelled })))
        }
        "get_state" => {
            let state = state.lock().await;
            let session_file = state
                .session
                .session_file()
                .map(|p| p.display().to_string());
            let message_count = state
                .session
                .entries()
                .iter()
                .filter(|e| e.type_name() == "message")
                .count();
            let mode = state.permission.lock().await.mode;
            let mode_str = match mode {
                tack_protocol::schemas::SessionMode::Ask => "ask",
                tack_protocol::schemas::SessionMode::AcceptEdits => "acceptEdits",
                tack_protocol::schemas::SessionMode::Plan => "plan",
                tack_protocol::schemas::SessionMode::Bypass => "bypass",
            };
            success(
                id,
                "get_state",
                Some(json!({
                    "model": state.model,
                    "thinkingLevel": state.thinking.map(|t| t.as_str()).unwrap_or("off"),
                    "isStreaming": state.is_streaming,
                    "isCompacting": state.is_compacting,
                    "steeringMode": state.steering_mode.as_deref().unwrap_or("all"),
                    "followUpMode": state.follow_up_mode.as_deref().unwrap_or("all"),
                    "sessionFile": session_file,
                    "sessionId": state.session.session_id(),
                    "sessionName": Value::Null,
                    "autoCompactionEnabled": settings.compaction.enabled,
                    "messageCount": message_count,
                    "pendingMessageCount": state.steering.len() + state.follow_up.len(),
                    "mode": mode_str,
                })),
            )
        }
        "get_messages" => {
            let state = state.lock().await;
            let messages = state.session.build_session_context().messages;
            success(id, "get_messages", Some(json!({ "messages": messages })))
        }
        "get_last_assistant_text" => {
            let state = state.lock().await;
            let messages = state.session.build_session_context().messages;
            let text = messages.iter().rev().find_map(|m| match m {
                AgentMessage::Assistant(a) => {
                    let t = a.text();
                    (!t.is_empty()).then_some(t)
                }
                _ => None,
            });
            success(id, "get_last_assistant_text", Some(json!({ "text": text })))
        }
        "set_model" => {
            let provider_name = command
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("");
            let model_id = command.get("modelId").and_then(Value::as_str).unwrap_or("");
            let agent_dir = tack_session::default_agent_dir();
            match crate::model::resolve_model(provider_name, Some(model_id), &agent_dir) {
                Ok(model) => {
                    let mut state = state.lock().await;
                    let previous = std::mem::replace(&mut state.model, model.clone());
                    rebind_provider_after_model_change(&mut state, &previous, settings).await;
                    if let Err(e) = state.session.append_model_change(provider_name, model_id) {
                        tracing::warn!("model_change persist failed: {e}");
                    }
                    success(
                        id,
                        "set_model",
                        Some(serde_json::to_value(&model).unwrap_or_default()),
                    )
                }
                Err(e) => error(id, "set_model", e),
            }
        }
        "get_available_models" => {
            let agent_dir = tack_session::default_agent_dir();
            let provider_name = state.lock().await.model.provider.clone();
            let catalog = tack_ai::providers::builtin_models(&provider_name);
            let models: Vec<&tack_ai::Model> = catalog.iter().collect();
            let customs = tack_ai::providers::load_custom_providers(&agent_dir);
            let _ = customs;
            success(
                id,
                "get_available_models",
                Some(json!({ "models": models })),
            )
        }
        "set_thinking_level" => {
            let level = command
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("off");
            match crate::print_mode::parse_thinking_level(level) {
                Ok(thinking) => {
                    let mut state = state.lock().await;
                    state.thinking = thinking;
                    if let Err(e) = state.session.append_thinking_level_change(level) {
                        tracing::warn!("thinking_level_change persist failed: {e}");
                    }
                    success(id, "set_thinking_level", None)
                }
                Err(e) => error(id, "set_thinking_level", e),
            }
        }
        "get_available_thinking_levels" => {
            let model = state.lock().await.model.clone();
            let levels: Vec<&str> = if !model.reasoning {
                vec!["off"]
            } else {
                ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .filter(|level| {
                        let mapped = model
                            .thinking_level_map
                            .as_ref()
                            .and_then(|m| m.get(*level));
                        if mapped.is_some_and(|v| v.is_none()) {
                            return false;
                        }
                        if *level == "xhigh" || *level == "max" {
                            return mapped.is_some();
                        }
                        true
                    })
                    .collect()
            };
            success(
                id,
                "get_available_thinking_levels",
                Some(json!({ "levels": levels })),
            )
        }
        "compact" => {
            let custom = command
                .get("customInstructions")
                .and_then(Value::as_str)
                .map(str::to_string);
            let (provider, auth) = {
                let state = state.lock().await;
                (state.provider.clone(), state.auth.clone())
            };
            match run_compact_command(state, &provider, settings, auth, custom).await {
                Ok(data) => success(id, "compact", Some(data)),
                Err(e) => error(id, "compact", e),
            }
        }
        "set_session_name" => {
            let name = command
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let result = state.lock().await.session.append_session_info(Some(name));
            match result {
                Ok(_) => success(id, "set_session_name", None),
                Err(e) => error(id, "set_session_name", e),
            }
        }
        "get_session_stats" => {
            let state = state.lock().await;
            let totals = state.session.session_totals();
            let context = state.session.build_session_context();
            let tokens = tack_session::estimate_context_tokens(&context.messages);
            success(
                id,
                "get_session_stats",
                Some(json!({
                    "sessionFile": state.session.session_file().map(|p| p.display().to_string()),
                    "sessionId": state.session.session_id(),
                    "tokenCount": tokens.tokens,
                    "totalCost": totals.cost.total,
                    "inputTokens": totals.input,
                    "outputTokens": totals.output,
                    "cacheReadTokens": totals.cache_read,
                    "cacheWriteTokens": totals.cache_write,
                })),
            )
        }
        "get_entries" => {
            let state = state.lock().await;
            let entries = state.session.build_session_path();
            let leaf = state.session.leaf_id().map(str::to_string);
            success(
                id,
                "get_entries",
                Some(json!({ "entries": entries, "leafId": leaf })),
            )
        }
        "fork" => {
            let entry_id = command.get("entryId").and_then(Value::as_str).unwrap_or("");
            // TS pi #9178/#9179: tree navigation is rejected while a response
            // is streaming or a compaction is in flight — the compaction
            // entry must land on the leaf it was computed from.
            {
                let state = state.lock().await;
                if state.is_streaming {
                    return error(
                        id,
                        "fork",
                        "Wait for the current response to finish before navigating the session tree.",
                    );
                }
                if state.is_compacting {
                    return error(
                        id,
                        "fork",
                        "Wait for the current compaction to finish before navigating the session tree.",
                    );
                }
            }
            let result = state.lock().await.session.branch(entry_id);
            match result {
                Ok(_) => success(id, "fork", Some(json!({ "text": "", "cancelled": false }))),
                Err(e) => error(id, "fork", e),
            }
        }
        "bash" => {
            let command_str = command
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let exclude = command
                .get("excludeFromContext")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match run_bash_command(state, command_str, exclude).await {
                Ok(data) => success(id, "bash", Some(data)),
                Err(e) => error(id, "bash", e),
            }
        }
        "abort_bash" => {
            // TS abortBash: cancels bash command executions only, never the
            // prompt run.
            state.lock().await.bash_cancel.cancel();
            success(id, "abort_bash", None)
        }
        "bash_tasks" => {
            let state = state.lock().await;
            success(id, "bash_tasks", Some(state.background.list()))
        }
        "bash_output" => {
            let task_id = command.get("taskId").and_then(Value::as_str).unwrap_or("");
            let state = state.lock().await;
            match state.background.snapshot(task_id) {
                Some(snapshot) => success(id, "bash_output", Some(snapshot)),
                None => error(id, "bash_output", "unknown task id"),
            }
        }
        "kill_shell" => {
            let task_id = command.get("taskId").and_then(Value::as_str).unwrap_or("");
            let state = state.lock().await;
            let outcome = state.background.kill(task_id);
            success(
                id,
                "kill_shell",
                Some(json!({
                    "signalled": outcome.signalled(),
                    "alreadyFinished": outcome.already_finished(),
                })),
            )
        }
        "abort_retry" => {
            // TS abortRetry: cancel the retry backoff only; the run ends
            // with the last provider error, not a user abort.
            state.lock().await.retry_cancel.cancel();
            success(id, "abort_retry", None)
        }
        "clone" => {
            let (file, cwd) = {
                let state = state.lock().await;
                (
                    state.session.session_file().map(|p| p.to_path_buf()),
                    state.session.cwd().to_path_buf(),
                )
            };
            let Some(file) = file else {
                return error(id, "clone", "session is not persisted");
            };
            match SessionManager::fork_from(&file, &cwd) {
                Ok(session) => {
                    let mut state = state.lock().await;
                    state.session = session;
                    // Invalidate the in-flight run's persistence guard.
                    state.generation += 1;
                    success(id, "clone", None)
                }
                Err(e) => error(id, "clone", e),
            }
        }
        "cycle_model" => {
            let (provider_name, current) = {
                let state = state.lock().await;
                (state.model.provider.clone(), state.model.id.clone())
            };
            let candidates: Vec<String> = tack_ai::providers::builtin_models(&provider_name)
                .iter()
                .map(|m| format!("{provider_name}/{}", m.id))
                .collect();
            if candidates.is_empty() {
                return error(id, "cycle_model", "no models to cycle");
            }
            let current_full = format!("{provider_name}/{current}");
            let index = candidates
                .iter()
                .position(|c| *c == current_full)
                .unwrap_or(0);
            let next = &candidates[(index + 1) % candidates.len()];
            let (next_provider, next_id) = next.split_once('/').expect("provider/id");
            let agent_dir = tack_session::default_agent_dir();
            match crate::model::resolve_model(next_provider, Some(next_id), &agent_dir) {
                Ok(model) => {
                    let mut state = state.lock().await;
                    state.model = model.clone();
                    let _ = state.session.append_model_change(next_provider, next_id);
                    success(
                        id,
                        "cycle_model",
                        Some(serde_json::to_value(&model).unwrap_or_default()),
                    )
                }
                Err(e) => error(id, "cycle_model", e),
            }
        }
        "cycle_thinking_level" => {
            const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];
            let current = {
                let state = state.lock().await;
                state
                    .thinking
                    .map(|t| t.as_str())
                    .unwrap_or("off")
                    .to_string()
            };
            let index = LEVELS
                .iter()
                .position(|l| *l == current)
                .map(|i| i + 1)
                .unwrap_or(0)
                % LEVELS.len();
            let next = LEVELS[index];
            match crate::print_mode::parse_thinking_level(next) {
                Ok(thinking) => {
                    let mut state = state.lock().await;
                    state.thinking = thinking;
                    let _ = state.session.append_thinking_level_change(next);
                    success(id, "cycle_thinking_level", Some(json!({ "level": next })))
                }
                Err(e) => error(id, "cycle_thinking_level", e),
            }
        }
        "export_html" => {
            let target = command
                .get("path")
                .and_then(Value::as_str)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    let state_cwd = std::env::current_dir().unwrap_or_default();
                    state_cwd.join("session-export.html")
                });
            let state = state.lock().await;
            match crate::tui::commands::export_html(&state.session, &target) {
                Ok(()) => success(
                    id,
                    "export_html",
                    Some(json!({ "path": target.display().to_string() })),
                ),
                Err(e) => error(id, "export_html", e),
            }
        }
        "get_commands" => success(
            id,
            "get_commands",
            Some(json!({ "commands": RPC_COMMANDS })),
        ),
        "get_fork_messages" => {
            let state = state.lock().await;
            let messages: Vec<Value> = state
                .session
                .build_session_path()
                .iter()
                .filter_map(|entry| {
                    if let tack_session::SessionEntry::Message { id, message, .. } = entry
                        && let AgentMessage::User(u) = message
                    {
                        let text = match &u.content {
                            tack_ai::UserContent::Text(t) => t.clone(),
                            tack_ai::UserContent::Blocks(blocks) => blocks
                                .iter()
                                .filter_map(|b| match b {
                                    tack_ai::InputContentBlock::Text { text, .. } => Some(text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(" "),
                        };
                        return Some(json!({ "entryId": id, "text": text.chars().take(120).collect::<String>() }));
                    }
                    None
                })
                .collect();
            success(
                id,
                "get_fork_messages",
                Some(json!({ "messages": messages })),
            )
        }
        "get_tree" => {
            let state = state.lock().await;
            let items = crate::tui::commands::build_tree_items(&state.session);
            let tree: Vec<Value> = items
                .iter()
                .map(|item| json!({ "entryId": item.value, "label": item.label }))
                .collect();
            success(id, "get_tree", Some(json!({ "tree": tree })))
        }
        "set_auto_compaction" => {
            let enabled = command
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let agent_dir = tack_session::default_agent_dir();
            if let Err(e) = save_nested_bool(&agent_dir, "compaction", "enabled", enabled) {
                return error(id, "set_auto_compaction", format!("save failed: {e}"));
            }
            success(
                id,
                "set_auto_compaction",
                Some(json!({ "enabled": enabled })),
            )
        }
        "set_auto_retry" => {
            let enabled = command
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let agent_dir = tack_session::default_agent_dir();
            if let Err(e) = save_nested_bool(&agent_dir, "retry", "enabled", enabled) {
                return error(id, "set_auto_retry", format!("save failed: {e}"));
            }
            success(id, "set_auto_retry", Some(json!({ "enabled": enabled })))
        }
        "set_steering_mode" => {
            let mode = command
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("all")
                .to_string();
            if !matches!(mode.as_str(), "all" | "one-at-a-time") {
                return error(id, "set_steering_mode", "mode must be all|one-at-a-time");
            }
            state.lock().await.steering_mode = Some(mode.clone());
            success(id, "set_steering_mode", Some(json!({ "mode": mode })))
        }
        "set_mode" => {
            let mode = command.get("mode").and_then(Value::as_str).unwrap_or("");
            let parsed = match mode {
                "ask" => tack_protocol::schemas::SessionMode::Ask,
                "acceptEdits" => tack_protocol::schemas::SessionMode::AcceptEdits,
                "plan" => tack_protocol::schemas::SessionMode::Plan,
                "bypass" => tack_protocol::schemas::SessionMode::Bypass,
                _ => return error(id, "set_mode", "mode must be ask|acceptEdits|plan|bypass"),
            };
            state.lock().await.permission.lock().await.mode = parsed;
            success(id, "set_mode", Some(json!({ "mode": mode })))
        }
        "permission_response" => {
            let request_id = command
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let decision_raw = command
                .get("decision")
                .and_then(Value::as_str)
                .unwrap_or("");
            let decision = match decision_raw {
                "allow" => tack_protocol::schemas::PermissionDecision::AllowOnce,
                "allowAlways" => tack_protocol::schemas::PermissionDecision::AllowAlways,
                "deny" => tack_protocol::schemas::PermissionDecision::Deny,
                _ => {
                    return error(
                        id,
                        "permission_response",
                        "decision must be allow|allowAlways|deny",
                    );
                }
            };
            let reason = command
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            let sender = {
                let state = state.lock().await;
                state
                    .permission
                    .lock()
                    .await
                    .pending
                    .lock()
                    .await
                    .remove(request_id)
            };
            match sender {
                Some(sender) => {
                    let _ = sender.send(permission::PermissionAnswer { decision, reason });
                    success(id, "permission_response", None)
                }
                None => error(id, "permission_response", "unknown or expired requestId"),
            }
        }
        "set_follow_up_mode" => {
            let mode = command
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("all")
                .to_string();
            if !matches!(mode.as_str(), "all" | "one-at-a-time") {
                return error(id, "set_follow_up_mode", "mode must be all|one-at-a-time");
            }
            state.lock().await.follow_up_mode = Some(mode.clone());
            success(id, "set_follow_up_mode", Some(json!({ "mode": mode })))
        }
        "set_mcp_servers" => {
            // Replace the session-scoped MCP server set (host-injected, e.g.
            // ZCode createSession.mcpServers). Shape: {"servers": {name:
            // entry}} with mcp.json entry shapes (Claude Code compatible).
            let servers = command.get("servers").cloned().unwrap_or_else(|| json!({}));
            if !servers.is_object() {
                return error(id, "set_mcp_servers", "servers must be an object");
            }
            let specs = crate::mcp_config::specs_from_value(
                &json!({ "mcpServers": servers }),
                "set_mcp_servers",
            );
            let mut state = state.lock().await;
            state.session_mcp_servers = specs;
            // No eager pool rebuild: the fingerprint covers the effective
            // spec set, so the next prompt / get_mcp_status connect applies
            // the change and drops superseded server processes.
            let names: Vec<&str> = state
                .session_mcp_servers
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            success(id, "set_mcp_servers", Some(json!({ "servers": names })))
        }
        "set_hooks" => {
            // Replace the session-scoped hook set (host-injected, e.g. ZCode
            // workspace-hooks passthrough after trust filtering). Shape:
            // Claude flat format {"PreToolUse": [{matcher, hooks: [...]}],
            // ...}; null or omitted clears. Merged after settings/bundle
            // hooks at the next prompt run — mid-session updates (trust
            // grants) take effect on the following turn.
            let raw = command.get("hooks").cloned().unwrap_or(Value::Null);
            if !raw.is_null() && !raw.is_object() {
                return error(id, "set_hooks", "hooks must be an object or null");
            }
            let config = if raw.is_null() {
                crate::shell_hooks::HookConfig::default()
            } else {
                crate::shell_hooks::parse_hooks(Some(&raw))
            };
            let mut handlers = serde_json::Map::new();
            for (event, group) in &config.groups {
                let entry = handlers
                    .entry(event.as_str().to_string())
                    .or_insert_with(|| json!(0));
                *entry = json!(entry.as_u64().unwrap_or(0) + group.hooks.len() as u64);
            }
            state.lock().await.session_hooks = config;
            success(
                id,
                "set_hooks",
                Some(json!({ "handlers": Value::Object(handlers) })),
            )
        }
        "get_mcp_status" => {
            // Per-server MCP status. {"connect": false} = status-only (read
            // the cached pool, never connect); default connects (settings-
            // page refresh semantics: build/reuse the prompt pool).
            let connect = command
                .get("connect")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let (provider, model, auth, cwd) = {
                let state = state.lock().await;
                (
                    state.provider.clone(),
                    state.model.clone(),
                    state.auth.clone(),
                    state.session.cwd().to_path_buf(),
                )
            };
            let agent_dir = tack_session::default_agent_dir();
            let llm = crate::mcp_config::SamplingLlm {
                provider,
                model,
                auth,
            };
            let statuses = mcp::mcp_status(state, settings, &llm, &cwd, &agent_dir, connect).await;
            let servers: Vec<Value> = statuses
                .iter()
                .map(|s| {
                    json!({
                        "name": s.name,
                        "transport": s.transport,
                        "status": s.status,
                        "toolCount": s.tool_count,
                        "error": s.error,
                    })
                })
                .collect();
            success(id, "get_mcp_status", Some(json!({ "servers": servers })))
        }
        "switch_session" => {
            let path_str = command.get("path").and_then(Value::as_str).unwrap_or("");
            if path_str.is_empty() {
                return error(id, "switch_session", "path required");
            }
            let cwd = std::env::current_dir().unwrap_or_default();
            let dir = tack_session::default_session_dir(&cwd, &tack_session::default_agent_dir());
            let Some(path) = tack_session::resolve_session_arg(path_str, &dir) else {
                return error(
                    id,
                    "switch_session",
                    format!("no session matching {path_str:?}"),
                );
            };
            match SessionManager::open(&path, Some(dir)) {
                Ok(mut session) => {
                    match session.repair_dangling_tool_calls() {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(
                            "repaired {n} dangling tool call(s) from an interrupted run"
                        ),
                        Err(e) => tracing::warn!("failed to repair dangling tool calls: {e}"),
                    }
                    let mut state = state.lock().await;
                    state.session = session;
                    state.session_start_hook_pending = true;
                    state.session_start_hook_source = "resume".to_string();
                    state.session_hook_context = None;
                    // Invalidate the in-flight run's persistence guard.
                    state.generation += 1;
                    success(id, "switch_session", None)
                }
                Err(e) => error(id, "switch_session", e),
            }
        }
        other => error(
            id,
            other,
            format!("unknown or unsupported command: {other}"),
        ),
    }
}

/// Commands supported by this RPC endpoint (get_commands response).
const RPC_COMMANDS: &[&str] = &[
    "prompt",
    "steer",
    "follow_up",
    "abort",
    "clear_queue",
    "abort_bash",
    "abort_retry",
    "new_session",
    "switch_session",
    "clone",
    "fork",
    "compact",
    "bash",
    "bash_tasks",
    "bash_output",
    "kill_shell",
    "set_model",
    "cycle_model",
    "get_available_models",
    "set_thinking_level",
    "cycle_thinking_level",
    "get_available_thinking_levels",
    "set_session_name",
    "get_state",
    "get_messages",
    "get_last_assistant_text",
    "get_session_stats",
    "get_entries",
    "get_fork_messages",
    "get_tree",
    "get_commands",
    "export_html",
    "set_auto_compaction",
    "set_auto_retry",
    "set_steering_mode",
    "set_follow_up_mode",
    "set_mcp_servers",
    "get_mcp_status",
    "set_hooks",
    "set_mode",
    "permission_response",
];

#[cfg(test)]
#[allow(unsafe_code, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Shared agent dir for this test binary's rpc tests.
    fn test_agent_dir() -> &'static std::path::Path {
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
    struct ScriptedProvider {
        scripts: std::sync::Mutex<Vec<tack_ai::AssistantMessage>>,
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

    fn test_model() -> tack_ai::Model {
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

    fn assistant_text(text: &str) -> tack_ai::AssistantMessage {
        let mut m = tack_ai::AssistantMessage::pending(&test_model());
        m.stop_reason = tack_ai::StopReason::Stop;
        m.content = vec![tack_ai::ContentBlock::text(text)];
        m
    }

    fn make_state(cwd: &std::path::Path) -> Arc<Mutex<RpcState>> {
        Arc::new(Mutex::new(RpcState {
            session: SessionManager::in_memory(cwd),
            model: test_model(),
            thinking: None,
            steering: Default::default(),
            follow_up: Default::default(),
            steering_mode: None,
            follow_up_mode: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            bash_cancel: tokio_util::sync::CancellationToken::new(),
            retry_cancel: tokio_util::sync::CancellationToken::new(),
            is_streaming: false,
            is_compacting: false,
            generation: 0,
            background: tack_tools::background::BackgroundTaskManager::new(),
            lsp: Settings::default().lsp_manager(cwd),
            extensions: Arc::new(Mutex::new(
                crate::extension_host::ExtensionManager::default(),
            )),
            provider: test_provider(vec![]),
            auth: test_auth(),
            mcp_connections: None,
            session_mcp_servers: Vec::new(),
            session_hooks: crate::shell_hooks::HookConfig::default(),
            session_hook_context: None,
            session_start_hook_pending: false,
            session_start_hook_source: "startup".to_string(),
            stop_hook_active: false,
            permission: Arc::new(Mutex::new(RpcPermissionState::default())),
            event_sink: tokio::sync::mpsc::unbounded_channel().0,
            subagent_limits: crate::subagent_tool::SubagentLimits::shared(None, None),
        }))
    }

    fn test_provider(scripts: Vec<tack_ai::AssistantMessage>) -> Arc<dyn Provider> {
        Arc::new(ScriptedProvider {
            scripts: std::sync::Mutex::new(scripts),
        })
    }

    fn test_auth() -> Arc<dyn tack_ai::oauth::AuthResolver> {
        Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string())))
    }

    async fn wait_for_run_end(state: &Arc<Mutex<RpcState>>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if !state.lock().await.is_streaming {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "run never finished");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Baseline: a prompt run persists its messages into the session.
    #[tokio::test]
    async fn prompt_run_persists_messages() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let provider = test_provider(vec![assistant_text("hello from rpc")]);
        let settings = Settings::default();
        spawn_prompt(
            &state,
            &provider,
            &settings,
            test_auth(),
            vec!["hi".into()],
            Vec::new(),
            false,
        )
        .await;
        wait_for_run_end(&state).await;
        let guard = state.lock().await;
        let messages = guard.session.build_session_context().messages;
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant(a) if a.text().contains("hello from rpc")
            )),
            "assistant message must be persisted: {messages:?}"
        );
    }

    /// Regression: new_session during a run cancelled the old run but its
    /// event forwarder kept appending trailing messages — into the NEW
    /// session file — and could clobber a later run's is_streaming flag.
    /// The generation guard prevents both.
    #[tokio::test]
    async fn new_session_during_run_keeps_new_session_clean() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        // Two scripts in case the run gets further than expected.
        let provider = test_provider(vec![
            assistant_text("stale-one"),
            assistant_text("stale-two"),
        ]);
        let settings = Settings::default();
        spawn_prompt(
            &state,
            &provider,
            &settings,
            test_auth(),
            vec!["hi".into()],
            Vec::new(),
            false,
        )
        .await;

        // Swap the session mid-run (cancels the in-flight run).
        let response = handle_command(
            "new_session",
            &json!({ "type": "new_session", "id": "r1" }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(response["success"], true);
        assert_eq!(response["data"]["cancelled"], true);
        assert!(
            !state.lock().await.is_streaming,
            "new_session clears the flag itself"
        );

        // Let the cancelled run's forwarder drain its terminal events.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let guard = state.lock().await;
        assert!(
            !guard
                .session
                .entries()
                .iter()
                .any(|e| e.type_name() == "message"),
            "stale run must not append to the replacement session: {:?}",
            guard
                .session
                .entries()
                .iter()
                .map(|e| e.type_name())
                .collect::<Vec<_>>()
        );
    }

    /// get_state reports the configured steering/follow-up modes.
    #[tokio::test]
    async fn abort_bash_does_not_cancel_prompt_run() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        let run_token = { state.lock().await.cancel.clone() };
        let response = handle_command(
            "abort_bash",
            &json!({ "type": "abort_bash" }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(response["success"], true);
        assert!(
            !run_token.is_cancelled(),
            "abort_bash must not kill the prompt run (TS abortBash semantics)"
        );
        assert!(state.lock().await.bash_cancel.is_cancelled());
    }

    /// F08: the MCP pool fingerprint is stable for identical inputs and
    /// changes with the spec set, the resolution dirs, or the model (the
    /// sampling callback captures provider/model/auth at connect time).
    #[test]
    fn mcp_cache_fingerprint_tracks_connection_inputs() {
        let dir = std::path::Path::new("/tmp/x");
        let other_dir = std::path::Path::new("/tmp/y");
        let spec = |name: &str| {
            tack_tools::mcp::McpServerSpec::stdio(name.into(), "cmd".into(), vec![], vec![], None)
        };
        let model = test_model();
        let base = mcp_cache_fingerprint(dir, dir, &[spec("a")], &model);
        assert_eq!(base, mcp_cache_fingerprint(dir, dir, &[spec("a")], &model));
        assert_ne!(base, mcp_cache_fingerprint(dir, dir, &[spec("b")], &model));
        assert_ne!(
            base,
            mcp_cache_fingerprint(dir, dir, &[spec("a"), spec("b")], &model)
        );
        assert_ne!(
            base,
            mcp_cache_fingerprint(other_dir, dir, &[spec("a")], &model)
        );
        let mut other_model = model.clone();
        other_model.id = "other".into();
        assert_ne!(
            base,
            mcp_cache_fingerprint(dir, dir, &[spec("a")], &other_model)
        );
    }

    /// F34 regression: get_state hardcoded isCompacting=false; it must
    /// reflect RpcState.is_compacting (maintained by run_compact_command).
    #[tokio::test]
    async fn get_state_reports_is_compacting() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        let read_flag = || async {
            handle_command(
                "get_state",
                &json!({ "type": "get_state" }),
                &state,
                &settings,
            )
            .await["data"]["isCompacting"]
                .as_bool()
                .expect("isCompacting bool")
        };
        assert!(!read_flag().await, "idle: not compacting");
        state.lock().await.is_compacting = true;
        assert!(read_flag().await, "in-flight compaction must be reported");
    }

    /// set_mcp_servers stores session-scoped specs (host-injected at session
    /// create) and reports the effective names; malformed entries are
    /// skipped per-entry (same loose parsing as mcp.json).
    #[tokio::test]
    async fn set_mcp_servers_stores_specs() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        let r = handle_command(
            "set_mcp_servers",
            &json!({ "type": "set_mcp_servers", "servers": {
                "good": { "command": "npx", "args": ["-y", "srv"] },
                "remote": { "type": "http", "url": "http://localhost:9/mcp" },
                "bad": { "args": "not-an-array" },
            }}),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        let names = r["data"]["servers"].as_array().unwrap();
        assert_eq!(names.len(), 2, "{r}");
        assert_eq!(state.lock().await.session_mcp_servers.len(), 2);

        // Replace semantics: a second call swaps the whole set.
        let r = handle_command(
            "set_mcp_servers",
            &json!({ "type": "set_mcp_servers", "servers": {
                "only": { "command": "npx" },
            }}),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        let names = r["data"]["servers"].as_array().unwrap();
        assert_eq!(names, &vec![json!("only")], "{r}");

        // Non-object servers payload is rejected.
        let r = handle_command(
            "set_mcp_servers",
            &json!({ "type": "set_mcp_servers", "servers": [] }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], false, "{r}");
    }

    /// get_mcp_status connect=false never connects: session servers report
    /// disconnected (not failed) and no pool is built.
    #[tokio::test]
    async fn get_mcp_status_status_only_does_not_connect() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        let r = handle_command(
            "set_mcp_servers",
            &json!({ "type": "set_mcp_servers", "servers": {
                "ghost": { "command": "definitely-not-a-real-tack-test-command" },
            }}),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        let r = handle_command(
            "get_mcp_status",
            &json!({ "type": "get_mcp_status", "connect": false }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        let servers = r["data"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1, "{r}");
        assert_eq!(servers[0]["name"], "ghost");
        assert_eq!(servers[0]["transport"], "stdio");
        assert_eq!(servers[0]["status"], "disconnected", "{r}");
        assert_eq!(servers[0]["toolCount"], 0);
        assert!(
            state.lock().await.mcp_connections.is_none(),
            "status-only must not build a pool"
        );
    }

    /// get_mcp_status (connect) with an unreachable stdio server reports
    /// failed + the connect error (settings-page semantics); partial pools
    /// are not cached.
    #[tokio::test]
    async fn get_mcp_status_reports_connect_failure() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        let r = handle_command(
            "set_mcp_servers",
            &json!({ "type": "set_mcp_servers", "servers": {
                "ghost": { "command": "definitely-not-a-real-tack-test-command" },
            }}),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        let r = handle_command(
            "get_mcp_status",
            &json!({ "type": "get_mcp_status" }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        let servers = r["data"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1, "{r}");
        assert_eq!(servers[0]["status"], "failed", "{r}");
        assert!(
            servers[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{r}"
        );
        assert!(
            state.lock().await.mcp_connections.is_none(),
            "partial pools must not be cached"
        );
    }

    /// get_state reports the configured steering/follow-up modes.
    #[tokio::test]
    async fn get_state_reports_modes() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        for (command, mode_field) in [
            ("set_steering_mode", "steeringMode"),
            ("set_follow_up_mode", "followUpMode"),
        ] {
            let r = handle_command(
                command,
                &json!({ "type": command, "mode": "one-at-a-time" }),
                &state,
                &settings,
            )
            .await;
            assert_eq!(r["success"], true, "{r}");
            let r = handle_command(
                "get_state",
                &json!({ "type": "get_state" }),
                &state,
                &settings,
            )
            .await;
            assert_eq!(r["data"][mode_field], "one-at-a-time", "{r}");
        }
    }

    /// clear_queue drains both queues and returns their text (TS 0.84.4
    /// parity: `data: { steering: [...], followUp: [...] }`); a second call
    /// returns empty arrays.
    #[tokio::test]
    async fn clear_queue_returns_and_empties_both_queues() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        for command in ["steer", "follow_up"] {
            for message in ["first", "second"] {
                let r = handle_command(
                    command,
                    &json!({ "type": command, "message": message }),
                    &state,
                    &settings,
                )
                .await;
                assert_eq!(r["success"], true, "{r}");
            }
        }

        let r = handle_command(
            "clear_queue",
            &json!({ "type": "clear_queue", "id": "cq1" }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["id"], "cq1", "{r}");
        assert_eq!(r["type"], "response", "{r}");
        assert_eq!(r["command"], "clear_queue", "{r}");
        assert_eq!(r["success"], true, "{r}");
        assert_eq!(r["data"]["steering"], json!(["first", "second"]), "{r}");
        assert_eq!(r["data"]["followUp"], json!(["first", "second"]), "{r}");
        {
            let guard = state.lock().await;
            assert!(guard.steering.is_empty());
            assert!(guard.follow_up.is_empty());
        }

        let r = handle_command(
            "clear_queue",
            &json!({ "type": "clear_queue" }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        assert_eq!(r["data"], json!({ "steering": [], "followUp": [] }), "{r}");
    }

    /// Regression: set_model across api kinds must rebind the provider
    /// adapter — the startup adapter used to stream the new model, failing
    /// with "No API key for provider: <model.provider>". Environment-
    /// independence comes from a stub CODEBUDDY_PATH: a non-executable
    /// file, so the CodeBuddy adapter deterministically fails at spawn
    /// with "failed to spawn codebuddy CLI" — with or without the real
    /// CLI installed (a real CLI answers with arbitrary service errors).
    #[tokio::test]
    async fn set_model_rebinds_provider_adapter() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let stub_dir = tempfile::tempdir().unwrap();
        let stub = stub_dir.path().join("codebuddy-stub");
        std::fs::write(&stub, "not an executable\n").unwrap();
        // Env is process-global; restore on drop even when the test panics.
        struct CodebuddyPathGuard(Option<std::ffi::OsString>);
        impl Drop for CodebuddyPathGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("CODEBUDDY_PATH", value),
                        None => std::env::remove_var("CODEBUDDY_PATH"),
                    }
                }
            }
        }
        let _guard = CodebuddyPathGuard(std::env::var_os("CODEBUDDY_PATH"));
        unsafe { std::env::set_var("CODEBUDDY_PATH", &stub) };

        let state = make_state(cwd.path()); // anthropic-messages startup model
        let settings = Settings::default();

        let r = handle_command(
            "set_model",
            &json!({ "type": "set_model", "provider": "codebuddy", "modelId": "some-model" }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        assert_eq!(
            state.lock().await.model.api,
            tack_ai::codebuddy::CODEBUDDY_API
        );

        let r = handle_command(
            "prompt",
            &json!({ "type": "prompt", "message": "hi" }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");
        wait_for_run_end(&state).await;

        let guard = state.lock().await;
        let errors: Vec<String> = guard
            .session
            .build_session_context()
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Assistant(a) => a.error_message.clone(),
                _ => None,
            })
            .collect();
        // The assertion target is that the error comes from the CodeBuddy
        // adapter (vs the startup adapter's "No API key"). The stub
        // CODEBUDDY_PATH makes that error deterministic on every machine:
        // the adapter resolves the stub as its CLI and fails to spawn it.
        assert!(
            errors
                .iter()
                .any(|e| e.contains("failed to spawn codebuddy CLI")),
            "expected an error from the CodeBuddy CLI adapter: {errors:?}"
        );
        assert!(
            !errors.iter().any(|e| e.contains("No API key")),
            "stream went through the startup adapter: {errors:?}"
        );
    }

    /// An over-cap stdin line errors out and is drained: the next command
    /// is still parsed, and no unbounded allocation happens.
    #[tokio::test]
    async fn read_line_bounded_rejects_over_cap_line() {
        let mut input = b"{\"type\":\"abort\"}\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES + 10));
        input.extend_from_slice(b"\n{\"type\":\"get_state\"}\n");
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();

        let line = read_line_bounded(&mut reader, &mut buf).await.unwrap();
        assert_eq!(line.as_deref(), Some("{\"type\":\"abort\"}"));

        let err = read_line_bounded(&mut reader, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("byte cap"), "{err}");

        // The remainder of the giant line was drained; the following
        // command reads cleanly.
        let line = read_line_bounded(&mut reader, &mut buf).await.unwrap();
        assert_eq!(line.as_deref(), Some("{\"type\":\"get_state\"}"));
        assert!(
            read_line_bounded(&mut reader, &mut buf)
                .await
                .unwrap()
                .is_none(),
            "clean EOF"
        );
    }

    /// An over-cap line without a trailing newline (EOF mid-line) still
    /// errors instead of being delivered.
    #[tokio::test]
    async fn read_line_bounded_over_cap_at_eof() {
        let input = vec![b'y'; MAX_LINE_BYTES + 1];
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let err = read_line_bounded(&mut reader, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn fork_rejected_while_streaming_or_compacting() {
        test_agent_dir();
        let cwd = tempfile::tempdir().unwrap();
        let state = make_state(cwd.path());
        let settings = Settings::default();
        let entry_id = {
            let mut guard = state.lock().await;
            guard
                .session
                .append_message(tack_agent_core::AgentMessage::user("hello"))
                .unwrap()
        };

        // Baseline: navigation works when idle.
        let r = handle_command(
            "fork",
            &json!({ "type": "fork", "entryId": entry_id }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], true, "{r}");

        state.lock().await.is_compacting = true;
        let r = handle_command(
            "fork",
            &json!({ "type": "fork", "entryId": entry_id }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("compaction"), "{r}");

        state.lock().await.is_compacting = false;
        state.lock().await.is_streaming = true;
        let r = handle_command(
            "fork",
            &json!({ "type": "fork", "entryId": entry_id }),
            &state,
            &settings,
        )
        .await;
        assert_eq!(r["success"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("response"), "{r}");
    }
}
