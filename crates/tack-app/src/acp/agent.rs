//! The ACP `Agent` implementation: initialize/new_session/prompt/cancel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, AvailableCommand,
    AvailableCommandsUpdate, CancelNotification, ClientCapabilities, ContentBlock, Cost,
    CurrentModeUpdate, Error, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, ModelId, ModelInfo, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, SessionConfigId,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelect,
    SessionConfigSelectOption, SessionConfigSelectOptions, SessionConfigValueId, SessionId,
    SessionInfoUpdate, SessionMode, SessionModeId, SessionModeState, SessionModelState,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
    SetSessionModelRequest, SetSessionModelResponse, UsageUpdate,
};
use tack_agent_core::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, BeforeToolCallContext,
    BeforeToolCallOutcome, ToolExecutionMode, agent_loop,
};
use tack_ai::Provider;
use tack_session::SessionManager;

use super::SharedConn;
use super::convert::{event_to_updates, stop_reason_for};
use super::session::{
    AcpSessionState, BridgeRequest, InFlightGuard, PermissionChoice, PermissionQuery, Sessions,
    create_session_state,
};
use crate::print_mode::assemble_system_prompt;
use crate::settings::Settings;

/// The ACP agent handler. Constructed before the connection exists; the
/// connection is stashed in `shared_conn` by `serve()`.
pub struct TackAcpAgent {
    shared_conn: SharedConn,
    sessions: Sessions,
    client_capabilities: Rc<RefCell<ClientCapabilities>>,
    model: tack_ai::Model,
    provider: Arc<dyn Provider>,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    thinking: Option<tack_ai::ThinkingLevel>,
    settings: Settings,
}

impl std::fmt::Debug for TackAcpAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TackAcpAgent").finish_non_exhaustive()
    }
}

/// CLI/session overrides for ACP mode. Precedence: these flags win over
/// settings.json, which wins over `TACK_*` env vars, which win over the
/// built-in default. `tack acp` used to silently drop `--provider` et al.
/// because `serve()` took no arguments.
#[derive(Debug, Default)]
pub struct AcpOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<tack_ai::ThinkingLevel>,
}

impl TackAcpAgent {
    pub fn new(shared_conn: SharedConn, overrides: &AcpOverrides) -> Self {
        // Resolve configuration synchronously at startup: some clients call
        // session/new immediately after initialize with no slack.
        let cwd = std::env::current_dir().unwrap_or_default();
        let agent_dir = tack_session::default_agent_dir();
        let settings = Settings::load(&cwd, &agent_dir);
        let env_provider = std::env::var("TACK_PROVIDER").ok();
        let env_model = std::env::var("TACK_MODEL").ok();
        let provider_name = overrides
            .provider
            .as_deref()
            .or(settings.default_provider.as_deref())
            .or(env_provider.as_deref())
            .unwrap_or("anthropic");
        let model_id = overrides
            .model
            .as_deref()
            .or(settings.default_model.as_deref())
            .or(env_model.as_deref());
        let model = crate::model::resolve_model(provider_name, model_id, &agent_dir)
            .unwrap_or_else(|e| panic!("ACP mode: {e}"));
        let provider = tack_ai::provider_for(&model).expect("built-in adapter exists");
        let provider: Arc<dyn Provider> = Arc::new(tack_ai::retry::RetryingProvider {
            inner: provider,
            policy: settings.retry.policy(),
            on_retry_scheduled: None,
        });
        let auth =
            crate::model::resolve_auth(&model.provider, overrides.api_key.clone(), &agent_dir);
        // Zed's acp logs capture stderr — make misconfiguration diagnosable.
        let has_credential = overrides.api_key.is_some()
            || crate::model::resolve_api_key(&model.provider, None, &agent_dir).is_some()
            || crate::auth::get_credential(&agent_dir, &model.provider).is_some();
        if !has_credential {
            eprintln!(
                "tack WARNING: no API key found for provider {} (checked auth.json, models.json, env vars)",
                model.provider
            );
        }
        eprintln!(
            "tack: provider={} model={} (agent dir: {})",
            model.provider,
            model.id,
            agent_dir.display()
        );
        let env_thinking = std::env::var("TACK_THINKING")
            .ok()
            .as_deref()
            .map(crate::print_mode::parse_thinking_level)
            .transpose()
            .ok()
            .flatten()
            .flatten();
        let thinking = overrides.thinking.or(env_thinking);

        Self::with_dependencies(shared_conn, model, provider, auth, settings, thinking)
    }

    /// Handle to the live sessions map, for connection-end teardown in
    /// `serve()` (the agent itself is moved into the connection).
    pub fn sessions(&self) -> Sessions {
        self.sessions.clone()
    }

    /// MCP client callbacks for server-initiated requests: sampling (opt-in)
    /// runs against the current model; elicitation auto-declines (headless).
    fn mcp_client_callbacks(&self, _cwd: &Path) -> tack_tools::mcp::McpClientCallbacks {
        crate::mcp_config::client_callbacks(
            &self.settings,
            Some(&crate::mcp_config::SamplingLlm {
                provider: self.provider.clone(),
                model: self.model.clone(),
                auth: self.auth.clone(),
            }),
            crate::mcp_sampling::log_usage_sink(),
            crate::mcp_elicitation::InteractionMode::Headless,
            None,
        )
    }

    /// Full-dependency constructor (tests inject scripted providers here).
    pub fn with_dependencies(
        shared_conn: SharedConn,
        model: tack_ai::Model,
        provider: Arc<dyn Provider>,
        auth: Arc<dyn tack_ai::oauth::AuthResolver>,
        settings: Settings,
        thinking: Option<tack_ai::ThinkingLevel>,
    ) -> Self {
        TackAcpAgent {
            shared_conn,
            sessions: Rc::new(RefCell::new(HashMap::new())),
            client_capabilities: Rc::new(RefCell::new(ClientCapabilities::new())),
            model,
            provider,
            auth,
            thinking,
            settings,
        }
    }

    fn notify(&self, session_id: &SessionId, update: SessionUpdate) {
        let conn = self.shared_conn.borrow().clone();
        if let Some(conn) = conn {
            let notification = SessionNotification::new(session_id.clone(), update);
            tokio::task::spawn_local(async move {
                use agent_client_protocol::Client;
                if let Err(e) = conn.session_notification(notification).await {
                    tracing::debug!("session_notification failed: {e}");
                }
            });
        }
    }

    /// Send a notification and wait for it to be written — used for the
    /// final error surface, which must precede the prompt response.
    async fn notify_sync(&self, session_id: &SessionId, update: SessionUpdate) {
        use agent_client_protocol::Client;
        let conn = self.shared_conn.borrow().clone();
        if let Some(conn) = conn
            && let Err(e) = conn
                .session_notification(SessionNotification::new(session_id.clone(), update))
                .await
        {
            tracing::debug!("session_notification failed: {e}");
        }
    }

    // --- modes / models / config options -----------------------------------

    /// Permission modes (Claude Code-style).
    fn available_modes() -> Vec<SessionMode> {
        vec![
            SessionMode::new("ask", "Default")
                .description("Read-only tools run freely; prompts for edits and commands"),
            SessionMode::new("acceptEdits", "Accept Edits")
                .description("Auto-accept file edits; prompts for commands"),
            SessionMode::new("plan", "Plan Mode")
                .description("Read-only: bash/edit/write are blocked"),
            SessionMode::new("bypass", "Bypass Permissions")
                .description("Run everything without prompts (use with care)"),
        ]
    }

    /// Tools that never need a prompt (read-only builtins; `git` by subcommand).
    fn is_read_only_tool(name: &str, args: &serde_json::Value) -> bool {
        crate::permissions::is_read_only_tool(name, args)
    }

    fn modes_state(&self, state: &AcpSessionState) -> SessionModeState {
        let current = state.mode.lock().unwrap_or_else(|e| e.into_inner()).clone();
        SessionModeState::new(SessionModeId::new(current), Self::available_modes())
    }

    fn models_state(&self, state: &AcpSessionState) -> SessionModelState {
        let model = state
            .model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let available: Vec<ModelInfo> = tack_ai::providers::builtin_models(&model.provider)
            .iter()
            .map(|m| ModelInfo::new(ModelId::new(m.id.clone()), m.name.clone()))
            .collect();
        SessionModelState::new(ModelId::new(model.id), available)
    }

    /// Supported thinking levels for a model (pi's getSupportedThinkingLevels).
    fn supported_thinking_levels(model: &tack_ai::Model) -> Vec<&'static str> {
        if !model.reasoning {
            return vec!["off"];
        }
        ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .filter(|level| {
                let mapped = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get(*level));
                if mapped.is_some_and(|v| v.is_none()) {
                    return false; // explicitly unsupported
                }
                if *level == "xhigh" || *level == "max" {
                    return mapped.is_some(); // require explicit mapping
                }
                true
            })
            .collect()
    }

    fn config_options(&self, state: &AcpSessionState) -> Vec<SessionConfigOption> {
        let model = state
            .model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let thinking = state.thinking.lock().unwrap_or_else(|e| e.into_inner());
        let current = thinking.map(|t| t.as_str()).unwrap_or("off");

        let effort_options: Vec<SessionConfigSelectOption> =
            Self::supported_thinking_levels(&model)
                .into_iter()
                .map(|level| {
                    SessionConfigSelectOption::new(
                        SessionConfigValueId::new(level),
                        match level {
                            "off" => "Off".to_string(),
                            other => other[0..1].to_uppercase() + &other[1..],
                        },
                    )
                })
                .collect();

        // Zed renders Mode/Model selectors from config options (the dedicated
        // modes/models fields are kept for clients that read them).
        let current_mode = state.mode.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mode_options: Vec<SessionConfigSelectOption> = Self::available_modes()
            .into_iter()
            .map(|m| {
                SessionConfigSelectOption::new(
                    SessionConfigValueId::new(m.id.0.to_string()),
                    m.name,
                )
            })
            .collect();

        let model_options: Vec<SessionConfigSelectOption> =
            tack_ai::providers::builtin_models(&model.provider)
                .iter()
                .map(|m| {
                    SessionConfigSelectOption::new(
                        SessionConfigValueId::new(m.id.clone()),
                        m.name.clone(),
                    )
                })
                .collect();

        vec![
            SessionConfigOption::new(
                SessionConfigId::new("mode"),
                "Mode",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    SessionConfigValueId::new(current_mode),
                    SessionConfigSelectOptions::Ungrouped(mode_options),
                )),
            )
            .category(SessionConfigOptionCategory::Mode),
            SessionConfigOption::new(
                SessionConfigId::new("model"),
                "Model",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    SessionConfigValueId::new(model.id.clone()),
                    SessionConfigSelectOptions::Ungrouped(model_options),
                )),
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::new(
                SessionConfigId::new("thinking"),
                "Effort",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    SessionConfigValueId::new(current),
                    SessionConfigSelectOptions::Ungrouped(effort_options),
                )),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ]
    }
    /// Handle a slash command prompt; returns the reply text.
    async fn handle_slash_command(
        &self,
        command: &str,
        state: &Rc<AcpSessionState>,
        turn_model: &tack_ai::Model,
    ) -> String {
        match command {
            "/compact" => {
                let (path, session_id) = {
                    let session = state.session.lock().await;
                    (
                        session.build_session_path(),
                        session.session_id().to_string(),
                    )
                };
                let Some(preparation) =
                    tack_session::prepare_compaction(&path, &self.settings.compaction)
                else {
                    return "Nothing to compact (session too small or already compacted)."
                        .to_string();
                };
                let tokens_before = preparation.tokens_before;
                let thinking = *state.thinking.lock().unwrap_or_else(|e| e.into_inner());
                let auth = match self.auth.resolve().await {
                    Ok(auth) => auth,
                    Err(e) => return format!("Compaction failed: {e}"),
                };
                let result = tack_session::compact(
                    &preparation,
                    turn_model,
                    &self.provider,
                    &auth,
                    None,
                    thinking,
                    Some(&session_id),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await;
                match result {
                    Ok(result) => {
                        let kept = &path[path
                            .iter()
                            .position(|e| e.id() == result.first_kept_entry_id)
                            .unwrap_or(path.len())..];
                        let retained_tail: Vec<AgentMessage> =
                            tack_session::retained_tail_from_kept_entries(kept);
                        let mut session = state.session.lock().await;
                        match session.append_compaction(
                            &result.summary,
                            Some(result.first_kept_entry_id.clone()),
                            result.tokens_before,
                            Some(retained_tail),
                            Some(result.details.clone()),
                            Some(result.usage.clone()),
                        ) {
                            Ok(_) => format!(
                                "Compacted: {tokens_before} tokens summarized into a checkpoint."
                            ),
                            Err(e) => format!("Failed to persist compaction: {e}"),
                        }
                    }
                    Err(e) => format!("Compaction failed: {e}"),
                }
            }
            "/rules" => {
                let session = state.session.lock().await;
                let cwd = session.cwd().to_path_buf();
                drop(session);
                let agent_dir = tack_session::default_agent_dir();
                let context_files = crate::resources::load_project_context_files(&cwd, &agent_dir);
                let skills = crate::skills::load_skills(&cwd, &agent_dir).0;
                let mut out = String::new();
                if context_files.is_empty() {
                    out.push_str("No project rules files loaded (AGENTS.md/CLAUDE.md).\n");
                } else {
                    out.push_str("Loaded rules:\n");
                    for f in &context_files {
                        out.push_str(&format!("- {} ({} chars)\n", f.path, f.content.len()));
                    }
                }
                if skills.is_empty() {
                    out.push_str("No skills loaded.");
                } else {
                    out.push_str(&format!("\nSkills ({}):\n", skills.len()));
                    for s in &skills {
                        out.push_str(&format!("- {}: {}\n", s.name, s.description));
                    }
                }
                out
            }
            other => format!("Unknown command: {other}. Available: /compact, /rules"),
        }
    }
}

/// Permission prompts wait on the client; a connected-but-unresponsive
/// one must not pend the turn forever. Mirrors the remote host's
/// PERMISSION_PROMPT_TIMEOUT.
const PERMISSION_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Hooks for an ACP session: compaction from `SessionHooks` plus the
/// permission gate wired to `session/request_permission`. Only Send+Sync
/// parts of the session state are held here (the hooks run inside the loop's
/// spawned tasks).
struct AcpHooks {
    inner: crate::hooks::SessionHooks,
    bridge: tokio::sync::mpsc::UnboundedSender<BridgeRequest>,
    allow_always: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    mode: Arc<std::sync::Mutex<String>>,
    /// Session-shared prompt-injection flag: set when untrusted external
    /// content (web/MCP/plugin tool output) entered the context.
    untrusted_seen: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl AgentHooks for AcpHooks {
    async fn transform_context(&self, messages: &[AgentMessage]) -> Option<Vec<AgentMessage>> {
        self.inner.transform_context(messages).await
    }

    async fn compact_for_overflow(&self) -> Option<Vec<AgentMessage>> {
        self.inner.compact_for_overflow().await
    }

    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        let mode = self.mode.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match mode.as_str() {
            "bypass" => return BeforeToolCallOutcome::Allow,
            "acceptEdits" => {
                // File edits and read-only tools run freely; commands prompt.
                if TackAcpAgent::is_read_only_tool(ctx.tool_name, ctx.args)
                    || matches!(ctx.tool_name, "edit" | "write")
                {
                    return BeforeToolCallOutcome::Allow;
                }
            }
            "plan" => {
                if TackAcpAgent::is_read_only_tool(ctx.tool_name, ctx.args) {
                    return BeforeToolCallOutcome::Allow;
                }
                return BeforeToolCallOutcome::Block {
                    reason: Some(format!(
                        "Plan mode: {} is disabled (read-only mode). Present the plan and ask the user to switch modes.",
                        ctx.tool_name
                    )),
                    terminate: false,
                };
            }
            _ => {
                // "ask" (default): read-only tools run without prompting.
                if TackAcpAgent::is_read_only_tool(ctx.tool_name, ctx.args) {
                    return BeforeToolCallOutcome::Allow;
                }
            }
        }

        let key_arg = ctx
            .args
            .get("path")
            .or_else(|| ctx.args.get("command"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let always_key = format!("{}:{key_arg}", ctx.tool_name);

        // Prompt-injection defense (TUI parity, tui/permission.rs): once
        // untrusted external content entered this session's context, cached
        // "always allow" decisions no longer auto-approve mutating tools —
        // the chain "untrusted page → allow-always bash" is the attack.
        // The client is asked instead. (Bypass mode short-circuits above,
        // as in the TUI: it is an explicit user override.)
        let untrusted = self
            .untrusted_seen
            .load(std::sync::atomic::Ordering::Relaxed)
            && !TackAcpAgent::is_read_only_tool(ctx.tool_name, ctx.args);
        if !untrusted {
            let res = {
                let cache = self.allow_always.lock().unwrap_or_else(|e| e.into_inner());
                cache.contains(ctx.tool_name) || cache.contains(&always_key)
            };
            if res {
                return BeforeToolCallOutcome::Allow;
            }
        }

        let (respond, response) = tokio::sync::oneshot::channel::<PermissionChoice>();
        let query = PermissionQuery {
            tool_call_id: ctx.tool_call_id.to_string(),
            tool_name: ctx.tool_name.to_string(),
            title: format!("{} {key_arg}", ctx.tool_name),
            raw_input: ctx.args.clone(),
            respond,
        };
        if self.bridge.send(BridgeRequest::Permission(query)).is_err() {
            return BeforeToolCallOutcome::Block {
                reason: Some("permission channel closed".into()),
                terminate: false,
            };
        }
        // A client that stays connected but never answers must not pend
        // the tool gate — and with it the whole turn — forever; the
        // cancel token cannot interrupt this await. Same bound as the
        // remote host's PERMISSION_PROMPT_TIMEOUT.
        match tokio::time::timeout(PERMISSION_PROMPT_TIMEOUT, response).await {
            Ok(Ok(PermissionChoice::AllowOnce)) => BeforeToolCallOutcome::Allow,
            Ok(Ok(PermissionChoice::AllowAlways)) => {
                self.allow_always
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(always_key);
                BeforeToolCallOutcome::Allow
            }
            Ok(Ok(PermissionChoice::Denied)) | Ok(Err(_)) => BeforeToolCallOutcome::Block {
                reason: Some("permission denied by user".into()),
                terminate: false,
            },
            Err(_) => BeforeToolCallOutcome::Block {
                reason: Some("permission prompt timed out".into()),
                terminate: false,
            },
        }
    }
}

/// Convert ACP prompt content blocks to a pi user message.
fn prompt_blocks_to_message(blocks: &[ContentBlock]) -> AgentMessage {
    let mut content: Vec<tack_ai::InputContentBlock> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => content.push(tack_ai::InputContentBlock::text(t.text.clone())),
            ContentBlock::Image(i) => content.push(tack_ai::InputContentBlock::Image {
                data: i.data.clone(),
                mime_type: i.mime_type.clone(),
            }),
            ContentBlock::ResourceLink(link) => content.push(tack_ai::InputContentBlock::text(
                format!("[resource link] {}: {}", link.name, link.uri),
            )),
            ContentBlock::Resource(resource) => {
                let text = match &resource.resource {
                    agent_client_protocol::EmbeddedResourceResource::TextResourceContents(t) => {
                        format!("[resource {}]\n{}", resource.resource.uri(), t.text)
                    }
                    _ => format!(
                        "[resource {}] (binary content omitted)",
                        resource.resource.uri()
                    ),
                };
                content.push(tack_ai::InputContentBlock::text(text));
            }
            _ => {}
        }
    }
    if content.is_empty() {
        content.push(tack_ai::InputContentBlock::text(""));
    }
    AgentMessage::user(tack_ai::UserContent::Blocks(content))
}

trait ResourceUri {
    fn uri(&self) -> &str;
}
impl ResourceUri for agent_client_protocol::EmbeddedResourceResource {
    fn uri(&self) -> &str {
        match self {
            agent_client_protocol::EmbeddedResourceResource::TextResourceContents(t) => &t.uri,
            agent_client_protocol::EmbeddedResourceResource::BlobResourceContents(b) => &b.uri,
            _ => "",
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for TackAcpAgent {
    async fn initialize(
        &self,
        args: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        *self.client_capabilities.borrow_mut() = args.client_capabilities.clone();
        let capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(true)
                    .audio(false)
                    .embedded_context(false),
            );
        // Version negotiation (ACP spec): answer with the newest version we
        // support, never higher than the client's. Echoing the client's
        // version verbatim would falsely claim support for a newer protocol.
        let version = std::cmp::min(
            args.protocol_version,
            agent_client_protocol::ProtocolVersion::LATEST,
        );
        // Identify ourselves: clients show agentInfo in pickers/about UIs.
        let agent_info = Implementation::new("tack", env!("CARGO_PKG_VERSION")).title("tack");
        Ok(InitializeResponse::new(version)
            .agent_capabilities(capabilities)
            .agent_info(agent_info))
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> agent_client_protocol::Result<AuthenticateResponse> {
        match self.auth.resolve().await {
            Ok(auth) if auth.api_key.is_some() || !auth.headers.is_empty() => {
                Ok(AuthenticateResponse::new())
            }
            Ok(_) => Err(Error::invalid_params().data(format!(
                "no API key for provider {} — set the provider's API key environment variable",
                self.model.provider
            ))),
            Err(e) => Err(Error::invalid_params().data(e)),
        }
    }

    async fn new_session(
        &self,
        args: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        let cwd = args.cwd.clone();

        // tack-ext plugins for this session's cwd (headless services: UI
        // dialogs degrade, exec is trust-gated). Bundle MCP servers merge
        // into the connection specs below.
        let agent_dir = tack_session::default_agent_dir();
        let bridge_state = crate::ext_provider_bridge::ProviderBridgeState::shared();
        let extensions = crate::extension_host::ExtensionManager::load(
            &cwd,
            &agent_dir,
            "acp",
            crate::ext_headless::HeadlessExtServices::new(
                "acp",
                crate::project_trust::is_trusted(&cwd, &agent_dir),
                bridge_state.clone(),
            ),
            self.settings.extension_lock_required,
            crate::mcp_config::plugin_mcp_callbacks(
                &self.settings,
                crate::mcp_elicitation::InteractionMode::Headless,
                None,
            ),
            bridge_state,
        )
        .await;

        // MCP servers: config files (global + project) first, then the
        // client's session-scoped servers (name conflicts deduped, client wins).
        let mut specs =
            crate::mcp_config::configured_servers(&cwd, &tack_session::default_agent_dir());
        specs.extend(extensions.bundle_mcp_servers.iter().cloned());
        for spec in crate::mcp_config::specs_from_acp(&args.mcp_servers) {
            specs.retain(|s| s.name != spec.name);
            specs.push(spec);
        }
        let connections = crate::mcp_oauth::connect_all_oauth(
            specs,
            &tack_session::default_agent_dir(),
            false,
            self.mcp_client_callbacks(&cwd),
        )
        .await;

        let session = SessionManager::create(&cwd, None)
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        let session_id = session.session_id().to_string();

        let state = create_session_state(
            session,
            self.shared_conn.clone(),
            SessionId::new(session_id.clone()),
            connections,
            self.model.clone(),
            self.thinking,
            extensions,
        );
        self.sessions
            .borrow_mut()
            .insert(session_id.clone(), state.clone());
        // tack-ext: session_start lifecycle event.
        state
            .extensions
            .lock()
            .await
            .notify(
                "session_start",
                serde_json::json!({
                    "sessionId": session_id,
                    "resumed": false,
                    "cwd": cwd.to_string_lossy(),
                }),
            )
            .await;

        // Advertise slash commands (rendered in the client's command menu).
        self.notify(
            &SessionId::new(session_id.clone()),
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("compact", "Compact the session context into a summary"),
                AvailableCommand::new("rules", "Show loaded project rules (AGENTS.md) and skills"),
            ])),
        );

        Ok(NewSessionResponse::new(SessionId::new(session_id))
            .modes(self.modes_state(&state))
            .models(self.models_state(&state))
            .config_options(self.config_options(&state)))
    }

    async fn load_session(
        &self,
        args: LoadSessionRequest,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        let session_key = args.session_id.0.to_string();
        let cwd = args.cwd.clone();

        // Reuse the live session when present; otherwise open from disk.
        let existing = { self.sessions.borrow().get(&session_key).cloned() };
        let state = match existing {
            Some(state) => state,
            None => {
                let session_dir =
                    tack_session::default_session_dir(&cwd, &tack_session::default_agent_dir());
                let path = tack_session::find_session_by_id(&session_dir, &session_key)
                    .ok_or_else(|| {
                        Error::invalid_params().data(format!("session not found: {session_key}"))
                    })?;
                let mut session = SessionManager::open(&path, None)
                    .map_err(|e| Error::internal_error().data(e.to_string()))?;
                match session.repair_dangling_tool_calls() {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!("repaired {n} dangling tool call(s) from an interrupted run")
                    }
                    Err(e) => tracing::warn!("failed to repair dangling tool calls: {e}"),
                }
                // Reloaded sessions get config-file MCP servers only.
                let agent_dir = tack_session::default_agent_dir();
                let bridge_state = crate::ext_provider_bridge::ProviderBridgeState::shared();
                let extensions = crate::extension_host::ExtensionManager::load(
                    &cwd,
                    &agent_dir,
                    "acp",
                    crate::ext_headless::HeadlessExtServices::new(
                        "acp",
                        crate::project_trust::is_trusted(&cwd, &agent_dir),
                        bridge_state.clone(),
                    ),
                    self.settings.extension_lock_required,
                    crate::mcp_config::plugin_mcp_callbacks(
                        &self.settings,
                        crate::mcp_elicitation::InteractionMode::Headless,
                        None,
                    ),
                    bridge_state,
                )
                .await;
                let mut specs =
                    crate::mcp_config::configured_servers(&cwd, &tack_session::default_agent_dir());
                specs.extend(extensions.bundle_mcp_servers.iter().cloned());
                let connections = crate::mcp_oauth::connect_all_oauth(
                    specs,
                    &tack_session::default_agent_dir(),
                    false,
                    self.mcp_client_callbacks(&cwd),
                )
                .await;

                let state = create_session_state(
                    session,
                    self.shared_conn.clone(),
                    SessionId::new(session_key.clone()),
                    connections,
                    self.model.clone(),
                    self.thinking,
                    extensions,
                );
                self.sessions
                    .borrow_mut()
                    .insert(session_key.clone(), state.clone());
                state
                    .extensions
                    .lock()
                    .await
                    .notify(
                        "session_start",
                        serde_json::json!({
                            "sessionId": session_key,
                            "resumed": true,
                            "cwd": cwd.to_string_lossy(),
                        }),
                    )
                    .await;
                state
            }
        };

        // Replay the visible transcript as session/update notifications.
        // Tool outputs are skipped (known client-perf pitfall); text is
        // replayed as single chunks.
        let replay: Vec<SessionUpdate> = {
            let session = state.session.lock().await;
            let messages = session.build_session_context().messages;
            super::convert::replay_updates_for_messages(&messages)
        };
        for update in replay {
            self.notify(&args.session_id, update);
        }

        Ok(LoadSessionResponse::new()
            .modes(self.modes_state(&state))
            .models(self.models_state(&state))
            .config_options(self.config_options(&state)))
    }

    async fn prompt(&self, args: PromptRequest) -> agent_client_protocol::Result<PromptResponse> {
        let session_key = args.session_id.0.to_string();
        let Some(state) = self.sessions.borrow().get(&session_key).cloned() else {
            return Err(Error::invalid_params().data(format!("unknown session: {session_key}")));
        };

        // One prompt turn per session at a time (F35): without this guard
        // two concurrent prompts ran two agent loops over the same
        // SessionManager (interleaved file writes) and the later prompt's
        // cancel token replaced the earlier one, leaving the first turn
        // uncancellable. Clients send prompts serially, so rejecting —
        // like the remote host's phase != Idle path — is the safe answer.
        let Some(_in_flight) = InFlightGuard::try_acquire(&state.in_flight) else {
            return Err(Error::invalid_params().data(
                "a prompt is already in progress for this session; wait for it to finish or cancel it",
            ));
        };

        // Fresh cancellation scope for this prompt turn; the cancel
        // notification cancels the token stored in the session state. The
        // in-flight guard above is what makes replacing the token safe:
        // no other turn can be holding the previous one.
        let cancel = {
            let mut slot = state.cancel.lock().unwrap_or_else(|e| e.into_inner());
            *slot = tokio_util::sync::CancellationToken::new();
            slot.clone()
        };

        let system_prompt = {
            let session = state.session.lock().await;
            let cwd = session.cwd().to_path_buf();
            drop(session);
            let agent_dir = tack_session::default_agent_dir();
            let tools = ["read", "bash", "edit", "write"].map(String::from);
            assemble_system_prompt(
                &cwd,
                &agent_dir,
                &self.settings,
                None,
                &tools,
                &crate::cli_flags::CliFlags::default(),
            )
        };

        let (tools, existing) = {
            let session = state.session.lock().await;
            let mut services = tack_tools::default_services(session.cwd().to_path_buf())
                .with_lsp(self.settings.lsp_manager(session.cwd()));
            if let Some(spec) = self.settings.sandbox_spec(session.cwd()) {
                services = services.with_sandbox(spec);
            }
            services = services
                .with_web_render(self.settings.web_render_mode())
                .with_web_search(self.settings.web_search_config())
                .with_background_tasks_enabled(self.settings.features.background_tasks)
                .with_memory_dir(self.settings.memory_directory.clone());
            // Share the session's untrusted-content flag with this turn's
            // tool services so web tools mark the SAME flag the permission
            // gate and the MCP/plugin tool wrappers read.
            services.untrusted_seen = state.untrusted_seen.clone();
            // Use client-owned terminals for bash when the client supports them.
            if self.client_capabilities.borrow().terminal {
                services = services.with_bash_executor(std::sync::Arc::new(
                    super::terminal::AcpTerminalExecutor {
                        bridge: state.bridge.clone(),
                    },
                ));
            }
            let mut tools = tack_tools::create_coding_tools(&services);
            tools.push(Arc::new(
                crate::session_search_tool::SessionSearchTool::new(
                    tack_session::default_agent_dir(),
                ),
            ));
            tools.extend(state.mcp_tools.iter().cloned());
            // tack-ext plugin tools (ext__<plugin>__<tool>); MCP-carrier
            // plugin output is untrusted and gets wrapped + flagged.
            tools.extend(
                state
                    .extensions
                    .lock()
                    .await
                    .tools_with_untrusted(Some(state.untrusted_seen.clone())),
            );
            let tools = crate::cli_flags::filter_feature_tools(tools, &self.settings.features);
            (tools, session.build_session_context().messages)
        };

        // tack-ext: plugin hooks + provider-boundary event sink for this turn.
        let (ext_hooks, ext_sink) = {
            let extensions = state.extensions.lock().await;
            (extensions.hooks(), extensions.clone_sink())
        };
        let provider: Arc<dyn Provider> = Arc::new(crate::extension_host::ExtNotifyProvider::new(
            self.provider.clone(),
            ext_sink,
        ));

        // Per-turn model/thinking from the session state (set_model /
        // set_config_option may have changed them).
        let turn_model = state
            .model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let turn_thinking = *state.thinking.lock().unwrap_or_else(|e| e.into_inner());

        let acp_hooks: Arc<dyn AgentHooks> = Arc::new(AcpHooks {
            inner: crate::hooks::SessionHooks {
                session: state.session.clone(),
                model: turn_model.clone(),
                provider: self.provider.clone(),
                auth: self.auth.clone(),
                reasoning: turn_thinking,
                settings: self.settings.compaction,
                cancel: cancel.clone(),
                on_compaction: None,
                history: self.settings.history(&tack_session::default_agent_dir()),
                // ACP mode: no hook engine wired (compaction hooks are a
                // TUI/print concern for now).
                hook_engine: crate::shell_hooks::HookEngine::new(
                    None,
                    std::env::current_dir().unwrap_or_default(),
                ),
                pre_compact: Vec::new(),
                post_compact: Vec::new(),
                hook_session_id: state.session.lock().await.session_id().to_string(),
            },
            bridge: state.bridge.clone(),
            allow_always: state.allow_always.clone(),
            mode: state.mode.clone(),
            untrusted_seen: state.untrusted_seen.clone(),
        });
        // permissions.deny applies in ACP sessions too (rpc/print parity):
        // deny rules run BEFORE the mode gate / user prompt, so a denied
        // tool is blocked even in bypass mode.
        let hooks = crate::permissions::chain_with_deny_rules(
            &self.settings,
            &tack_session::default_agent_dir(),
            acp_hooks,
        );
        // tack-ext plugin hooks see the final arguments after every other
        // hook in the chain.
        let hooks: Arc<dyn AgentHooks> = if ext_hooks.is_empty() {
            hooks
        } else {
            let mut chain = vec![hooks];
            chain.extend(ext_hooks);
            Arc::new(tack_agent_core::HooksChain::new(chain))
        };

        let config = AgentLoopConfig {
            model: turn_model.clone(),
            provider: provider.clone(),
            hooks,
            tool_execution: ToolExecutionMode::Parallel,
            reasoning: turn_thinking,
            auth: self.auth.clone(),
            max_tokens: None,
            temperature: None,
            session_id: Some(session_key.clone()),
            cache_retention: self.settings.cache_retention_mode(),
            fallback_models: crate::model::resolve_fallback_models(
                &self.settings.fallback_models,
                &turn_model,
                &tack_session::default_agent_dir(),
            ),
            tool_pool: Vec::new(),
            retry_cancel: None,
        };

        let context = AgentContext {
            system_prompt: Some(system_prompt),
            messages: existing,
            tools,
        };
        let prompt_message = prompt_blocks_to_message(&args.prompt);

        // Slash commands (advertised via available_commands_update).
        let slash = match &prompt_message {
            AgentMessage::User(u) => match &u.content {
                tack_ai::UserContent::Text(t) => t.trim().to_string(),
                tack_ai::UserContent::Blocks(b) if b.len() == 1 => match &b[0] {
                    tack_ai::InputContentBlock::Text { text, .. } => text.trim().to_string(),
                    _ => String::new(),
                },
                _ => String::new(),
            },
            _ => String::new(),
        };
        if slash.starts_with('/') {
            let reply = self.handle_slash_command(&slash, &state, &turn_model).await;
            self.notify_sync(
                &args.session_id,
                SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                    ContentBlock::Text(agent_client_protocol::TextContent::new(reply)),
                )),
            )
            .await;
            return Ok(PromptResponse::new(
                agent_client_protocol::StopReason::EndTurn,
            ));
        }

        // Session title from the first user prompt (drives the client's
        // thread list label via session_info_update).
        {
            let session = state.session.lock().await;
            if session
                .entries()
                .iter()
                .filter(|e| e.type_name() == "message")
                .count()
                <= 1
                && let AgentMessage::User(u) = &prompt_message
            {
                let text = match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    tack_ai::UserContent::Blocks(b) => b
                        .iter()
                        .filter_map(|b| match b {
                            tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                let title: String = text.chars().take(60).collect();
                if !title.is_empty() {
                    drop(session);
                    self.notify(
                        &args.session_id,
                        SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
                    );
                }
            }
        }

        let mut stream = agent_loop(vec![prompt_message], context, config, cancel.clone());

        // Track the run's terminal state from the event stream.
        let mut last_stop = tack_ai::StopReason::Stop;
        let mut error_text: Option<String> = None;
        let mut streamed_any_text = false;
        while let Some(event) = stream.next().await {
            // Persist completed messages.
            if let AgentEvent::MessageEnd { message } = &event {
                if !matches!(message, AgentMessage::Custom(_))
                    && let Err(e) = state.session.lock().await.append_message(message.clone())
                {
                    tracing::warn!("failed to persist message: {e}");
                }
                if let AgentMessage::Assistant(a) = message {
                    last_stop = a.stop_reason;
                    if a.stop_reason == tack_ai::StopReason::Error {
                        error_text = a.error_message.clone();
                    }
                    // Context usage + cumulative cost for the client UI.
                    if !matches!(
                        a.stop_reason,
                        tack_ai::StopReason::Error | tack_ai::StopReason::Aborted
                    ) {
                        let used = tack_session::calculate_context_tokens(&a.usage);
                        if used > 0 {
                            let totals = state.session.lock().await.session_totals();
                            let cost = (totals.cost.total > 0.0)
                                .then(|| Cost::new(totals.cost.total, "USD"));
                            self.notify(
                                &args.session_id,
                                SessionUpdate::UsageUpdate(
                                    UsageUpdate::new(used, turn_model.context_window as u64)
                                        .cost(cost),
                                ),
                            );
                        }
                    }
                }
            }
            if matches!(
                &event,
                AgentEvent::MessageUpdate {
                    assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta { .. },
                    ..
                }
            ) {
                streamed_any_text = true;
            }
            for update in event_to_updates(&event) {
                self.notify(&args.session_id, update);
            }
            if event.is_terminal() {
                break;
            }
        }
        // Drain the final result to let the loop task finish cleanly.
        let _ = stream.result().await;

        // Never fail silently: if the turn errored without any visible text,
        // surface the error as an agent message so the client shows it.
        if last_stop == tack_ai::StopReason::Error && !streamed_any_text {
            let text = error_text.unwrap_or_else(|| "Unknown provider error".to_string());
            self.notify_sync(
                &args.session_id,
                SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                    ContentBlock::Text(agent_client_protocol::TextContent::new(format!(
                        "⚠️ {text}\n\n(check API key/provider config; tack logs go to stderr)"
                    ))),
                )),
            )
            .await;
        }

        let cancelled = cancel.is_cancelled();
        Ok(PromptResponse::new(stop_reason_for(last_stop, cancelled)))
    }

    async fn set_session_mode(
        &self,
        args: SetSessionModeRequest,
    ) -> agent_client_protocol::Result<SetSessionModeResponse> {
        let session_key = args.session_id.0.to_string();
        let Some(state) = self.sessions.borrow().get(&session_key).cloned() else {
            return Err(Error::invalid_params().data(format!("unknown session: {session_key}")));
        };
        let mode_id = args.mode_id.0.to_string();
        if !Self::available_modes()
            .iter()
            .any(|m| m.id.0.as_ref() == mode_id)
        {
            return Err(Error::invalid_params().data(format!("unknown mode: {mode_id}")));
        }
        *state.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode_id.clone();
        self.notify(
            &args.session_id,
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new(mode_id))),
        );
        Ok(SetSessionModeResponse::new())
    }

    async fn set_session_model(
        &self,
        args: SetSessionModelRequest,
    ) -> agent_client_protocol::Result<SetSessionModelResponse> {
        let session_key = args.session_id.0.to_string();
        let Some(state) = self.sessions.borrow().get(&session_key).cloned() else {
            return Err(Error::invalid_params().data(format!("unknown session: {session_key}")));
        };
        let model_id = args.model_id.0.to_string();
        let current_provider = state
            .model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .provider
            .clone();
        let Some(model) = tack_ai::providers::builtin_model(&current_provider, &model_id) else {
            return Err(Error::invalid_params().data(format!(
                "unknown model for provider {current_provider}: {model_id}"
            )));
        };
        *state.model.lock().unwrap_or_else(|e| e.into_inner()) = model.clone();
        if let Err(e) = state
            .session
            .lock()
            .await
            .append_model_change(&model.provider, &model.id)
        {
            tracing::warn!("failed to record model change: {e}");
        }
        Ok(SetSessionModelResponse::new())
    }

    async fn set_session_config_option(
        &self,
        args: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        let session_key = args.session_id.0.to_string();
        let Some(state) = self.sessions.borrow().get(&session_key).cloned() else {
            return Err(Error::invalid_params().data(format!("unknown session: {session_key}")));
        };
        match args.config_id.0.as_ref() {
            "thinking" => {
                let value = args.value.0.as_ref();
                let thinking = match crate::print_mode::parse_thinking_level(value) {
                    Ok(t) => t,
                    Err(e) => return Err(Error::invalid_params().data(e.to_string())),
                };
                *state.thinking.lock().unwrap_or_else(|e| e.into_inner()) = thinking;
                if let Err(e) = state
                    .session
                    .lock()
                    .await
                    .append_thinking_level_change(thinking.map(|t| t.as_str()).unwrap_or("off"))
                {
                    tracing::warn!("failed to record thinking level: {e}");
                }
            }
            "mode" => {
                let mode_id = args.value.0.to_string();
                if !Self::available_modes()
                    .iter()
                    .any(|m| m.id.0.as_ref() == mode_id)
                {
                    return Err(Error::invalid_params().data(format!("unknown mode: {mode_id}")));
                }
                *state.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode_id.clone();
                self.notify(
                    &args.session_id,
                    SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new(
                        mode_id,
                    ))),
                );
            }
            "model" => {
                let model_id = args.value.0.to_string();
                let provider = state
                    .model
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .provider
                    .clone();
                let Some(model) = tack_ai::providers::builtin_model(&provider, &model_id) else {
                    return Err(Error::invalid_params()
                        .data(format!("unknown model for provider {provider}: {model_id}")));
                };
                *state.model.lock().unwrap_or_else(|e| e.into_inner()) = model.clone();
                if let Err(e) = state
                    .session
                    .lock()
                    .await
                    .append_model_change(&model.provider, &model.id)
                {
                    tracing::warn!("failed to record model change: {e}");
                }
            }
            other => {
                return Err(Error::invalid_params().data(format!("unknown config option: {other}")));
            }
        }
        Ok(SetSessionConfigOptionResponse::new(
            self.config_options(&state),
        ))
    }

    async fn cancel(&self, args: CancelNotification) -> agent_client_protocol::Result<()> {
        let session_key = args.session_id.0.to_string();
        if let Some(state) = self.sessions.borrow().get(&session_key).cloned() {
            state
                .cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancel();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[derive(Debug)]
    struct StubProvider;

    impl Provider for StubProvider {
        fn stream(
            &self,
            _model: &tack_ai::Model,
            _context: &tack_ai::Context,
            _options: tack_ai::StreamOptions,
        ) -> tack_ai::AssistantMessageEventStream {
            panic!("not used by these tests")
        }
    }

    fn test_model() -> tack_ai::Model {
        tack_ai::Model {
            id: "mock".into(),
            name: "Mock".into(),
            api: "mock".into(),
            provider: "mock".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn test_hooks(mode: &str) -> AcpHooks {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<BridgeRequest>();
        let settings = Settings::default();
        AcpHooks {
            inner: crate::hooks::SessionHooks {
                session: Arc::new(tokio::sync::Mutex::new(
                    tack_session::SessionManager::in_memory(std::path::Path::new("")),
                )),
                model: test_model(),
                provider: Arc::new(StubProvider),
                auth: Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string()))),
                reasoning: None,
                settings: settings.compaction,
                cancel: tokio_util::sync::CancellationToken::new(),
                on_compaction: None,
                history: None,
                hook_engine: crate::shell_hooks::HookEngine::new(
                    None,
                    Path::new(".").to_path_buf(),
                ),
                pre_compact: Vec::new(),
                post_compact: Vec::new(),
                hook_session_id: "test".to_string(),
            },
            bridge: tx,
            allow_always: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            mode: Arc::new(std::sync::Mutex::new(mode.to_string())),
            untrusted_seen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn call_ctx<'a>(
        message: &'a tack_ai::AssistantMessage,
        tool_name: &'a str,
        args: &'a serde_json::Value,
    ) -> BeforeToolCallContext<'a> {
        BeforeToolCallContext {
            assistant_message: message,
            tool_call_id: "call-1",
            tool_name,
            args,
            context: &[],
        }
    }

    /// Security regression: ACP sessions used to skip DenyRulesHooks, so
    /// permissions.deny in settings was silently ignored (even bypass mode
    /// must not override a deny rule). Also: poisoning the std mutexes must
    /// not panic the hooks (lock-poisoning recovery).
    #[tokio::test]
    async fn deny_rules_block_even_in_bypass_mode() {
        let mut settings = Settings::default();
        settings.permission_deny = vec!["Bash(rm *)".to_string()];
        let tmp = tempfile::tempdir().unwrap();
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = serde_json::json!({"command": "rm -rf /x"});

        let hooks = crate::permissions::chain_with_deny_rules(
            &settings,
            tmp.path(),
            Arc::new(test_hooks("bypass")),
        );
        let outcome = hooks
            .before_tool_call(&call_ctx(&message, "bash", &args))
            .await;
        assert!(
            matches!(outcome, BeforeToolCallOutcome::Block { .. }),
            "deny rule must block in bypass mode"
        );

        // Sanity: bypass mode alone would have allowed the same call.
        let outcome = test_hooks("bypass")
            .before_tool_call(&call_ctx(&message, "bash", &args))
            .await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
    }

    /// F35: a prompt arriving while another prompt is in flight on the
    /// same session is REJECTED (not queued into a second agent loop);
    /// once the turn ends the session accepts prompts again. Slash
    /// commands drive the test so no provider stream is needed.
    #[tokio::test]
    async fn concurrent_prompt_is_rejected_until_turn_ends() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let shared_conn: SharedConn = Rc::new(RefCell::new(None));
                let agent = TackAcpAgent::with_dependencies(
                    shared_conn.clone(),
                    test_model(),
                    Arc::new(StubProvider),
                    Arc::new(tack_ai::oauth::StaticAuth::from(Some("key".to_string()))),
                    Settings::default(),
                    None,
                );
                let tmp = tempfile::tempdir().unwrap();
                let state = create_session_state(
                    SessionManager::in_memory(tmp.path()),
                    shared_conn,
                    SessionId::new("s1"),
                    Vec::new(),
                    test_model(),
                    None,
                    crate::extension_host::ExtensionManager::default(),
                );
                agent
                    .sessions
                    .borrow_mut()
                    .insert("s1".to_string(), state.clone());
                let request = || {
                    PromptRequest::new(
                        SessionId::new("s1"),
                        vec![ContentBlock::Text(agent_client_protocol::TextContent::new(
                            "/help",
                        ))],
                    )
                };

                // A turn is in flight (its guard held by the running prompt).
                let held = crate::acp::session::InFlightGuard::try_acquire(&state.in_flight)
                    .expect("session starts free");
                let err = agent.prompt(request()).await.unwrap_err();
                assert!(
                    format!("{err:?}").contains("already in progress"),
                    "concurrent prompt must be rejected: {err:?}"
                );

                drop(held);
                let response = agent
                    .prompt(request())
                    .await
                    .expect("prompt accepted once the turn ended");
                assert_eq!(
                    response.stop_reason,
                    agent_client_protocol::StopReason::EndTurn
                );
                // ... and the in-flight flag was released again.
                assert!(
                    crate::acp::session::InFlightGuard::try_acquire(&state.in_flight).is_some(),
                    "prompt releases the in-flight flag on every exit path"
                );
            })
            .await;
    }

    /// A poisoned mode/allow_always mutex must degrade gracefully
    /// (recover the guard) instead of panicking the agent loop task.
    #[tokio::test]
    async fn poisoned_mutexes_do_not_panic() {
        let hooks = test_hooks("plan");
        // Poison both std mutexes.
        {
            let mode = hooks.mode.clone();
            let allow_always = hooks.allow_always.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _guard = mode.lock().unwrap();
                let _guard2 = allow_always.lock().unwrap();
                panic!("poison");
            }));
        }
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = serde_json::json!({"path": "README.md"});
        // Plan mode + recovered mutexes: read-only tools run freely.
        let outcome = hooks
            .before_tool_call(&call_ctx(&message, "read", &args))
            .await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
    }
}
