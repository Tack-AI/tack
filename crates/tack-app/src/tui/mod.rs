//! Interactive TUI mode: `tack` with no prompt launches the chat UI.
//! Port of `modes/interactive/interactive-mode.ts` onto the tack-tui runtime.

pub mod autocomplete;
pub mod chat;
pub mod commands;
pub mod dialogs;
pub mod export;
#[cfg(feature = "ext")]
pub mod ext;
pub mod footer;
pub mod fullscreen;
pub mod history_search;
pub mod images;
pub mod input;
pub mod mermaid;
#[cfg(feature = "mermaid")]
pub mod mermaid_text;
pub mod notify;
pub mod permission;
pub mod plan_mode;
pub mod render;
pub mod run;
pub mod status;
pub mod stream_md;
pub mod theme;
pub mod tool_render;
pub mod update_check;
#[cfg(feature = "ext")]
pub mod widgets;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use tack_agent_core::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, HooksChain, ToolExecutionMode,
    agent_loop,
};
use tack_ai::{Model, Provider, ThinkingLevel};
use tack_session::SessionManager;
use tack_tui::Component as _;
use tack_tui::components::editor::Editor;
use tack_tui::{InputEvent, Line, Span, Style, Tui, TuiMode};
use tokio::sync::{Mutex, mpsc};

use crate::settings::Settings;
use chat::{ChatEntry, NoticeKind};
use footer::FooterStats;
use permission::{PermissionMode, PermissionQuery, TuiPermissionHooks};

use theme::Theme;

/// Options for the TUI (constructed by main.rs).
pub struct TuiOptions {
    pub model: Model,
    pub auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    pub thinking: Option<ThinkingLevel>,
    pub cwd: PathBuf,
    pub continue_session: bool,
    pub system_prompt: Option<String>,
    pub session_dir: Option<PathBuf>,
    pub flags: crate::cli_flags::CliFlags,
}

impl std::fmt::Debug for TuiOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiOptions")
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// Drain-collapse policy: may a held-back streaming update be dropped in
/// favor of `incoming`? Only when `incoming` carries equal-or-newer state
/// for the SAME render target:
/// - `MessageUpdate` snapshots the whole assistant message, so the latest
///   always supersedes an earlier one.
/// - `ToolExecutionUpdate` snapshots ONE tool's cumulative partial result,
///   so it only supersedes an earlier update for the same tool call.
///   Parallel execution interleaves updates from different tool calls;
///   collapsing across them would drop the other tool's progress, and a
///   tool update never carries message state (or vice versa).
///
/// Live footer stats during a run: base snapshot + the streaming message's
/// usage (matches session_totals accumulation semantics).
pub(crate) fn fold_run_stats(base: &FooterStats, a: &tack_ai::AssistantMessage) -> FooterStats {
    let mut s = base.clone();
    // Providers that only report usage in the final chunk would freeze the
    // footer mid-stream; estimate output from generated text then.
    let output = if a.usage.output > 0 {
        a.usage.output
    } else {
        (a.text().chars().count() / 4) as u64
    };
    s.input += a.usage.input;
    s.output += output;
    s.cache_read += a.usage.cache_read;
    s.cache_write += a.usage.cache_write;
    s.cost += a.usage.cost.total;
    // Context grows by what the model has generated (tool results are not
    // tracked here; good enough for a live percentage).
    s.context_tokens += output;
    s
}

pub(crate) fn stream_update_supersedes(pending: &AgentEvent, incoming: &AgentEvent) -> bool {
    match (pending, incoming) {
        (AgentEvent::MessageUpdate { .. }, AgentEvent::MessageUpdate { .. }) => true,
        (
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: pending_id,
                ..
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: incoming_id,
                ..
            },
        ) => pending_id == incoming_id,
        _ => false,
    }
}

/// Events flowing into the app loop.
#[derive(Debug)]
pub enum AppEvent {
    Agent(Box<AgentEvent>),
    RunFinished,
    /// The run task hands the session back on completion.
    SessionBack(Box<SessionManager>),
    /// Background task wants to post a chat notice (e.g. /share result).
    Notice(String, NoticeKind),
    /// A retry backoff was scheduled (attempt, max, delay_ms, error).
    RetryScheduled(u32, u32, u64, String),
    /// Auto-compaction finished (summary, tokens_before).
    CompactionSummary(String, u64),
    /// Manual /compact finished on a background task: the compact result
    /// plus the retained tail (kept entries as context messages) for
    /// append_compaction. Session mutation happens on the main loop.
    ManualCompactDone {
        session_id: String,
        /// The session leaf the compaction was prepared from; the landing
        /// guard rejects if the lineage moved (tree nav while compacting).
        leaf: Option<String>,
        result: Result<(tack_session::CompactionResult, Vec<AgentMessage>), String>,
    },
    /// A spawned `!cmd` bash task finished; the tool card and the
    /// BashExecution session entry land on the main loop.
    BangDone {
        id: String,
        command: String,
        /// Session the command was started in; landing is guarded on it.
        session_id: String,
        exclude_from_context: bool,
        cancelled: bool,
        result: tack_agent_core::AgentToolResult,
        is_error: bool,
    },
    /// LLM branch-summary finished on a background task (fork/tree
    /// navigation). The summary entry attaches to the abandoned branch's
    /// tip (from_id) on the main loop.
    BranchSummaryDone {
        session_id: String,
        from_id: Option<String>,
        result: Result<tack_session::BranchSummaryResult, String>,
    },
    /// A plugin asks the host for a UI dialog / exec (tack-ext bridge).
    ExtUiRequest(crate::extension_host::ExtUiRequest),
    /// v2.1: a plugin pushed a widget state update (full-state snapshot).
    #[cfg(feature = "ext")]
    ExtWidgetUpdate {
        plugin: String,
        update: tack_ext::rpc3::WidgetUpdateParams,
    },
    /// v2.1: a plugin's peer hit EOF — drop its widgets (no UI residue).
    ExtPluginDead(String),
    /// v2.2: merged extension autocomplete suggestions arrived (generation
    /// guards against stale results landing after more keystrokes).
    ExtAutocompleteReady {
        generation: u64,
        auto: autocomplete::Autocomplete,
    },
    /// The run task established the MCP connections and assembled the
    /// prompt; app-side products land here (the connections keep the
    /// servers alive between runs, the char counts feed /context).
    RunContextReady {
        connections: Vec<std::sync::Arc<tack_tools::mcp::McpConnection>>,
        tools_chars: usize,
        system_chars: usize,
    },
    /// An MCP server asks the user for structured input (elicitation).
    McpElicitation(crate::mcp_elicitation::ElicitationQuery),
    /// The ask_user tool asks the user a batch of questions.
    AskUser(crate::ask_user::AskUserQuery),
    /// An MCP sampling completion finished; fold usage into the footer.
    McpSamplingDone {
        usage: tack_ai::Usage,
        model: String,
    },
    /// The background startup update check found a newer tack release.
    UpdateAvailable(String),
}

/// Event bus between producers (run task, plugins, retry callbacks) and
/// the app loop.
///
/// Streaming updates (`MessageUpdate` / `ToolExecutionUpdate`) each carry
/// a full-state snapshot, so when the app loop falls behind (a slow
/// render), a newer update that supersedes the one at the BACK of the
/// queue replaces it in place instead of queueing behind it — the
/// "latest-value-overwrite" slot. Without this an unbounded channel
/// backlogs stale frames faster than they can ever be drained and the UI
/// keeps dribbling long after the run ends. All other events keep strict
/// FIFO order.
struct BusInner {
    queue: std::sync::Mutex<VecDeque<AppEvent>>,
    notify: tokio::sync::Notify,
    /// Live sender count; `recv` returns None once the queue is drained
    /// and every sender is gone (mpsc semantics).
    senders: std::sync::atomic::AtomicUsize,
    /// Receiver liveness; `send` fails once the receiver is dropped.
    receiver_alive: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for BusInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusInner").finish_non_exhaustive()
    }
}

/// Sending half of the app event bus.
#[derive(Debug)]
pub struct AppEventTx {
    inner: Arc<BusInner>,
}

impl Clone for AppEventTx {
    fn clone(&self) -> Self {
        self.inner
            .senders
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        AppEventTx {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for AppEventTx {
    fn drop(&mut self) {
        if self
            .inner
            .senders
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            // Last sender gone: wake a waiting recv so it can return None.
            self.inner.notify.notify_waiters();
        }
    }
}

impl AppEventTx {
    /// Send an event; fails (returning it, boxed to keep the Result
    /// small) when the receiver is gone.
    pub(crate) fn send(&self, event: AppEvent) -> Result<(), Box<AppEvent>> {
        if !self
            .inner
            .receiver_alive
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Box::new(event));
        }
        {
            let mut queue = lock_recover(&self.inner.queue);
            if let AppEvent::Agent(incoming) = &event {
                let is_stream_update = matches!(
                    incoming.as_ref(),
                    AgentEvent::MessageUpdate { .. } | AgentEvent::ToolExecutionUpdate { .. }
                );
                // Latest-value slot: replace a queued streaming update the
                // new one supersedes (same render target, equal-or-newer
                // state) instead of backlogging stale frames.
                if is_stream_update
                    && let Some(AppEvent::Agent(pending)) = queue.back()
                    && stream_update_supersedes(pending, incoming)
                {
                    *queue.back_mut().expect("back() was Some") = event;
                    drop(queue);
                    self.inner.notify.notify_one();
                    return Ok(());
                }
            }
            queue.push_back(event);
        }
        self.inner.notify.notify_one();
        Ok(())
    }
}

/// Receiving half of the app event bus.
#[derive(Debug)]
pub struct AppEventRx {
    inner: Arc<BusInner>,
}

impl Drop for AppEventRx {
    fn drop(&mut self) {
        self.inner
            .receiver_alive
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl AppEventRx {
    /// Await the next event; None once the queue is drained and all
    /// senders are dropped.
    pub(crate) async fn recv(&self) -> Option<AppEvent> {
        loop {
            // Register the waiter BEFORE checking the queue (no lost wakeup).
            let notified = self.inner.notify.notified();
            {
                let mut queue = lock_recover(&self.inner.queue);
                if let Some(event) = queue.pop_front() {
                    return Some(event);
                }
                if self.inner.senders.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Non-blocking pop (drain loop).
    pub(crate) fn try_recv(&self) -> Option<AppEvent> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
}

pub(crate) fn app_event_bus() -> (AppEventTx, AppEventRx) {
    let inner = Arc::new(BusInner {
        queue: std::sync::Mutex::new(VecDeque::new()),
        notify: tokio::sync::Notify::new(),
        senders: std::sync::atomic::AtomicUsize::new(1),
        receiver_alive: std::sync::atomic::AtomicBool::new(true),
    });
    (
        AppEventTx {
            inner: inner.clone(),
        },
        AppEventRx { inner },
    )
}

/// Shared session state (mutated by commands and the run hooks).
struct AppState {
    session: SessionManager,
    model: Model,
    thinking: Option<ThinkingLevel>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}

/// The interactive app.
pub struct TuiApp {
    // Config
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: Settings,
    provider: Arc<dyn Provider>,
    auth: Arc<dyn tack_ai::oauth::AuthResolver>,
    theme: Theme,
    // Session state
    state: AppState,
    // Transcript
    items: Vec<chat::TranscriptItem>,
    tools: HashMap<String, tool_render::ToolEntry>,
    tool_order: Vec<String>,
    streaming: Option<tack_ai::AssistantMessage>,
    /// Bumped on every streaming-message mutation (render cache key).
    stream_rev: u64,
    /// Cached markdown render of the streaming partial, keyed by
    /// (stream_rev, width) — markdown over a long partial per frame is
    /// the dominant large-context frame cost.
    stream_render: Option<(u64, u16, Vec<Line>, bool)>,
    /// Throttle mark for streaming re-parses: (content bytes, block count,
    /// when) of the last markdown render. Reparsing the whole partial on
    /// every drain batch is O(n²) over a long stream; small, rapid deltas
    /// reuse the previous render for a frame or two instead.
    stream_render_mark: Option<(usize, usize, Instant)>,
    line_cache: Vec<render::LineCacheEntry>,
    /// Per-item absolute start line / height (recomputed every frame by
    /// `compute_offsets`; drives fullscreen windowed rendering).
    offsets: Vec<usize>,
    item_heights: Vec<usize>,
    transcript_total: usize,
    /// B: tool-card render cache (completed cards are immutable; running
    /// cards keyed by partial-output length). Avoids re-rendering hundreds
    /// of tool cards per frame at large contexts.
    tool_render_cache: HashMap<String, (u16, bool, u64, Vec<Line>, bool)>,
    /// Tool-card heights keyed like `tool_render_cache` — survive render
    /// eviction so `compute_offsets` never re-renders an evicted card just
    /// to learn its height.
    tool_heights: HashMap<String, (u16, bool, u64, usize)>,
    /// Incremental markdown cache for the streaming partial (see
    /// stream_md.rs): closed-prefix renders reused across re-parse passes.
    stream_md_cache: stream_md::StreamMarkdownCache,
    // Run control
    running: bool,
    /// Manual /compact in flight (spawned; the result lands as
    /// AppEvent::ManualCompactDone). Blocks new runs — the completion
    /// handler mutates self.state.session, which a run would have parked.
    compacting: bool,
    /// Cancel tokens of in-flight `!cmd` bash tasks (per tool id; Esc
    /// cancels them all, completion removes its own).
    bang_cancel: HashMap<String, tokio_util::sync::CancellationToken>,
    /// Cancel token of an in-flight branch-summary task (fork/tree
    /// navigation). Own token, NOT self.cancel: that one may already be
    /// cancelled by an earlier Esc, and a pre-cancelled token makes the
    /// summary abort immediately (the LLM call checks is_cancelled up
    /// front). Esc cancels it; landing clears it.
    branch_summary_cancel: Option<tokio_util::sync::CancellationToken>,
    cancel: tokio_util::sync::CancellationToken,
    steering: Arc<Mutex<VecDeque<String>>>,
    follow_up: Arc<Mutex<VecDeque<String>>>,
    /// Ctrl+Enter while running: abort the current run and submit this text
    /// as soon as it finishes ("send now").
    pending_send_now: Option<String>,
    mode: Arc<std::sync::Mutex<PermissionMode>>,
    allow_always: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    custom_system_prompt: Option<String>,
    /// Keybinding registry (defaults + user overrides from keybindings.json).
    kb: tack_tui::keys::Keybindings,
    // UI
    editor: Editor,
    autocomplete: Option<autocomplete::Autocomplete>,
    status: Option<status::StatusIndicator>,
    dialog: Option<commands::Dialog>,
    tui: Tui,
    // Fullscreen (alt-screen) state
    fullscreen: bool,
    scroll: tack_tui::components::scroll_view::ScrollView,
    /// Last rendered frame — fullscreen mode only (selection extraction,
    /// fullscreen copy). Regular mode never keeps a joined frame: building
    /// one was an O(transcript) clone per keystroke. /debug uses
    /// `debug_capture` to join one on demand.
    last_frame: Vec<Line>,
    /// Arm a /debug frame dump: the next render joins the frame parts and
    /// writes the dump (see commands::context::write_debug_dump).
    debug_capture: bool,
    /// Autocomplete sources (template + skill names + ext commands):
    /// loading them walks directories and parses frontmatter, so they are
    /// cached briefly instead of re-read from disk per keystroke.
    ac_sources: Option<(Vec<String>, Vec<crate::skills::Skill>, Instant)>,
    /// Jump-to-bottom pill hitbox (row, col_start, col_end) from the last
    /// fullscreen render; `None` when following the end.
    jump_pill: Option<(u16, u16, u16)>,
    prompt_rows: Vec<usize>,
    selection: Option<fullscreen::Selection>,
    selecting: Option<(u16, u16)>,
    search: Option<fullscreen::SearchState>,
    image_protocol: Option<tack_tui::image::ImageProtocol>,
    mermaid_enabled: bool,
    /// ctrl+o expand-all also covers completed thinking blocks.
    thinking_expanded: bool,
    /// Live MCP connections (replaced per run; browsed by /mcp).
    mcp_connections: Vec<std::sync::Arc<tack_tools::mcp::McpConnection>>,
    /// CLI flags carried through the session (TS cli/args.ts parity).
    flags: crate::cli_flags::CliFlags,
    /// Running tack-ext plugins (empty when none are installed).
    extensions: crate::extension_host::ExtensionManager,
    /// An extension UI dialog awaiting an answer (method, responder).
    pending_ext_ui: Option<(
        String,
        tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    )>,
    /// In-flight MCP elicitation dialog (per-field input walk).
    pending_elicitation: Option<crate::mcp_elicitation::PendingElicitation>,
    /// In-flight ask_user dialog (per-question select/input walk).
    pending_ask_user: Option<crate::ask_user::PendingAskUser>,
    /// Plugin-provided status line text (ui.set_status).
    ext_label: Option<String>,
    /// v2.1: per-widget panel interaction state (selection/scroll/cache).
    #[cfg(feature = "ext")]
    ext_panel_ui: HashMap<String, widgets::PanelUi>,
    /// v2.1: the ext panel holding keyboard focus (widget host key).
    #[cfg(feature = "ext")]
    ext_panel_focus: Option<String>,
    /// v2.1: host-side master hide for all ext panels (ctrl+b).
    #[cfg(feature = "ext")]
    ext_panels_hidden: bool,
    /// v2.2: extension autocomplete providers (captured at startup).
    #[cfg(feature = "ext")]
    ext_ac_providers: Vec<crate::extension_host::ExtAutocompleteProvider>,
    /// v2.2: bumped per query batch; stale results are discarded.
    ext_ac_generation: u64,
    /// rpiv-todo: shared todo state (rebuilt from session history; the tool
    /// mutates it, the panel renders it).
    todo_state: Arc<Mutex<tack_tools::todo::TodoState>>,
    /// /context: sizes of the last run's fixed context parts (system prompt
    /// chars, serialized tool-schema chars). Zero before the first run.
    context_system_chars: usize,
    context_tools_chars: usize,
    /// Background bash task registry (persists across runs; completion
    /// notifications are drained into the steering queue).
    background_tasks: tack_tools::background::BackgroundTaskManager,
    bg_notify_rx: Option<mpsc::UnboundedReceiver<tack_tools::background::TaskNotification>>,
    /// CodeBuddy rate-limit notices (provider layer has no UI channel; the
    /// provider's global notifier pushes into this channel).
    rate_limit_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// LSP registry (language servers stay warm across runs).
    lsp: tack_tools::lsp::LspManager,
    /// Session-wide sub-agent concurrency cap + token budget. Tools are
    /// rebuilt every prompt, so the limits object must outlive any single
    /// tool instance — otherwise the budget resets each turn and background
    /// children's usage accrues to a stale counters object.
    subagent_limits: Option<Arc<crate::subagent_tool::SubagentLimits>>,
    /// Per-turn file snapshots (enabled per session id at run start).
    checkpoints: tack_tools::checkpoint::CheckpointManager,
    /// Token-budget warning already shown this session.
    budget_warned: bool,
    /// Scheduled prompts (~/.tack/agent/cron.json), checked on the UI tick.
    cron: crate::cron::CronStore,
    cron_last_check: Instant,
    /// Lifecycle hook config (settings + managed + bundle) and executor.
    hook_config: crate::shell_hooks::HookConfig,
    hook_engine: crate::shell_hooks::HookEngine,
    /// PreToolUse permission decisions consumed by the permission hooks.
    hook_decisions: crate::shell_hooks::HookDecisions,
    /// Reentrancy guard: set while a run continues from a Stop-hook block.
    stop_hook_active: bool,
    /// SessionStart hook output, injected into the first run's system prompt.
    session_context: Option<String>,
    /// UI language (settings language / LANG env).
    lang: crate::i18n::Lang,
    /// Newer tack version found by the startup update check (footer hint).
    update_hint: Option<String>,
    /// Ctrl+R incremental history search state (None = inactive).
    history_search: Option<history_search::HistorySearch>,
    /// Desktop-notification throttle (per event source).
    notify_gate: notify::NotifyGate,
    // Channels
    event_tx: AppEventTx,
    event_rx: AppEventRx,
    permission_tx: mpsc::UnboundedSender<PermissionQuery>,
    permission_rx: Option<mpsc::UnboundedReceiver<PermissionQuery>>,
    /// Permission queries waiting for the currently open dialog to close.
    /// Without this queue a second query would REPLACE the open permission
    /// dialog, dropping its oneshot sender — silently denying that tool
    /// call ("denied by user") without any user interaction.
    pending_permissions: std::collections::VecDeque<PermissionQuery>,
    compaction_rx: Option<mpsc::UnboundedReceiver<(String, u64)>>,
    compaction_tx: mpsc::UnboundedSender<(String, u64)>,
    /// Budget enforcement notifications (pause/downgrade triggers).
    budget_rx: Option<mpsc::UnboundedReceiver<String>>,
    budget_tx: mpsc::UnboundedSender<String>,
    // Exit
    should_quit: bool,
    /// --resume at startup: the Sessions picker is open over an in-memory
    /// placeholder session; cancelling it exits (TS: "No session selected").
    resume_startup: bool,
    last_ctrl_c: Option<Instant>,
    /// Last-known footer stats (shown during runs while the session is parked).
    last_stats: footer::FooterStats,
    /// Session (id, revision) `last_stats` was computed from. The recompute
    /// deep-clones every session entry (O(context size)), so it must not run
    /// per frame: keystrokes/paste re-render and would each pay that cost.
    stats_key: Option<(String, u64)>,
    /// Footer stats snapshot at run start (session totals + context estimate
    /// of the outgoing context). Live stats during a run = base + the
    /// streaming message's usage; None when idle.
    run_stats_base: Option<footer::FooterStats>,
    /// MCP sampling usage accrued this session (server-initiated LLM calls
    /// run outside the agent loop, so the session's own totals don't see
    /// them). Folded into the footer stats wherever session totals are.
    mcp_sampling_stats: footer::FooterStats,
    last_esc: Option<Instant>,
}

impl std::fmt::Debug for TuiApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiApp").finish_non_exhaustive()
    }
}

/// Lock a std mutex, recovering from poisoning: a panic in one code path
/// must not cascade into a crash on every subsequent lock (e.g. every
/// keystroke locks the permission-mode mutex — a poisoned lock there used
/// to panic on every keypress and take the whole app down).
pub(crate) fn lock_recover<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Entry point: `tack` with no prompt.
///
/// The event loop is wrapped in `catch_unwind` (mirroring the agent loop):
/// a panic in input/render handling degrades to an error message + non-zero
/// exit instead of a hard crash. The terminal is restored before we get here
/// — `TerminalGuard` inside `run()` drops during unwinding — and the global
/// panic hook has already appended details to `<agent_dir>/crash.log`.
pub async fn run_tui(options: TuiOptions) -> Result<i32> {
    use futures_util::FutureExt;

    let mut app = TuiApp::new(options).await?;
    match std::panic::AssertUnwindSafe(app.run()).catch_unwind().await {
        Ok(result) => result,
        Err(panic) => {
            let payload = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            // Best-effort: writeln! returns a Result instead of panicking
            // when stderr is a broken pipe (unlike eprintln!).
            use std::io::Write as _;
            let stderr = std::io::stderr();
            let mut err = stderr.lock();
            let _ = writeln!(err, "\ntack TUI crashed: {payload}");
            let _ = writeln!(
                err,
                "The session is persisted — restart with `tack --continue` to pick up where you left off."
            );
            Ok(1)
        }
    }
}

/// Display setup result (theme + terminal capability picks) assembled by
/// `TuiApp::init_display`.
struct DisplayConfig {
    theme: Theme,
    fullscreen: bool,
    clear_on_shrink: bool,
    mermaid_enabled: bool,
    image_protocol: Option<tack_tui::image::ImageProtocol>,
}

impl TuiApp {
    pub async fn new(options: TuiOptions) -> Result<Self> {
        let agent_dir = tack_session::default_agent_dir();
        let flags = options.flags.clone();
        // First run: no settings file at all (TS first-time-setup wizard).
        let first_run = !agent_dir.join("settings.json").exists();
        let (settings, catalog_auto_refresh) =
            Self::load_settings(&options.cwd, &agent_dir, &flags);
        let provider: Arc<dyn Provider> = tack_ai::provider_for(&options.model)
            .with_context(|| format!("no adapter for api kind {}", options.model.api))?;

        let (session, resume_placeholder) =
            Self::open_session(&options, &settings, &flags, &agent_dir)?;

        let (event_tx, event_rx) = app_event_bus();
        let (permission_tx, permission_rx) = mpsc::unbounded_channel();
        let (compaction_tx, compaction_rx) = mpsc::unbounded_channel();
        let (budget_tx, budget_rx) = mpsc::unbounded_channel::<String>();

        let (provider, extensions) =
            Self::init_provider_stack(provider, &settings, &event_tx, &options.cwd, &agent_dir)
                .await;
        #[cfg(feature = "ext")]
        let ext_ac_providers = extensions.autocomplete_providers();

        let DisplayConfig {
            theme,
            fullscreen,
            clear_on_shrink,
            mermaid_enabled,
            image_protocol,
        } = Self::init_display(&settings, &flags, &agent_dir, &options.cwd);

        let mut scroll = tack_tui::components::scroll_view::ScrollView::new();
        scroll.scrollbar = true;
        let mut editor = Editor::new();
        editor.focused = true;

        let kb = Self::default_keybindings(&agent_dir);

        let todo_state = rebuild_todo_state(&session);
        let cron = crate::cron::CronStore::load(&agent_dir);
        let (hook_config, hook_engine, session_context) =
            Self::init_hooks(&settings, &agent_dir, &extensions, &options, &session).await;
        let lang = crate::i18n::Lang::resolve(settings.language.as_deref());
        crate::i18n::set_current(lang);
        // Background bash tasks: one registry per app session; completion
        // notifications flow through this channel into the steering queue.
        let background_tasks = tack_tools::background::BackgroundTaskManager::new();
        let (bg_notify_tx, app_bg_notify_rx) = mpsc::unbounded_channel();
        background_tasks.set_notify(bg_notify_tx);
        // CodeBuddy rate-limit events surface as chat notices + desktop
        // notifications (throttled, settings-gated) via this channel.
        let (rate_limit_tx, rate_limit_rx) = mpsc::unbounded_channel::<String>();
        tack_ai::codebuddy::set_rate_limit_notifier(Some(std::sync::Arc::new(move |msg| {
            let _ = rate_limit_tx.send(msg.to_string());
        })));
        let lsp = settings.lsp_manager(&options.cwd);
        let mut app = TuiApp {
            cwd: options.cwd.clone(),
            agent_dir: agent_dir.clone(),
            settings,
            provider,
            auth: options.auth,
            theme,
            state: AppState {
                session,
                model: options.model,
                thinking: options.thinking,
            },
            items: Vec::new(),
            tools: HashMap::new(),
            tool_order: Vec::new(),
            streaming: None,
            stream_rev: 0,
            stream_render: None,
            stream_render_mark: None,
            line_cache: Vec::new(),
            offsets: Vec::new(),
            item_heights: Vec::new(),
            transcript_total: 0,
            tool_render_cache: HashMap::new(),
            tool_heights: HashMap::new(),
            stream_md_cache: stream_md::StreamMarkdownCache::new(),
            running: false,
            compacting: false,
            bang_cancel: HashMap::new(),
            branch_summary_cancel: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            steering: Arc::new(Mutex::new(VecDeque::new())),
            follow_up: Arc::new(Mutex::new(VecDeque::new())),
            pending_send_now: None,
            mode: Arc::new(std::sync::Mutex::new(PermissionMode::default())),
            allow_always: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            custom_system_prompt: options.system_prompt.clone(),
            kb,
            editor,
            autocomplete: None,
            status: None,
            dialog: None,
            tui: {
                let mut tui = Tui::new(if fullscreen {
                    TuiMode::Fullscreen
                } else {
                    TuiMode::Regular
                });
                tui.set_clear_on_shrink(clear_on_shrink);
                tui
            },
            fullscreen,
            scroll,
            last_frame: Vec::new(),
            debug_capture: false,
            ac_sources: None,
            jump_pill: None,
            prompt_rows: Vec::new(),
            selection: None,
            selecting: None,
            search: None,
            image_protocol,
            mermaid_enabled,
            thinking_expanded: false,
            mcp_connections: Vec::new(),
            flags: flags.clone(),
            extensions,
            pending_ext_ui: None,
            pending_elicitation: None,
            pending_ask_user: None,
            ext_label: None,
            #[cfg(feature = "ext")]
            ext_panel_ui: HashMap::new(),
            #[cfg(feature = "ext")]
            ext_panel_focus: None,
            #[cfg(feature = "ext")]
            ext_panels_hidden: false,
            #[cfg(feature = "ext")]
            ext_ac_providers,
            ext_ac_generation: 0,
            todo_state,
            context_system_chars: 0,
            context_tools_chars: 0,
            background_tasks,
            bg_notify_rx: Some(app_bg_notify_rx),
            rate_limit_rx: Some(rate_limit_rx),
            lsp,
            subagent_limits: None,
            checkpoints: tack_tools::checkpoint::CheckpointManager::new(),
            budget_warned: false,
            cron,
            cron_last_check: Instant::now(),
            hook_config,
            hook_engine,
            hook_decisions: crate::shell_hooks::HookDecisions::default(),
            stop_hook_active: false,
            session_context,
            lang,
            update_hint: None,
            history_search: None,
            notify_gate: notify::NotifyGate::default(),
            event_tx,
            event_rx,
            compaction_rx: Some(compaction_rx),
            compaction_tx,
            budget_rx: Some(budget_rx),
            budget_tx,
            permission_tx,
            permission_rx: Some(permission_rx),
            pending_permissions: std::collections::VecDeque::new(),
            should_quit: false,
            resume_startup: false,
            last_ctrl_c: None,
            last_stats: footer::FooterStats::default(),
            stats_key: None,
            run_stats_base: None,
            mcp_sampling_stats: footer::FooterStats::default(),
            last_esc: None,
        };
        // tack-ext: a dead plugin's widgets vanish (peer EOF → AppEvent).
        #[cfg(feature = "ext")]
        for plugin in &app.extensions.plugins {
            let Some(handle) = &plugin.handle else {
                continue;
            };
            let peer = handle.peer().clone();
            let name = plugin.id.to_string();
            let tx = app.event_tx.clone();
            crate::extension_host::watch_plugin_death(peer, move || {
                let _ = tx.send(AppEvent::ExtPluginDead(name));
            });
        }
        app.startup_sequence(
            options.continue_session,
            &flags,
            first_run,
            resume_placeholder,
            catalog_auto_refresh,
        )
        .await;
        Ok(app)
    }

    /// Load settings and apply the startup plumbing driven by them: cached
    /// model catalog override, HTTP tuning and the CLI overrides
    /// (--models / --use-theme / --tui-mode). Returns the settings plus
    /// whether the opt-in background catalog refresh is due.
    fn load_settings(
        cwd: &Path,
        agent_dir: &Path,
        flags: &crate::cli_flags::CliFlags,
    ) -> (Settings, bool) {
        let mut settings = Settings::load(cwd, agent_dir);
        // Model catalog: install a previously refreshed catalog from cache
        // (no network). Startup network refresh is opt-in via
        // `modelCatalogRefresh` (and disabled by --offline / TACK_OFFLINE).
        if let Some((providers, models)) = crate::catalog_refresh::load_cached_override(agent_dir) {
            tracing::info!(
                providers,
                models,
                "model catalog override loaded from cache"
            );
        }
        let catalog_auto_refresh = settings.model_catalog_refresh
            && !flags.offline
            && std::env::var_os("TACK_OFFLINE").is_none();
        if let Some(ms) = settings.http_idle_timeout_ms {
            tack_ai::api::set_http_idle_timeout_ms(ms);
        }
        if let Some(mode) = &settings.transport {
            tack_ai::api::codex_ws::set_transport(mode);
        }
        // CLI overrides (--models / --use-theme / --tui-mode).
        if !flags.models.is_empty() {
            settings.scoped_models = flags.models.clone();
        }
        if let Some(theme) = &flags.use_theme {
            settings.theme = Some(theme.clone());
        }
        if let Some(mode) = &flags.tui_mode {
            settings.tui_mode = Some(mode.clone());
        }
        (settings, catalog_auto_refresh)
    }

    /// Open (or create) the session per the CLI flags. Returns the manager
    /// plus whether it is an in-memory --resume placeholder.
    fn open_session(
        options: &TuiOptions,
        settings: &Settings,
        flags: &crate::cli_flags::CliFlags,
        agent_dir: &Path,
    ) -> Result<(SessionManager, bool)> {
        let session_dir = options
            .session_dir
            .clone()
            .unwrap_or_else(|| tack_session::default_session_dir(&options.cwd, agent_dir));
        // --resume: do NOT create an empty session file just to show the
        // picker (it pollutes the session list and sorts first by mtime).
        // Run on an in-memory placeholder until a session is picked;
        // cancelling the picker exits (TS selectSession parity).
        let resume_placeholder = flags.resume
            && !options.continue_session
            && !flags.no_session
            && flags.session.is_none()
            && flags.session_id.is_none()
            && flags.fork.is_none();
        let mut session = if resume_placeholder {
            SessionManager::in_memory(&options.cwd)
        } else {
            crate::cli_flags::open_session_for_flags(
                &options.cwd,
                Some(session_dir.clone()),
                options.continue_session,
                flags,
                tack_session::SessionBackend::from_setting(settings.session_backend.as_deref()),
            )?
        };
        crate::cli_flags::apply_session_name(&mut session, &flags.name);
        Ok((session, resume_placeholder))
    }

    /// tack-ext plugins (discover + start; failures are logged and skipped)
    /// and the provider stack wrapping it: plugin lifecycle notifications,
    /// then retry backoffs surfaced in the status line.
    async fn init_provider_stack(
        provider: Arc<dyn Provider>,
        settings: &Settings,
        event_tx: &AppEventTx,
        cwd: &Path,
        agent_dir: &Path,
    ) -> (Arc<dyn Provider>, crate::extension_host::ExtensionManager) {
        let ext_services = Arc::new(crate::extension_host::TuiExtServices::new(
            event_tx.clone(),
            crate::project_trust::is_trusted(cwd, agent_dir),
        ));
        let extensions = crate::extension_host::ExtensionManager::load(
            cwd,
            agent_dir,
            "tui",
            ext_services,
            settings.extension_lock_required,
        )
        .await;
        // tack-ext: provider-boundary lifecycle events (before/after request),
        // forwarded to subscribed plugins.
        let provider: Arc<dyn Provider> = Arc::new(crate::extension_host::ExtNotifyProvider::new(
            provider,
            extensions.clone_sink(),
        ));
        // Surface retry backoffs in the status line (attempt + countdown).
        let retry_tx = event_tx.clone();
        let on_retry: tack_ai::retry::RetryScheduledCallback =
            Arc::new(move |attempt, max, delay_ms, error| {
                let _ = retry_tx.send(AppEvent::RetryScheduled(attempt, max, delay_ms, error));
            });
        let provider: Arc<dyn Provider> = Arc::new(tack_ai::retry::RetryingProvider {
            inner: provider,
            policy: settings.retry.policy(),
            on_retry_scheduled: Some(on_retry),
        });
        (provider, extensions)
    }

    /// Re-resolve the provider adapter + request auth after the active model
    /// changed (e.g. `/model`). The adapter is resolved once at startup, so
    /// without this a cross-api switch streams the new model through the old
    /// adapter — which then reports e.g. "No API key for provider: codebuddy"
    /// (the HTTP adapters name the *model's* provider in that error).
    ///
    /// Auth is only re-resolved when the provider id changed, so an explicit
    /// `--api-key` for the startup provider survives same-provider switches.
    fn rebind_provider(&mut self, api_changed: bool, provider_changed: bool) {
        if api_changed {
            match tack_ai::provider_for(&self.state.model) {
                Some(base) => {
                    let provider: Arc<dyn Provider> =
                        Arc::new(crate::extension_host::ExtNotifyProvider::new(
                            base,
                            self.extensions.clone_sink(),
                        ));
                    let retry_tx = self.event_tx.clone();
                    let on_retry: tack_ai::retry::RetryScheduledCallback =
                        Arc::new(move |attempt, max, delay_ms, error| {
                            let _ = retry_tx
                                .send(AppEvent::RetryScheduled(attempt, max, delay_ms, error));
                        });
                    self.provider = Arc::new(tack_ai::retry::RetryingProvider {
                        inner: provider,
                        policy: self.settings.retry.policy(),
                        on_retry_scheduled: Some(on_retry),
                    });
                }
                None => {
                    self.notice(
                        format!("no adapter for api kind {}", self.state.model.api),
                        NoticeKind::Error,
                    );
                }
            }
        }
        if provider_changed {
            self.auth =
                crate::model::resolve_auth(&self.state.model.provider, None, &self.agent_dir);
        }
    }

    /// Theme + terminal capability setup: theme resolution, fullscreen /
    /// mermaid toggles, capability overrides and the image protocol pick.
    fn init_display(
        settings: &Settings,
        flags: &crate::cli_flags::CliFlags,
        agent_dir: &Path,
        cwd: &Path,
    ) -> DisplayConfig {
        let theme = if flags.no_themes {
            Theme::from_name(settings.theme.as_deref())
        } else {
            Theme::resolve(settings.theme.as_deref(), agent_dir, cwd)
        };
        let mut theme = theme;
        if let Some(indent) = &settings.code_block_indent {
            theme.markdown.code_block_indent = indent.chars().count().min(16) as u8;
        }
        let fullscreen = settings.tui_mode.as_deref() == Some("fullscreen");
        let clear_on_shrink = settings.clear_on_shrink;
        let mermaid_enabled = settings.mermaid.as_deref() != Some("off") && settings.show_images;
        // Terminal capability overrides (TS 0.84.4): settings
        // (terminal.hyperlinks/images/trueColor) take precedence over the
        // TACK_HYPERLINKS / TACK_IMAGE_PROTOCOL / TACK_TRUE_COLOR env vars; both
        // are applied inside Capabilities::detect() from here on.
        tack_tui::terminal::set_capability_overrides(settings.terminal_capability_overrides);
        // Legacy tack-only override: imageProtocol setting /
        // TACK_IMAGE_PROTOCOL env ("kitty" | "iterm2" | "half-block" — the
        // last forces the fallback). Wins over the TS-parity overrides above.
        let proto_override = std::env::var("TACK_IMAGE_PROTOCOL")
            .ok()
            .or_else(|| {
                settings
                    .raw()
                    .get("imageProtocol")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .map(|s| s.to_lowercase());
        let image_protocol = if !settings.show_images {
            None
        } else {
            match proto_override.as_deref() {
                Some("kitty") => Some(tack_tui::image::ImageProtocol::Kitty),
                Some("iterm2") | Some("iterm") => Some(tack_tui::image::ImageProtocol::ITerm2),
                Some("half-block") | Some("halfblock") | Some("none") => None,
                _ => {
                    let caps = tack_tui::terminal::Capabilities::detect();
                    if caps.kitty_images {
                        Some(tack_tui::image::ImageProtocol::Kitty)
                    } else if caps.iterm2_images {
                        Some(tack_tui::image::ImageProtocol::ITerm2)
                    } else {
                        None
                    }
                }
            }
        };
        DisplayConfig {
            theme,
            fullscreen,
            clear_on_shrink,
            mermaid_enabled,
            image_protocol,
        }
    }

    /// Keybindings: TS pi's app-level defaults, overridable via
    /// <agentDir>/keybindings.json (same schema).
    fn default_keybindings(agent_dir: &Path) -> tack_tui::keys::Keybindings {
        let mut kb = tack_tui::keys::Keybindings::new();
        kb.register("app.interrupt", &["escape"]);
        kb.register("app.clear", &["ctrl+c"]);
        kb.register("app.exit", &["ctrl+d"]);
        kb.register("app.tools.expand", &["ctrl+o"]);
        kb.register("app.editor.external", &["ctrl+g"]);
        kb.register("app.clipboard.pasteImage", &["ctrl+v", "alt+v"]);
        kb.register("app.model.cycleForward", &["ctrl+p"]);
        kb.register("app.model.cycleBackward", &["shift+ctrl+p", "ctrl+shift+p"]);
        kb.register("app.model.select", &["ctrl+l"]);
        kb.register("app.models.save", &[]);
        kb.register("app.mode.cycle", &["shift+tab"]);
        // TS assigns shift+tab to app.thinking.cycle; tack keeps shift+tab
        // for permission-mode cycling (Claude-Code convention), so the
        // thinking actions ship unbound (rebindable via keybindings.json).
        kb.register("app.thinking.cycle", &[]);
        kb.register("app.thinking.toggle", &["ctrl+t"]);
        kb.register("app.message.copy", &["ctrl+x"]);
        // alt+↑ needs a terminal that reports Option as Meta/Esc+ (stock
        // Terminal.app and iTerm2's defaults swallow it). ctrl+r used to be
        // a second binding here; it now opens the history reverse search
        // (bash convention), leaving alt+up as the dequeue binding.
        kb.register("app.message.dequeue", &["alt+up"]);
        // Bash-style incremental history search (reverse-i-search).
        kb.register("app.editor.historySearch", &["ctrl+r"]);
        // Enter queues while the agent runs; Ctrl+Enter aborts the run and
        // sends immediately. Ctrl+S is the non-Kitty fallback (Ctrl+Enter
        // needs the Kitty keyboard protocol to be distinguishable).
        kb.register("app.message.sendNow", &["ctrl+enter", "ctrl+s"]);
        // alt+enter is an editor newline (Claude-Code convention); the
        // follow-up queue action ships unbound, rebindable via
        // keybindings.json.
        kb.register("app.message.followUp", &[]);
        kb.register("app.session.new", &[]);
        kb.register("app.session.tree", &[]);
        kb.register("app.session.fork", &[]);
        kb.register("app.session.resume", &[]);
        kb.register("app.session.rename", &[]);
        kb.register("app.session.delete", &[]);
        kb.register("app.suspend", if cfg!(windows) { &[] } else { &["ctrl+z"] });
        kb.register("app.search.open", &["ctrl+shift+f"]);
        kb.register("app.search.next", &["enter", "ctrl+g"]);
        kb.register("app.search.previous", &["shift+enter", "ctrl+shift+g"]);
        kb.register("app.search.close", &["escape"]);
        kb.register("app.scroll.promptPrevious", &["ctrl+shift+up"]);
        kb.register("app.scroll.promptNext", &["ctrl+shift+down"]);
        // tack-ext panels (v2.1): host-side visibility toggle + focus cycle.
        // ctrl+b / alt+p are unused by the editor and other app actions.
        kb.register("app.ext.panels.toggle", &["ctrl+b"]);
        kb.register("app.ext.panel.focusNext", &["alt+p"]);
        kb.apply_overrides(&agent_dir.join("keybindings.json"));
        kb
    }

    /// Lifecycle hooks: settings hooks.* + managed hooks + bundle hooks
    /// contributed by installed extensions. Runs the SessionStart hooks;
    /// their additionalContext becomes extra system-prompt context for the
    /// first run (UserPromptSubmit can block later prompts).
    async fn init_hooks(
        settings: &Settings,
        agent_dir: &Path,
        extensions: &crate::extension_host::ExtensionManager,
        options: &TuiOptions,
        session: &SessionManager,
    ) -> (
        crate::shell_hooks::HookConfig,
        crate::shell_hooks::HookEngine,
        Option<String>,
    ) {
        let mut hook_config = if settings.features.shell_hooks {
            crate::shell_hooks::load_hooks_config(settings, agent_dir)
        } else {
            crate::shell_hooks::HookConfig::default()
        };
        hook_config.extend(extensions.bundle_hooks.clone());
        let hook_shell = tack_tools::shell::resolve_shell(settings.shell_path.as_deref())
            .ok()
            .map(Arc::new);
        let hook_engine =
            crate::shell_hooks::HookEngine::new(hook_shell.clone(), options.cwd.clone())
                .with_evaluator(Arc::new(crate::shell_hooks::LlmEvaluator {
                    model: options.model.clone(),
                    auth: options.auth.clone(),
                    agent_dir: agent_dir.to_path_buf(),
                    cwd: options.cwd.clone(),
                }));
        let mut session_context = String::new();
        {
            let groups = hook_config.take_groups(crate::shell_hooks::HookEvent::SessionStart);
            if !groups.is_empty() {
                let verdict = hook_engine
                    .run(
                        &groups,
                        None,
                        &serde_json::json!({
                            "session_id": session.session_id(),
                            "transcript_path": serde_json::Value::Null,
                            "cwd": options.cwd,
                            "hook_event_name": "SessionStart",
                            "model": options.model.id,
                            "permission_mode": "default",
                            "source": "startup",
                        }),
                    )
                    .await;
                for context in &verdict.additional_context {
                    session_context.push_str(context);
                    session_context.push('\n');
                }
                for message in &verdict.system_messages {
                    tracing::warn!("SessionStart hook: {message}");
                }
            }
        }
        let session_context = if session_context.trim().is_empty() {
            None
        } else {
            Some(session_context)
        };
        (hook_config, hook_engine, session_context)
    }

    /// Post-construction startup sequence: background catalog refresh and
    /// managed-tools downloads, plugin shortcuts, welcome/changelog/
    /// onboarding transcript entries, first-run and trust dialogs, the
    /// --resume picker and the tack-ext session_start lifecycle event.
    async fn startup_sequence(
        &mut self,
        continue_session: bool,
        flags: &crate::cli_flags::CliFlags,
        first_run: bool,
        resume_placeholder: bool,
        catalog_auto_refresh: bool,
    ) {
        // Opt-in startup catalog refresh (settings `modelCatalogRefresh`):
        // fetch in the background; result lands as a notice, never blocks
        // startup and never fails the session.
        if catalog_auto_refresh {
            let notify = self.budget_tx.clone();
            let agent_dir_rc = self.agent_dir.clone();
            crate::task::spawn_guarded("catalog-refresh", async move {
                match crate::catalog_refresh::refresh_from_npm(
                    &agent_dir_rc,
                    std::time::Duration::from_secs(30),
                )
                .await
                {
                    Ok(s) => {
                        let _ = notify.send(crate::i18n::trf(
                            "msg.catalog_refreshed_startup",
                            &[
                                ("version", &s.version),
                                ("providers", &s.providers.to_string()),
                                ("models", &s.models.to_string()),
                            ],
                        ));
                    }
                    Err(e) => {
                        tracing::warn!("model catalog refresh failed: {e:#}");
                    }
                }
            });
        }
        self.welcome_banner();
        // Startup update check: background GitHub query (24h cache; skipped
        // offline / when settings updateCheck is false). Never blocks.
        self.start_update_check();
        if self.settings.quiet_startup {
            self.items.clear(); // quietStartup: no banner, no changelog
        } else if continue_session {
            self.replay_transcript();
        } else if let Some(markdown) = crate::changelog::startup_markdown(&self.agent_dir) {
            // Post-upgrade "what's new" (skipped for resumed sessions, like TS).
            self.items
                .push(chat::TranscriptItem::Chat(chat::ChatEntry::Markdown {
                    text: markdown,
                }));
        }
        // Project trust: resources are already gated as untrusted; ask now and
        // reload on a trust decision (TS resolveProjectTrusted prompt).
        // Skipped on the very first run — the setup wizard takes the dialog
        // slot (trust is asked on the next launch).
        if !first_run
            && crate::project_trust::state(&self.cwd, &self.agent_dir)
                == crate::project_trust::TrustState::Ask
        {
            self.open_trust_dialog();
        }
        // First-run wizard (TS FirstTimeSetupComponent: theme choice; the
        // analytics step is N/A — tack has no telemetry).
        if first_run {
            self.open_first_run_dialog();
        }
        // Getting-started guidance when NO provider can authenticate:
        // without it a fresh install fails on the first prompt with a bare
        // "no API key" error. Point at the discovery commands instead.
        if !continue_session {
            let custom_has_key = tack_ai::providers::load_custom_providers(&self.agent_dir)
                .iter()
                .any(|cp| cp.api_key.is_some());
            let any_auth = custom_has_key
                || tack_ai::providers::BUILTIN_PROVIDERS
                    .iter()
                    .any(|d| crate::model::provider_has_auth(&self.agent_dir, d.id));
            if !any_auth {
                self.items
                    .push(chat::TranscriptItem::Chat(chat::ChatEntry::Markdown {
                        text: crate::i18n::tr("msg.onboarding_no_credentials"),
                    }));
            }
        }
        // Managed tools (TS interactive startup): make sure fd/rg exist for
        // the agent's bash usage — download into <agent>/bin if missing.
        // Runs in the background; statuses surface as transcript notices.
        {
            let tx = self.event_tx.clone();
            let dir = self.agent_dir.clone();
            let offline = flags.offline;
            crate::task::spawn_guarded("startup-tools", async move {
                for status in crate::tools_manager::ensure_startup_tools(&dir, offline).await {
                    let (msg, kind) = match status {
                        crate::tools_manager::ToolStatus::Info(m) => (m, NoticeKind::Info),
                        crate::tools_manager::ToolStatus::Warning(m) => (m, NoticeKind::Warning),
                    };
                    if tx.send(AppEvent::Notice(msg, kind)).is_err() {
                        break;
                    }
                }
            });
        }
        // --resume: open the session picker right away.
        if flags.resume {
            self.resume_startup = resume_placeholder;
            if !self.command_resume() {
                // Nothing to resume: TS exits here.
                eprintln!("no previous sessions for this directory");
                self.should_quit = true;
            }
        }
        // tack-ext: session_start lifecycle event.
        self.extensions
            .notify(
                "session_start",
                serde_json::json!({
                    "sessionId": self.state.session.session_id(),
                    "resumed": continue_session,
                    "cwd": self.cwd.to_string_lossy(),
                }),
            )
            .await;
    }

    fn welcome_banner(&mut self) {
        self.items
            .push(chat::TranscriptItem::Chat(ChatEntry::notice(
                crate::i18n::t(
                    self.lang,
                    "msg.welcome",
                    &[
                        ("version", env!("CARGO_PKG_VERSION")),
                        ("model", &self.state.model.id),
                        ("provider", &self.state.model.provider),
                    ],
                ),
                NoticeKind::Info,
            )));
    }

    /// Rebuild transcript entries from the resumed session's messages.
    pub fn replay_transcript(&mut self) {
        let messages = self.state.session.build_session_context().messages;
        self.items.clear();
        self.tools.clear();
        self.tool_order.clear();
        self.line_cache.clear();
        // Map tool results onto their calls.
        let results: HashMap<String, &tack_ai::ToolResultMessage> = messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(t) => Some((t.tool_call_id.clone(), t)),
                _ => None,
            })
            .collect();
        for message in &messages {
            match message {
                AgentMessage::User(u) => {
                    let text = match &u.content {
                        tack_ai::UserContent::Text(t) => t.clone(),
                        tack_ai::UserContent::Blocks(b) => b
                            .iter()
                            .filter_map(|b| match b {
                                tack_ai::InputContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    };
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::User { text }));
                }
                AgentMessage::Assistant(a) => {
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::Assistant {
                            message: a.clone(),
                            streaming: false,
                        }));
                    for block in &a.content {
                        if let tack_ai::ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } = block
                        {
                            let result = results.get(id);
                            self.register_tool(tool_render::ToolEntry {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                                args_fp: tool_render::args_fingerprint(arguments),
                                args: arguments.clone(),
                                state: match result {
                                    Some(r) => tool_render::ToolState::Done {
                                        result: tack_agent_core::AgentToolResult {
                                            content: r.content.clone(),
                                            details: r.details.clone().unwrap_or_default(),
                                            usage: r.usage.clone(),
                                            terminate: false,
                                            added_tool_names: None,
                                        },
                                        is_error: r.is_error,
                                    },
                                    None => tool_render::ToolState::Done {
                                        result: tack_agent_core::AgentToolResult::text(""),
                                        is_error: false,
                                    },
                                },
                                expanded: false,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn register_tool(&mut self, entry: tool_render::ToolEntry) {
        self.tool_order.push(entry.tool_call_id.clone());
        self.items
            .push(chat::TranscriptItem::Tool(entry.tool_call_id.clone()));
        self.tools.insert(entry.tool_call_id.clone(), entry);
    }

    /// Test helper: force the terminal size (headless tests have no TTY).
    #[doc(hidden)]
    pub fn test_resize(&mut self, width: u16, height: u16) {
        self.tui.resize(width, height);
    }

    /// Test helper: current editor text.
    #[doc(hidden)]
    pub fn test_editor_text(&self) -> String {
        self.editor.text()
    }

    /// Test helper: advance the spinner one frame.
    #[doc(hidden)]
    pub fn test_tick(&mut self) {
        if let Some(status) = &mut self.status {
            status.tick();
        }
    }

    /// Test helper: pretend an agent run is active (queue paths).
    #[doc(hidden)]
    pub fn test_set_running(&mut self, running: bool) {
        self.running = running;
    }

    /// Test helper: push a chat entry into the transcript (render benches).
    #[doc(hidden)]
    pub fn test_push_chat(&mut self, entry: chat::ChatEntry) {
        self.items.push(chat::TranscriptItem::Chat(entry));
    }

    /// Test helper: queued steering/follow-up messages (FIFO).
    #[doc(hidden)]
    pub fn test_queued_messages(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .steering
            .try_lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default();
        if let Ok(q) = self.follow_up.try_lock() {
            out.extend(q.iter().cloned());
        }
        out
    }

    /// Test helper: drive an app event (RunFinished, agent events, ...).
    #[doc(hidden)]
    pub async fn test_app_event(&mut self, event: AppEvent) {
        self.handle_app_event(event).await;
    }

    /// Test helper: drop any startup dialog (first-run wizard, trust
    /// prompt, session picker). Tests that route input through
    /// `handle_input` need the editor reachable; parallel tests race on
    /// TACK_AGENT_DIR, so a dialog may open despite the fixture.
    #[doc(hidden)]
    pub fn test_close_dialog(&mut self) {
        self.dialog = None;
        self.resume_startup = false;
    }

    /// Test helper: is a run active.
    #[doc(hidden)]
    pub fn test_is_running(&self) -> bool {
        self.running
    }

    /// Test helper: toggle background-task auto-wake.
    #[doc(hidden)]
    pub fn test_set_background_auto_wake(&mut self, enabled: bool) {
        self.settings.background_auto_wake = enabled;
    }

    /// Test helper: register an ext widget without a running plugin.
    #[cfg(feature = "ext")]
    #[doc(hidden)]
    pub fn test_register_ext_widget(&mut self, plugin: &str, spec: tack_ext::rpc3::WidgetSpec) {
        self.extensions.test_insert_widget(plugin, spec);
    }

    /// Test helper: drain pending app events (as the main loop would).
    #[doc(hidden)]
    pub async fn test_pump_events(&mut self) {
        while let Some(event) = self.event_rx.try_recv() {
            self.handle_app_event(event).await;
        }
    }

    /// Test helper: labels of the current autocomplete popup.
    #[doc(hidden)]
    pub fn test_autocomplete_labels(&self) -> Vec<String> {
        self.autocomplete
            .as_ref()
            .map(|a| a.list.items.iter().map(|i| i.label.clone()).collect())
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------
    // Input handling
    // -----------------------------------------------------------------

    pub(crate) async fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Notice(text, kind) => {
                self.notice(text, kind);
            }
            AppEvent::RetryScheduled(attempt, max, delay_ms, _error) => {
                self.status = Some(status::StatusIndicator::retrying(attempt, max, delay_ms));
            }
            AppEvent::CompactionSummary(summary, tokens_before) => {
                self.items
                    .push(chat::TranscriptItem::Chat(ChatEntry::CompactionSummary {
                        summary,
                        tokens_before,
                    }));
                // Push keeps line_cache alignment; no clear (same rationale
                // as MessageEnd — earlier entries stay cache-valid).
            }
            AppEvent::ManualCompactDone {
                session_id,
                leaf,
                result,
            } => {
                self.handle_manual_compact_done(session_id, leaf, result)
                    .await;
            }
            AppEvent::BangDone {
                id,
                command,
                session_id,
                exclude_from_context,
                cancelled,
                result,
                is_error,
            } => {
                self.handle_bang_done(
                    &id,
                    &command,
                    &session_id,
                    commands::context::BangOutcome {
                        exclude_from_context,
                        cancelled,
                        result,
                        is_error,
                    },
                );
            }
            AppEvent::BranchSummaryDone {
                session_id,
                from_id,
                result,
            } => {
                self.handle_branch_summary_done(session_id, from_id, result);
            }
            AppEvent::SessionBack(session) => {
                self.state.session = *session;
            }
            AppEvent::McpElicitation(query) => {
                let pending = crate::mcp_elicitation::PendingElicitation::new(query);
                if pending.current().is_none() {
                    // Nothing to ask (empty schema): accept with empty content.
                    pending.finish();
                } else if self.dialog.is_some() || self.pending_elicitation.is_some() {
                    // Never clobber an open dialog; let the server continue.
                    pending.decline();
                } else {
                    self.open_elicitation_dialog(&pending);
                    self.pending_elicitation = Some(pending);
                }
            }
            AppEvent::AskUser(query) => {
                let pending = crate::ask_user::PendingAskUser::new(query);
                if pending.current().is_none() {
                    // Nothing to ask: resolve immediately with no answers.
                    pending.finish();
                } else if self.dialog.is_some()
                    || self.pending_elicitation.is_some()
                    || self.pending_ask_user.is_some()
                {
                    // Never clobber an open dialog; the tool result tells
                    // the model the user declined so the run continues.
                    pending.cancel();
                } else {
                    self.open_ask_user_dialog(&pending);
                    self.pending_ask_user = Some(pending);
                }
            }
            AppEvent::RunContextReady {
                connections,
                tools_chars,
                system_chars,
            } => {
                self.mcp_connections = connections;
                self.context_tools_chars = tools_chars;
                self.context_system_chars = system_chars;
            }
            AppEvent::McpSamplingDone { usage, model } => {
                self.handle_mcp_sampling_done(&usage, &model);
            }
            AppEvent::ExtUiRequest(request) => {
                // Without extensions no plugin exists to send this; drop.
                #[cfg(feature = "ext")]
                self.handle_ext_ui_request(request).await;
                #[cfg(not(feature = "ext"))]
                let _ = request;
            }
            #[cfg(feature = "ext")]
            AppEvent::ExtWidgetUpdate { plugin, update } => {
                if !self.extensions.apply_widget_update(&plugin, &update) {
                    // Unknown widget (plugin racing its death, or a typo):
                    // warn and ignore — never break the UI over it.
                    tracing::warn!("widget.update for unknown widget {plugin}:{}", update.id);
                } else {
                    // A plugin-provided selectedId re-seeds the host's
                    // list-panel selection cursor on each update.
                    let key = format!("{plugin}:{}", update.id);
                    if let Some(widget) = self.extensions.widgets().iter().find(|w| w.key == key)
                        && widget.spec.r#type == tack_ext::rpc3::WidgetKind::ListPanel
                        && let Some(state) = widget.state.clone()
                        && let Ok(state) =
                            serde_json::from_value::<tack_ext::rpc3::ListPanelState>(state)
                        && let Some(selected_id) = state.selected_id
                        && let Some(pos) = state.items.iter().position(|i| i.id == selected_id)
                    {
                        self.ext_panel_ui.entry(key).or_default().selected = pos;
                    }
                }
            }
            AppEvent::ExtPluginDead(plugin) => {
                #[cfg(feature = "ext")]
                {
                    let removed = self.extensions.remove_plugin_widgets(&plugin);
                    if !removed.is_empty() {
                        tracing::info!("plugin {plugin} died: dropped {} widget(s)", removed.len());
                    }
                    for key in removed {
                        self.ext_panel_ui.remove(&key);
                        if self.ext_panel_focus.as_deref() == Some(key.as_str()) {
                            self.ext_panel_focus = None;
                        }
                    }
                }
                #[cfg(not(feature = "ext"))]
                let _ = plugin;
            }
            AppEvent::ExtAutocompleteReady { generation, auto } => {
                self.apply_ext_autocomplete(generation, auto);
            }
            AppEvent::UpdateAvailable(version) => {
                self.set_update_hint(&version);
            }
            AppEvent::RunFinished => {
                self.running = false;
                self.status = None;
                self.run_stats_base = None;
                // Stop hooks. A "block" verdict continues the agent with the
                // reason as a steering message (Claude semantics); honored at
                // most once per stop point to bound hook-induced loops.
                let stop_groups = self
                    .hook_config
                    .take_groups(crate::shell_hooks::HookEvent::Stop);
                if !stop_groups.is_empty() {
                    let totals = self.state.session.session_totals();
                    let last_text = self
                        .items
                        .iter()
                        .rev()
                        .find_map(|item| match item {
                            chat::TranscriptItem::Chat(chat::ChatEntry::Assistant {
                                message,
                                ..
                            }) => Some(
                                message
                                    .content
                                    .iter()
                                    .filter_map(|b| match b {
                                        tack_ai::ContentBlock::Text { text, .. } => {
                                            Some(text.as_str())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let payload = serde_json::json!({
                        "session_id": self.state.session.session_id(),
                        "transcript_path": serde_json::Value::Null,
                        "cwd": self.cwd,
                        "hook_event_name": "Stop",
                        "model": self.state.model.id,
                        "permission_mode": self.mode.lock().map(|m| m.as_str().to_string()).unwrap_or_default(),
                        "stop_hook_active": self.stop_hook_active,
                        "last_assistant_message": last_text,
                        "totalTokens": totals.total_tokens,
                        "totalCost": totals.cost.total,
                    });
                    let verdict = self.hook_engine.run(&stop_groups, None, &payload).await;
                    if let (Some(reason), false) = (&verdict.blocked, self.stop_hook_active) {
                        let reason = reason.clone();
                        self.stop_hook_active = true;
                        self.notice(
                            crate::i18n::t(
                                self.lang,
                                "notice.stop_hook_continue",
                                &[("reason", &reason)],
                            ),
                            chat::NoticeKind::Info,
                        );
                        self.start_run_with_content(tack_ai::UserContent::Text(format!(
                            "<stop_hook_feedback>\n{reason}\n</stop_hook_feedback>"
                        )))
                        .await;
                        return;
                    }
                }
                // Desktop notification: run finished/failed (per-source
                // throttled; a user-initiated abort stays silent — the
                // user is already looking at the terminal).
                let last_stop = self.streaming.as_ref().map(|a| a.stop_reason).or_else(|| {
                    self.items.iter().rev().find_map(|item| match item {
                        chat::TranscriptItem::Chat(chat::ChatEntry::Assistant {
                            message, ..
                        }) => Some(message.stop_reason),
                        _ => None,
                    })
                });
                match last_stop {
                    Some(tack_ai::StopReason::Aborted) => {}
                    other => {
                        let key = if other == Some(tack_ai::StopReason::Error) {
                            "notify.run_error"
                        } else {
                            "notify.run_done"
                        };
                        let title =
                            crate::i18n::t(self.lang, key, &[("model", &self.state.model.id)]);
                        self.desktop_notify("run", "tack", &title);
                    }
                }
                if let Some(partial) = self.streaming.take() {
                    self.stream_rev += 1;
                    self.items
                        .push(chat::TranscriptItem::Chat(ChatEntry::Assistant {
                            message: partial,
                            streaming: false,
                        }));
                }
                // Finalize any running tools (shouldn't happen, but stay robust).
                for id in self.tool_order.clone() {
                    if let Some(tool) = self.tools.get_mut(&id)
                        && matches!(tool.state, tool_render::ToolState::Running { .. })
                    {
                        tool.state = tool_render::ToolState::Done {
                            result: tack_agent_core::AgentToolResult::error("interrupted"),
                            is_error: true,
                        };
                    }
                }
                self.line_cache.clear();
                // Token budget: warn once when the session crosses it.
                if let Some(budget) = self.settings.token_budget
                    && !self.budget_warned
                {
                    let totals = self.state.session.session_totals();
                    if totals.total_tokens > budget {
                        self.budget_warned = true;
                        self.notice(
                            crate::i18n::t(
                                self.lang,
                                "notice.budget",
                                &[
                                    ("used", &totals.total_tokens.to_string()),
                                    ("budget", &budget.to_string()),
                                    ("cost", &format!("{:.4}", totals.cost.total)),
                                ],
                            ),
                            chat::NoticeKind::Warning,
                        );
                    }
                }
                // "Send now" (ctrl+enter while running): the aborted run is
                // over — drop the queued echo and submit for real.
                if let Some(text) = self.pending_send_now.take() {
                    self.items.retain(|item| {
                        !matches!(
                            item,
                            chat::TranscriptItem::Chat(ChatEntry::Queued { text: t, .. })
                                if *t == text
                        )
                    });
                    self.line_cache.clear();
                    self.on_submit(text).await;
                }
            }
            AppEvent::Agent(event) => self.handle_agent_event(*event).await,
        }
    }

    /// Agent profile directory captured at construction (tests should use
    /// this instead of the process-global TACK_AGENT_DIR, which parallel
    /// tests rewrite).
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }
}

/// Rebuild the todo state from the latest todo tool result in the session
/// (survives compaction/resume — the result's details carry the full list).
fn rebuild_todo_state(session: &SessionManager) -> Arc<Mutex<tack_tools::todo::TodoState>> {
    let state = session
        .build_session_path()
        .iter()
        .rev()
        .find_map(|entry| {
            if let tack_session::SessionEntry::Message { message, .. } = entry
                && let AgentMessage::ToolResult(t) = message
                && t.tool_name == "todo"
            {
                return t
                    .details
                    .as_ref()
                    .map(tack_tools::todo::TodoState::from_json);
            }
            None
        })
        .unwrap_or_default();
    Arc::new(Mutex::new(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_update(id: &str) -> AgentEvent {
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: id.to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::Value::Null,
            partial_result: tack_agent_core::AgentToolResult::text(format!("out-{id}")),
        }
    }

    fn message_update() -> AgentEvent {
        let model = Model {
            id: "mock".to_string(),
            name: "Mock".to_string(),
            api: "mock".to_string(),
            provider: "mock".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        };
        let partial = tack_ai::AssistantMessage::pending(&model);
        AgentEvent::MessageUpdate {
            assistant_message_event: tack_ai::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".to_string(),
                partial: partial.clone(),
            },
            message: AgentMessage::Assistant(partial),
        }
    }

    #[test]
    fn fold_run_stats_accumulates_usage_and_estimates_output() {
        let base = FooterStats {
            input: 1000,
            output: 200,
            cache_read: 500,
            cache_write: 50,
            cost: 0.5,
            context_tokens: 10_000,
        };
        // Usage reported: straight accumulation.
        let model = Model {
            id: "m".into(),
            name: "m".into(),
            api: "x".into(),
            provider: "x".into(),
            base_url: "http://x".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            context_window: 1000,
            max_tokens: 100,
            sampling_params: None,
            headers: None,
            compat: None,
        };
        let mut a = tack_ai::AssistantMessage::pending(&model);
        a.usage.input = 300;
        a.usage.output = 40;
        a.usage.cache_read = 20;
        a.usage.cost.total = 0.01;
        let s = fold_run_stats(&base, &a);
        assert_eq!(s.input, 1300);
        assert_eq!(s.output, 240);
        assert_eq!(s.cache_read, 520);
        assert!((s.cost - 0.51).abs() < 1e-9);
        assert_eq!(s.context_tokens, 10_040);

        // No usage yet (provider reports only in the final chunk): output is
        // estimated from generated text so the footer still moves mid-stream.
        let mut b = tack_ai::AssistantMessage::pending(&model);
        b.content = vec![tack_ai::ContentBlock::text("x".repeat(400))];
        let s2 = fold_run_stats(&base, &b);
        assert_eq!(s2.output, 200 + 100, "chars/4 fallback");
        assert_eq!(s2.context_tokens, 10_100);
    }

    /// A MessageUpdate snapshots the whole assistant message, so the latest
    /// always supersedes an earlier one.
    #[test]
    fn message_updates_collapse() {
        assert!(stream_update_supersedes(
            &message_update(),
            &message_update()
        ));
    }

    /// A ToolExecutionUpdate snapshots one tool's cumulative partial result,
    /// so it supersedes an earlier update for the SAME tool call.
    #[test]
    fn tool_updates_for_same_call_collapse() {
        assert!(stream_update_supersedes(
            &tool_update("a"),
            &tool_update("a")
        ));
    }

    /// Parallel tool execution interleaves updates for different tool calls;
    /// collapsing across them would drop the other tool's progress entirely.
    #[test]
    fn tool_updates_for_different_calls_do_not_collapse() {
        assert!(!stream_update_supersedes(
            &tool_update("a"),
            &tool_update("b")
        ));
    }

    /// A tool update does not carry message state (and vice versa), so the
    /// two stream kinds must never collapse into each other.
    #[test]
    fn message_and_tool_updates_do_not_collapse() {
        assert!(!stream_update_supersedes(
            &message_update(),
            &tool_update("a")
        ));
        assert!(!stream_update_supersedes(
            &tool_update("a"),
            &message_update()
        ));
    }

    /// The app event bus collapses a superseded streaming update AT THE
    /// TAIL of the queue (latest-value slot): a slow app loop can never
    /// backlog an unbounded tail of stale streaming frames. Other events
    /// keep FIFO order, and updates separated by a different event are
    /// both delivered.
    #[tokio::test]
    async fn bus_collapses_superseded_streaming_updates() {
        let (tx, rx) = app_event_bus();
        let agent = |e: AgentEvent| AppEvent::Agent(Box::new(e));
        assert!(tx.send(agent(message_update())).is_ok());
        assert!(tx.send(agent(message_update())).is_ok()); // supersedes the queued one
        assert!(tx.send(agent(tool_update("a"))).is_ok());
        assert!(tx.send(agent(tool_update("b"))).is_ok()); // different tool: no collapse
        assert!(tx.send(agent(tool_update("b"))).is_ok()); // supersedes queued "b"
        assert!(tx.send(AppEvent::RunFinished).is_ok());
        assert!(tx.send(agent(message_update())).is_ok()); // behind RunFinished: kept

        let mut kinds = Vec::new();
        while let Some(event) = rx.try_recv() {
            kinds.push(match &event {
                AppEvent::Agent(e) => match e.as_ref() {
                    AgentEvent::MessageUpdate { .. } => "msg",
                    AgentEvent::ToolExecutionUpdate { tool_call_id, .. } => {
                        if tool_call_id == "a" {
                            "tool-a"
                        } else {
                            "tool-b"
                        }
                    }
                    _ => "other",
                },
                AppEvent::RunFinished => "fin",
                _ => "other",
            });
        }
        assert_eq!(
            kinds,
            vec!["msg", "tool-a", "tool-b", "fin", "msg"],
            "one collapsed msg update, tool updates in order, tail preserved"
        );
    }

    /// mpsc semantics: send fails once the receiver is gone; recv returns
    /// None once the queue is drained and all senders are dropped.
    #[tokio::test]
    async fn bus_close_semantics() {
        let (tx, rx) = app_event_bus();
        assert!(tx.send(AppEvent::RunFinished).is_ok());
        let tx2 = tx.clone();
        drop(tx);
        drop(tx2);
        assert!(matches!(rx.recv().await, Some(AppEvent::RunFinished)));
        assert!(rx.recv().await.is_none());

        let (tx, rx) = app_event_bus();
        drop(rx);
        assert!(tx.send(AppEvent::RunFinished).is_err());
    }
}
