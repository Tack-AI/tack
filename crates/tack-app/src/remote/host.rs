//! Live session hosting: `SessionHost`, session command handling, snapshots,
//! and the idle-session reaper.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tack_agent_core::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, HooksChain,
    ToolExecutionMode, agent_loop,
};
use tack_ai::Provider;
use tack_protocol::schemas::*;
use tack_session::SessionManager;
use tokio::sync::{Mutex, broadcast};

use super::MAX_CONNECTIONS;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Live session
// ---------------------------------------------------------------------------

struct LiveSession {
    manager: SessionManager,
    model: tack_ai::Model,
    thinking: Option<tack_ai::ThinkingLevel>,
    cancel: tokio_util::sync::CancellationToken,
    phase: SessionPhase,
    revision: u64,
    attached: u32,
    created_at: u64,
    /// Last time a command or run touched this session (epoch millis);
    /// drives idle reaping.
    last_active: u64,
    queued_steer: std::collections::VecDeque<String>,
    /// Permission mode. Default `bypass`: pre-extension remote servers ran
    /// every non-denied tool without prompting, so prompting must be
    /// opt-in (`set_mode`) — clients that cannot answer permission
    /// requests keep working unchanged.
    mode: SessionMode,
    /// Session-scoped allow-always cache (key: tool + first arg).
    allow_always: std::collections::HashSet<String>,
}

impl std::fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSession").finish_non_exhaustive()
    }
}

/// Short human-readable summary of a tool call for the permission prompt.
fn permission_title(tool_name: &str, args: &Value) -> String {
    let first = args
        .get("path")
        .or_else(|| args.get("command"))
        .or_else(|| args.get("pattern"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if first.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}: {first}")
    }
}

/// An unanswered permission prompt: the run's `before_tool_call` hook is
/// parked on `respond` until a client sends `permission_response`, the
/// prompt times out (`PERMISSION_PROMPT_TIMEOUT`), or the entry is
/// dropped on abort/reap/last-disconnect (which denies the call).
struct PendingPermission {
    session_id: String,
    respond: tokio::sync::oneshot::Sender<PermissionDecision>,
}

impl std::fmt::Debug for PendingPermission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPermission")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// Unique id for permission requests (millis alone can collide for
/// parallel tool calls in the same session).
fn next_permission_request_id(session_id: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "perm-{session_id}-{}-{}",
        tack_ai::now_millis(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Max time a permission prompt waits for a client answer (F17).
/// Without a bound, a prompt no client ever answers parked the run — and
/// its session, which the reaper will not touch mid-turn — forever; only
/// Abort cleaned up. On timeout the call is denied, same as an explicit
/// deny. Ten minutes is generous for a human at a terminal while still
/// guaranteeing the agent task unwinds.
const PERMISSION_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Hooks-side permission gate for remote sessions: the TUI permission
/// state machine (ask/acceptEdits/plan/bypass, allow rules, allow-always
/// cache) bridged to protocol messages. Prompts are broadcast as
/// `ServerEvent::PermissionRequest`; the hook parks until a client answers
/// with `Command::PermissionResponse`, the prompt times out, or the entry
/// is dropped on abort/reap/last-disconnect = deny.
struct RemotePermissionHooks {
    host: SharedHost,
    session_id: String,
    rules: crate::permissions::PermissionRules,
    agent_dir: PathBuf,
    /// Prompt-injection defense: when untrusted external content (web/MCP/
    /// plugin tool output) entered the context this run, mutating tools
    /// always prompt — allow rules, the allow-always cache AND the plugin
    /// approval chain are bypassed (rpc/TUI parity).
    untrusted_seen: Arc<std::sync::atomic::AtomicBool>,
    /// Plugin approval chain (tack-RPC `approval/review`): reviewers get
    /// first crack at a decision that would otherwise be broadcast as a
    /// `ServerEvent::PermissionRequest` (see `crate::approval`).
    approval_chain: crate::approval::ApprovalChain,
}

impl std::fmt::Debug for RemotePermissionHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemotePermissionHooks")
            .field("session_id", &self.session_id)
            .finish()
    }
}

#[async_trait::async_trait]
impl AgentHooks for RemotePermissionHooks {
    async fn before_tool_call(
        &self,
        ctx: &tack_agent_core::hooks::BeforeToolCallContext<'_>,
    ) -> tack_agent_core::hooks::BeforeToolCallOutcome {
        use tack_agent_core::hooks::BeforeToolCallOutcome as Outcome;
        // Declarative deny wins over every mode (including bypass).
        if let Some(rule) = self.rules.deny_match(ctx.tool_name, ctx.args) {
            return Outcome::Block {
                reason: Some(format!(
                    "denied by permissions.deny rule \"{}\"",
                    rule.render()
                )),
                terminate: false,
            };
        }
        let read_only = crate::permissions::is_read_only_tool(ctx.tool_name, ctx.args);
        let mode = {
            let host_guard = self.host.lock().await;
            host_guard
                .sessions
                .get(&self.session_id)
                .map(|s| s.mode)
                .unwrap_or_default()
        };
        match mode {
            SessionMode::Bypass => return Outcome::Allow,
            SessionMode::Plan => {
                // exit_plan_mode self-gates (its own approval dialog).
                if read_only || ctx.tool_name == "exit_plan_mode" {
                    return Outcome::Allow;
                }
                return Outcome::Block {
                    reason: Some("plan mode: edits and commands are disabled".into()),
                    terminate: false,
                };
            }
            SessionMode::AcceptEdits => {
                if read_only || matches!(ctx.tool_name, "edit" | "write") {
                    return Outcome::Allow;
                }
            }
            SessionMode::Ask => {
                if read_only {
                    return Outcome::Allow;
                }
            }
        }
        // Prompt-injection defense (rpc/TUI parity): once untrusted
        // external content entered this run's context, cached "always
        // allow" decisions and allow rules no longer auto-approve
        // mutating tools — the chain "untrusted page → allow-always
        // bash" is the attack. The client is asked instead.
        let untrusted = self
            .untrusted_seen
            .load(std::sync::atomic::Ordering::Relaxed)
            && !read_only;
        // Shared keying (permissions::allow_always_key) so ext__* tools
        // bind the loaded plugin version here too — a plugin upgrade must
        // invalidate stale allow-always entries on every surface.
        let key = crate::permissions::allow_always_key(&self.agent_dir, ctx.tool_name, ctx.args);
        // Declarative allow rules and the session allow-always cache skip
        // the prompt — but not in an untrusted run.
        if !untrusted {
            if self.rules.allow_match(ctx.tool_name, ctx.args).is_some() {
                return Outcome::Allow;
            }
            let cached = {
                let host_guard = self.host.lock().await;
                host_guard
                    .sessions
                    .get(&self.session_id)
                    .is_some_and(|s| s.allow_always.contains(&key))
            };
            if cached {
                return Outcome::Allow;
            }
            // Plugin approval chain: reviewers get first crack at the
            // decision that would otherwise be broadcast to clients —
            // EXCEPT in an untrusted run, where a chain claim must not
            // silently approve a call the human has to see.
            let policy = match mode {
                SessionMode::Ask => "ask",
                SessionMode::AcceptEdits => "acceptEdits",
                SessionMode::Plan => "plan",
                SessionMode::Bypass => "bypass",
            };
            if self
                .approval_chain
                .claims_approval(
                    "remote",
                    ctx.tool_call_id,
                    ctx.tool_name,
                    ctx.args,
                    policy,
                    self.untrusted_seen
                        .load(std::sync::atomic::Ordering::Relaxed),
                    read_only,
                )
                .await
            {
                return Outcome::Allow;
            }
        }
        // Prompt every attached client; the first answer wins.
        let request_id = next_permission_request_id(&self.session_id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let run_cancel = {
            let mut host_guard = self.host.lock().await;
            // Nobody connected: the broadcast reaches zero clients and no
            // answer can ever come — deny immediately instead of parking
            // the run (and its unreapable session) on thin air (F17).
            if host_guard.active_connections == 0 {
                return Outcome::Block {
                    reason: Some(
                        "denied: no client connected to answer the permission prompt".into(),
                    ),
                    terminate: false,
                };
            }
            host_guard.pending_permissions.insert(
                request_id.clone(),
                PendingPermission {
                    session_id: self.session_id.clone(),
                    respond: tx,
                },
            );
            host_guard.broadcast(ServerEvent::PermissionRequest {
                session_id: self.session_id.clone(),
                request_id: request_id.clone(),
                tool_call_id: ctx.tool_call_id.to_string(),
                tool_name: ctx.tool_name.to_string(),
                title: permission_title(ctx.tool_name, ctx.args),
                input: ctx.args.clone(),
            });
            host_guard
                .sessions
                .get(&self.session_id)
                .map(|s| s.cancel.clone())
        };
        // Park for the answer — bounded: a cancelled run returns
        // immediately and an unanswered prompt times out as a deny (F17).
        let answer = match run_cancel {
            Some(cancel) => {
                tokio::select! {
                    answer = rx => Ok(answer),
                    _ = cancel.cancelled() => Err("the run was cancelled"),
                    _ = tokio::time::sleep(PERMISSION_PROMPT_TIMEOUT) => Err("timed out"),
                }
            }
            None => {
                tokio::select! {
                    answer = rx => Ok(answer),
                    _ = tokio::time::sleep(PERMISSION_PROMPT_TIMEOUT) => Err("timed out"),
                }
            }
        };
        let answer = match answer {
            Ok(answer) => answer,
            Err(why) => {
                // Drop the entry ourselves: a late answer must not find a
                // stale prompt (and the map must not leak it).
                self.host
                    .lock()
                    .await
                    .pending_permissions
                    .remove(&request_id);
                return Outcome::Block {
                    reason: Some(format!("permission prompt {why}; denied")),
                    terminate: false,
                };
            }
        };
        match answer {
            Ok(PermissionDecision::AllowOnce) => Outcome::Allow,
            Ok(PermissionDecision::AllowAlways) => {
                let mut host_guard = self.host.lock().await;
                if let Some(session) = host_guard.sessions.get_mut(&self.session_id) {
                    session.allow_always.insert(key);
                }
                drop(host_guard);
                // Persist: the answer survives restarts (permissions.json).
                crate::permissions::persist_allow_always(&self.agent_dir, ctx.tool_name, ctx.args);
                Outcome::Allow
            }
            // Deny is an explicit user answer; a dropped sender means the
            // prompt was closed without one (abort/reap/shutdown/
            // last-client-disconnect) — don't misreport it as user denial.
            Ok(PermissionDecision::Deny) => Outcome::Block {
                reason: Some("denied by user".into()),
                terminate: false,
            },
            Err(_) => Outcome::Block {
                reason: Some(
                    "denied: the permission prompt was closed without an answer \
                     (client disconnected or session reaped)"
                        .into(),
                ),
                terminate: false,
            },
        }
    }
}

/// `tack-ai` model → protocol metadata (for `list_models` / the hello
/// snapshot). `supported_thinking_levels`: every level unless the model's
/// `thinking_level_map` explicitly disables it (null mapping).
fn model_metadata(model: &tack_ai::Model, authenticated: bool) -> ModelMetadata {
    const LEVELS: [(ThinkingLevel, &str); 6] = [
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Xhigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ];
    let supported_thinking_levels = if model.reasoning {
        LEVELS
            .iter()
            .filter(|(_, name)| {
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(*name))
                    .is_none_or(|mapped| mapped.is_some())
            })
            .map(|(level, _)| *level)
            .collect()
    } else {
        Vec::new()
    };
    ModelMetadata {
        provider: model.provider.clone(),
        id: model.id.clone(),
        name: model.name.clone(),
        api: model.api.clone(),
        reasoning: model.reasoning,
        input: model
            .input
            .iter()
            .map(|kind| match kind {
                tack_ai::InputKind::Text => "text".to_string(),
                tack_ai::InputKind::Image => "image".to_string(),
            })
            .collect(),
        context_window: model.context_window,
        max_tokens: model.max_tokens,
        cost: ModelCost {
            input: model.cost.input,
            output: model.cost.output,
            cache_read: model.cost.cache_read,
            cache_write: model.cost.cache_write,
        },
        supported_thinking_levels,
        authenticated,
    }
}

/// Built-in catalog + models.json custom providers, annotated with auth
/// availability (mirrors `tack models`).
fn list_model_metadata(agent_dir: &std::path::Path) -> Vec<ModelMetadata> {
    let mut out = Vec::new();
    for def in tack_ai::providers::BUILTIN_PROVIDERS {
        let authenticated = crate::model::provider_has_auth(agent_dir, def.id);
        for model in tack_ai::providers::builtin_models(def.id).iter() {
            out.push(model_metadata(model, authenticated));
        }
    }
    for custom in tack_ai::providers::load_custom_providers(agent_dir) {
        let authenticated =
            custom.api_key.is_some() || crate::model::provider_has_auth(agent_dir, &custom.id);
        for model in &custom.models {
            out.push(model_metadata(model, authenticated));
        }
    }
    out
}

pub struct SessionHost {
    sessions: HashMap<String, LiveSession>,
    server_id: String,
    revision: u64,
    pub(crate) events: broadcast::Sender<ServerEvent>,
    provider: Arc<dyn Provider>,
    /// What the host-wide provider adapter was built for (api kind /
    /// provider id): run_prompt uses the host adapter while the session's
    /// model matches this pair (covers the startup model and injected
    /// test doubles) and builds a model-specific adapter only when the
    /// session has crossed the boundary via set_model (F32).
    provider_api: String,
    provider_id: String,
    default_model: tack_ai::Model,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    settings: Settings,
    /// Shared-token auth; None = no auth (loopback-only deployments).
    pub(crate) auth_token: Option<String>,
    /// Cap on concurrent client connections (accept-loop backpressure:
    /// past the limit, new connections are dropped at the door).
    pub(crate) conn_permits: Arc<tokio::sync::Semaphore>,
    /// Permission prompts awaiting a client answer, keyed by request id.
    pending_permissions: HashMap<String, PendingPermission>,
    /// Live client connections (every transport; incremented after the
    /// handshake, decremented by `connection_closed`). Drives permission
    /// liveness: with no connection there is nobody to answer a prompt,
    /// so parked prompts are denied instead of parking the run forever.
    pub(crate) active_connections: u32,
    /// tack-ext plugins (loaded once at serve startup, headless services):
    /// hook bridges join every session's chain (plugin rewrites land
    /// before the permission layer, rpc/ACP parity), plugin tools join
    /// every session's tool set, and the approval chain gets first crack
    /// at would-prompt decisions.
    pub(crate) extensions: crate::extension_host::ExtensionManager,
    /// Plugin-initiated UI routed to clients (widgets, dialogs, MCP
    /// elicitation): shared with the plugin host services and the
    /// elicitation handler (see remote/ext_bridge.rs).
    pub(crate) ext_bridge: Arc<super::ext_bridge::RemoteExtBridge>,
    /// Late-bound LLM for MCP sampling (plugin MCP servers connect at
    /// serve startup, before any session exists; the executor resolves
    /// this cell per request). Each session run publishes its own stack —
    /// with concurrent sessions the LAST started run wins (the plugin
    /// connection is host-global; a sampling request cannot be
    /// attributed to a session).
    sampling_llm: crate::mcp_sampling::SharedSamplingLlm,
}

impl std::fmt::Debug for SessionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHost").finish_non_exhaustive()
    }
}

pub(crate) type SharedHost = Arc<Mutex<SessionHost>>;

fn to_protocol_usage(u: &tack_ai::Usage) -> Usage {
    Usage {
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_write: u.cache_write,
        reasoning: u.reasoning,
        total_tokens: u.total_tokens,
        cost: UsageCost {
            input: u.cost.input,
            output: u.cost.output,
            cache_read: u.cost.cache_read,
            cache_write: u.cost.cache_write,
            total: u.cost.total,
        },
    }
}

fn message_to_transcript(id: String, message: &AgentMessage) -> Option<TranscriptItem> {
    match message {
        AgentMessage::User(u) => {
            let content = match &u.content {
                tack_ai::UserContent::Text(t) => vec![UserContent::Text { text: t.clone() }],
                tack_ai::UserContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        tack_ai::InputContentBlock::Text { text, .. } => {
                            UserContent::Text { text: text.clone() }
                        }
                        tack_ai::InputContentBlock::Image { data, mime_type } => {
                            UserContent::Image {
                                data: data.clone(),
                                mime_type: mime_type.clone(),
                            }
                        }
                    })
                    .collect(),
            };
            Some(TranscriptItem::User {
                id,
                content,
                timestamp: u.timestamp,
            })
        }
        AgentMessage::Assistant(a) => {
            let (status, stop_reason, error_message) = match a.stop_reason {
                tack_ai::StopReason::Stop
                | tack_ai::StopReason::ToolUse
                | tack_ai::StopReason::Length => (
                    "complete".to_string(),
                    Some(
                        match a.stop_reason {
                            tack_ai::StopReason::Stop => "stop",
                            tack_ai::StopReason::Length => "length",
                            _ => "toolUse",
                        }
                        .to_string(),
                    ),
                    None,
                ),
                tack_ai::StopReason::Error => (
                    "error".to_string(),
                    Some("error".to_string()),
                    a.error_message.clone(),
                ),
                tack_ai::StopReason::Aborted => (
                    "aborted".to_string(),
                    Some("aborted".to_string()),
                    a.error_message.clone(),
                ),
                tack_ai::StopReason::Pending => ("streaming".to_string(), None, None),
                tack_ai::StopReason::Deferred => {
                    ("complete".to_string(), Some("stop".to_string()), None)
                }
            };
            let content = a
                .content
                .iter()
                .map(|b| match b {
                    tack_ai::ContentBlock::Text { text, .. } => {
                        AssistantContent::Text { text: text.clone() }
                    }
                    tack_ai::ContentBlock::Thinking {
                        thinking, redacted, ..
                    } => AssistantContent::Thinking {
                        thinking: thinking.clone(),
                        redacted: *redacted,
                    },
                    tack_ai::ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => AssistantContent::ToolCall {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        input: arguments.clone(),
                    },
                    tack_ai::ContentBlock::Image { data, mime_type } => AssistantContent::Text {
                        text: format!("[image {mime_type}, {} bytes]", data.len()),
                    },
                })
                .collect();
            Some(TranscriptItem::Assistant {
                id,
                content,
                model: ModelRef {
                    provider: a.provider.clone(),
                    id: a.model.clone(),
                },
                response_model: a.response_model.clone(),
                usage: Some(to_protocol_usage(&a.usage)),
                timestamp: a.timestamp,
                status,
                stop_reason,
                error_message,
            })
        }
        AgentMessage::ToolResult(t) => {
            let content = t
                .content
                .iter()
                .map(|b| match b {
                    tack_ai::InputContentBlock::Text { text, .. } => {
                        UserContent::Text { text: text.clone() }
                    }
                    tack_ai::InputContentBlock::Image { data, mime_type } => UserContent::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    },
                })
                .collect();
            Some(TranscriptItem::Tool {
                id,
                tool_call_id: t.tool_call_id.clone(),
                tool_name: t.tool_name.clone(),
                input: Value::Null,
                content,
                details: t.details.clone(),
                usage: t.usage.as_ref().map(to_protocol_usage),
                timestamp: t.timestamp,
                status: if t.is_error { "error" } else { "complete" }.to_string(),
                is_error: t.is_error,
            })
        }
        _ => None,
    }
}

use serde_json::Value;

fn live_snapshot(session: &LiveSession) -> SessionSnapshot {
    let context = session.manager.build_session_context();
    let transcript: Vec<TranscriptItem> = context
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| message_to_transcript(format!("msg-{i}"), m))
        .collect();
    SessionSnapshot {
        id: session.manager.session_id().to_string(),
        name: None,
        cwd: session.manager.cwd().to_string_lossy().to_string(),
        created_at: session.created_at,
        updated_at: tack_ai::now_millis(),
        phase: session.phase,
        model: ModelRef {
            provider: session.model.provider.clone(),
            id: session.model.id.clone(),
        },
        thinking_level: match session.thinking {
            Some(tack_ai::ThinkingLevel::Minimal) => ThinkingLevel::Minimal,
            Some(tack_ai::ThinkingLevel::Low) => ThinkingLevel::Low,
            Some(tack_ai::ThinkingLevel::Medium) => ThinkingLevel::Medium,
            Some(tack_ai::ThinkingLevel::High) => ThinkingLevel::High,
            Some(tack_ai::ThinkingLevel::Xhigh) => ThinkingLevel::Xhigh,
            Some(tack_ai::ThinkingLevel::Max) => ThinkingLevel::Max,
            None => ThinkingLevel::Off,
        },
        attached: session.attached > 0,
        locked: false,
        revision: session.revision,
        mode: Some(session.mode),
        transcript,
        queued_steer: session
            .queued_steer
            .iter()
            .enumerate()
            .map(|(i, text)| TranscriptItem::User {
                id: format!("queued-steer-{i}"),
                content: vec![UserContent::Text { text: text.clone() }],
                timestamp: tack_ai::now_millis(),
            })
            .collect(),
        queued_steer_count: session.queued_steer.len() as u32,
    }
}

impl SessionHost {
    pub(crate) fn server_snapshot(&self) -> ServerSnapshot {
        ServerSnapshot {
            server_id: self.server_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            revision: self.revision,
            sessions: self
                .sessions
                .values()
                .map(|s| SessionMetadata {
                    id: s.manager.session_id().to_string(),
                    created_at: s.created_at,
                    updated_at: Some(tack_ai::now_millis()),
                    parent_session_id: None,
                    session_name: None,
                    cwd: Some(s.manager.cwd().to_string_lossy().to_string()),
                })
                .collect(),
            models: list_model_metadata(&tack_session::default_agent_dir()),
        }
    }

    fn broadcast(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    /// Test seam for the ext-surface integration tests (widget
    /// registration goes straight into the shared manager).
    #[doc(hidden)]
    pub fn extensions_for_testing(&mut self) -> &mut crate::extension_host::ExtensionManager {
        &mut self.extensions
    }

    async fn run_prompt(host: SharedHost, session_id: String, text: String, steer: bool) {
        let cancel = {
            let mut host_guard = host.lock().await;
            let Some(session) = host_guard.sessions.get_mut(&session_id) else {
                return;
            };
            if steer {
                session.queued_steer.push_back(text.clone());
            }
            session.phase = SessionPhase::Turn;
            // The cancel token was registered by the Prompt command
            // handler BEFORE this task was spawned (F33): an Abort
            // arriving in the spawn window cancels THIS run's token.
            // Re-creating it here would cancel the old token and leave
            // the new run uncancellable.
            session.cancel.clone()
        };

        let agent_dir = tack_session::default_agent_dir();
        // Build the run.
        let (provider, model, thinking, auth, settings, existing, cwd) = {
            let host_guard = host.lock().await;
            let Some(session) = host_guard.sessions.get(&session_id) else {
                // Deleted/aborted between the command and the spawned run.
                return;
            };
            let model = session.model.clone();
            // Use the host adapter while the session's model still
            // matches what it was built for (the startup model — and
            // injected test doubles); build a model-specific adapter +
            // auth only after a set_model crossed the api/provider
            // boundary, so one session's switch can never misroute
            // another session's stream (F32). The adapter is stateless
            // (base_url travels with the model), so building is cheap.
            let matches_host =
                model.api == host_guard.provider_api && model.provider == host_guard.provider_id;
            let provider: std::sync::Arc<dyn tack_ai::Provider> = if matches_host {
                host_guard.provider.clone()
            } else {
                match tack_ai::provider_for(&model) {
                    Some(base) => std::sync::Arc::new(tack_ai::retry::RetryingProvider {
                        inner: base,
                        policy: host_guard.settings.retry.policy(),
                        on_retry_scheduled: None,
                    }),
                    None => {
                        tracing::warn!(
                            api = %model.api,
                            "no adapter for api kind; falling back to the startup provider"
                        );
                        host_guard.provider.clone()
                    }
                }
            };
            // Auth is provider-scoped: reuse the host's resolution when
            // the provider matches, otherwise resolve this model's own.
            let auth = if model.provider == host_guard.provider_id {
                host_guard.auth.clone()
            } else {
                crate::model::resolve_auth(&model.provider, None, &agent_dir)
            };
            // Late-bound MCP sampling (plugin MCP servers) resolves the
            // shared cell per request — publish this run's stack (last
            // writer wins across concurrent sessions, documented on the
            // SessionHost field).
            host_guard.sampling_llm.set(crate::mcp_config::SamplingLlm {
                provider: provider.clone(),
                model: model.clone(),
                auth: auth.clone(),
            });
            (
                provider,
                model,
                session.thinking,
                auth,
                host_guard.settings.clone(),
                session.manager.build_session_context().messages,
                session.manager.cwd().to_path_buf(),
            )
        };
        // tack-ext: plugin hook bridges + approval chain for this run.
        // The manager is shared in-process across sessions (JsonRpcPeer
        // multiplexes concurrent calls); hooks() re-collects per run so a
        // disable/enable between runs takes effect, rpc/TUI parity.
        let (ext_hooks, approval_chain) = {
            let host_guard = host.lock().await;
            (
                host_guard.extensions.hooks(),
                host_guard.extensions.approval_chain(),
            )
        };
        // Sandbox from settings (same wiring as rpc/print): without this a
        // remote client would bypass the configured sandbox entirely.
        let mut services = tack_tools::default_services(cwd.clone())
            .with_memory_dir(settings.memory_directory.clone());
        if let Some(spec) = settings.sandbox_spec(&cwd) {
            services = services.with_sandbox(spec);
        }
        // Shared prompt-injection flag for this run: web/MCP/plugin tool
        // output marks it, the permission gate below reads it (rpc parity).
        let untrusted_seen = services.untrusted_seen.clone();
        let mut tools = tack_tools::create_coding_tools(&services);
        // tack-ext plugin tools (ext__<plugin>__<tool>); MCP-carrier plugins
        // get the untrusted-content defense (rpc/print parity).
        tools.extend({
            let host_guard = host.lock().await;
            host_guard
                .extensions
                .tools_with_untrusted(Some(untrusted_seen.clone()))
        });
        let selected: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        let system_prompt = crate::print_mode::assemble_system_prompt(
            &cwd,
            &agent_dir,
            &settings,
            None,
            &selected,
            &crate::cli_flags::CliFlags::default(),
        );

        // Steering hook draining the session's queue.
        struct SteerHooks {
            host: SharedHost,
            session_id: String,
        }
        impl std::fmt::Debug for SteerHooks {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("SteerHooks").finish()
            }
        }
        #[async_trait::async_trait]
        impl AgentHooks for SteerHooks {
            async fn steering_messages(&self) -> Vec<AgentMessage> {
                let mut host = self.host.lock().await;
                let Some(session) = host.sessions.get_mut(&self.session_id) else {
                    return Vec::new();
                };
                session
                    .queued_steer
                    .drain(..)
                    .map(AgentMessage::user)
                    .collect()
            }
        }

        // Compaction hooks need an Arc<Mutex<SessionManager>>; the host owns
        // it, so compaction runs through a wrapper hook that re-enters the host.
        struct HostCompactionHooks {
            host: SharedHost,
            session_id: String,
            model: tack_ai::Model,
            provider: Arc<dyn Provider>,
            auth: Arc<dyn tack_ai::oauth::AuthResolver>,
            reasoning: Option<tack_ai::ThinkingLevel>,
            settings: tack_session::CompactionSettings,
            cancel: tokio_util::sync::CancellationToken,
        }
        impl std::fmt::Debug for HostCompactionHooks {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("HostCompactionHooks").finish()
            }
        }
        impl HostCompactionHooks {
            /// The compaction core shared by threshold (transform_context)
            /// and overflow (compact_for_overflow) triggers: snapshot →
            /// prepare → summarize → persist → rebuild. `None` on any
            /// failure or when the session vanished mid-flight.
            async fn run_compaction(&self) -> Option<Vec<AgentMessage>> {
                // Snapshot under the lock, then DROP it: compaction is a full
                // LLM call and the host lock is global — holding it would
                // stall every session and every client command.
                let path = {
                    let host = self.host.lock().await;
                    let session = host.sessions.get(&self.session_id)?;
                    session.manager.build_session_path()
                };
                let preparation = tack_session::prepare_compaction(&path, &self.settings)?;
                let auth = match self.auth.resolve().await {
                    Ok(auth) => auth,
                    Err(_) => return None,
                };
                let result = match tack_session::compact(
                    &preparation,
                    &self.model,
                    &self.provider,
                    &auth,
                    None,
                    self.reasoning,
                    None,
                    &self.cancel,
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => return None,
                };
                let kept = &path[path
                    .iter()
                    .position(|e| e.id() == result.first_kept_entry_id)
                    .unwrap_or(path.len())..];
                let retained_tail: Vec<AgentMessage> = kept
                    .iter()
                    .flat_map(tack_session::session_entry_to_context_messages)
                    .collect();
                let mut host = self.host.lock().await;
                let session = host.sessions.get_mut(&self.session_id)?;
                if session
                    .manager
                    .append_compaction(
                        &result.summary,
                        Some(result.first_kept_entry_id.clone()),
                        result.tokens_before,
                        Some(retained_tail),
                        Some(result.details.clone()),
                        Some(result.usage.clone()),
                    )
                    .is_err()
                {
                    return None;
                }
                let leaf = session.manager.leaf_id().map(str::to_string);
                Some(
                    tack_session::build_session_context(
                        &session.manager.entries(),
                        leaf.as_deref(),
                    )
                    .messages,
                )
            }
        }

        #[async_trait::async_trait]
        impl AgentHooks for HostCompactionHooks {
            async fn transform_context(
                &self,
                messages: &[AgentMessage],
            ) -> Option<Vec<AgentMessage>> {
                let estimate = tack_session::estimate_context_tokens(messages);
                if !tack_session::should_compact(
                    estimate.tokens,
                    self.model.context_window as u64,
                    &self.settings,
                ) {
                    return None;
                }
                self.run_compaction().await
            }

            /// Overflow compact-and-retry (upstream `_checkCompaction`
            /// case 1): the provider's overflow error is the trigger, so
            /// the token-threshold gate is skipped.
            async fn compact_for_overflow(&self) -> Option<Vec<AgentMessage>> {
                if !self.settings.enabled {
                    return None;
                }
                self.run_compaction().await
            }
        }

        let hooks: Arc<dyn AgentHooks> = {
            // tack-ext plugin bridges run FIRST (rpc/ACP parity —
            // SECURITY): a plugin `hooks/beforeToolCall` Rewrite lands
            // before the permission layer, so deny rules, the mode gate,
            // the approval chain and the client prompt all see the FINAL,
            // post-rewrite arguments.
            let mut hook_list: Vec<Arc<dyn AgentHooks>> = ext_hooks;
            hook_list.push(Arc::new(HostCompactionHooks {
                host: host.clone(),
                session_id: session_id.clone(),
                model: model.clone(),
                provider: provider.clone(),
                auth: auth.clone(),
                reasoning: thinking,
                settings: settings.compaction,
                cancel: cancel.clone(),
            }));
            hook_list.push(Arc::new(SteerHooks {
                host: host.clone(),
                session_id: session_id.clone(),
            }));
            // permissions.deny applies to remote runs too (same headless
            // safety net as rpc/print — a remote client must not bypass it).
            let permission_rules = crate::permissions::PermissionRules::load(&settings, &agent_dir);
            if !permission_rules.deny.is_empty() {
                hook_list.push(Arc::new(crate::permissions::DenyRulesHooks {
                    rules: permission_rules.clone(),
                }));
            }
            // Permission modes (ask/acceptEdits/plan/bypass) bridged to
            // protocol messages. Sessions default to bypass (pre-extension
            // behavior); clients opt into prompting via set_mode.
            hook_list.push(Arc::new(RemotePermissionHooks {
                host: host.clone(),
                session_id: session_id.clone(),
                rules: permission_rules,
                agent_dir: agent_dir.clone(),
                untrusted_seen,
                approval_chain,
            }));
            Arc::new(HooksChain::new(hook_list))
        };

        let config = AgentLoopConfig {
            fallback_models: crate::model::resolve_fallback_models(
                &settings.fallback_models,
                &model,
                &tack_session::default_agent_dir(),
            ),
            model,
            provider,
            hooks,
            tool_execution: ToolExecutionMode::Parallel,
            reasoning: thinking,
            auth,
            max_tokens: None,
            temperature: None,
            session_id: Some(session_id.clone()),
            cache_retention: settings.cache_retention_mode(),
            tool_pool: Vec::new(),
            retry_cancel: None,
        };
        let context = AgentContext {
            system_prompt: Some(system_prompt),
            messages: existing,
            tools,
        };

        let mut stream = agent_loop(
            vec![AgentMessage::user(text)],
            context,
            config,
            cancel.clone(),
        );

        let mut assistant_msg_ids = std::collections::HashMap::<usize, String>::new();
        let mut current_message_id = String::new();
        while let Some(event) = stream.next().await {
            let progress: Option<TranscriptProgress> = match &event {
                AgentEvent::MessageStart { message } => {
                    current_message_id = format!("{}-{}", session_id, tack_ai::now_millis());
                    assistant_msg_ids.clear();
                    message_to_transcript(current_message_id.clone(), message)
                        .map(|item| TranscriptProgress::ItemStarted { item })
                }
                AgentEvent::MessageUpdate {
                    assistant_message_event,
                    ..
                } => {
                    use tack_ai::AssistantMessageEvent as E;
                    let (content_index, kind, delta) = match assistant_message_event {
                        E::TextDelta {
                            content_index,
                            delta,
                            ..
                        } => (*content_index, "text", delta.clone()),
                        E::ThinkingDelta {
                            content_index,
                            delta,
                            ..
                        } => (*content_index, "thinking", delta.clone()),
                        E::ToolCallDelta {
                            content_index,
                            delta,
                            ..
                        } => (*content_index, "toolCall", delta.clone()),
                        _ => continue,
                    };
                    Some(TranscriptProgress::AssistantDelta {
                        message_id: current_message_id.clone(),
                        content_index: content_index as u32,
                        kind: kind.to_string(),
                        delta,
                    })
                }
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                } => Some(TranscriptProgress::ItemStarted {
                    item: TranscriptItem::Tool {
                        id: tool_call_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        input: args.clone(),
                        content: Vec::new(),
                        details: None,
                        usage: None,
                        timestamp: tack_ai::now_millis(),
                        status: "running".to_string(),
                        is_error: false,
                    },
                }),
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    tool_name,
                    result,
                    is_error,
                } => Some(TranscriptProgress::ItemFinished {
                    item: TranscriptItem::Tool {
                        id: tool_call_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        input: Value::Null,
                        content: result
                            .content
                            .iter()
                            .map(|b| match b {
                                tack_ai::InputContentBlock::Text { text, .. } => {
                                    UserContent::Text { text: text.clone() }
                                }
                                tack_ai::InputContentBlock::Image { data, mime_type } => {
                                    UserContent::Image {
                                        data: data.clone(),
                                        mime_type: mime_type.clone(),
                                    }
                                }
                            })
                            .collect(),
                        details: Some(result.details.clone()),
                        usage: result.usage.as_ref().map(to_protocol_usage),
                        timestamp: tack_ai::now_millis(),
                        status: if *is_error { "error" } else { "complete" }.to_string(),
                        is_error: *is_error,
                    },
                }),
                AgentEvent::MessageEnd { message } => {
                    // Persist + finish item.
                    let mut host_guard = host.lock().await;
                    if let Some(session) = host_guard.sessions.get_mut(&session_id)
                        && !matches!(message, AgentMessage::Custom(_))
                    {
                        if let Err(e) = session.manager.append_message(message.clone()) {
                            tracing::warn!("persist failed: {e}");
                        }
                        session.revision += 1;
                    }
                    drop(host_guard);
                    message_to_transcript(current_message_id.clone(), message)
                        .map(|item| TranscriptProgress::ItemFinished { item })
                }
                AgentEvent::AgentEnd { .. } => {
                    let mut host_guard = host.lock().await;
                    if let Some(session) = host_guard.sessions.get_mut(&session_id) {
                        session.phase = SessionPhase::Idle;
                        session.revision += 1;
                        session.last_active = tack_ai::now_millis();
                        let snapshot = live_snapshot(session);
                        host_guard.broadcast(ServerEvent::SessionSnapshot { snapshot });
                    }
                    None
                }
                _ => None,
            };

            if let Some(progress) = progress {
                let host_guard = host.lock().await;
                host_guard.broadcast(ServerEvent::SessionProgress {
                    session_id: session_id.clone(),
                    progress,
                });
            }
            if event.is_terminal() {
                break;
            }
        }
        let _ = stream.result().await;
    }

    /// Handle one client command. Lock discipline: the host lock is only
    /// held for in-memory map mutations — model resolution and
    /// `SessionManager` file IO (create/append) run OUTSIDE the critical
    /// section, so a slow disk can never stall every session behind one
    /// client's command.
    pub(crate) async fn handle_command(
        host: &SharedHost,
        command: Command,
    ) -> Result<CommandResult, ProtocolError> {
        match command {
            Command::List => {
                let host_guard = host.lock().await;
                let sessions = host_guard
                    .sessions
                    .values()
                    .map(|s| SessionMetadata {
                        id: s.manager.session_id().to_string(),
                        created_at: s.created_at,
                        updated_at: Some(tack_ai::now_millis()),
                        parent_session_id: None,
                        session_name: None,
                        cwd: Some(s.manager.cwd().to_string_lossy().to_string()),
                    })
                    .collect();
                Ok(CommandResult::List { sessions })
            }
            Command::Create {
                cwd,
                name,
                model,
                thinking_level,
            } => {
                let cwd = cwd
                    .map(PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                // File IO (models.json lookup, session file creation)
                // happens before the host lock is taken.
                let default_model = host.lock().await.default_model.clone();
                let model = match model {
                    Some(reference) => crate::model::resolve_model(
                        &reference.provider,
                        Some(&reference.id),
                        &tack_session::default_agent_dir(),
                    )
                    .map_err(|e| ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: e,
                        details: None,
                    })?,
                    None => default_model,
                };
                let manager = SessionManager::create(&cwd, None).map_err(|e| ProtocolError {
                    code: ProtocolErrorCode::InternalError,
                    message: e.to_string(),
                    details: None,
                })?;
                let thinking = thinking_level.map(|l| match l {
                    ThinkingLevel::Minimal => tack_ai::ThinkingLevel::Minimal,
                    ThinkingLevel::Low => tack_ai::ThinkingLevel::Low,
                    ThinkingLevel::Medium => tack_ai::ThinkingLevel::Medium,
                    ThinkingLevel::High => tack_ai::ThinkingLevel::High,
                    ThinkingLevel::Xhigh => tack_ai::ThinkingLevel::Xhigh,
                    ThinkingLevel::Max => tack_ai::ThinkingLevel::Max,
                    ThinkingLevel::Off => tack_ai::ThinkingLevel::Minimal, // off maps to None; placeholder
                });
                let thinking = if matches!(thinking_level, Some(ThinkingLevel::Off) | None) {
                    None
                } else {
                    thinking
                };
                let now = tack_ai::now_millis();
                let mut session = LiveSession {
                    created_at: now,
                    last_active: now,
                    manager,
                    model,
                    thinking,
                    cancel: tokio_util::sync::CancellationToken::new(),
                    phase: SessionPhase::Idle,
                    revision: 0,
                    attached: 0,
                    queued_steer: Default::default(),
                    mode: SessionMode::default(),
                    allow_always: Default::default(),
                };
                let id = session.manager.session_id().to_string();
                if let Some(name) = name
                    && let Err(e) = session.manager.append_session_info(Some(name))
                {
                    tracing::warn!("failed to set session name: {e}");
                }
                let snapshot = live_snapshot(&session);
                let mut host_guard = host.lock().await;
                host_guard.sessions.insert(id, session);
                host_guard.revision += 1;
                Ok(CommandResult::Create { session: snapshot })
            }
            Command::Attach { session_id } => {
                let mut host_guard = host.lock().await;
                let Some(session) = host_guard.sessions.get_mut(&session_id) else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("session not found: {session_id}"),
                        details: None,
                    });
                };
                session.attached += 1;
                session.last_active = tack_ai::now_millis();
                Ok(CommandResult::Attach {
                    session: live_snapshot(session),
                })
            }
            Command::Detach { session_id } => {
                let mut host_guard = host.lock().await;
                let Some(session) = host_guard.sessions.get_mut(&session_id) else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("session not found: {session_id}"),
                        details: None,
                    });
                };
                session.attached = session.attached.saturating_sub(1);
                session.last_active = tack_ai::now_millis();
                Ok(CommandResult::Detach { session_id })
            }
            prompt_or_steer @ (Command::Prompt { .. } | Command::Steer { .. }) => {
                let (session_id, text) = match &prompt_or_steer {
                    Command::Prompt { session_id, text } | Command::Steer { session_id, text } => {
                        (session_id.clone(), text.clone())
                    }
                    _ => unreachable!(),
                };
                let steer = matches!(&prompt_or_steer, Command::Steer { .. });
                let (is_turn, snapshot) = {
                    let mut host_guard = host.lock().await;
                    let Some(session) = host_guard.sessions.get_mut(&session_id) else {
                        return Err(ProtocolError {
                            code: ProtocolErrorCode::NotFound,
                            message: format!("session not found: {session_id}"),
                            details: None,
                        });
                    };
                    session.last_active = tack_ai::now_millis();
                    let is_turn = session.phase != SessionPhase::Idle;
                    if steer || is_turn {
                        // Queue as steering (one-at-a-time not modeled; all).
                        session.queued_steer.push_back(text.clone());
                        (true, live_snapshot(session))
                    } else {
                        // Mark the turn in-progress NOW, under the host lock:
                        // run_prompt is spawned (not awaited), so without this
                        // a second prompt arriving before the spawned task
                        // first locks the host would start a concurrent run
                        // on the same session.
                        session.phase = SessionPhase::Turn;
                        // Register the run's cancellation scope NOW too
                        // (F33): an Abort arriving before the spawned run
                        // first locks the host must cancel THIS run —
                        // previously run_prompt replaced the token itself,
                        // so that Abort cancelled the previous run's token
                        // and the new run was uncancellable.
                        session.cancel = tokio_util::sync::CancellationToken::new();
                        (false, live_snapshot(session))
                    }
                };
                if !is_turn {
                    tokio::spawn(Self::run_prompt(
                        host.clone(),
                        session_id.clone(),
                        text,
                        false,
                    ));
                }
                Ok(if steer {
                    CommandResult::Steer { session: snapshot }
                } else {
                    CommandResult::Prompt { session: snapshot }
                })
            }
            Command::Abort { session_id } => {
                let mut host_guard = host.lock().await;
                let Some(session) = host_guard.sessions.get_mut(&session_id) else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("session not found: {session_id}"),
                        details: None,
                    });
                };
                session.cancel.cancel();
                session.last_active = tack_ai::now_millis();
                let snapshot = live_snapshot(session);
                // Unblock parked permission prompts for this session: the
                // dropped senders make the hook deny the call so the
                // aborted run can unwind instead of hanging on an answer
                // that will never come.
                host_guard
                    .pending_permissions
                    .retain(|_, p| p.session_id != session_id);
                Ok(CommandResult::Abort { session: snapshot })
            }
            Command::SetMode { session_id, mode } => {
                let mut host_guard = host.lock().await;
                let Some(session) = host_guard.sessions.get_mut(&session_id) else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("session not found: {session_id}"),
                        details: None,
                    });
                };
                session.mode = mode;
                session.revision += 1;
                session.last_active = tack_ai::now_millis();
                Ok(CommandResult::SetMode {
                    session: live_snapshot(session),
                })
            }
            Command::PermissionResponse {
                request_id,
                decision,
            } => {
                let mut host_guard = host.lock().await;
                let Some(pending) = host_guard.pending_permissions.remove(&request_id) else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("permission request not found: {request_id}"),
                        details: None,
                    });
                };
                let _ = pending.respond.send(decision);
                Ok(CommandResult::PermissionResponse)
            }
            Command::ListModels => {
                let models = list_model_metadata(&tack_session::default_agent_dir());
                Ok(CommandResult::ListModels { models })
            }
            Command::ListExtCommands => {
                let host_guard = host.lock().await;
                Ok(CommandResult::ListExtCommands {
                    commands: host_guard.extensions.command_specs(),
                })
            }
            Command::InvokeExtCommand { name, args } => {
                // Resolve the invoker under the lock, then invoke
                // OFF-lock: the plugin may itself call back into the
                // host (ui/select → ext_dialog_request) while the
                // invocation runs (the connection loop spawned this
                // handler off-loop for exactly that reason).
                let invoker = {
                    let host_guard = host.lock().await;
                    host_guard.extensions.command_invoker(&name)
                };
                let Some(invoker) = invoker else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("unknown extension command {name:?}"),
                        details: None,
                    });
                };
                let result = invoker
                    .invoke(args.unwrap_or_default())
                    .await
                    .map_err(|e| ProtocolError {
                        code: ProtocolErrorCode::InternalError,
                        message: format!("extension command {name:?} failed: {e}"),
                        details: None,
                    })?;
                Ok(CommandResult::InvokeExtCommand { result })
            }
            Command::ListExtWidgets => {
                let host_guard = host.lock().await;
                Ok(CommandResult::ListExtWidgets {
                    widgets: host_guard
                        .extensions
                        .widgets()
                        .iter()
                        .map(|entry| entry.to_protocol())
                        .collect(),
                })
            }
            Command::ExtWidgetAction {
                key,
                action,
                item_id,
            } => {
                let route = {
                    let host_guard = host.lock().await;
                    host_guard.extensions.widget_action_route(&key)
                };
                let Some(route) = route else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("widget not found (or plugin not running): {key}"),
                        details: None,
                    });
                };
                route.notify(action, item_id).await;
                Ok(CommandResult::ExtWidgetAction)
            }
            Command::ListExtAutocomplete => {
                let host_guard = host.lock().await;
                Ok(CommandResult::ListExtAutocomplete {
                    providers: host_guard
                        .extensions
                        .autocomplete_providers()
                        .iter()
                        .map(|provider| provider.to_protocol())
                        .collect(),
                })
            }
            Command::ExtAutocomplete {
                provider_key,
                query,
                cursor_offset,
            } => {
                // Owned provider clone resolved under the lock; the
                // plugin round-trip itself runs off-lock (contract
                // degradation: unknown key → empty suggestions).
                let provider = {
                    let host_guard = host.lock().await;
                    host_guard
                        .extensions
                        .autocomplete_providers()
                        .into_iter()
                        .find(|p| p.key() == provider_key)
                };
                let suggestions = match &provider {
                    Some(provider) => {
                        provider
                            .provide_protocol(&query, cursor_offset as usize)
                            .await
                    }
                    None => Vec::new(),
                };
                Ok(CommandResult::ExtAutocomplete { suggestions })
            }
            Command::ExtDialogResponse {
                request_id,
                cancelled,
                value,
            } => {
                let bridge = {
                    let host_guard = host.lock().await;
                    host_guard.ext_bridge.clone()
                };
                if !bridge.answer_dialog(&request_id, cancelled, value) {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("ext dialog request not found: {request_id}"),
                        details: None,
                    });
                }
                Ok(CommandResult::ExtDialogResponse)
            }
            Command::Unknown => Err(ProtocolError {
                code: ProtocolErrorCode::NotImplemented,
                message: "unknown command (newer protocol extension?)".to_string(),
                details: None,
            }),
            Command::SetModel { session_id, model } => {
                // Model resolution reads models.json off disk: outside the
                // host lock.
                let new_model = crate::model::resolve_model(
                    &model.provider,
                    Some(&model.id),
                    &tack_session::default_agent_dir(),
                )
                .map_err(|e| ProtocolError {
                    code: ProtocolErrorCode::InvalidRequest,
                    message: e,
                    details: None,
                })?;
                let mut host_guard = host.lock().await;
                if !host_guard.sessions.contains_key(&session_id) {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("session not found: {session_id}"),
                        details: None,
                    });
                }
                // The model is per-session; run_prompt resolves the
                // provider adapter + auth from the session's model at run
                // time, so a set_model here neither requires a host-wide
                // rebind nor can it steer other sessions' streams (F32).
                let session = host_guard
                    .sessions
                    .get_mut(&session_id)
                    .expect("checked above");
                session.model = new_model;
                let _ = session
                    .manager
                    .append_model_change(&model.provider, &model.id);
                session.revision += 1;
                session.last_active = tack_ai::now_millis();
                Ok(CommandResult::SetModel {
                    session: live_snapshot(session),
                })
            }
            Command::SetThinking {
                session_id,
                thinking_level,
            } => {
                let mut host_guard = host.lock().await;
                let Some(session) = host_guard.sessions.get_mut(&session_id) else {
                    return Err(ProtocolError {
                        code: ProtocolErrorCode::NotFound,
                        message: format!("session not found: {session_id}"),
                        details: None,
                    });
                };
                session.thinking = match thinking_level {
                    ThinkingLevel::Off => None,
                    ThinkingLevel::Minimal => Some(tack_ai::ThinkingLevel::Minimal),
                    ThinkingLevel::Low => Some(tack_ai::ThinkingLevel::Low),
                    ThinkingLevel::Medium => Some(tack_ai::ThinkingLevel::Medium),
                    ThinkingLevel::High => Some(tack_ai::ThinkingLevel::High),
                    ThinkingLevel::Xhigh => Some(tack_ai::ThinkingLevel::Xhigh),
                    ThinkingLevel::Max => Some(tack_ai::ThinkingLevel::Max),
                };
                let level = match thinking_level {
                    ThinkingLevel::Off => "off",
                    ThinkingLevel::Minimal => "minimal",
                    ThinkingLevel::Low => "low",
                    ThinkingLevel::Medium => "medium",
                    ThinkingLevel::High => "high",
                    ThinkingLevel::Xhigh => "xhigh",
                    ThinkingLevel::Max => "max",
                };
                let _ = session.manager.append_thinking_level_change(level);
                session.revision += 1;
                session.last_active = tack_ai::now_millis();
                Ok(CommandResult::SetThinking {
                    session: live_snapshot(session),
                })
            }
        }
    }

    /// Connection teardown, called on EVERY exit path of the per-connection
    /// loop (clean EOF, io error, event-pump shutdown). Releases everything
    /// the connection still held (F07): sessions it attached without a
    /// matching detach — a dropped connection used to leave `attached`
    /// inflated forever, making those sessions immune to the idle reaper —
    /// and, when the LAST client went away, every parked permission prompt
    /// (dropping the senders makes the hooks deny; nobody can answer).
    pub(crate) async fn connection_closed(
        host: &SharedHost,
        attached: &std::collections::HashMap<String, u32>,
        dialog_capable: bool,
    ) {
        let mut host_guard = host.lock().await;
        host_guard.active_connections = host_guard.active_connections.saturating_sub(1);
        host_guard.ext_bridge.client_disconnected(dialog_capable);
        for (session_id, count) in attached {
            if let Some(session) = host_guard.sessions.get_mut(session_id) {
                session.attached = session.attached.saturating_sub(*count);
                // The idle clock starts at disconnect: an abandoned
                // session becomes reapable one TTL from now.
                session.last_active = tack_ai::now_millis();
            }
        }
        if host_guard.active_connections == 0 {
            host_guard.pending_permissions.clear();
        }
    }

    /// Remove sessions that are detached, idle (no run in flight) and have
    /// not been touched for `max_idle`. Returns the removed ids; clients
    /// are told via `ServerEvent::SessionRemoved`.
    fn reap_idle_sessions(&mut self, max_idle: std::time::Duration) -> Vec<String> {
        let now = tack_ai::now_millis();
        let max_idle_ms = u64::try_from(max_idle.as_millis()).unwrap_or(u64::MAX);
        let removed: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.attached == 0
                    && s.phase == SessionPhase::Idle
                    && now.saturating_sub(s.last_active) > max_idle_ms
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &removed {
            if let Some(session) = self.sessions.remove(id) {
                session.cancel.cancel();
                self.revision += 1;
                // Deny parked permission prompts of the reaped session.
                self.pending_permissions.retain(|_, p| p.session_id != *id);
                self.broadcast(ServerEvent::SessionRemoved {
                    session_id: id.clone(),
                });
            }
        }
        removed
    }
}

/// `tack serve --listen <addr>`: host sessions over framed CBOR.
/// Addresses: `tcp:127.0.0.1:7749` (default), `ws:127.0.0.1:7749`
/// (WebSocket + embedded web client, see remote_ws.rs), `unix:/path/to.sock`
/// (unix / named-pipe-on-Windows via tokio uds).
/// Build a host (shared by `serve` and tests).
/// `ext_bridge` is created by the caller (the plugin services and the
/// elicitation handler are wired with it BEFORE the host exists) and
/// shared here; `RemoteExtBridge::attach` finishes the wiring.
#[allow(clippy::too_many_arguments)]
pub fn build_host(
    provider: Arc<dyn Provider>,
    default_model: tack_ai::Model,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    settings: Settings,
    auth_token: Option<String>,
    extensions: crate::extension_host::ExtensionManager,
    sampling_llm: crate::mcp_sampling::SharedSamplingLlm,
    ext_bridge: Arc<super::ext_bridge::RemoteExtBridge>,
) -> Arc<Mutex<SessionHost>> {
    let (events, _) = broadcast::channel(512);
    Arc::new(Mutex::new(SessionHost {
        sessions: HashMap::new(),
        server_id: format!("server-{}", tack_ai::now_millis()),
        revision: 0,
        events,
        provider,
        provider_api: default_model.api.clone(),
        provider_id: default_model.provider.clone(),
        default_model,
        auth,
        settings,
        auth_token,
        conn_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
        pending_permissions: HashMap::new(),
        active_connections: 0,
        extensions,
        ext_bridge,
        sampling_llm,
    }))
}

/// Idle detached sessions are reaped after this long without any command
/// touching them (the map was previously insert-only).
const SESSION_IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How often the reaper scans for idle sessions.
const SESSION_REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Periodically reap idle detached sessions so `sessions` doesn't grow
/// without bound over the server's lifetime.
pub fn spawn_session_reaper(host: &Arc<Mutex<SessionHost>>) -> tokio::task::JoinHandle<()> {
    let host = host.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SESSION_REAP_INTERVAL);
        // First tick fires immediately; skip it (nothing can be idle yet).
        tick.tick().await;
        loop {
            tick.tick().await;
            let removed = host.lock().await.reap_idle_sessions(SESSION_IDLE_TTL);
            if !removed.is_empty() {
                tracing::info!(
                    "reaped {} idle session(s): {}",
                    removed.len(),
                    removed.join(", ")
                );
            }
        }
    })
}

#[cfg(test)]
#[allow(unsafe_code, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::remote::testutil::*;

    /// Session in Ask mode (permission prompts active), created through
    /// the real command path.
    async fn ask_mode_session(host: &Arc<Mutex<SessionHost>>) -> String {
        let cwd = tempfile::tempdir().unwrap();
        let result = SessionHost::handle_command(
            host,
            Command::Create {
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                name: None,
                model: None,
                thinking_level: None,
            },
        )
        .await
        .unwrap();
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        host.lock()
            .await
            .sessions
            .get_mut(&session.id)
            .expect("session stored")
            .mode = SessionMode::Ask;
        session.id
    }

    fn perm_ctx<'a>(
        message: &'a tack_ai::AssistantMessage,
        args: &'a serde_json::Value,
    ) -> tack_agent_core::hooks::BeforeToolCallContext<'a> {
        tack_agent_core::hooks::BeforeToolCallContext {
            assistant_message: message,
            tool_call_id: "call-1",
            tool_name: "bash",
            args,
            context: &[],
        }
    }

    #[derive(Debug)]
    struct ClaimReviewer(crate::approval::ChainAction);

    #[async_trait::async_trait]
    impl crate::approval::ApprovalReviewer for ClaimReviewer {
        async fn review(
            &self,
            request: &crate::approval::ApprovalRequest,
        ) -> Option<crate::approval::ChainDecision> {
            assert_eq!(request.approval_policy, "ask");
            assert_eq!(request.tool_name, "bash");
            assert_eq!(request.evidence["surface"], "remote");
            Some(crate::approval::ChainDecision {
                action: self.0,
                reason: None,
            })
        }
    }

    fn permission_hooks(
        host: &Arc<Mutex<SessionHost>>,
        session_id: &str,
        untrusted: bool,
        approval_chain: crate::approval::ApprovalChain,
    ) -> RemotePermissionHooks {
        RemotePermissionHooks {
            host: host.clone(),
            session_id: session_id.to_string(),
            rules: crate::permissions::PermissionRules {
                allow: vec![crate::permissions::Rule::parse("Bash(cargo test)").unwrap()],
                deny: vec![],
            },
            agent_dir: std::path::PathBuf::new(),
            untrusted_seen: Arc::new(std::sync::atomic::AtomicBool::new(untrusted)),
            approval_chain,
        }
    }

    /// Plugin approval chain in remote sessions: a claim approves the
    /// call WITHOUT broadcasting a PermissionRequest to clients (rpc/TUI
    /// parity).
    #[tokio::test]
    async fn approval_chain_claim_skips_permission_broadcast() {
        let host = test_host(vec![]);
        let session_id = ask_mode_session(&host).await;
        let mut events = host.lock().await.events.subscribe();
        let mut chain = crate::approval::ApprovalChain::empty();
        chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        let hooks = permission_hooks(&host, &session_id, false, chain);
        let message = tack_ai::AssistantMessage::pending(&test_model());
        // Matches neither the allow rule nor the cache: the built-in
        // flow WOULD broadcast a prompt.
        let args = serde_json::json!({"command": "rm -rf build"});
        let outcome = hooks.before_tool_call(&perm_ctx(&message, &args)).await;
        assert!(matches!(
            outcome,
            tack_agent_core::hooks::BeforeToolCallOutcome::Allow
        ));
        assert!(
            events.try_recv().is_err(),
            "no PermissionRequest broadcast expected"
        );
    }

    /// Prompt-injection defense: in an untrusted run, allow rules AND the
    /// plugin approval chain are both bypassed — with no client
    /// connected to answer, the would-be prompt denies immediately.
    #[tokio::test]
    async fn untrusted_run_skips_allow_rules_and_chain() {
        let host = test_host(vec![]);
        let session_id = ask_mode_session(&host).await;
        let mut chain = crate::approval::ApprovalChain::empty();
        chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        let hooks = permission_hooks(&host, &session_id, true, chain);
        let message = tack_ai::AssistantMessage::pending(&test_model());
        // "cargo test" matches the allow rule — trusted runs would allow.
        let args = serde_json::json!({"command": "cargo test"});
        let outcome = hooks.before_tool_call(&perm_ctx(&message, &args)).await;
        assert!(matches!(
            outcome,
            tack_agent_core::hooks::BeforeToolCallOutcome::Block { .. }
        ));
    }

    /// Regression: the session name used to be applied via
    /// `sessions.get_mut(&id)` BEFORE the session was inserted — always a
    /// miss, so the name was silently dropped.
    #[tokio::test]
    async fn create_applies_session_name() {
        let host = test_host(vec![]);
        let cwd = tempfile::tempdir().unwrap();
        let result = SessionHost::handle_command(
            &host,
            Command::Create {
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                name: Some("my-session".to_string()),
                model: None,
                thinking_level: None,
            },
        )
        .await
        .unwrap();
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        let guard = host.lock().await;
        let live = guard.sessions.get(&session.id).expect("session stored");
        assert!(
            live.manager.entries().iter().any(|e| matches!(
                e,
                tack_session::SessionEntry::SessionInfo { name: Some(n), .. } if n == "my-session"
            )),
            "session_info entry with the name must exist"
        );
    }

    /// Regression: the turn phase is marked under the host lock, so a second
    /// prompt arriving before the spawned run task first locks the host is
    /// queued as steering instead of starting a concurrent run on the same
    /// session.
    #[tokio::test]
    async fn concurrent_prompts_are_serialized() {
        let host = test_host(vec![assistant_text("first"), assistant_text("second")]);
        let cwd = tempfile::tempdir().unwrap();
        let result = SessionHost::handle_command(
            &host,
            Command::Create {
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                name: None,
                model: None,
                thinking_level: None,
            },
        )
        .await
        .unwrap();
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        let session_id = session.id.clone();

        // First prompt: starts a run (phase flips to Turn synchronously).
        let first = SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "one".into(),
            },
        )
        .await
        .unwrap();
        let CommandResult::Prompt { session: snap } = &first else {
            panic!()
        };
        assert_eq!(snap.phase, SessionPhase::Turn);
        assert_eq!(snap.queued_steer_count, 0);

        // Second prompt immediately after: must queue, not spawn a run.
        let second = SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "two".into(),
            },
        )
        .await
        .unwrap();
        let CommandResult::Prompt { session: snap } = &second else {
            panic!()
        };
        assert_eq!(
            snap.queued_steer_count, 1,
            "second prompt must be queued as steering"
        );
        assert_eq!(snap.queued_steer.len(), 1);

        // The run drains the queue and returns to Idle.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            {
                let guard = host.lock().await;
                if guard.sessions[&session_id].phase == SessionPhase::Idle {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run never returned to Idle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// F07 regression: a connection that attached and then dropped (EOF)
    /// without detaching used to leave `attached` inflated forever — the
    /// session was immune to the idle reaper and lived for the server's
    /// lifetime. Connection teardown must release every attachment the
    /// connection still held, after which the reaper collects the
    /// session.
    #[tokio::test]
    async fn disconnect_releases_session_attachments() {
        let host = test_host(vec![]);
        let session_id = create_session(&host).await;
        let mut io = crate::remote::MockIo::new(vec![
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                token: None,
                capabilities: Vec::new(),
            },
            ClientMessage::Request {
                id: "a1".into(),
                request: Command::Attach {
                    session_id: session_id.clone(),
                },
            },
            ClientMessage::Request {
                id: "a2".into(),
                request: Command::Attach {
                    session_id: session_id.clone(),
                },
            },
            ClientMessage::Request {
                id: "d1".into(),
                request: Command::Detach {
                    session_id: session_id.clone(),
                },
            },
            // EOF: one attachment (of two) still held, never detached.
        ]);
        crate::remote::handle_frames(&mut io, &host).await.unwrap();

        let mut guard = host.lock().await;
        assert_eq!(guard.active_connections, 0, "connection deregistered");
        {
            let session = guard.sessions.get(&session_id).expect("session alive");
            assert_eq!(
                session.attached, 0,
                "disconnect must release the remaining attachment"
            );
        }
        // ... and the idle reaper can now collect the session.
        guard
            .sessions
            .get_mut(&session_id)
            .expect("session alive")
            .last_active = 0;
        let removed = guard.reap_idle_sessions(std::time::Duration::from_secs(3600));
        assert_eq!(removed, vec![session_id.clone()]);
    }

    /// F07/F17: a failed attach (unknown session) must not be tracked —
    /// teardown of that connection touches nothing.
    #[tokio::test]
    async fn failed_attach_is_not_released_on_disconnect() {
        let host = test_host(vec![]);
        let mut io = crate::remote::MockIo::new(vec![
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                token: None,
                capabilities: Vec::new(),
            },
            ClientMessage::Request {
                id: "a1".into(),
                request: Command::Attach {
                    session_id: "no-such-session".into(),
                },
            },
        ]);
        crate::remote::handle_frames(&mut io, &host).await.unwrap();
        let guard = host.lock().await;
        assert_eq!(guard.active_connections, 0);
        // The attach response was an error, and teardown was a no-op.
        assert!(
            io.written()
                .iter()
                .any(|m| matches!(m, ServerMessage::Response { ok: false, .. }))
        );
    }

    /// F07/F17: when the LAST client disconnects, parked permission
    /// prompts are denied (the dropped senders unblock the hook) instead
    /// of parking the run until the timeout.
    #[tokio::test]
    async fn last_disconnect_denies_pending_permissions() {
        let host = test_host(vec![]);
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut guard = host.lock().await;
            guard.active_connections = 1;
            guard.pending_permissions.insert(
                "perm-x".into(),
                PendingPermission {
                    session_id: "s".into(),
                    respond: tx,
                },
            );
        }
        SessionHost::connection_closed(&host, &Default::default(), false).await;
        assert!(
            rx.await.is_err(),
            "the parked hook's sender must be dropped (deny)"
        );
        assert!(host.lock().await.pending_permissions.is_empty());
    }

    /// Regression (F33): an Abort landing in the window between the
    /// Prompt command and the spawned run starting used to cancel the OLD
    /// run's token — the new run replaced it on startup and ran on
    /// uncancellable. The Prompt handler now registers the token, so
    /// Abort cancels the exact token the run will use.
    #[tokio::test]
    async fn abort_before_run_start_cancels_the_new_run() {
        let host = test_host(vec![assistant_text("hi")]);
        let session_id = create_session(&host).await;
        SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "go".into(),
            },
        )
        .await
        .unwrap();
        SessionHost::handle_command(
            &host,
            Command::Abort {
                session_id: session_id.clone(),
            },
        )
        .await
        .unwrap();
        let token = { host.lock().await.sessions[&session_id].cancel.clone() };
        assert!(
            token.is_cancelled(),
            "abort must cancel the token the spawned run will use"
        );
    }

    fn assistant_tool_call(
        id: &str,
        name: &str,
        args: serde_json::Value,
    ) -> tack_ai::AssistantMessage {
        let mut m = tack_ai::AssistantMessage::pending(&test_model());
        m.stop_reason = tack_ai::StopReason::ToolUse;
        m.content = vec![tack_ai::ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
            thought_signature: None,
            namespace: None,
        }];
        m
    }

    /// Security regression: remote runs used to build tools from bare
    /// `default_services` with no DenyRulesHooks, so a remote client could
    /// run commands the operator denied in settings. The deny rule must
    /// block the tool call before execution (same as rpc/print).
    #[tokio::test]
    async fn run_prompt_enforces_permission_deny_rules() {
        test_agent_dir();
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            scripts: std::sync::Mutex::new(vec![
                assistant_tool_call(
                    "call-1",
                    "bash",
                    serde_json::json!({"command": "rm -rf /definitely/denied"}),
                ),
                assistant_text("done"),
            ]),
        });
        let mut settings = Settings::default();
        settings.permission_deny = vec!["Bash(rm *)".to_string()];
        let host = build_host(
            provider,
            test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string()))),
            settings,
            None,
            crate::extension_host::ExtensionManager::default(),
            crate::mcp_sampling::SharedSamplingLlm::default(),
            crate::remote::ext_bridge::RemoteExtBridge::new(),
        );
        let mut events = host.lock().await.events.subscribe();

        let cwd = tempfile::tempdir().unwrap();
        let result = SessionHost::handle_command(
            &host,
            Command::Create {
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                name: None,
                model: None,
                thinking_level: None,
            },
        )
        .await
        .unwrap();
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session.id.clone(),
                text: "clean up".into(),
            },
        )
        .await
        .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut denial: Option<String> = None;
        loop {
            assert!(std::time::Instant::now() < deadline, "turn never finished");
            let event = tokio::time::timeout(std::time::Duration::from_secs(15), events.recv())
                .await
                .expect("event timeout")
                .expect("event stream closed");
            match event {
                ServerEvent::SessionProgress {
                    progress:
                        TranscriptProgress::ItemFinished {
                            item:
                                TranscriptItem::Tool {
                                    is_error: true,
                                    content,
                                    ..
                                },
                        },
                    ..
                } => {
                    denial = content.iter().find_map(|c| {
                        let UserContent::Text { text } = c else {
                            return None;
                        };
                        Some(text.clone())
                    });
                }
                ServerEvent::SessionSnapshot { snapshot }
                    if snapshot.phase == SessionPhase::Idle =>
                {
                    break;
                }
                _ => {}
            }
        }
        let denial = denial.expect("the denied tool call must surface as an error tool item");
        assert!(
            denial.contains("denied by permissions.deny rule"),
            "unexpected tool result: {denial}"
        );
    }

    // ------------------------------------------------------------------
    // Permission modes + permission_request bridge
    // ------------------------------------------------------------------

    /// Host with the OS sandbox off: sandbox-exec is unavailable in some
    /// test environments, and these tests assert on the permission layer,
    /// not on sandboxing (covered by run_prompt_enforces_permission_deny_rules).
    fn test_host_no_sandbox(scripts: Vec<tack_ai::AssistantMessage>) -> Arc<Mutex<SessionHost>> {
        test_agent_dir();
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            scripts: std::sync::Mutex::new(scripts),
        });
        let mut settings = Settings::default();
        settings.sandbox = false;
        build_host(
            provider,
            test_model(),
            Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string()))),
            settings,
            None,
            crate::extension_host::ExtensionManager::default(),
            crate::mcp_sampling::SharedSamplingLlm::default(),
            crate::remote::ext_bridge::RemoteExtBridge::new(),
        )
    }

    /// Create a session and return its id.
    async fn create_session(host: &Arc<Mutex<SessionHost>>) -> String {
        let cwd = tempfile::tempdir().unwrap();
        let result = SessionHost::handle_command(
            host,
            Command::Create {
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                name: None,
                model: None,
                thinking_level: None,
            },
        )
        .await
        .unwrap();
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        session.id
    }

    /// Collect events until the session is idle again; returns every
    /// permission prompt title seen and every finished tool item.
    async fn run_until_idle(
        events: &mut broadcast::Receiver<ServerEvent>,
        host: &Arc<Mutex<SessionHost>>,
        session_id: &str,
        decision: Option<PermissionDecision>,
    ) -> (Vec<String>, Vec<TranscriptItem>) {
        let mut permission_titles = Vec::new();
        let mut finished_tools = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "turn never finished");
            let event = tokio::time::timeout(std::time::Duration::from_secs(20), events.recv())
                .await
                .expect("event timeout")
                .expect("event stream closed");
            match event {
                ServerEvent::PermissionRequest {
                    session_id: sid,
                    request_id,
                    title,
                    ..
                } if sid == session_id => {
                    permission_titles.push(title);
                    if let Some(decision) = decision {
                        SessionHost::handle_command(
                            host,
                            Command::PermissionResponse {
                                request_id,
                                decision,
                            },
                        )
                        .await
                        .unwrap();
                    }
                }
                ServerEvent::SessionProgress {
                    session_id: sid,
                    progress:
                        TranscriptProgress::ItemFinished {
                            item: item @ TranscriptItem::Tool { .. },
                        },
                } if sid == session_id => finished_tools.push(item),
                ServerEvent::SessionSnapshot { snapshot }
                    if snapshot.id == session_id && snapshot.phase == SessionPhase::Idle =>
                {
                    break;
                }
                _ => {}
            }
        }
        (permission_titles, finished_tools)
    }

    fn bash_echo_script() -> Vec<tack_ai::AssistantMessage> {
        vec![
            assistant_tool_call("call-1", "bash", serde_json::json!({"command": "echo hi"})),
            assistant_text("done"),
        ]
    }

    /// set_mode updates the session mode and is reflected in the snapshot;
    /// unknown sessions / unknown permission request ids error.
    #[tokio::test]
    async fn set_mode_updates_snapshot() {
        let host = test_host(vec![]);
        let session_id = create_session(&host).await;

        // Default is bypass (pre-extension behavior: no prompts).
        {
            let guard = host.lock().await;
            assert_eq!(guard.sessions[&session_id].mode, SessionMode::Bypass);
        }

        let result = SessionHost::handle_command(
            &host,
            Command::SetMode {
                session_id: session_id.clone(),
                mode: SessionMode::Ask,
            },
        )
        .await
        .unwrap();
        let CommandResult::SetMode { session } = result else {
            panic!("expected set_mode result")
        };
        assert_eq!(session.mode, Some(SessionMode::Ask));
        {
            let guard = host.lock().await;
            assert_eq!(guard.sessions[&session_id].mode, SessionMode::Ask);
        }

        let err = SessionHost::handle_command(
            &host,
            Command::SetMode {
                session_id: "nope".into(),
                mode: SessionMode::Plan,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::NotFound);

        let err = SessionHost::handle_command(
            &host,
            Command::PermissionResponse {
                request_id: "perm-nope".into(),
                decision: PermissionDecision::AllowOnce,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::NotFound);
    }

    fn tool_text(item: &TranscriptItem) -> Option<String> {
        let TranscriptItem::Tool { content, .. } = item else {
            return None;
        };
        content.iter().find_map(|c| {
            let UserContent::Text { text } = c else {
                return None;
            };
            Some(text.clone())
        })
    }

    /// Default (bypass) mode: mutating tools run without any prompt —
    /// pre-extension clients see no new events.
    #[tokio::test]
    async fn bypass_mode_never_prompts() {
        let host = test_host_no_sandbox(bash_echo_script());
        let mut events = host.lock().await.events.subscribe();
        let session_id = create_session(&host).await;
        SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "go".into(),
            },
        )
        .await
        .unwrap();
        let (prompts, tools) = run_until_idle(&mut events, &host, &session_id, None).await;
        assert!(prompts.is_empty(), "bypass must not prompt");
        // The tool reached execution (not blocked by the permission
        // layer). Execution itself may fail in minimal test environments
        // (no /bin/bash) — that is not the permission gate's doing.
        let text = tool_text(&tools[0]).unwrap_or_default();
        assert!(
            !text.contains("denied by user") && !text.contains("plan mode"),
            "bypass must not block: {text}"
        );
    }

    /// ask mode: a mutating tool call surfaces as a PermissionRequest
    /// event; answering allow_once lets it run.
    #[tokio::test]
    async fn ask_mode_prompts_and_allow_once_runs_tool() {
        let host = test_host_no_sandbox(bash_echo_script());
        // Simulate a connected client: with zero connections the
        // permission hook denies outright (F17 — nobody could answer).
        host.lock().await.active_connections = 1;
        let mut events = host.lock().await.events.subscribe();
        let session_id = create_session(&host).await;
        SessionHost::handle_command(
            &host,
            Command::SetMode {
                session_id: session_id.clone(),
                mode: SessionMode::Ask,
            },
        )
        .await
        .unwrap();
        SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "go".into(),
            },
        )
        .await
        .unwrap();
        let (prompts, tools) = run_until_idle(
            &mut events,
            &host,
            &session_id,
            Some(PermissionDecision::AllowOnce),
        )
        .await;
        assert_eq!(prompts.len(), 1, "exactly one prompt");
        assert!(prompts[0].contains("bash"), "title: {}", prompts[0]);
        // allow_once: the permission gate did not block the call
        // (execution itself may fail in minimal test environments).
        let text = tool_text(&tools[0]).unwrap_or_default();
        assert!(
            !text.contains("denied by user") && !text.contains("plan mode"),
            "allowed tool must not be blocked: {text}"
        );
        // The pending request was consumed by the answer.
        assert!(host.lock().await.pending_permissions.is_empty());
    }

    /// ask mode: denying the prompt blocks the tool with a user-denial
    /// error (same shape as the TUI dialog's No).
    #[tokio::test]
    async fn ask_mode_deny_blocks_tool() {
        let host = test_host_no_sandbox(bash_echo_script());
        // Simulate a connected client (see above).
        host.lock().await.active_connections = 1;
        let mut events = host.lock().await.events.subscribe();
        let session_id = create_session(&host).await;
        SessionHost::handle_command(
            &host,
            Command::SetMode {
                session_id: session_id.clone(),
                mode: SessionMode::Ask,
            },
        )
        .await
        .unwrap();
        SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "go".into(),
            },
        )
        .await
        .unwrap();
        let (prompts, tools) = run_until_idle(
            &mut events,
            &host,
            &session_id,
            Some(PermissionDecision::Deny),
        )
        .await;
        assert_eq!(prompts.len(), 1);
        let TranscriptItem::Tool { is_error, .. } = &tools[0] else {
            panic!("expected tool item")
        };
        assert!(is_error, "denied tool must error: {tools:?}");
        assert_eq!(tool_text(&tools[0]).as_deref(), Some("denied by user"));
    }

    /// F17: with no client connected a permission prompt can never be
    /// answered — the hook must deny immediately instead of parking the
    /// run on thin air (previously: indefinite park, only Abort unwound
    /// it). The tool surfaces the deny reason as an error item.
    #[tokio::test]
    async fn ask_mode_without_clients_denies_without_prompt() {
        let host = test_host_no_sandbox(bash_echo_script());
        let mut events = host.lock().await.events.subscribe();
        let session_id = create_session(&host).await;
        SessionHost::handle_command(
            &host,
            Command::SetMode {
                session_id: session_id.clone(),
                mode: SessionMode::Ask,
            },
        )
        .await
        .unwrap();
        SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "go".into(),
            },
        )
        .await
        .unwrap();
        let (prompts, tools) = run_until_idle(&mut events, &host, &session_id, None).await;
        assert!(prompts.is_empty(), "no client: nothing to prompt");
        let text = tool_text(&tools[0]).unwrap_or_default();
        assert!(
            text.contains("no client connected"),
            "unexpected block reason: {text}"
        );
    }

    /// plan mode: mutating tools are blocked WITHOUT a prompt.
    #[tokio::test]
    async fn plan_mode_blocks_mutations_without_prompt() {
        let host = test_host_no_sandbox(bash_echo_script());
        let mut events = host.lock().await.events.subscribe();
        let session_id = create_session(&host).await;
        SessionHost::handle_command(
            &host,
            Command::SetMode {
                session_id: session_id.clone(),
                mode: SessionMode::Plan,
            },
        )
        .await
        .unwrap();
        SessionHost::handle_command(
            &host,
            Command::Prompt {
                session_id: session_id.clone(),
                text: "go".into(),
            },
        )
        .await
        .unwrap();
        let (prompts, tools) = run_until_idle(&mut events, &host, &session_id, None).await;
        assert!(prompts.is_empty(), "plan mode blocks without prompting");
        let TranscriptItem::Tool { is_error, .. } = &tools[0] else {
            panic!("expected tool item")
        };
        assert!(is_error);
        let text = tool_text(&tools[0]);
        assert!(
            text.as_deref().is_some_and(|t| t.contains("plan mode")),
            "unexpected block reason: {text:?}"
        );
    }

    /// F32 regression: set_model across api kinds used to keep the
    /// startup adapter — the new model streamed through the wrong adapter
    /// and failed with "No API key for provider: <model.provider>" (the
    /// TUI/RPC instances of this bug: 3eaf5b4 / 7e41cde). run_prompt now
    /// resolves the adapter + auth from the SESSION's model per run, so a
    /// cross-api set_model takes effect without touching host-global
    /// state — and cannot misroute other sessions.
    #[tokio::test]
    async fn set_model_leaves_host_provider_untouched() {
        let host = test_host(vec![]);
        let session_id = create_session(&host).await;
        let new_model = tack_ai::providers::builtin_models("openai")
            .iter()
            .find(|m| m.api != "anthropic-messages")
            .expect("an openai model with a non-anthropic api kind")
            .clone();
        let old_provider = { host.lock().await.provider.clone() };
        let old_auth = { host.lock().await.auth.clone() };
        let result = SessionHost::handle_command(
            &host,
            Command::SetModel {
                session_id: session_id.clone(),
                model: ModelRef {
                    provider: new_model.provider.clone(),
                    id: new_model.id.clone(),
                },
            },
        )
        .await
        .unwrap();
        let CommandResult::SetModel { session } = result else {
            panic!("expected set_model result")
        };
        assert_eq!(session.model.id, new_model.id);
        let guard = host.lock().await;
        // Host-global adapter/auth stay the startup pair; per-run
        // resolution in run_prompt picks up the new model's api/provider.
        assert!(Arc::ptr_eq(&old_provider, &guard.provider));
        assert!(Arc::ptr_eq(&old_auth, &guard.auth));
        assert_eq!(
            guard
                .sessions
                .get(&session_id)
                .expect("session exists")
                .model
                .api,
            new_model.api
        );
    }

    /// F32: a same-provider/same-api set_model keeps the startup adapter
    /// (no gratuitous rebuild).
    #[tokio::test]
    async fn set_model_same_api_keeps_adapter() {
        let host = test_host(vec![]);
        let session_id = create_session(&host).await;
        let sibling = tack_ai::providers::builtin_models("anthropic")
            .iter()
            .find(|m| m.api == "anthropic-messages")
            .expect("an anthropic catalog model")
            .clone();
        let old_provider = { host.lock().await.provider.clone() };
        SessionHost::handle_command(
            &host,
            Command::SetModel {
                session_id: session_id.clone(),
                model: ModelRef {
                    provider: sibling.provider.clone(),
                    id: sibling.id.clone(),
                },
            },
        )
        .await
        .unwrap();
        let guard = host.lock().await;
        assert!(
            Arc::ptr_eq(&old_provider, &guard.provider),
            "same-api switch must not rebuild the adapter"
        );
    }

    /// list_models returns the built-in catalog as protocol metadata.
    #[tokio::test]
    async fn list_models_returns_catalog() {
        let host = test_host(vec![]);
        let result = SessionHost::handle_command(&host, Command::ListModels)
            .await
            .unwrap();
        let CommandResult::ListModels { models } = result else {
            panic!("expected list_models result")
        };
        assert!(models.len() > 10, "built-in catalog expected");
        assert!(
            models.iter().any(|m| m.provider == "anthropic"),
            "anthropic models listed"
        );
        assert!(
            models.iter().all(|m| !m.id.is_empty() && !m.api.is_empty()),
            "metadata complete"
        );
    }

    /// Idle detached sessions were previously kept forever (the map was
    /// insert-only). The reaper must remove them and tell clients.
    #[tokio::test]
    async fn idle_sessions_are_reaped() {
        let host = test_host(vec![]);
        let mut events = host.lock().await.events.subscribe();
        let cwd = tempfile::tempdir().unwrap();
        let create = |cwd: &std::path::Path| Command::Create {
            cwd: Some(cwd.to_string_lossy().to_string()),
            name: None,
            model: None,
            thinking_level: None,
        };
        let result = SessionHost::handle_command(&host, create(cwd.path()))
            .await
            .unwrap();
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        let stale_id = session.id.clone();
        let result = SessionHost::handle_command(&host, create(cwd.path()))
            .await
            .unwrap();
        let CommandResult::Create { session } = result else {
            panic!("expected create")
        };
        let fresh_id = session.id.clone();

        {
            let mut guard = host.lock().await;
            guard
                .sessions
                .get_mut(&stale_id)
                .expect("stale session")
                .last_active = 0;
            let removed = guard.reap_idle_sessions(std::time::Duration::from_secs(3600));
            assert_eq!(removed, vec![stale_id.clone()]);
            assert!(!guard.sessions.contains_key(&stale_id));
            assert!(
                guard.sessions.contains_key(&fresh_id),
                "fresh session must stay"
            );
        }
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("removal broadcast")
            .expect("events open");
        assert!(
            matches!(event, ServerEvent::SessionRemoved { session_id } if session_id == stale_id),
            "clients must be told about the removal"
        );
    }

    // ------------------------------------------------------------------
    // Extension surfaces (remote/ext_bridge.rs + the ext command arms)
    // ------------------------------------------------------------------

    /// Pull surfaces with no plugins loaded: empty lists, and every
    /// plugin-backed command degrades to a clean error (never a panic,
    /// never a hang).
    #[tokio::test]
    async fn ext_surfaces_degrade_without_plugins() {
        let host = test_host(vec![]);
        let result = SessionHost::handle_command(&host, Command::ListExtCommands)
            .await
            .unwrap();
        assert!(
            matches!(result, CommandResult::ListExtCommands { commands } if commands.is_empty())
        );
        let result = SessionHost::handle_command(&host, Command::ListExtWidgets)
            .await
            .unwrap();
        assert!(matches!(result, CommandResult::ListExtWidgets { widgets } if widgets.is_empty()));
        let result = SessionHost::handle_command(&host, Command::ListExtAutocomplete)
            .await
            .unwrap();
        assert!(
            matches!(result, CommandResult::ListExtAutocomplete { providers } if providers.is_empty())
        );
        let result = SessionHost::handle_command(
            &host,
            Command::ExtAutocomplete {
                provider_key: "ghost:hash".into(),
                query: "#a".into(),
                cursor_offset: 2,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(result, CommandResult::ExtAutocomplete { suggestions } if suggestions.is_empty())
        );
        for command in [
            Command::InvokeExtCommand {
                name: "nope".into(),
                args: None,
            },
            Command::ExtWidgetAction {
                key: "ghost:w".into(),
                action: "select".into(),
                item_id: None,
            },
            Command::ExtDialogResponse {
                request_id: "dlg-nope".into(),
                cancelled: false,
                value: None,
            },
        ] {
            let error = SessionHost::handle_command(&host, command)
                .await
                .expect_err("unknown plugin resource must error");
            assert_eq!(error.code, ProtocolErrorCode::NotFound, "{error:?}");
        }
    }

    /// `invoke_ext_command` runs OFF the connection loop (a plugin
    /// command may itself show a dialog only this connection can
    /// answer): the loop must keep reading and answering later requests
    /// while the invocation is in flight. Both responses arrive (the
    /// inline one may overtake the spawned one — ids correlate).
    #[tokio::test]
    async fn invoke_ext_command_runs_off_loop() {
        let host = test_host(vec![]);
        let mut io = crate::remote::MockIo::open_ended(vec![
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                token: None,
                capabilities: Vec::new(),
            },
            ClientMessage::Request {
                id: "i1".into(),
                request: Command::InvokeExtCommand {
                    name: "nope".into(),
                    args: None,
                },
            },
            ClientMessage::Request {
                id: "l1".into(),
                request: Command::List,
            },
        ]);
        let notify = io.written_notify();
        let written = io.written_shared();
        let connection = tokio::spawn(async move {
            let _ = crate::remote::handle_frames(&mut io, &host).await;
        });
        // Wait for BOTH responses (bounded): the off-loop invoke must not
        // block the inline List.
        let both = async {
            loop {
                {
                    let snapshot = written.lock().unwrap_or_else(|e| e.into_inner());
                    let has = |id: &str| {
                        snapshot.iter().any(
                            |m| matches!(m, ServerMessage::Response { id: rid, .. } if rid == id),
                        )
                    };
                    if has("i1") && has("l1") {
                        return;
                    }
                }
                notify.notified().await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), both)
            .await
            .expect("both responses must arrive (off-loop invoke must not block the loop)");
        connection.abort();
        let written = io_written(&written);
        let response = |id: &str| {
            written.iter().find_map(|message| {
                let ServerMessage::Response {
                    id: response_id,
                    ok,
                    error,
                    ..
                } = message
                else {
                    return None;
                };
                (response_id == id).then(|| (*ok, error.clone()))
            })
        };
        let (ok, error) = response("i1").expect("invoke answered");
        assert!(!ok, "unknown command errors: {error:?}");
        assert_eq!(error.map(|e| e.code), Some(ProtocolErrorCode::NotFound));
        let (ok, _) = response("l1").expect("list answered");
        assert!(ok, "the loop kept reading while the invoke ran");
    }

    fn io_written(
        written: &std::sync::Arc<std::sync::Mutex<Vec<ServerMessage>>>,
    ) -> Vec<ServerMessage> {
        written.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The dialog-capable connection count tracks the connection
    /// lifetime: incremented after a `CAP_EXT_DIALOGS` handshake,
    /// decremented on teardown (so a dialog never parks on thin air).
    #[tokio::test]
    async fn dialog_capability_tracks_connection_lifetime() {
        let host = test_host(vec![]);
        let bridge = host.lock().await.ext_bridge.clone();
        let mut io = crate::remote::MockIo::new(vec![ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            token: None,
            capabilities: vec![CAP_EXT_DIALOGS.to_string()],
        }]);
        crate::remote::handle_frames(&mut io, &host).await.unwrap();
        assert_eq!(
            bridge.dialog_answerers(),
            0,
            "EOF must release the dialog-capable registration"
        );
    }

    /// The server hello advertises the extension surfaces so clients
    /// can feature-detect them.
    #[tokio::test]
    async fn server_hello_advertises_ext_capabilities() {
        let host = test_host(vec![]);
        let mut io = crate::remote::MockIo::new(vec![ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            token: None,
            capabilities: Vec::new(),
        }]);
        crate::remote::handle_frames(&mut io, &host).await.unwrap();
        let written = io.written();
        let Some(ServerMessage::Hello { capabilities, .. }) = written.first() else {
            panic!("expected server hello, got {:?}", written.first())
        };
        assert_eq!(
            capabilities,
            &SERVER_CAPABILITIES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }
}
