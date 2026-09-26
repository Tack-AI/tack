//! Settings: global `~/.tack/agent/settings.json` + project `.pi/settings.json`,
//! deep-merged (project wins). Lean port of settings-manager.ts — unknown keys
//! are preserved and ignored by typed accessors.

use std::path::{Path, PathBuf};

use tack_session::CompactionSettings;

#[derive(Clone, Copy, Debug)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    /// Cap for each computed retry delay (`maxAgentDelayMs`, TS #8826);
    /// `None` falls back to 60s in the retry loop.
    pub max_agent_delay_ms: Option<u64>,
}

impl Default for RetrySettings {
    fn default() -> Self {
        RetrySettings {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
            max_agent_delay_ms: None,
        }
    }
}

impl RetrySettings {
    pub fn policy(&self) -> tack_ai::retry::RetryPolicy {
        tack_ai::retry::RetryPolicy {
            enabled: self.enabled,
            max_retries: self.max_retries,
            base_delay_ms: self.base_delay_ms,
            max_agent_delay_ms: self.max_agent_delay_ms,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub shell_path: Option<PathBuf>,
    /// User-scope memory root override (`memoryDirectory`).
    /// `TACK_MEMORY_DIR` env wins; project scope lives under `<root>/projects/`.
    pub memory_directory: Option<PathBuf>,
    pub compaction: CompactionSettings,
    pub retry: RetrySettings,
    pub append_system_prompt: Option<String>,
    pub theme: Option<String>,
    pub tui_mode: Option<String>,
    /// Session storage backend: "jsonl" (default) or "sqlite" (experimental).
    pub session_backend: Option<String>,
    /// Codex transport: "auto" (WS first, SSE fallback) | "sse" | "websocket".
    pub transport: Option<String>,
    /// Mermaid rendering: "image" (default) or "off" (plain code blocks).
    pub mermaid: Option<String>,
    /// GitHub repo ("owner/name") for `tack update` self-updates. Only
    /// the global and managed settings layers may set this — the project
    /// layer is ignored (a malicious clone must not redirect updates).
    pub update_repo: Option<String>,
    pub scoped_models: Vec<String>,
    pub enable_skill_commands: bool,
    // --- Behavior (TS settings-manager parity) ---
    /// "all" (default) or "one-at-a-time" queue delivery.
    pub steering_mode: Option<String>,
    pub follow_up_mode: Option<String>,
    /// Empty-editor double-Esc: "tree" (default), "fork", or "none".
    pub double_escape_action: Option<String>,
    /// Built-in tool allowlist (empty = all).
    pub default_tools: Vec<String>,
    /// /tree filter: "default" | "no-tools" | "user-only" | "labeled-only" | "all".
    pub tree_filter_mode: Option<String>,
    pub hide_thinking_block: bool,
    /// Startup changelog: one-line notice instead of full entries.
    pub collapse_changelog: bool,
    /// Skip banner + changelog at startup.
    pub quiet_startup: bool,
    pub show_cache_miss_notices: bool,
    /// Prompt-cache retention: "short" (default, 5-minute writes), "long"
    /// (1h writes; 24h on OpenAI), or "off" (no cache markers). Unset defers
    /// to the `TACK_CACHE_RETENTION` env var, then "short".
    pub cache_retention: Option<String>,
    // --- Extra resource dirs ---
    pub skill_paths: Vec<String>,
    pub theme_paths: Vec<String>,
    pub prompt_template_paths: Vec<String>,
    // --- Images ---
    /// Inline image rendering (default true).
    pub show_images: bool,
    /// Never send images to the LLM (default false).
    pub block_images: bool,
    pub image_width_cells: Option<u16>,
    // --- Layout ---
    pub editor_padding_x: u16,
    /// Literal indent before code blocks (TS markdown.codeBlockIndent).
    pub code_block_indent: Option<String>,
    pub autocomplete_max_visible: Option<u16>,
    // --- Terminal ---
    pub clear_on_shrink: bool,
    /// "transcript" prints the transcript when leaving fullscreen.
    pub fullscreen_exit_output: Option<String>,
    /// Fullscreen drag-select auto-copies to the clipboard (default true;
    /// TS fullscreenCopyOnSelect). When false, selections stay highlighted
    /// and Ctrl+X copies the active selection.
    pub fullscreen_copy_on_select: bool,
    /// Terminal capability overrides (TS terminal.hyperlinks / terminal.images
    /// / terminal.trueColor): applied after detection and after the
    /// TACK_HYPERLINKS / TACK_IMAGE_PROTOCOL / TACK_TRUE_COLOR env vars (settings
    /// take precedence). Unset / "auto" keeps detection.
    pub terminal_capability_overrides: tack_tui::terminal::CapabilityOverrides,
    pub external_editor_command: Option<String>,
    pub http_idle_timeout_ms: Option<u64>,
    // --- LSP ---
    /// Append diagnostics to edit/write results (default true).
    pub lsp_edit_feedback: bool,
    /// Extension → language server overrides ("rs" → {command, args}).
    pub lsp_servers: Vec<(String, tack_tools::lsp::ServerSpec)>,
    /// Pluggable feature switches (see FeatureFlags).
    pub features: FeatureFlags,
    // --- Sandbox ---
    /// OS-level sandbox for bash ("sandbox": "on"/"off", default on).
    pub sandbox: bool,
    /// Allow network inside the sandbox (default true).
    pub sandbox_network: bool,
    /// Per-session total-token budget; the TUI warns when it's exceeded.
    pub token_budget: Option<u64>,
    /// Budget enforcement: "warn" (default) | "pause" | "downgrade".
    pub token_budget_action: Option<String>,
    /// Model for tokenBudgetAction=downgrade ("provider/id"); defaults to
    /// the last fallbackModels entry.
    pub budget_downgrade_model: Option<String>,
    /// Headless rendering for web_fetch: "auto" (default) | "always" | "off".
    pub web_render: Option<String>,
    /// Ordered fallback chain ("provider/id") for retryable model failures.
    pub fallback_models: Vec<String>,
    // --- Managed (organization) enforcement ---
    /// A managed settings file is in effect.
    pub managed_active: bool,
    /// Managed: the bypass permission mode is unavailable.
    pub disable_bypass: bool,
    /// Managed: sessions may only use this provider.
    pub locked_provider: Option<String>,
    /// Managed: sessions may only use this model id.
    pub locked_model: Option<String>,
    /// Union of permissions.allow across all layers.
    pub permission_allow: Vec<String>,
    /// Union of permissions.deny across all layers.
    pub permission_deny: Vec<String>,
    /// Microcompaction of old oversized tool results (default on).
    pub microcompact_enabled: bool,
    /// Byte budget (the settings key says "chars" for compatibility; the
    /// thresholds are approximate token budgets and byte length is what
    /// hooks.rs compares against — the defaults are 4× the historical
    /// char budgets to compensate for multi-byte text).
    pub microcompact_max_chars: usize,
    pub microcompact_keep_recent: usize,
    /// Only rewrite history when microcompaction reclaims at least this
    /// many bytes (protects the provider prompt cache from cheap rewrites).
    pub microcompact_min_savings_chars: usize,
    /// Mask a `read` result when the same file was re-read later with
    /// identical content (default on).
    pub mask_duplicate_reads: bool,
    /// Hard cap for ANY tool result, fresh ones included (0 = off). Byte
    /// budget, see microcompact_max_chars.
    pub tool_result_max_chars: usize,
    /// Per-file cap for AGENTS.md/CLAUDE.md context files (0 = off).
    pub rules_max_chars: usize,
    /// Additional working directories (multi-root; settings additionalDirs).
    pub additional_dirs: Vec<String>,
    /// TUI language: "en" (default) | "zh".
    pub language: Option<String>,
    /// Defer MCP tools (tool_search activates them on demand) when the total
    /// tool count exceeds this. 0 = off (default).
    pub mcp_defer_threshold: usize,
    /// MCP sampling: servers may request LLM completions (run against the
    /// session's current model in an isolated, untrusted context). Default
    /// false — the capability is not advertised unless opted in.
    pub mcp_sampling: bool,
    /// MCP elicitation: servers may ask the user for structured input.
    /// Default true; headless modes (print/rpc/acp/serve) always decline.
    pub mcp_elicitation: bool,
    /// Encrypt session entries at rest (AES-256-GCM; key in the OS keyring).
    pub session_encryption: bool,
    /// Fetch a fresh model catalog from the published tack-ai npm package at
    /// TUI startup (default false). A previously fetched catalog is always
    /// loaded from cache; `/models refresh` works regardless of this switch.
    pub model_catalog_refresh: bool,
    /// Background task finished while idle: auto-start a run so the agent
    /// reads the output and continues (default true; "backgroundAutoWake").
    pub background_auto_wake: bool,
    /// Startup update check against GitHub releases (default true;
    /// "updateCheck"). Runs in the background, never blocks startup, and
    /// caches the result for 24h in `<agentDir>/update-check.json`. Always
    /// skipped in offline mode (--offline / TACK_OFFLINE).
    pub update_check: bool,
    /// Desktop notifications via OSC 9 / OSC 777 (default true;
    /// "notifications"): permission prompts, run completion, background
    /// task completion (throttled per source).
    pub notifications: bool,
    /// Sub-agent coordination: max concurrently running child loops
    /// (subagents.maxConcurrent; 0 = unlimited) and the shared token budget
    /// across all children of a session (subagents.budgetTokens; 0 = none).
    pub subagents_max_concurrent: usize,
    pub subagents_budget_tokens: u64,
    /// Supply-chain enforcement for installed extensions (default true):
    /// a user-dir plugin whose git HEAD drifted from its lockfile commit is
    /// skipped at startup. false downgrades the mismatch to a warning.
    pub extension_lock_required: bool,
    raw: serde_json::Value,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_provider: None,
            default_model: None,
            shell_path: None,
            memory_directory: None,
            compaction: tack_session::DEFAULT_COMPACTION_SETTINGS,
            retry: RetrySettings::default(),
            append_system_prompt: None,
            theme: None,
            tui_mode: None,
            session_backend: None,
            transport: None,
            mermaid: None,
            update_repo: None,
            scoped_models: Vec::new(),
            enable_skill_commands: true,
            steering_mode: None,
            follow_up_mode: None,
            double_escape_action: None,
            default_tools: Vec::new(),
            tree_filter_mode: None,
            hide_thinking_block: false,
            collapse_changelog: false,
            quiet_startup: false,
            show_cache_miss_notices: false,
            cache_retention: None,
            skill_paths: Vec::new(),
            theme_paths: Vec::new(),
            prompt_template_paths: Vec::new(),
            show_images: true,
            block_images: false,
            image_width_cells: None,
            editor_padding_x: 0,
            code_block_indent: None,
            autocomplete_max_visible: None,
            clear_on_shrink: false,
            fullscreen_exit_output: None,
            fullscreen_copy_on_select: true,
            terminal_capability_overrides: tack_tui::terminal::CapabilityOverrides::default(),
            external_editor_command: None,
            http_idle_timeout_ms: None,
            lsp_edit_feedback: true,
            lsp_servers: Vec::new(),
            features: FeatureFlags::default(),
            sandbox: true,
            sandbox_network: true,
            token_budget: None,
            token_budget_action: None,
            budget_downgrade_model: None,
            web_render: None,
            fallback_models: Vec::new(),
            managed_active: false,
            disable_bypass: false,
            locked_provider: None,
            locked_model: None,
            permission_allow: Vec::new(),
            permission_deny: Vec::new(),
            microcompact_enabled: true,
            microcompact_max_chars: 80_000,
            microcompact_keep_recent: 3,
            microcompact_min_savings_chars: 32_000,
            mask_duplicate_reads: true,
            tool_result_max_chars: 240_000,
            rules_max_chars: 40_000,
            additional_dirs: Vec::new(),
            language: None,
            mcp_defer_threshold: 0,
            mcp_sampling: false,
            mcp_elicitation: true,
            session_encryption: false,
            model_catalog_refresh: false,
            background_auto_wake: true,
            update_check: true,
            notifications: true,
            subagents_max_concurrent: 0,
            subagents_budget_tokens: 0,
            extension_lock_required: true,
            raw: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// Parse the `terminal` capability-override keys (TS
/// settings-manager.getTerminalCapabilityOverrides): `hyperlinks` and
/// `trueColor` accept `true|false|"auto"`, `images` accepts
/// `"kitty"|"iterm2"|false|"auto"`. Only forced (non-auto) values land in
/// the returned overrides.
fn parse_terminal_capability_overrides(
    raw: Option<&serde_json::Value>,
) -> tack_tui::terminal::CapabilityOverrides {
    use tack_tui::terminal::{CapabilityOverrides, ImageCapabilityOverride};
    let Some(terminal) = raw else {
        return CapabilityOverrides::default();
    };
    // TS: `typeof value === "boolean"` forces; the string "auto" (and any
    // other non-boolean) keeps detection.
    let bool_key = |key: &str| terminal.get(key).and_then(|v| v.as_bool());
    let images = match terminal.get("images") {
        Some(serde_json::Value::String(s)) => CapabilityOverrides::parse_images(s),
        Some(serde_json::Value::Bool(false)) => Some(ImageCapabilityOverride::Disabled),
        // `true` is not a protocol; treat it like "auto" (TS ignores it).
        _ => None,
    };
    CapabilityOverrides {
        hyperlinks: bool_key("hyperlinks"),
        true_color: bool_key("trueColor"),
        images,
    }
}

/// Parse the `lspServers` setting: `{ "ext": "command" | {command, args} }`.
fn parse_lsp_servers(
    raw: Option<&serde_json::Value>,
) -> Vec<(String, tack_tools::lsp::ServerSpec)> {
    let Some(map) = raw.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (ext, spec) in map {
        let ext = ext.trim_start_matches('.').to_lowercase();
        match spec {
            serde_json::Value::String(command) => out.push((
                ext,
                tack_tools::lsp::ServerSpec {
                    command: command.clone(),
                    args: Vec::new(),
                },
            )),
            serde_json::Value::Object(_) => {
                let command = spec
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                if command.is_empty() {
                    continue;
                }
                let args = spec
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                out.push((
                    ext,
                    tack_tools::lsp::ServerSpec {
                        command: command.to_string(),
                        args,
                    },
                ));
            }
            _ => {}
        }
    }
    out
}

fn deep_merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                deep_merge(
                    base.entry(key.clone()).or_insert(serde_json::Value::Null),
                    value,
                );
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

/// Pluggable feature switches (settings `features.*`). A disabled feature is
/// INVISIBLE to the agent: its tools are not registered, nothing about it is
/// injected into the system prompt, and its subsystem never starts.
///
/// Legacy keys map onto the same flags: `lspDisabled` ≡ `features.lsp: false`,
/// `checkpointsDisabled` ≡ `features.checkpoints: false` (the new key wins
/// when both are present).
#[derive(Clone, Debug)]
pub struct FeatureFlags {
    /// lsp tool + edit/write diagnostics feedback + language-server processes.
    pub lsp: bool,
    /// Per-turn file snapshots + git baseline + /checkpoints.
    pub checkpoints: bool,
    /// bash run_in_background + bash_output/bash_wait/kill_shell tools.
    pub background_tasks: bool,
    /// memory tool + system-prompt index injection + /memory.
    pub memory: bool,
    /// settings hooks.{PreToolUse,PostToolUse} (configured hooks don't run).
    pub shell_hooks: bool,
    /// Scheduled prompts (/cron + tick firing).
    pub cron: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        FeatureFlags {
            lsp: true,
            checkpoints: true,
            background_tasks: true,
            memory: true,
            shell_hooks: true,
            cron: true,
        }
    }
}

impl FeatureFlags {
    pub fn from_raw(raw: &serde_json::Value) -> Self {
        let features = raw.get("features");
        let flag = |key: &str, legacy_disabled_key: Option<&str>| -> bool {
            if let Some(v) = features.and_then(|f| f.get(key)).and_then(|v| v.as_bool()) {
                return v;
            }
            if let Some(legacy) = legacy_disabled_key
                && raw.get(legacy).and_then(|v| v.as_bool()) == Some(true)
            {
                return false;
            }
            true
        };
        FeatureFlags {
            lsp: flag("lsp", Some("lspDisabled")),
            checkpoints: flag("checkpoints", Some("checkpointsDisabled")),
            background_tasks: flag("backgroundTasks", None),
            memory: flag("memory", None),
            shell_hooks: flag("shellHooks", None),
            cron: flag("cron", None),
        }
    }

    /// Merge a project-level flagset: the project may only DISABLE features,
    /// never enable them (a repository must not expand the agent's surface).
    pub fn merge_project(&mut self, project: &FeatureFlags) {
        self.lsp &= project.lsp;
        self.checkpoints &= project.checkpoints;
        self.background_tasks &= project.background_tasks;
        self.memory &= project.memory;
        self.shell_hooks &= project.shell_hooks;
        self.cron &= project.cron;
    }

    /// Managed-layer override: managed settings are the highest authority and
    /// may force features ON or OFF (e.g. an org mandating sandboxed, LSP-on
    /// configurations). Only keys explicitly present apply.
    pub fn apply_managed(&mut self, raw: &serde_json::Value) {
        let Some(features) = raw.get("features").and_then(|f| f.as_object()) else {
            return;
        };
        for (key, slot) in [
            ("lsp", &mut self.lsp),
            ("checkpoints", &mut self.checkpoints),
            ("backgroundTasks", &mut self.background_tasks),
            ("memory", &mut self.memory),
            ("shellHooks", &mut self.shell_hooks),
            ("cron", &mut self.cron),
        ] {
            if let Some(v) = features.get(key).and_then(|v| v.as_bool()) {
                *slot = v;
            }
        }
    }
}

fn load_json_file(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&content) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("ignoring malformed settings file {}: {e}", path.display());
            None
        }
    }
}

/// The organization-managed settings file (highest precedence):
///   Windows: %ProgramData%\tack\managed-settings.json
///   macOS:   /Library/Application Support/tack/managed-settings.json
///   Linux:   /etc/tack/managed-settings.json
/// `TACK_MANAGED_SETTINGS` overrides the path (and tests).
pub fn managed_settings_path() -> PathBuf {
    if let Some(custom) = std::env::var_os("TACK_MANAGED_SETTINGS") {
        return PathBuf::from(custom);
    }
    #[cfg(windows)]
    {
        let base = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
        PathBuf::from(base)
            .join("tack")
            .join("managed-settings.json")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/tack/managed-settings.json")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        PathBuf::from("/etc/tack/managed-settings.json")
    }
}

/// Parse sandbox enablement: `features.sandbox` (new) or legacy `sandbox`
/// key ("on"/true). Default **on** — bash commands are OS-sandboxed unless
/// explicitly disabled (writes confined to the workspace; see sandbox.rs).
fn parse_sandbox_key(raw: &serde_json::Value) -> bool {
    if let Some(v) = raw
        .get("features")
        .and_then(|f| f.get("sandbox"))
        .and_then(|v| v.as_bool())
    {
        return v;
    }
    raw.get("sandbox")
        .map(|v| match v {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => s == "on" || s == "true",
            _ => false,
        })
        .unwrap_or(true)
}

impl Settings {
    pub fn load(cwd: &Path, agent_dir: &Path) -> Self {
        let global_raw = load_json_file(&agent_dir.join("settings.json"));
        let project_raw = if crate::project_trust::is_trusted(cwd, agent_dir) {
            load_json_file(&cwd.join(".pi").join("settings.json"))
        } else {
            None
        };
        let managed_raw = load_json_file(&managed_settings_path());

        let mut raw = serde_json::json!({});
        for layer in [&global_raw, &project_raw, &managed_raw]
            .into_iter()
            .flatten()
        {
            deep_merge(&mut raw, layer);
        }
        let mut settings = Self::from_raw(raw);

        // `updateRepo` steers BINARY downloads for `tack update`: a
        // project layer must not redirect self-updates to an attacker
        // repo. Resolve it from the global and managed layers only
        // (managed has the highest authority, matching the merge order).
        settings.update_repo = [&managed_raw, &global_raw]
            .into_iter()
            .flatten()
            .find_map(|layer| layer.get("updateRepo").and_then(|v| v.as_str()))
            .map(str::to_string);

        // Feature flags: project may only DISABLE (AND-merge); managed may
        // force either direction (highest authority).
        let mut features = FeatureFlags::from_raw(&global_raw.clone().unwrap_or_default());
        if let Some(project) = &project_raw {
            features.merge_project(&FeatureFlags::from_raw(project));
        }
        if let Some(managed) = &managed_raw {
            features.apply_managed(managed);
        }
        settings.features = features;

        // Sandbox: managed wins outright; else global may enable, project may
        // only explicitly disable.
        if let Some(managed) = &managed_raw {
            if managed
                .get("features")
                .and_then(|f| f.get("sandbox"))
                .is_some()
                || managed.get("sandbox").is_some()
            {
                settings.sandbox = parse_sandbox_key(managed);
            }
        } else if project_raw.as_ref().is_some_and(|p| {
            p.get("features")
                .and_then(|f| f.get("sandbox"))
                .and_then(|v| v.as_bool())
                == Some(false)
                || p.get("sandbox")
                    .and_then(|v| v.as_str().map(|s| s == "off").or(v.as_bool().map(|b| !b)))
                    == Some(true)
        }) {
            settings.sandbox = false;
        } else {
            settings.sandbox = parse_sandbox_key(&global_raw.clone().unwrap_or_default());
        }

        // Managed enforcement fields.
        settings.managed_active = managed_raw.is_some();
        if let Some(managed) = &managed_raw {
            settings.disable_bypass = managed
                .get("disableBypass")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            settings.locked_provider = managed
                .get("lockedProvider")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            settings.locked_model = managed
                .get("lockedModel")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // Managed may force the extension lock requirement in either
            // direction (deep-merge already applies it last; this makes the
            // enforcement explicit, mirroring disable_bypass).
            if let Some(v) = managed
                .get("extensionLockRequired")
                .and_then(|v| v.as_bool())
            {
                settings.extension_lock_required = v;
            }
        }

        // Permission rules UNION across all layers (a layer can only ADD
        // rules; nothing drops the org's deny list).
        let union = |key: &str| -> Vec<String> {
            let mut out: Vec<String> = Vec::new();
            for layer in [&global_raw, &project_raw, &managed_raw]
                .into_iter()
                .flatten()
            {
                if let Some(list) = layer
                    .get("permissions")
                    .and_then(|p| p.get(key))
                    .and_then(|v| v.as_array())
                {
                    for rule in list.iter().filter_map(|v| v.as_str()) {
                        if !out.iter().any(|r| r == rule) {
                            out.push(rule.to_string());
                        }
                    }
                }
            }
            out
        };
        settings.permission_allow = union("allow");
        settings.permission_deny = union("deny");
        settings
    }

    pub fn from_raw(raw: serde_json::Value) -> Self {
        let get_str = |key: &str| raw.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let get_bool = |raw: &serde_json::Value, key: &str, default: bool| {
            raw.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
        };
        let get_str_list = |raw: &serde_json::Value, key: &str| -> Vec<String> {
            raw.get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };

        let compaction =
            raw.get("compaction")
                .map_or(tack_session::DEFAULT_COMPACTION_SETTINGS, |c| {
                    let d = tack_session::DEFAULT_COMPACTION_SETTINGS;
                    CompactionSettings {
                        enabled: c
                            .get("enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(d.enabled),
                        reserve_tokens: c
                            .get("reserveTokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(d.reserve_tokens),
                        keep_recent_tokens: c
                            .get("keepRecentTokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(d.keep_recent_tokens),
                        goal_recitation: c
                            .get("goalRecitation")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(d.goal_recitation),
                        min_kept_turns: c
                            .get("minKeptTurns")
                            .and_then(|v| v.as_u64())
                            .and_then(|v| usize::try_from(v).ok())
                            .unwrap_or(d.min_kept_turns),
                    }
                });

        let retry = raw.get("retry").map_or_else(RetrySettings::default, |r| {
            let d = RetrySettings::default();
            RetrySettings {
                enabled: r
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(d.enabled),
                max_retries: r
                    .get("maxRetries")
                    .and_then(|v| v.as_u64())
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(d.max_retries),
                base_delay_ms: r
                    .get("baseDelayMs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(d.base_delay_ms),
                max_agent_delay_ms: r
                    .get("maxAgentDelayMs")
                    .and_then(|v| v.as_u64())
                    .or(d.max_agent_delay_ms),
            }
        });

        Settings {
            default_provider: get_str("defaultProvider"),
            default_model: get_str("defaultModel"),
            shell_path: get_str("shellPath").map(PathBuf::from),
            memory_directory: get_str("memoryDirectory").map(PathBuf::from),
            compaction,
            retry,
            append_system_prompt: get_str("appendSystemPrompt"),
            theme: get_str("theme"),
            tui_mode: get_str("tuiMode"),
            session_backend: get_str("sessionBackend"),
            transport: get_str("transport"),
            mermaid: get_str("mermaid"),
            update_repo: get_str("updateRepo"),
            scoped_models: raw
                .get("scopedModels")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            enable_skill_commands: raw
                .get("enableSkillCommands")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            steering_mode: get_str("steeringMode"),
            follow_up_mode: get_str("followUpMode"),
            double_escape_action: get_str("doubleEscapeAction"),
            default_tools: get_str_list(&raw, "defaultTools"),
            tree_filter_mode: get_str("treeFilterMode"),
            hide_thinking_block: get_bool(&raw, "hideThinkingBlock", false),
            collapse_changelog: get_bool(&raw, "collapseChangelog", false),
            quiet_startup: get_bool(&raw, "quietStartup", false),
            show_cache_miss_notices: get_bool(&raw, "showCacheMissNotices", false),
            cache_retention: get_str("cacheRetention"),
            skill_paths: get_str_list(&raw, "skills"),
            theme_paths: get_str_list(&raw, "themes"),
            prompt_template_paths: get_str_list(&raw, "prompts"),
            show_images: raw
                .get("terminal")
                .and_then(|t| t.get("showImages"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            block_images: raw
                .get("images")
                .and_then(|t| t.get("blockImages"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            image_width_cells: raw
                .get("terminal")
                .and_then(|t| t.get("imageWidthCells"))
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok()),
            editor_padding_x: raw
                .get("editorPaddingX")
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .map(|v| v.min(3))
                .unwrap_or(0),
            code_block_indent: raw
                .get("markdown")
                .and_then(|m| m.get("codeBlockIndent"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            autocomplete_max_visible: raw
                .get("autocompleteMaxVisible")
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .map(|v| v.clamp(3, 20)),
            clear_on_shrink: raw
                .get("terminal")
                .and_then(|t| t.get("clearOnShrink"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            fullscreen_exit_output: get_str("fullscreenExitOutput"),
            fullscreen_copy_on_select: get_bool(&raw, "fullscreenCopyOnSelect", true),
            terminal_capability_overrides: parse_terminal_capability_overrides(raw.get("terminal")),
            external_editor_command: get_str("externalEditorCommand"),
            http_idle_timeout_ms: raw.get("httpIdleTimeoutMs").and_then(|v| v.as_u64()),
            lsp_edit_feedback: get_bool(&raw, "lspEditFeedback", true),
            lsp_servers: parse_lsp_servers(raw.get("lspServers")),
            features: FeatureFlags::from_raw(&raw),
            sandbox: parse_sandbox_key(&raw),
            sandbox_network: get_bool(&raw, "sandboxNetwork", true),
            token_budget: raw.get("tokenBudget").and_then(|v| v.as_u64()),
            token_budget_action: get_str("tokenBudgetAction"),
            budget_downgrade_model: get_str("budgetDowngradeModel"),
            web_render: get_str("webRender"),
            fallback_models: get_str_list(&raw, "fallbackModels"),
            managed_active: false,
            disable_bypass: get_bool(&raw, "disableBypass", false),
            locked_provider: get_str("lockedProvider"),
            locked_model: get_str("lockedModel"),
            permission_allow: raw
                .get("permissions")
                .and_then(|p| p.get("allow"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            permission_deny: raw
                .get("permissions")
                .and_then(|p| p.get("deny"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            microcompact_enabled: raw
                .get("microcompact")
                .and_then(|m| m.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            microcompact_max_chars: raw
                .get("microcompact")
                .and_then(|m| m.get("maxChars"))
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(80_000),
            microcompact_keep_recent: raw
                .get("microcompact")
                .and_then(|m| m.get("keepRecent"))
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(3),
            microcompact_min_savings_chars: raw
                .get("microcompact")
                .and_then(|m| m.get("minSavingsChars"))
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(32_000),
            mask_duplicate_reads: get_bool(&raw, "maskDuplicateReads", true),
            tool_result_max_chars: raw
                .get("toolResultMaxChars")
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(240_000),
            rules_max_chars: raw
                .get("rulesMaxChars")
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(40_000),
            additional_dirs: get_str_list(&raw, "additionalDirs"),
            language: get_str("language"),
            mcp_defer_threshold: raw
                .get("mcpDeferThreshold")
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(0),
            mcp_sampling: get_bool(&raw, "mcpSampling", false),
            mcp_elicitation: get_bool(&raw, "mcpElicitation", true),
            session_encryption: get_bool(&raw, "sessionEncryption", false),
            model_catalog_refresh: get_bool(&raw, "modelCatalogRefresh", false),
            background_auto_wake: get_bool(&raw, "backgroundAutoWake", true),
            update_check: get_bool(&raw, "updateCheck", true),
            notifications: get_bool(&raw, "notifications", true),
            subagents_max_concurrent: raw
                .get("subagents")
                .and_then(|s| s.get("maxConcurrent"))
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(0),
            subagents_budget_tokens: raw
                .get("subagents")
                .and_then(|s| s.get("budgetTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            extension_lock_required: get_bool(&raw, "extensionLockRequired", true),
            raw,
        }
    }

    pub fn raw(&self) -> &serde_json::Value {
        &self.raw
    }

    /// `cacheRetention` setting as the provider hint for `StreamOptions`.
    /// Unset/unknown values return None, deferring to the provider's own
    /// resolution (`TACK_CACHE_RETENTION` env, then short 5-minute writes).
    pub fn cache_retention_mode(&self) -> Option<tack_ai::CacheRetention> {
        match self.cache_retention.as_deref() {
            Some("off") => Some(tack_ai::CacheRetention::None),
            Some("short") => Some(tack_ai::CacheRetention::Short),
            Some("long") => Some(tack_ai::CacheRetention::Long),
            _ => None,
        }
    }

    /// Build an LSP manager honoring the lsp* settings. Workspace roots are
    /// the primary cwd plus existing additional dirs (multi-root sessions).
    pub fn lsp_manager(&self, cwd: &Path) -> tack_tools::lsp::LspManager {
        let mut roots = vec![cwd.to_path_buf()];
        for dir in &self.additional_dirs {
            let path = PathBuf::from(dir);
            if path.is_dir() && !roots.contains(&path) {
                roots.push(path);
            }
        }
        let manager = tack_tools::lsp::LspManager::with_roots(roots);
        manager.configure(
            self.lsp_servers.iter().cloned().collect(),
            !self.features.lsp,
        );
        manager.set_edit_feedback(self.lsp_edit_feedback);
        // Pre-warm the primary server: the session-level manager persists,
        // so this one spawn covers every lsp call of the session. (Sub-
        // agents build their own services directly, not via this fn —
        // they don't pay for servers they may never use.)
        manager.warmup();
        manager
    }

    /// OS sandbox policy for `cwd` when sandboxing is enabled.
    pub fn sandbox_spec(&self, cwd: &Path) -> Option<tack_tools::sandbox::SandboxSpec> {
        if !self.sandbox {
            return None;
        }
        let mut writable = vec![cwd.to_path_buf()];
        // Multi-root: additional working directories are writable too.
        for dir in &self.additional_dirs {
            let path = PathBuf::from(dir);
            if !writable.contains(&path) {
                writable.push(path);
            }
        }
        Some(tack_tools::sandbox::SandboxSpec {
            writable,
            network: self.sandbox_network,
            max_processes: self
                .raw
                .get("sandboxMaxProcesses")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok()),
            max_memory_mb: self.raw.get("sandboxMaxMemoryMb").and_then(|v| v.as_u64()),
        })
    }

    /// Headless-render mode for web_fetch (settings `webRender`).
    pub fn web_render_mode(&self) -> tack_tools::browser::WebRenderMode {
        tack_tools::browser::WebRenderMode::from_setting(self.web_render.as_deref())
    }

    /// web_search backend config (settings `webSearch: {provider, apiKey}`).
    pub fn web_search_config(&self) -> tack_tools::web::WebSearchConfig {
        let section = self.raw.get("webSearch");
        let backend = tack_tools::web::SearchBackend::from_setting(
            section
                .and_then(|s| s.get("provider"))
                .and_then(|v| v.as_str()),
        );
        let api_key = section
            .and_then(|s| s.get("apiKey"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        tack_tools::web::WebSearchConfig { backend, api_key }
    }

    /// History-optimization settings + spill dir, when enabled.
    pub fn history(&self, agent_dir: &Path) -> Option<(crate::hooks::HistorySettings, PathBuf)> {
        if !self.microcompact_enabled {
            return None;
        }
        Some((
            crate::hooks::HistorySettings {
                micro: crate::hooks::MicrocompactSettings {
                    max_chars: self.microcompact_max_chars,
                    keep_recent: self.microcompact_keep_recent,
                    min_savings_chars: self.microcompact_min_savings_chars,
                },
                mask_duplicate_reads: self.mask_duplicate_reads,
                tool_result_max_chars: self.tool_result_max_chars,
            },
            agent_dir.join("microcompact"),
        ))
    }

    /// Extra resource paths from the global settings file (`skills` /
    /// `prompts` / `themes` arrays, TS getSkillPaths/getPromptTemplatePaths/
    /// getThemePaths). Loaders read these directly so call sites don't all
    /// need the Settings struct.
    pub fn extra_paths(agent_dir: &Path, key: &str) -> Vec<String> {
        std::fs::read_to_string(agent_dir.join("settings.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .map(|v| {
                v.get(key)
                    .and_then(|k| k.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Persist one key into the global settings file (deep-merges onto the
    /// existing content).
    pub fn save_global(
        agent_dir: &Path,
        key: &str,
        value: serde_json::Value,
    ) -> std::io::Result<()> {
        let path = agent_dir.join("settings.json");
        let mut content: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        // A valid-but-non-object file (array, string, …) would panic on
        // as_object_mut; treat it like a missing file instead.
        if !content.is_object() {
            content = serde_json::Value::Object(serde_json::Map::new());
        }
        content
            .as_object_mut()
            .expect("settings object")
            .insert(key.to_string(), value);
        crate::atomic_write::atomic_write(&path, &serde_json::to_string_pretty(&content)?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn mcp_sampling_elicitation_defaults_and_overrides() {
        let settings = Settings::from_raw(serde_json::json!({}));
        // Safe defaults: sampling off (servers can't request LLM calls),
        // elicitation on (headless modes still auto-decline).
        assert!(!settings.mcp_sampling);
        assert!(settings.mcp_elicitation);

        let settings = Settings::from_raw(serde_json::json!({
            "mcpSampling": true,
            "mcpElicitation": false,
        }));
        assert!(settings.mcp_sampling);
        assert!(!settings.mcp_elicitation);
    }

    #[test]
    fn cache_retention_setting_maps_to_provider_hint() {
        // Unset: defer to provider resolution (TACK_CACHE_RETENTION env, then short).
        let settings = Settings::from_raw(serde_json::json!({}));
        assert_eq!(settings.cache_retention, None);
        assert_eq!(settings.cache_retention_mode(), None);

        let settings = Settings::from_raw(serde_json::json!({
            "cacheRetention": "long",
        }));
        assert_eq!(
            settings.cache_retention_mode(),
            Some(tack_ai::CacheRetention::Long)
        );
        let settings = Settings::from_raw(serde_json::json!({
            "cacheRetention": "short",
        }));
        assert_eq!(
            settings.cache_retention_mode(),
            Some(tack_ai::CacheRetention::Short)
        );
        let settings = Settings::from_raw(serde_json::json!({
            "cacheRetention": "off",
        }));
        assert_eq!(
            settings.cache_retention_mode(),
            Some(tack_ai::CacheRetention::None)
        );
        // Unknown values fall back to provider resolution rather than
        // silently disabling caching.
        let settings = Settings::from_raw(serde_json::json!({
            "cacheRetention": "forever",
        }));
        assert_eq!(settings.cache_retention_mode(), None);
    }

    #[test]
    fn features_default_all_on() {
        let flags = FeatureFlags::from_raw(&serde_json::json!({}));
        assert!(
            flags.lsp
                && flags.checkpoints
                && flags.background_tasks
                && flags.memory
                && flags.shell_hooks
                && flags.cron
        );
    }

    #[test]
    fn features_new_key_and_legacy_mapping() {
        let flags = FeatureFlags::from_raw(
            &serde_json::json!({ "features": { "memory": false, "cron": false } }),
        );
        assert!(!flags.memory && !flags.cron && flags.lsp);
        // Legacy disabled keys map onto the flags.
        let flags = FeatureFlags::from_raw(
            &serde_json::json!({ "lspDisabled": true, "checkpointsDisabled": true }),
        );
        assert!(!flags.lsp && !flags.checkpoints);
        // New key wins over legacy.
        let flags = FeatureFlags::from_raw(
            &serde_json::json!({ "lspDisabled": true, "features": { "lsp": true } }),
        );
        assert!(flags.lsp);
    }

    #[test]
    fn update_check_and_notifications_default_on() {
        let settings = Settings::from_raw(serde_json::json!({}));
        assert!(settings.update_check, "updateCheck defaults to true");
        assert!(settings.notifications, "notifications defaults to true");
        let settings =
            Settings::from_raw(serde_json::json!({ "updateCheck": false, "notifications": false }));
        assert!(!settings.update_check);
        assert!(!settings.notifications);
        // Non-boolean junk falls back to the default.
        let settings = Settings::from_raw(serde_json::json!({ "updateCheck": "no" }));
        assert!(settings.update_check);
    }

    #[test]
    fn update_repo_ignores_the_project_layer() {
        let agent = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".pi")).unwrap();
        std::fs::write(
            project.path().join(".pi").join("settings.json"),
            r#"{"updateRepo": "attacker/repo"}"#,
        )
        .unwrap();
        // Project settings only load for trusted projects — trust it so
        // the layer is actually read (and still ignored for updateRepo).
        crate::project_trust::set_decision(agent.path(), project.path(), true, false);

        // A project-only updateRepo must not resolve at all.
        let settings = Settings::load(project.path(), agent.path());
        assert_eq!(settings.update_repo, None);

        // The global layer still wins (and is the only one honored).
        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"updateRepo": "org/pi"}"#,
        )
        .unwrap();
        let settings = Settings::load(project.path(), agent.path());
        assert_eq!(settings.update_repo.as_deref(), Some("org/pi"));
    }

    #[test]
    fn project_can_only_disable() {
        let mut global =
            FeatureFlags::from_raw(&serde_json::json!({ "features": { "shellHooks": false } }));
        // Project tries to enable shellHooks (no-op) and disable memory (works).
        global.merge_project(&FeatureFlags::from_raw(
            &serde_json::json!({ "features": { "shellHooks": true, "memory": false } }),
        ));
        assert!(
            !global.shell_hooks,
            "project must not enable a globally-disabled feature"
        );
        assert!(!global.memory, "project may disable");
        assert!(global.lsp);
    }

    #[test]
    fn feature_filter_removes_disabled_tools() {
        let services = tack_tools::default_services(std::env::current_dir().unwrap());
        let tools = tack_tools::create_coding_tools(&services);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(
            names.contains(&"lsp") && names.contains(&"memory") && names.contains(&"bash_output")
        );

        let flags = FeatureFlags::from_raw(
            &serde_json::json!({ "features": { "lsp": false, "memory": false, "backgroundTasks": false } }),
        );
        let tools = crate::cli_flags::filter_feature_tools(tools, &flags);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"lsp"), "{names:?}");
        assert!(!names.contains(&"memory"), "{names:?}");
        assert!(
            !names.contains(&"bash_output") && !names.contains(&"kill_shell"),
            "{names:?}"
        );
        assert!(
            names.contains(&"bash") && names.contains(&"edit"),
            "{names:?}"
        );
    }

    #[test]
    fn bash_hides_background_param_when_disabled() {
        use tack_agent_core::AgentTool as _;
        let services = tack_tools::default_services(std::env::current_dir().unwrap());
        let enabled = tack_tools::BashTool::new(services.clone());
        assert!(enabled.parameters_schema()["properties"]["run_in_background"].is_object());
        assert!(enabled.description().contains("run_in_background"));

        let disabled = tack_tools::BashTool::new(services.with_background_tasks_enabled(false));
        assert!(disabled.parameters_schema()["properties"]["run_in_background"].is_null());
        assert!(!disabled.description().contains("run_in_background"));
    }

    #[test]
    fn save_global_tolerates_non_object_file() {
        // Regression: a valid-but-non-object settings.json (e.g. `[1]`)
        // panicked in as_object_mut.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), "[1, 2, 3]").unwrap();
        Settings::save_global(tmp.path(), "theme", serde_json::json!("dark")).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written.get("theme").and_then(|v| v.as_str()), Some("dark"));
    }

    #[test]
    fn terminal_capability_overrides_parse() {
        use tack_tui::terminal::ImageCapabilityOverride;

        // Unset: everything auto, copy-on-select defaults to true.
        let settings = Settings::from_raw(serde_json::json!({}));
        assert!(settings.terminal_capability_overrides.is_empty());
        assert!(settings.fullscreen_copy_on_select);

        let settings = Settings::from_raw(serde_json::json!({
            "terminal": {
                "hyperlinks": false,
                "trueColor": true,
                "images": "kitty"
            },
            "fullscreenCopyOnSelect": false
        }));
        let overrides = settings.terminal_capability_overrides;
        assert_eq!(overrides.hyperlinks, Some(false));
        assert_eq!(overrides.true_color, Some(true));
        assert_eq!(overrides.images, Some(ImageCapabilityOverride::Kitty));
        assert!(!settings.fullscreen_copy_on_select);

        // "auto" keeps detection; images:false disables images; iterm2 parses.
        let settings = Settings::from_raw(serde_json::json!({
            "terminal": {
                "hyperlinks": "auto",
                "trueColor": "auto",
                "images": false
            }
        }));
        let overrides = settings.terminal_capability_overrides;
        assert_eq!(overrides.hyperlinks, None);
        assert_eq!(overrides.true_color, None);
        assert_eq!(overrides.images, Some(ImageCapabilityOverride::Disabled));

        let settings = Settings::from_raw(serde_json::json!({
            "terminal": { "images": "iterm2" }
        }));
        assert_eq!(
            settings.terminal_capability_overrides.images,
            Some(ImageCapabilityOverride::ITerm2)
        );
        // images:"auto" is not a protocol: detection kept.
        let settings = Settings::from_raw(serde_json::json!({
            "terminal": { "images": "auto" }
        }));
        assert_eq!(settings.terminal_capability_overrides.images, None);
    }

    #[test]
    fn capability_override_precedence_settings_over_env() {
        use tack_tui::terminal::{Capabilities, CapabilityOverrides};
        // TS precedence: detection < env vars < settings. The parse step
        // only produces forced values, so applying env-then-settings
        // reproduces Capabilities::detect()'s ordering.
        let settings = Settings::from_raw(serde_json::json!({
            "terminal": { "hyperlinks": true }
        }));
        let env = CapabilityOverrides {
            hyperlinks: Some(false),
            ..Default::default()
        };
        let mut caps = Capabilities::default();
        env.apply_to(&mut caps);
        settings.terminal_capability_overrides.apply_to(&mut caps);
        assert!(caps.hyperlinks, "settings override beats the env override");
    }

    #[test]
    fn sandbox_parse_new_and_legacy_keys() {
        assert!(parse_sandbox_key(
            &serde_json::json!({ "features": { "sandbox": true } })
        ));
        assert!(parse_sandbox_key(&serde_json::json!({ "sandbox": "on" })));
        assert!(
            parse_sandbox_key(&serde_json::json!({})),
            "sandbox defaults to on"
        );
        // features key wins
        assert!(!parse_sandbox_key(
            &serde_json::json!({ "sandbox": "on", "features": { "sandbox": false } })
        ));
    }
}
