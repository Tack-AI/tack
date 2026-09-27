//! Shared tool services injected into every tool at construction.

use std::path::PathBuf;
use std::sync::Arc;

use crate::executor::BashExecutor;
use crate::shell::ShellConfig;

/// Per-tool-call services: working directory, the file-mutation lock (port of
/// `file-mutation-queue.ts` — a single fair mutex; pi serializes per path,
/// one global lock is a conservative simplification), and shell config.
#[derive(Clone)]
pub struct ToolServices {
    pub cwd: PathBuf,
    pub mutation_lock: Arc<tokio::sync::Mutex<()>>,
    pub shell: Option<Arc<ShellConfig>>,
    /// Custom bash execution backend (e.g. ACP client terminals). When None,
    /// bash runs locally via `shell`.
    pub bash_executor: Option<Arc<dyn BashExecutor>>,
    /// Background bash task registry. Persist one instance across agent runs
    /// so tasks survive; `ToolServices::new` creates a fresh one.
    pub background: crate::background::BackgroundTaskManager,
    /// Language-server registry (LSP diagnostics). Lazily spawns servers;
    /// persist across runs to keep them warm.
    pub lsp: crate::lsp::LspManager,
    /// Per-turn file snapshots for rollback. Disabled until the host enables
    /// it with a session-scoped storage root.
    pub checkpoints: crate::checkpoint::CheckpointManager,
    /// OS sandbox policy for bash execution (None = unsandboxed). The
    /// backend (bwrap/seatbelt) is resolved per execution.
    pub sandbox: Option<crate::sandbox::SandboxSpec>,
    /// When web_fetch may use headless-browser rendering (settings webRender).
    pub web_render: crate::browser::WebRenderMode,
    /// web_search backend + API key (settings webSearch).
    pub web_search: crate::web::WebSearchConfig,
    /// features.backgroundTasks=false: bash hides run_in_background and
    /// bash_output/bash_wait/kill_shell are not registered (host-side filter).
    pub background_tasks_enabled: bool,
    /// Extra environment applied to every locally-executed command (e.g.
    /// a per-worktree CARGO_TARGET_DIR for isolated sub-agents).
    pub env: Vec<(String, std::ffi::OsString)>,
    /// Set by web/MCP tools when untrusted external content enters the
    /// context. Permission hooks use it to re-prompt for mutating tools even
    /// in acceptEdits / allow-always paths (prompt-injection defense).
    pub untrusted_seen: Arc<std::sync::atomic::AtomicBool>,
    /// User-scope memory root override (settings `memoryDirectory`).
    /// `TACK_MEMORY_DIR` env wins over this; see `memory::user_memory_dir`.
    pub memory_dir_override: Option<PathBuf>,
    /// Interactive "ask the user" channel for the ask_user tool. The TUI
    /// installs a dialog-backed handler; headless modes leave it None and
    /// the tool returns an in-band "no interactive user" message.
    pub ask_user: Option<Arc<dyn crate::ask_user::AskUserHandler>>,
}

impl std::fmt::Debug for ToolServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolServices")
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl ToolServices {
    pub fn new(cwd: PathBuf) -> Self {
        let lsp = crate::lsp::LspManager::new(cwd.clone());
        ToolServices {
            cwd,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
            shell: None,
            bash_executor: None,
            background: crate::background::BackgroundTaskManager::new(),
            lsp,
            checkpoints: crate::checkpoint::CheckpointManager::new(),
            sandbox: None,
            web_render: crate::browser::WebRenderMode::default(),
            web_search: crate::web::WebSearchConfig::default(),
            background_tasks_enabled: true,
            untrusted_seen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            memory_dir_override: None,
            ask_user: None,
            env: Vec::new(),
        }
    }

    pub fn with_env(mut self, env: Vec<(String, std::ffi::OsString)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_shell(mut self, shell: ShellConfig) -> Self {
        self.shell = Some(Arc::new(shell));
        self
    }

    pub fn with_bash_executor(mut self, executor: Arc<dyn BashExecutor>) -> Self {
        self.bash_executor = Some(executor);
        self
    }

    pub fn with_background(mut self, background: crate::background::BackgroundTaskManager) -> Self {
        self.background = background;
        self
    }

    pub fn with_lsp(mut self, lsp: crate::lsp::LspManager) -> Self {
        self.lsp = lsp;
        self
    }

    pub fn with_checkpoints(mut self, checkpoints: crate::checkpoint::CheckpointManager) -> Self {
        self.checkpoints = checkpoints;
        self
    }

    pub fn with_sandbox(mut self, sandbox: crate::sandbox::SandboxSpec) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    pub fn with_web_render(mut self, mode: crate::browser::WebRenderMode) -> Self {
        self.web_render = mode;
        self
    }

    pub fn with_web_search(mut self, config: crate::web::WebSearchConfig) -> Self {
        self.web_search = config;
        self
    }

    pub fn with_background_tasks_enabled(mut self, enabled: bool) -> Self {
        self.background_tasks_enabled = enabled;
        self
    }

    pub fn with_memory_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.memory_dir_override = dir;
        self
    }

    pub fn with_ask_user(mut self, handler: Arc<dyn crate::ask_user::AskUserHandler>) -> Self {
        self.ask_user = Some(handler);
        self
    }
}

/// Mark + wrap untrusted external content (web pages, MCP tool results) so
/// the model treats it as data, not instructions, and permission hooks know
/// to re-prompt for mutating tools in this run (prompt-injection defense).
pub fn wrap_untrusted(services: &ToolServices, source: &str, text: String) -> String {
    services
        .untrusted_seen
        .store(true, std::sync::atomic::Ordering::Relaxed);
    format!("<untrusted_content source=\"{source}\">\n{text}\n</untrusted_content>")
}
