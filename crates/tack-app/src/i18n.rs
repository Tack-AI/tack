//! TUI internationalization layer. `settings.language: "en" | "zh"`
//! (default: `en`; `zh` also picked up from LANG=zh* when the setting is
//! absent). Strings are keyed constants; `t()` falls back to English and
//! then to the key itself, so missing translations degrade gracefully.
//!
//! Coverage: all user-visible TUI strings (slash-command help, dialogs,
//! notices, status line, chat chrome). Not translated on purpose: log/debug
//! output, command names, setting-key names, protocol values (permission
//! mode names, thinking levels), and agent-facing tool results.
//!
//! Adding a language = one new table; adding a string = one row in each
//! table. `tests::tables_are_paired` keeps the tables in sync.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    En,
    Zh,
}

impl Lang {
    /// From settings `language`; falls back to the LANG env var.
    pub fn resolve(setting: Option<&str>) -> Self {
        Self::resolve_with(setting, std::env::var("LANG").ok().as_deref())
    }

    /// Testable core: explicit setting value + explicit LANG value.
    fn resolve_with(setting: Option<&str>, lang_env: Option<&str>) -> Self {
        let value = setting
            .map(str::to_string)
            .or_else(|| lang_env.map(str::to_string))
            .unwrap_or_default();
        if value.trim().to_lowercase().starts_with("zh") {
            Lang::Zh
        } else {
            Lang::En
        }
    }
}

/// Process-wide current language, set once at TUI startup. Components
/// without a `lang` field (status indicator, chat renderer, dialogs built
/// deep in the widget tree) use `tr`/`trf` instead of threading `Lang`
/// through every constructor.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Serializes tests that mutate or observe the process-global language —
/// `#[test]` fns run on parallel threads within one test binary, so a test
/// that flips `CURRENT` would otherwise race with tests asserting on
/// localized text (flaked as `hooks::tests::pause_action_stops_once_over_budget`
/// on macOS CI).
#[cfg(test)]
pub(crate) static TEST_LANG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn set_current(lang: Lang) {
    CURRENT.store(
        match lang {
            Lang::En => 0,
            Lang::Zh => 1,
        },
        Ordering::Relaxed,
    );
}

pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Zh,
        _ => Lang::En,
    }
}

/// Translate `key` in the current language (no interpolation).
pub fn tr(key: &str) -> String {
    t(current(), key, &[])
}

/// Translate `key` in the current language, substituting `{name}` args.
pub fn trf(key: &str, args: &[(&str, &str)]) -> String {
    t(current(), key, args)
}

const EN: &[(&str, &str)] = &[
    // -- permission dialog --
    ("permission.title", "Allow {what}?"),
    ("permission.yes", "Yes"),
    ("permission.always", "Yes, always (this tool/input)"),
    ("permission.no", "No"),
    ("permission.hint", " ↑↓ select • enter confirm • esc deny"),
    (
        "permission.plan",
        "Approve plan? {preview}… (full plan: {path})",
    ),
    // -- notices (run/mode/budget) --
    ("notice.mode", "Permission mode: {mode}"),
    (
        "notice.budget",
        "Token budget exceeded: {used} / {budget} tokens used (${cost} so far). Adjust tokenBudget in settings.json.",
    ),
    ("notice.bg_done", "Background task {id} {status}: {command}"),
    (
        "notice.quit_grace",
        "Cancelling the run — exiting when it settles (quit again to force).",
    ),
    ("notice.prompt_blocked", "prompt blocked: {reason}"),
    (
        "notice.stop_hook_continue",
        "Stop hook requests continuation: {reason}",
    ),
    ("notice.hook_blocked", "blocked by UserPromptSubmit hook"),
    (
        "notice.budget_pause",
        "Token budget exceeded ({used} / {budget}); run paused. Continue with a new prompt or adjust tokenBudget.",
    ),
    (
        "notice.budget_downgrade",
        "Token budget exceeded ({used} / {budget}); switching to budget model {model}.",
    ),
    (
        "notice.mode_bypass_disabled",
        "bypass mode is disabled by managed settings",
    ),
    // -- status indicator --
    ("status.working", "Working…"),
    ("status.compacting", "Compacting context…"),
    ("status.retrying", "Retrying (attempt {attempt}/{max})"),
    ("status.retry_countdown", "{label} in {secs}s"),
    // -- chat transcript chrome --
    (
        "chat.compacted",
        "◆ Compacted context ({tokens} tokens summarized)",
    ),
    (
        "chat.compacted_summary",
        "◆ Compacted context ({tokens} tokens) — summary:",
    ),
    ("chat.skill", "[skill] {name}"),
    ("chat.queued_followup", "when the agent stops"),
    ("chat.queued_steering", "at the next turn boundary"),
    (
        "chat.queued",
        "⏳ queued — sent {when} · alt+↑ recall · ctrl+s send now",
    ),
    (
        "chat.thought",
        "◦ Thought for a while ({chars} chars) — ctrl+t to expand",
    ),
    ("chat.aborted", "◦ Aborted"),
    ("chat.truncated", "◦ Output truncated (max tokens)"),
    // -- tool cards --
    ("tool.more_lines", "…({remaining} more lines)"),
    (
        "tool.more_lines_expand",
        "…({remaining} more lines, Ctrl+O to expand)",
    ),
    // -- shared dialog chrome --
    ("dialog.filter", "filter: "),
    (
        "dialog.hint.select",
        " type to filter • ↑↓ select • enter confirm • esc cancel",
    ),
    ("dialog.hint.input", " enter confirm • esc cancel"),
    // -- ask_user tool dialogs --
    ("ask_user.step", " Question {step}/{total}:"),
    ("ask_user.other", "Other (type a custom answer)"),
    ("ask_user.placeholder", "type your answer"),
    ("ask_user.empty", "answer must not be empty"),
    (
        "dialog.hint.scoped",
        " type to filter • ↑↓ move • enter/space toggle • esc done",
    ),
    (
        "dialog.scoped_models.title",
        " Scoped models (cycled with ctrl+p / shift+ctrl+p)",
    ),
    // -- /model dialog --
    ("modeld.title", " Select model"),
    ("modeld.scope", " Scope: "),
    ("modeld.scope_all", "all"),
    ("modeld.scope_scoped", "scoped"),
    ("modeld.scope_hint", "  (tab to switch)"),
    (
        "modeld.only_configured",
        " Only showing models from configured providers. Use /login to add providers.",
    ),
    ("modeld.default_badge", " · default"),
    ("modeld.no_match", "  No matching models"),
    ("modeld.model_name", "  Model Name: {name}"),
    (
        "modeld.hint",
        "  Enter to select · Ctrl+S to set as default · Esc to cancel",
    ),
    // -- /resume session dialog --
    ("sd.title", " Resume session (by {sort})"),
    ("sd.sort_name", "name"),
    ("sd.sort_time", "time"),
    (
        "sd.hint_browse",
        " type to filter • ↑↓ select • enter resume • ^r rename • ^d delete • ^s sort • esc",
    ),
    (
        "sd.confirm_delete",
        " delete {label}? enter/^d confirm • esc cancel",
    ),
    ("sd.rename", " rename: {input}"),
    ("sd.meta", "{count} msgs • {date}"),
    ("sd.delete_failed", "delete failed: {error}"),
    ("sd.rename_failed", "rename failed: {error}"),
    ("sd.no_delete_active", "cannot delete the active session"),
    ("sd.rename_active", "rename the active session with /name"),
    // -- /tree dialog --
    ("td.title", " Session tree [{filter}]"),
    (
        "td.hint",
        " enter jump • ^e edit label • ^t cycle filter • type to filter • esc",
    ),
    ("td.edit_label", " label: {input}"),
    ("tree.assistant_tools", "· assistant (tools)"),
    ("tree.compacted", "◆ compacted ({tokens} tokens)"),
    ("tree.model_change", "◦ model: {ref}"),
    // -- /help --
    ("help.commands", "**Commands**"),
    ("help.keys", "**Keys** (customize in `keybindings.json`)"),
    ("help.unbound", "*(unbound)*"),
    (
        "help.editor",
        "\n**Editor**\n- `enter` submit (queues while the agent runs) • `shift+enter`/`alt+enter`/`ctrl+j` newline\n\
         - `ctrl+enter`/`ctrl+s` send now (aborts the current run) • `alt+↑`/`ctrl+r` recall queued message\n\
         - note: `ctrl+enter`/`alt+↑` need a Kitty-protocol or Esc+ terminal; `ctrl+s`/`ctrl+r` work everywhere\n\
         - `up/down` history at edges • `ctrl+a/e` line start/end • `ctrl+←/→` word jump\n\
         - `ctrl+w/u/k` kill word/line-start/line-end • `ctrl+y` yank • `alt+y` yank-pop • `ctrl+-` undo\n\
         - paste >10 lines or >1000 chars collapses to `[paste #N]` marker (expanded on submit)\n\
         - `@path` attaches a file or image • `!cmd` bash in context • `!!cmd` bash excluded\n",
    ),
    // -- slash command descriptions (/help + autocomplete) --
    ("cmd.desc.help", "Show commands and keybindings"),
    ("cmd.desc.model", "Switch model (fuzzy, e.g. /model sonnet)"),
    (
        "cmd.desc.scoped-models",
        "Enable/reorder models for ctrl+p cycling",
    ),
    ("cmd.desc.thinking", "Set thinking level"),
    (
        "cmd.desc.mode",
        "Permission mode: /mode or /mode ask|acceptEdits|plan|bypass",
    ),
    ("cmd.desc.fullscreen", "Toggle fullscreen (alt-screen) mode"),
    (
        "cmd.desc.compact",
        "Compact context now (optional custom instructions)",
    ),
    ("cmd.desc.new", "New session"),
    ("cmd.desc.resume", "Resume a previous session"),
    ("cmd.desc.tree", "Navigate the session tree (branches)"),
    ("cmd.desc.fork", "Fork from an earlier user message"),
    (
        "cmd.desc.rewind",
        "Rewind to any earlier checkpoint (branch summarized)",
    ),
    (
        "cmd.desc.checkpoints",
        "List file checkpoints; /checkpoints restore <turn> rolls files back",
    ),
    (
        "cmd.desc.memory",
        "Show persistent memories; /memory forget|edit <name> [project|user]",
    ),
    (
        "cmd.desc.search",
        "Full-text search across all past sessions: /search <query>",
    ),
    (
        "cmd.desc.cron",
        "Scheduled prompts: /cron, /cron add \"<schedule>\" <prompt>, /cron remove|pause|resume <id>",
    ),
    (
        "cmd.desc.trace",
        "Show recent structured log events (/trace [level] [target-prefix])",
    ),
    (
        "cmd.desc.clone",
        "Duplicate the session at the current position",
    ),
    ("cmd.desc.name", "Name the session"),
    ("cmd.desc.session", "Session stats"),
    (
        "cmd.desc.cost",
        "Token usage and cost breakdown (per model)",
    ),
    (
        "cmd.desc.context",
        "Context-window breakdown: system prompt, tools, history segments (tokens)",
    ),
    (
        "cmd.desc.todo",
        "Show the session todo list (/todo clear empties it)",
    ),
    ("cmd.desc.rules", "Show context files and skills"),
    ("cmd.desc.changelog", "Show the changelog"),
    (
        "cmd.desc.debug",
        "Write rendered frame + session messages to the debug log",
    ),
    ("cmd.desc.copy", "Copy selection or last assistant message"),
    (
        "cmd.desc.export",
        "Export the session (.jsonl; .html in a later milestone)",
    ),
    ("cmd.desc.login", "OAuth login for a provider"),
    ("cmd.desc.logout", "Remove a stored credential"),
    ("cmd.desc.reload", "Reload settings/skills/prompts"),
    (
        "cmd.desc.trust",
        "Trust this project's .pi settings/resources/MCP servers",
    ),
    (
        "cmd.desc.mcp",
        "Browse MCP resources/prompts, insert into editor",
    ),
    ("cmd.desc.settings", "Open the settings menu"),
    (
        "cmd.desc.theme",
        "Switch color theme (supports light/dark auto pair)",
    ),
    (
        "cmd.desc.share",
        "Share the session as a secret GitHub gist (needs GITHUB_TOKEN)",
    ),
    ("cmd.desc.import", "Import a session .jsonl and resume it"),
    (
        "cmd.desc.models",
        "Model catalog status; `/models refresh` fetches the latest from npm",
    ),
    (
        "cmd.desc.providers",
        "List providers with auth status and model counts",
    ),
    ("cmd.desc.quit", "Exit"),
    // -- /settings categories --
    ("settings.title", "Settings"),
    ("settings.desc.theme", "Color theme (built-ins/custom JSON)"),
    ("settings.desc.tuiMode", "TUI mode (regular/fullscreen)"),
    ("settings.desc.autoCompact", "Automatic context compaction"),
    (
        "settings.desc.autoRetry",
        "Automatic provider retry on transient errors",
    ),
    (
        "settings.desc.steeringMode",
        "Steering message delivery (all/one-at-a-time)",
    ),
    (
        "settings.desc.followUpMode",
        "Follow-up message delivery (all/one-at-a-time)",
    ),
    (
        "settings.desc.doubleEscapeAction",
        "Double-Esc on empty editor (tree/fork/none)",
    ),
    (
        "settings.desc.treeFilterMode",
        "/tree filter (default/no-tools/user-only/labeled-only/all)",
    ),
    (
        "settings.desc.hideThinkingBlock",
        "Hide thinking blocks (on/off)",
    ),
    (
        "settings.desc.blockImages",
        "Never send images to the LLM (on/off)",
    ),
    (
        "settings.desc.showImages",
        "Inline image rendering (on/off)",
    ),
    (
        "settings.desc.showCacheMissNotices",
        "Prompt-cache miss notices (on/off)",
    ),
    (
        "settings.desc.cacheRetention",
        "Prompt-cache retention (short/long/off)",
    ),
    (
        "settings.desc.clearOnShrink",
        "Clear screen when the terminal shrinks (on/off)",
    ),
    ("settings.desc.mermaid", "Mermaid diagrams (image/off)"),
    (
        "settings.desc.quietStartup",
        "Skip banner + changelog at startup (on/off)",
    ),
    // -- /settings values --
    ("settings.val.dark", "built-in dark"),
    ("settings.val.light", "built-in light"),
    ("settings.val.builtin_theme", "built-in theme"),
    ("settings.val.user_theme", "user theme"),
    ("settings.val.regular", "output stays in scrollback"),
    ("settings.val.fullscreen", "alternate screen"),
    (
        "settings.val.autoCompact_on",
        "compact when nearing the context window",
    ),
    ("settings.val.autoCompact_off", "manual /compact only"),
    (
        "settings.val.autoRetry_on",
        "retry transient provider errors",
    ),
    ("settings.val.autoRetry_off", "fail immediately"),
    (
        "settings.val.deliver_all",
        "deliver all queued messages at once",
    ),
    ("settings.val.deliver_one", "deliver the oldest only"),
    ("settings.val.esc_tree", "open the session tree"),
    ("settings.val.esc_fork", "open the fork picker"),
    ("settings.val.esc_none", "do nothing"),
    ("settings.val.tf_default", "hide settings entries"),
    ("settings.val.tf_no_tools", "default minus tool results"),
    ("settings.val.tf_user_only", "only user messages"),
    ("settings.val.tf_labeled_only", "only labeled entries"),
    ("settings.val.tf_all", "everything"),
    ("settings.val.mermaid_image", "render diagrams as images"),
    ("settings.val.mermaid_off", "plain code blocks"),
    (
        "settings.val.cache_short",
        "5-minute writes (provider default)",
    ),
    (
        "settings.val.cache_long",
        "1h writes (24h on OpenAI); higher write cost",
    ),
    (
        "settings.val.cache_off",
        "no cache markers (read-only where supported)",
    ),
    ("settings.val.on", "enabled"),
    ("settings.val.off", "disabled"),
    // -- keybinding docs (/help keys section) --
    (
        "keys.desc.app.interrupt",
        "interrupt the running agent / close search",
    ),
    ("keys.desc.app.clear", "clear editor; press twice to exit"),
    ("keys.desc.app.exit", "exit when the editor is empty"),
    (
        "keys.desc.app.tools.expand",
        "expand/collapse all tool outputs",
    ),
    (
        "keys.desc.app.editor.external",
        "open the prompt in $EDITOR",
    ),
    (
        "keys.desc.app.clipboard.pasteImage",
        "paste an image from the clipboard",
    ),
    (
        "keys.desc.app.model.cycleForward",
        "cycle to the next scoped model",
    ),
    (
        "keys.desc.app.model.cycleBackward",
        "cycle to the previous scoped model",
    ),
    (
        "keys.desc.app.mode.cycle",
        "cycle permission mode (ask/acceptEdits/plan/bypass)",
    ),
    (
        "keys.desc.app.thinking.toggle",
        "cycle thinking blocks: collapsed → expanded → hidden",
    ),
    (
        "keys.desc.app.message.dequeue",
        "recall the newest queued message",
    ),
    (
        "keys.desc.app.message.sendNow",
        "abort the run and send immediately",
    ),
    (
        "keys.desc.app.message.followUp",
        "queue a follow-up message",
    ),
    (
        "keys.desc.app.search.open",
        "search the transcript (fullscreen)",
    ),
    ("keys.desc.app.search.next", "next search match"),
    ("keys.desc.app.search.previous", "previous search match"),
    (
        "keys.desc.app.scroll.promptPrevious",
        "jump to the previous prompt (fullscreen)",
    ),
    (
        "keys.desc.app.scroll.promptNext",
        "jump to the next prompt (fullscreen)",
    ),
    // -- command notices / messages --
    ("msg.tui_mode", "TUI mode: {mode}"),
    (
        "msg.invalid_mode",
        "invalid mode {mode} (ask|acceptEdits|plan|bypass)",
    ),
    ("msg.offline_share", "offline mode: /share is disabled"),
    ("msg.offline_login", "offline mode: OAuth login is disabled"),
    (
        "msg.offline_catalog",
        "offline mode: model catalog refresh is disabled",
    ),
    (
        "msg.ext_command_failed",
        "extension command /{name} failed: {error}",
    ),
    (
        "msg.unknown_command",
        "unknown command: /{name} (try /help)",
    ),
    ("msg.no_model_match", "no model matching {query}"),
    (
        "msg.no_providers",
        "no providers configured — /login or set a provider API key env var first",
    ),
    (
        "msg.no_models_cycle",
        "no models to cycle (set /scoped-models)",
    ),
    (
        "msg.invalid_thinking",
        "invalid thinking level {level} (available: {levels})",
    ),
    (
        "msg.thinking_record_failed",
        "failed to record thinking level: {error}",
    ),
    ("msg.thinking_set", "Thinking: {level}"),
    ("msg.dialog_thinking_title", "Thinking level"),
    (
        "msg.session_create_failed",
        "failed to create session: {error}",
    ),
    (
        "msg.no_previous_sessions",
        "no previous sessions for this directory",
    ),
    (
        "msg.checkpoints_disabled",
        "file checkpoints are disabled (features.checkpoints)",
    ),
    ("msg.invalid_turn", "invalid turn number: {arg}"),
    (
        "msg.restored_header",
        "Restored turn {turn} checkpoint ({count} file(s)):",
    ),
    ("msg.restored_action", "restored"),
    ("msg.deleted_action", "deleted (created in turn)"),
    ("msg.restore_failed", "restore failed: {error}"),
    (
        "msg.no_file_checkpoints",
        "No file checkpoints yet. Every turn's edit/write changes are snapshotted; use /checkpoints restore <turn> to roll files back.",
    ),
    (
        "msg.checkpoints_header",
        "**File checkpoints** (pre-turn file states)",
    ),
    (
        "msg.checkpoint_row",
        "- turn {turn}: {count} file(s) — {names}{more}",
    ),
    ("msg.more_files", " +{count} more"),
    (
        "msg.checkpoints_footer",
        "\nRestore with `/checkpoints restore <turn>`.",
    ),
    (
        "msg.memory_disabled",
        "persistent memory is disabled (features.memory)",
    ),
    ("msg.no_memory_named", "no memory named {name}"),
    ("msg.memory_index_failed", "index rebuild failed: {error}"),
    ("msg.memory_deleted", "Memory {name} deleted."),
    ("msg.memory_delete_failed", "cannot delete {name}: {error}"),
    (
        "msg.memory_forget_usage",
        "usage: /memory forget <name> [project|user]",
    ),
    (
        "msg.memory_scope_unknown",
        "unknown scope {scope} (project|user)",
    ),
    ("msg.memory_scope_header", "**{scope} memory** (`{dir}`)"),
    (
        "msg.memory_footer",
        "Read a file for details; `/memory edit <name>` edits one, `/memory forget <name>` deletes. Scopes: project (this repository, shared across worktrees) and user (cross-project). The agent manages these via the memory tool.",
    ),
    (
        "msg.no_memories",
        "No memories yet. The agent saves durable facts (preferences, conventions) with the memory tool; they are injected into every future session.",
    ),
    ("msg.usage_search", "usage: /search <query>"),
    ("msg.no_sessions_match", "no sessions match {query}"),
    (
        "msg.sessions_matching",
        "**Sessions matching {query}** ({count})",
    ),
    ("msg.search_row", "### {title} — {time} ({count} match(es))"),
    (
        "msg.search_footer",
        "Open one with `/resume`, or `tack --session <id>` from the CLI.",
    ),
    (
        "msg.cron_disabled",
        "scheduled prompts are disabled (features.cron)",
    ),
    (
        "msg.cron_usage",
        "usage: /cron add \"every 10m\" <prompt>  or  /cron add \"*/5 * * * *\" <prompt>",
    ),
    (
        "msg.cron_scheduled",
        "Scheduled job {id} ({schedule}): {prompt}",
    ),
    ("msg.cron_removed", "Removed job {id}."),
    ("msg.cron_no_job", "no job {id}"),
    ("msg.cron_paused", "Paused job {id}."),
    ("msg.cron_resumed", "Resumed job {id}."),
    (
        "msg.cron_none",
        "No scheduled jobs. Add one with /cron add \"every 10m\" <prompt> or a 5-field cron expression. Jobs fire into this session; runs missed while tack was closed fire once at startup.",
    ),
    ("msg.cron_header", "**Scheduled jobs** (`cron.json`)"),
    ("msg.cron_paused_state", "paused"),
    (
        "msg.cron_unknown",
        "unknown /cron subcommand {cmd} (add|remove|pause|resume)",
    ),
    (
        "msg.cron_fired",
        "Scheduled task fired ({schedule}): {prompt}",
    ),
    (
        "msg.trace_none",
        "no trace events (enable with TACK_TRACE_FILE=1 or observability.enabled in settings)",
    ),
    ("msg.trace_header", "**Recent trace events**"),
    ("msg.rewind_detail", "+{chars} chars of replies"),
    ("msg.no_checkpoints", "no checkpoints yet"),
    (
        "msg.rewind_title",
        "Rewind to checkpoint (enter jumps; the abandoned branch is summarized first)",
    ),
    ("msg.nothing_to_fork", "nothing to fork from"),
    ("msg.fork_title", "Fork from message"),
    ("msg.tree_empty", "session tree is empty"),
    ("msg.ext_editor_failed", "external editor failed: {error}"),
    ("msg.ext_readback_failed", "read back failed: {error}"),
    ("msg.editor_exited", "editor exited with {status}"),
    ("msg.editor_launch_failed", "cannot launch editor: {error}"),
    ("msg.setting_save_failed", "failed to save setting: {error}"),
    ("msg.setting_saved", "{key} = {value} (saved)"),
    ("msg.session_not_persisted", "session is not persisted"),
    (
        "msg.share_needs_token",
        "/share needs GITHUB_TOKEN (secret gist)",
    ),
    ("msg.share_read_failed", "cannot read session file"),
    ("msg.sharing", "sharing via gist…"),
    ("msg.shared", "shared: {url} (link copied)"),
    ("msg.share_failed", "share failed: {error}"),
    ("msg.usage_import", "usage: /import <session.jsonl>"),
    ("msg.session_imported", "session imported"),
    ("msg.import_failed", "import failed: {error}"),
    ("msg.session_cloned", "session cloned"),
    ("msg.clone_failed", "clone failed: {error}"),
    ("msg.usage_name", "usage: /name <name>"),
    ("msg.session_named", "session named {name}"),
    ("msg.failed", "failed: {error}"),
    ("msg.copied_selection", "copied selection"),
    ("msg.copied_last", "copied last assistant message"),
    ("msg.nothing_to_copy", "nothing to copy"),
    ("msg.copy_failed", "copy failed: {error}"),
    ("msg.debug_written", "✓ debug log written: {path}"),
    ("msg.debug_failed", "debug log failed: {error}"),
    ("msg.exported", "exported to {path}"),
    ("msg.export_failed", "export failed: {error}"),
    ("msg.reloaded", "reloaded settings/skills/prompts"),
    ("msg.scoped_saved", "scoped models saved"),
    (
        "msg.scoped_save_failed",
        "failed to save scopedModels: {error}",
    ),
    ("msg.label_failed", "label failed: {error}"),
    ("msg.default_model_saved", "default model saved: {ref}"),
    (
        "msg.default_model_save_failed",
        "failed to save default model: {error}",
    ),
    ("msg.thinking_expanded", "thinking blocks expanded"),
    ("msg.thinking_hidden", "thinking blocks hidden"),
    ("msg.thinking_collapsed", "thinking blocks collapsed"),
    ("msg.no_queued", "no queued messages"),
    (
        "msg.model_fallback",
        "model fallback: {from} → {to} ({reason})",
    ),
    (
        "msg.cache_miss",
        "cache miss: {count} input tokens uncached",
    ),
    (
        "msg.provider_locked",
        "provider is locked to {locked} by managed settings",
    ),
    (
        "msg.model_locked",
        "model is locked to {locked} by managed settings",
    ),
    (
        "msg.model_record_failed",
        "failed to record model change: {error}",
    ),
    ("msg.model_set", "model: {ref}"),
    ("msg.session_resumed", "session resumed"),
    ("msg.resume_failed", "resume failed: {error}"),
    ("msg.forked", "forked to that point"),
    ("msg.fork_failed", "fork failed: {error}"),
    (
        "msg.theme_saved",
        "theme: {value} (saved). /help shows commands — happy hacking!",
    ),
    ("msg.theme_save_failed", "failed to save theme: {error}"),
    (
        "msg.provider_registered",
        "provider registered: {id} (/model {id}/…)",
    ),
    (
        "msg.welcome",
        "tack {version} — {model} ({provider}). /help for commands, /model to switch models.",
    ),
    (
        "msg.onboarding_no_credentials",
        "**No provider credentials found — let's set one up.**\n\n\
         1. See what's available: `/providers` (or `tack providers` in a shell) — every provider with model counts and how to enable it\n\
         2. Log in: `tack login --provider <id>` (OAuth for anthropic/github-copilot/openrouter/…; reads an API key from stdin otherwise)\n\
         3. Or set an env var, e.g. `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`\n\
         4. Browse and pick models: `/model` here, or `tack models [pattern]`\n\n\
         Custom endpoints go in `~/.tack/agent/models.json` (see docs/configuration.md).",
    ),
    // -- login / logout --
    ("msg.no_credentials", "no stored credentials"),
    ("msg.logout_title", "Log out of provider"),
    ("msg.logged_out", "logged out of {provider}"),
    (
        "msg.no_credential_for",
        "no stored credential for {provider}",
    ),
    ("msg.logout_failed", "logout failed: {error}"),
    ("msg.usage_login", "usage: /login <provider>"),
    (
        "msg.no_oauth_flow",
        "{provider} has no OAuth flow; store a key with: tack login --provider {provider} --api-key …",
    ),
    (
        "msg.oauth_starting",
        "starting OAuth login for {provider} — check your browser",
    ),
    // -- compaction --
    (
        "msg.compact_running",
        "compaction runs automatically; wait for the current run",
    ),
    (
        "msg.nav_running",
        "wait for the current response or compaction to finish before navigating the session tree",
    ),
    (
        "msg.nothing_to_compact",
        "nothing to compact (session too small)",
    ),
    ("msg.compact_auth_failed", "compaction auth failed: {error}"),
    (
        "msg.compact_persist_failed",
        "failed to persist compaction: {error}",
    ),
    ("msg.compact_failed", "compaction failed: {error}"),
    // -- bash / branch summary --
    ("msg.bash_persist_failed", "bash persist failed: {error}"),
    (
        "msg.branch_auth_failed",
        "branch summary auth failed: {error}",
    ),
    ("msg.branch_summarizing", "summarizing abandoned branch…"),
    (
        "msg.branch_record_failed",
        "failed to record branch summary: {error}",
    ),
    ("msg.branch_failed", "branch summary failed: {error}"),
    // -- /models catalog --
    ("msg.catalog_refreshing", "refreshing model catalog…"),
    (
        "msg.catalog_refreshed",
        "model catalog refreshed from @earendil-works/pi-ai {version}: {providers} providers, {models} models — effective immediately",
    ),
    (
        "msg.catalog_refresh_failed",
        "model catalog refresh failed: {error}",
    ),
    (
        "msg.catalog_reset",
        "cached model catalog removed — the embedded catalog is used again after restart",
    ),
    (
        "msg.catalog_refreshed_startup",
        "model catalog refreshed from @earendil-works/pi-ai {version}: {providers} providers, {models} models",
    ),
    (
        "catalog.status_cached",
        "**Model catalog**: refreshed from `@earendil-works/pi-ai` {version} — {providers} providers, {models} models\n\nCached in `{path}`. `/models refresh` fetches the latest; `/models reset` returns to the embedded catalog.",
    ),
    (
        "catalog.status_embedded",
        "**Model catalog**: embedded (built into this binary, {count} provider overrides active).\n\n`/models refresh` fetches the latest catalog from the published `@earendil-works/pi-ai` npm package. Automatic refresh at startup can be enabled with `\"modelCatalogRefresh\": true` in settings.json.",
    ),
    // -- /providers --
    (
        "providers.header",
        "**Providers** — ✓ = credentials available\n\n",
    ),
    (
        "providers.login_hint",
        " — `tack login --provider {id}` or set `{env}`",
    ),
    ("providers.models_count", "{count} model(s)"),
    ("providers.custom_suffix", " (custom, models.json)"),
    (
        "providers.footer",
        "\n{ready} provider(s) ready. Browse models with `/model`, the full catalog with `tack models [pattern]`, and refresh it with `/models refresh`.",
    ),
    // -- /session --
    (
        "session.body",
        "**Session**\n- file: `{file}`\n- id: `{id}`\n- context: {tokens} tokens ({pct}% of {window}k)\n- totals: ↑{input} ↓{output} R{cr} W{cw} — ${cost}\n",
    ),
    // -- /cost --
    ("cost.header", "**Usage & cost**\n\n"),
    ("cost.no_usage", "(no usage recorded yet)\n"),
    (
        "cost.table_header",
        "| model | input | output | cache R | cache W | cost |\n|---|---|---|---|---|---|\n",
    ),
    (
        "cost.total",
        "\n**Total**: {tokens} tokens ({input} in / {output} out) — **${cost}**\n",
    ),
    (
        "cost.budget",
        "\nBudget: {used} / {budget} tokens ({pct}%){extra}\n",
    ),
    ("cost.budget_exceeded", " — **exceeded**"),
    // -- /todo --
    ("todo.cleared", "cleared {count} todo item(s)"),
    ("todo.header", "**Todo list**\n\n"),
    (
        "todo.empty",
        "(empty — the agent maintains this via its todo tool)\n",
    ),
    ("todo.done_count", "\n{done}/{total} done\n"),
    ("panel.todos", " Todos"),
    (
        "panel.ext_focused",
        "  [focused · ↑↓ nav · enter select · esc]",
    ),
    // -- /context --
    ("ctx.header", "**Context usage**\n\n"),
    ("ctx.table_header", "| segment | ~tokens |\n|---|---|\n"),
    ("ctx.seg_system", "system prompt"),
    ("ctx.seg_tools", "tool schemas"),
    ("ctx.seg_user", "user messages"),
    ("ctx.seg_assistant", "assistant messages"),
    ("ctx.seg_thinking", "thinking"),
    ("ctx.seg_tool_results", "tool results"),
    ("ctx.sum", "| **sum (estimated)** | **{total}** |\n"),
    (
        "ctx.provider_count",
        "\nLast provider count: {usage} tokens (+ ~{trailing} since, estimated) → **{total} total**\n",
    ),
    (
        "ctx.no_usage",
        "\nNo provider usage yet — all numbers are chars/4 estimates.\n",
    ),
    (
        "ctx.window",
        "\nWindow: {used} / {window} tokens ({pct}%)\n",
    ),
    (
        "ctx.autocompaction",
        "Auto-compaction triggers at ~{trigger} tokens (reserve {reserve}); {remaining} remaining.\n",
    ),
    ("ctx.by_size", "\nTool results by size:\n"),
    ("ctx.by_size_row", "- {name}: ~{tokens} tokens\n"),
    (
        "ctx.cache",
        "\nPrompt cache (session totals): {read} tokens read, {write} written · hit rate {pct}%\n",
    ),
    (
        "ctx.budgets",
        "\nHistory optimization: tool-result cap {cap} chars · microcompact {micro} chars (min savings {savings}) · duplicate-read masking {dedup} · rules cap {rules} chars · goal recitation {recite}\n",
    ),
    ("ctx.flag_on", "on"),
    ("ctx.flag_off", "off"),
    // -- /rules --
    ("rules.context_files", "**Context files**\n"),
    ("rules.none", "- (none)\n"),
    ("rules.skills", "\n**Skills**\n"),
    ("rules.file_row", "- `{path}` ({chars} chars)\n"),
    // -- /trust dialog --
    ("trust.title", "Trust project folder? {path}"),
    ("trust.trust", "Trust"),
    (
        "trust.trust_desc",
        "persist: load this project's .pi settings/resources and MCP servers",
    ),
    ("trust.parent", "Trust parent folder ({path})"),
    (
        "trust.parent_desc",
        "persist: trust everything under the parent",
    ),
    ("trust.trust_session", "Trust (this session only)"),
    ("trust.distrust", "Do not trust"),
    (
        "trust.distrust_desc",
        "persist: ignore project .pi resources",
    ),
    ("trust.distrust_session", "Do not trust (this session only)"),
    (
        "trust.now_trusted",
        "project trusted — .pi settings/resources apply (project MCP servers from the next run)",
    ),
    (
        "trust.now_untrusted",
        "project not trusted — project .pi resources ignored",
    ),
    // -- first-run wizard --
    (
        "firstrun.title",
        "Welcome to tack! Choose a theme — detected system appearance: {detected} (change anytime with /theme)",
    ),
    ("firstrun.dark", "Dark"),
    ("firstrun.dark_desc", "dark background terminals"),
    ("firstrun.light", "Light"),
    ("firstrun.light_desc", "light background terminals"),
    ("firstrun.auto", "Auto (light/dark)"),
    (
        "firstrun.auto_desc",
        "detect the terminal background via OSC 11",
    ),
    // -- /mcp --
    (
        "mcp.none_configured",
        "no MCP servers configured (mcp.json)",
    ),
    ("mcp.connecting", "connecting MCP servers…"),
    ("mcp.none_connected", "no MCP servers could be connected"),
    (
        "mcp.empty",
        "connected MCP servers expose no resources or prompts",
    ),
    ("mcp.title", "MCP resources & prompts (insert into editor)"),
    ("mcp.resource_inserted", "resource inserted: {id}"),
    ("mcp.read_failed", "read_resource failed: {error}"),
    ("mcp.prompt_inserted", "prompt inserted: {id}"),
    ("mcp.prompt_failed", "get_prompt failed: {error}"),
    ("mcp.image", "[image]"),
    ("mcp.content", "[content]"),
    // -- autocomplete --
    ("auto.prompt_template", "prompt template"),
    // -- mermaid caption --
    ("mermaid.full_resolution", "  (full resolution: {path})"),
    // -- footer --
    ("footer.thinking_off", "off"),
    ("footer.update_available", "↑ v{version}"),
    // -- update check notice --
    (
        "msg.update_available",
        "New tack version available: v{version} (current: v{current}) — run `tack update` to upgrade.",
    ),
    // -- history reverse search (ctrl+r) --
    (
        "search.history_hint",
        "type to search history · ctrl+r/↑ older · ↓ newer · enter accept · esc cancel",
    ),
    ("search.history_no_match", "no match"),
    // -- desktop notifications --
    ("notify.permission_title", "Permission needed"),
    ("notify.run_done", "Agent run finished ({model})"),
    ("notify.run_error", "Agent run failed ({model})"),
    ("notify.bg_task_title", "Background task {status}"),
    // -- fullscreen jump-to-bottom pill --
    ("fs.back_to_bottom", "↓ Back to bottom · End"),
];

const ZH: &[(&str, &str)] = &[
    // -- 权限对话框 --
    ("permission.title", "允许 {what}？"),
    ("permission.yes", "允许"),
    ("permission.always", "允许，且记住（此工具/输入）"),
    ("permission.no", "拒绝"),
    ("permission.hint", " ↑↓ 选择 • 回车确认 • Esc 拒绝"),
    (
        "permission.plan",
        "批准这个计划？{preview}…（完整计划：{path}）",
    ),
    // -- 通知（运行/模式/预算） --
    ("notice.mode", "权限模式：{mode}"),
    (
        "notice.budget",
        "Token 预算超限：已用 {used} / {budget}（约 ${cost}）。可在 settings.json 调整 tokenBudget。",
    ),
    ("notice.bg_done", "后台任务 {id} {status}：{command}"),
    (
        "notice.quit_grace",
        "正在取消运行——收尾完成后退出（再次按退出键强制退出）。",
    ),
    ("notice.prompt_blocked", "提示词被拦截：{reason}"),
    (
        "notice.stop_hook_continue",
        "Stop 钩子要求继续运行：{reason}",
    ),
    ("notice.hook_blocked", "被 UserPromptSubmit 钩子拦截"),
    (
        "notice.budget_pause",
        "Token 预算超限（{used} / {budget}）；本次运行已暂停。可输入新提示词继续，或调整 tokenBudget。",
    ),
    (
        "notice.budget_downgrade",
        "Token 预算超限（{used} / {budget}）；切换到预算模型 {model}。",
    ),
    (
        "notice.mode_bypass_disabled",
        "bypass 模式已被组织策略（managed settings）禁用",
    ),
    // -- 状态指示器 --
    ("status.working", "正在工作…"),
    ("status.compacting", "正在压缩上下文…"),
    ("status.retrying", "正在重试（第 {attempt}/{max} 次）"),
    ("status.retry_countdown", "{label}（{secs} 秒后）"),
    // -- 会话记录区 --
    ("chat.compacted", "◆ 已压缩上下文（压缩了 {tokens} tokens）"),
    (
        "chat.compacted_summary",
        "◆ 已压缩上下文（{tokens} tokens）——摘要：",
    ),
    ("chat.skill", "[技能] {name}"),
    ("chat.queued_followup", "agent 停止后"),
    ("chat.queued_steering", "下一个回合边界"),
    (
        "chat.queued",
        "⏳ 已排队——{when}发送 · alt+↑ 撤回 · ctrl+s 立即发送",
    ),
    ("chat.thought", "◦ 思考完成（{chars} 字符）——ctrl+t 展开"),
    ("chat.aborted", "◦ 已中止"),
    ("chat.truncated", "◦ 输出被截断（达到 max tokens）"),
    // -- 工具卡片 --
    ("tool.more_lines", "…（还有 {remaining} 行）"),
    (
        "tool.more_lines_expand",
        "…（还有 {remaining} 行，Ctrl+O 展开）",
    ),
    // -- 对话框通用 --
    ("dialog.filter", "过滤："),
    (
        "dialog.hint.select",
        " 输入过滤 • ↑↓ 选择 • 回车确认 • Esc 取消",
    ),
    ("dialog.hint.input", " 回车确认 • Esc 取消"),
    // -- ask_user 工具对话框 --
    ("ask_user.step", " 问题 {step}/{total}:"),
    ("ask_user.other", "其他（自定义回答）"),
    ("ask_user.placeholder", "输入你的回答"),
    ("ask_user.empty", "回答不能为空"),
    (
        "dialog.hint.scoped",
        " 输入过滤 • ↑↓ 移动 • 回车/空格 切换 • Esc 完成",
    ),
    (
        "dialog.scoped_models.title",
        " 循环模型列表（ctrl+p / shift+ctrl+p 循环切换）",
    ),
    // -- /model 对话框 --
    ("modeld.title", " 选择模型"),
    ("modeld.scope", " 范围："),
    ("modeld.scope_all", "全部"),
    ("modeld.scope_scoped", "范围内"),
    ("modeld.scope_hint", "（tab 切换）"),
    (
        "modeld.only_configured",
        " 仅显示已配置提供商的模型。用 /login 添加提供商。",
    ),
    ("modeld.default_badge", " · 默认"),
    ("modeld.no_match", "  没有匹配的模型"),
    ("modeld.model_name", "  模型名称：{name}"),
    ("modeld.hint", "  回车选择 · Ctrl+S 设为默认 · Esc 取消"),
    // -- /resume 会话对话框 --
    ("sd.title", " 恢复会话（按{sort}）"),
    ("sd.sort_name", "名称"),
    ("sd.sort_time", "时间"),
    (
        "sd.hint_browse",
        " 输入过滤 • ↑↓ 选择 • 回车恢复 • ^r 重命名 • ^d 删除 • ^s 排序 • Esc 取消",
    ),
    (
        "sd.confirm_delete",
        " 删除 {label}？回车/^d 确认 • Esc 取消",
    ),
    ("sd.rename", " 重命名：{input}"),
    ("sd.meta", "{count} 条消息 • {date}"),
    ("sd.delete_failed", "删除失败：{error}"),
    ("sd.rename_failed", "重命名失败：{error}"),
    ("sd.no_delete_active", "不能删除当前会话"),
    ("sd.rename_active", "当前会话请用 /name 重命名"),
    // -- /tree 对话框 --
    ("td.title", " 会话树 [{filter}]"),
    (
        "td.hint",
        " 回车跳转 • ^e 编辑标签 • ^t 切换过滤 • 输入过滤 • Esc 取消",
    ),
    ("td.edit_label", " 标签：{input}"),
    ("tree.assistant_tools", "· 助手（工具调用）"),
    ("tree.compacted", "◆ 已压缩（{tokens} tokens）"),
    ("tree.model_change", "◦ 模型：{ref}"),
    // -- /help --
    ("help.commands", "**命令**"),
    ("help.keys", "**按键**（可在 `keybindings.json` 自定义）"),
    ("help.unbound", "*（未绑定）*"),
    (
        "help.editor",
        "\n**编辑器**\n- `enter` 提交（agent 运行时会排队）• `shift+enter`/`alt+enter`/`ctrl+j` 换行\n\
         - `ctrl+enter`/`ctrl+s` 立即发送（中止当前运行）• `alt+↑`/`ctrl+r` 撤回排队消息\n\
         - 注意：`ctrl+enter`/`alt+↑` 需要支持 Kitty 键盘协议或 Esc+ 的终端；`ctrl+s`/`ctrl+r` 到处可用\n\
         - `up/down` 到边界时翻历史 • `ctrl+a/e` 行首/行尾 • `ctrl+←/→` 按词跳转\n\
         - `ctrl+w/u/k` 删除词/删到行首/删到行尾 • `ctrl+y` 粘贴回 • `alt+y` 循环粘贴 • `ctrl+-` 撤销\n\
         - 粘贴超过 10 行或 1000 字符会折叠为 `[paste #N]` 标记（提交时展开）\n\
         - `@path` 附加文件或图片 • `!cmd` 在上下文中执行 bash • `!!cmd` 执行但不进上下文\n",
    ),
    // -- 斜杠命令描述 --
    ("cmd.desc.help", "查看命令和按键绑定"),
    ("cmd.desc.model", "切换模型（模糊匹配，如 /model sonnet）"),
    ("cmd.desc.scoped-models", "启用/排序 ctrl+p 循环的模型"),
    ("cmd.desc.thinking", "设置思考级别"),
    (
        "cmd.desc.mode",
        "权限模式：/mode 或 /mode ask|acceptEdits|plan|bypass",
    ),
    ("cmd.desc.fullscreen", "切换全屏（alt-screen）模式"),
    ("cmd.desc.compact", "立即压缩上下文（可附自定义指令）"),
    ("cmd.desc.new", "新会话"),
    ("cmd.desc.resume", "恢复之前的会话"),
    ("cmd.desc.tree", "浏览会话树（分支）"),
    ("cmd.desc.fork", "从更早的用户消息分叉"),
    (
        "cmd.desc.rewind",
        "回退到任意较早的检查点（被放弃的分支会先总结）",
    ),
    (
        "cmd.desc.checkpoints",
        "列出文件检查点；/checkpoints restore <turn> 回滚文件",
    ),
    (
        "cmd.desc.memory",
        "查看持久记忆；/memory forget|edit <name> [project|user]",
    ),
    ("cmd.desc.search", "全文搜索所有历史会话：/search <query>"),
    (
        "cmd.desc.cron",
        "定时提示词：/cron、/cron add \"<schedule>\" <prompt>、/cron remove|pause|resume <id>",
    ),
    (
        "cmd.desc.trace",
        "查看最近的结构化日志事件（/trace [level] [target-prefix]）",
    ),
    ("cmd.desc.clone", "在当前位置复制会话"),
    ("cmd.desc.name", "命名会话"),
    ("cmd.desc.session", "会话统计"),
    ("cmd.desc.cost", "Token 用量与费用明细（按模型）"),
    (
        "cmd.desc.context",
        "上下文窗口明细：系统提示、工具、历史分段（token）",
    ),
    ("cmd.desc.todo", "查看会话任务列表（/todo clear 清空）"),
    ("cmd.desc.rules", "查看上下文文件和技能"),
    ("cmd.desc.changelog", "查看更新日志"),
    ("cmd.desc.debug", "把渲染帧和会话消息写入调试日志"),
    ("cmd.desc.copy", "复制选区，否则复制最后一条助手消息"),
    ("cmd.desc.export", "导出会话（.jsonl；.html 在后续里程碑）"),
    ("cmd.desc.login", "提供商 OAuth 登录"),
    ("cmd.desc.logout", "删除已保存的凭据"),
    ("cmd.desc.reload", "重新加载设置/技能/提示词模板"),
    ("cmd.desc.trust", "信任本项目的 .pi 设置/资源/MCP 服务器"),
    ("cmd.desc.mcp", "浏览 MCP 资源/提示词并插入编辑器"),
    ("cmd.desc.settings", "打开设置菜单"),
    ("cmd.desc.theme", "切换颜色主题（支持 light/dark 自动配对）"),
    (
        "cmd.desc.share",
        "把会话分享为私密 GitHub gist（需要 GITHUB_TOKEN）",
    ),
    ("cmd.desc.import", "导入会话 .jsonl 并恢复"),
    (
        "cmd.desc.models",
        "模型目录状态；`/models refresh` 从 npm 拉取最新",
    ),
    ("cmd.desc.providers", "列出提供商及其认证状态和模型数量"),
    ("cmd.desc.quit", "退出"),
    // -- /settings 分类 --
    ("settings.title", "设置"),
    ("settings.desc.theme", "颜色主题（内置多套/自定义 JSON）"),
    ("settings.desc.tuiMode", "TUI 模式（regular/fullscreen）"),
    ("settings.desc.autoCompact", "自动压缩上下文"),
    ("settings.desc.autoRetry", "临时性提供商错误自动重试"),
    (
        "settings.desc.steeringMode",
        "steering 消息投递方式（all/one-at-a-time）",
    ),
    (
        "settings.desc.followUpMode",
        "follow-up 消息投递方式（all/one-at-a-time）",
    ),
    (
        "settings.desc.doubleEscapeAction",
        "空编辑器双击 Esc 的行为（tree/fork/none）",
    ),
    (
        "settings.desc.treeFilterMode",
        "/tree 过滤器（default/no-tools/user-only/labeled-only/all）",
    ),
    ("settings.desc.hideThinkingBlock", "隐藏思考块（on/off）"),
    ("settings.desc.blockImages", "从不向模型发送图片（on/off）"),
    ("settings.desc.showImages", "内联渲染图片（on/off）"),
    (
        "settings.desc.showCacheMissNotices",
        "提示缓存未命中提醒（on/off）",
    ),
    (
        "settings.desc.cacheRetention",
        "提示缓存保留时长（short/long/off）",
    ),
    ("settings.desc.clearOnShrink", "终端缩小时清屏（on/off）"),
    ("settings.desc.mermaid", "Mermaid 图表（image/off）"),
    (
        "settings.desc.quietStartup",
        "启动时跳过横幅和更新日志（on/off）",
    ),
    // -- /settings 取值 --
    ("settings.val.dark", "内置暗色"),
    ("settings.val.light", "内置亮色"),
    ("settings.val.builtin_theme", "内置主题"),
    ("settings.val.user_theme", "自定义主题"),
    ("settings.val.regular", "输出留在回滚区"),
    ("settings.val.fullscreen", "alternate screen（全屏）"),
    (
        "settings.val.autoCompact_on",
        "接近上下文窗口上限时自动压缩",
    ),
    ("settings.val.autoCompact_off", "仅手动 /compact"),
    ("settings.val.autoRetry_on", "重试临时性提供商错误"),
    ("settings.val.autoRetry_off", "立即失败"),
    ("settings.val.deliver_all", "一次性投递所有排队消息"),
    ("settings.val.deliver_one", "每次只投递最早一条"),
    ("settings.val.esc_tree", "打开会话树"),
    ("settings.val.esc_fork", "打开分叉选择器"),
    ("settings.val.esc_none", "什么都不做"),
    ("settings.val.tf_default", "隐藏设置类条目"),
    ("settings.val.tf_no_tools", "default 再去掉工具结果"),
    ("settings.val.tf_user_only", "只看用户消息"),
    ("settings.val.tf_labeled_only", "只看有标签的条目"),
    ("settings.val.tf_all", "全部"),
    ("settings.val.mermaid_image", "图表渲染为图片"),
    ("settings.val.mermaid_off", "纯代码块"),
    ("settings.val.cache_short", "5 分钟写入（提供商默认）"),
    (
        "settings.val.cache_long",
        "1 小时写入（OpenAI 为 24h）；写入费更高",
    ),
    ("settings.val.cache_off", "不发缓存标记（支持处只读）"),
    ("settings.val.on", "启用"),
    ("settings.val.off", "禁用"),
    // -- 按键说明 --
    ("keys.desc.app.interrupt", "中断正在运行的 agent / 关闭搜索"),
    ("keys.desc.app.clear", "清空编辑器；再按一次退出"),
    ("keys.desc.app.exit", "编辑器为空时退出"),
    ("keys.desc.app.tools.expand", "展开/折叠所有工具输出"),
    ("keys.desc.app.editor.external", "在 $EDITOR 中编辑提示词"),
    ("keys.desc.app.clipboard.pasteImage", "从剪贴板粘贴图片"),
    ("keys.desc.app.model.cycleForward", "循环到下一个范围内模型"),
    (
        "keys.desc.app.model.cycleBackward",
        "循环到上一个范围内模型",
    ),
    (
        "keys.desc.app.mode.cycle",
        "循环权限模式（ask/acceptEdits/plan/bypass）",
    ),
    (
        "keys.desc.app.thinking.toggle",
        "切换思考块：折叠 → 展开 → 隐藏",
    ),
    ("keys.desc.app.message.dequeue", "撤回最新一条排队消息"),
    ("keys.desc.app.message.sendNow", "中止当前运行并立即发送"),
    ("keys.desc.app.message.followUp", "排队一条 follow-up 消息"),
    ("keys.desc.app.search.open", "搜索会话记录（全屏模式）"),
    ("keys.desc.app.search.next", "下一个搜索匹配"),
    ("keys.desc.app.search.previous", "上一个搜索匹配"),
    (
        "keys.desc.app.scroll.promptPrevious",
        "跳到上一条提示词（全屏模式）",
    ),
    (
        "keys.desc.app.scroll.promptNext",
        "跳到下一条提示词（全屏模式）",
    ),
    // -- 命令通知 / 消息 --
    ("msg.tui_mode", "TUI 模式：{mode}"),
    (
        "msg.invalid_mode",
        "无效模式 {mode}（ask|acceptEdits|plan|bypass）",
    ),
    ("msg.offline_share", "离线模式：/share 已禁用"),
    ("msg.offline_login", "离线模式：OAuth 登录已禁用"),
    ("msg.offline_catalog", "离线模式：模型目录刷新已禁用"),
    ("msg.ext_command_failed", "扩展命令 /{name} 失败：{error}"),
    ("msg.unknown_command", "未知命令：/{name}（试试 /help）"),
    ("msg.no_model_match", "没有匹配 {query} 的模型"),
    (
        "msg.no_providers",
        "尚未配置任何提供商——先 /login 或设置提供商 API key 环境变量",
    ),
    (
        "msg.no_models_cycle",
        "没有可循环的模型（请用 /scoped-models 设置）",
    ),
    (
        "msg.invalid_thinking",
        "无效思考级别 {level}（可用：{levels}）",
    ),
    ("msg.thinking_record_failed", "记录思考级别失败：{error}"),
    ("msg.thinking_set", "思考级别：{level}"),
    ("msg.dialog_thinking_title", "思考级别"),
    ("msg.session_create_failed", "创建会话失败：{error}"),
    ("msg.no_previous_sessions", "此目录没有历史会话"),
    (
        "msg.checkpoints_disabled",
        "文件检查点已禁用（features.checkpoints）",
    ),
    ("msg.invalid_turn", "无效的轮次编号：{arg}"),
    (
        "msg.restored_header",
        "已恢复第 {turn} 轮检查点（{count} 个文件）：",
    ),
    ("msg.restored_action", "已恢复"),
    ("msg.deleted_action", "已删除（该轮新建）"),
    ("msg.restore_failed", "恢复失败：{error}"),
    (
        "msg.no_file_checkpoints",
        "还没有文件检查点。每轮的 edit/write 改动都会快照；用 /checkpoints restore <turn> 回滚文件。",
    ),
    (
        "msg.checkpoints_header",
        "**文件检查点**（每轮前的文件状态）",
    ),
    (
        "msg.checkpoint_row",
        "- 第 {turn} 轮：{count} 个文件——{names}{more}",
    ),
    ("msg.more_files", "，还有 {count} 个"),
    (
        "msg.checkpoints_footer",
        "\n用 `/checkpoints restore <turn>` 恢复。",
    ),
    ("msg.memory_disabled", "持久记忆已禁用（features.memory）"),
    ("msg.no_memory_named", "没有名为 {name} 的记忆"),
    ("msg.memory_index_failed", "索引重建失败：{error}"),
    ("msg.memory_deleted", "记忆 {name} 已删除。"),
    ("msg.memory_delete_failed", "无法删除 {name}：{error}"),
    (
        "msg.memory_forget_usage",
        "用法：/memory forget <name> [project|user]",
    ),
    (
        "msg.memory_scope_unknown",
        "未知作用域 {scope}（project|user）",
    ),
    ("msg.memory_scope_header", "**{scope} 记忆**（`{dir}`）"),
    (
        "msg.memory_footer",
        "阅读文件查看详情；`/memory edit <name>` 编辑，`/memory forget <name>` 删除。作用域：project（本仓库，worktree 共享）与 user（跨项目）。agent 通过 memory 工具管理这些记忆。",
    ),
    (
        "msg.no_memories",
        "还没有记忆。agent 会用 memory 工具保存可复用的事实（偏好、约定），并注入到之后的每个会话。",
    ),
    ("msg.usage_search", "用法：/search <query>"),
    ("msg.no_sessions_match", "没有匹配 {query} 的会话"),
    (
        "msg.sessions_matching",
        "**匹配 {query} 的会话**（{count}）",
    ),
    ("msg.search_row", "### {title} — {time}（{count} 处匹配）"),
    (
        "msg.search_footer",
        "用 `/resume` 打开，或在命令行用 `tack --session <id>`。",
    ),
    ("msg.cron_disabled", "定时提示词已禁用（features.cron）"),
    (
        "msg.cron_usage",
        "用法：/cron add \"every 10m\" <prompt> 或 /cron add \"*/5 * * * *\" <prompt>",
    ),
    (
        "msg.cron_scheduled",
        "已创建定时任务 {id}（{schedule}）：{prompt}",
    ),
    ("msg.cron_removed", "已删除任务 {id}。"),
    ("msg.cron_no_job", "没有任务 {id}"),
    ("msg.cron_paused", "已暂停任务 {id}。"),
    ("msg.cron_resumed", "已恢复任务 {id}。"),
    (
        "msg.cron_none",
        "没有定时任务。用 /cron add \"every 10m\" <prompt> 或 5 段 cron 表达式添加。任务会触发到本会话；tack 关闭期间错过的运行会在启动时补触发一次。",
    ),
    ("msg.cron_header", "**定时任务**（`cron.json`）"),
    ("msg.cron_paused_state", "已暂停"),
    (
        "msg.cron_unknown",
        "未知的 /cron 子命令 {cmd}（add|remove|pause|resume）",
    ),
    ("msg.cron_fired", "定时任务触发（{schedule}）：{prompt}"),
    (
        "msg.trace_none",
        "没有 trace 事件（用 TACK_TRACE_FILE=1 或设置 observability.enabled 开启）",
    ),
    ("msg.trace_header", "**最近的 trace 事件**"),
    ("msg.rewind_detail", "+{chars} 字符的回复"),
    ("msg.no_checkpoints", "还没有检查点"),
    (
        "msg.rewind_title",
        "回退到检查点（回车跳转；被放弃的分支会先总结）",
    ),
    ("msg.nothing_to_fork", "没有可分叉的位置"),
    ("msg.fork_title", "从消息分叉"),
    ("msg.tree_empty", "会话树为空"),
    ("msg.ext_editor_failed", "外部编辑器失败：{error}"),
    ("msg.ext_readback_failed", "读回编辑器内容失败：{error}"),
    ("msg.editor_exited", "编辑器退出，状态：{status}"),
    ("msg.editor_launch_failed", "无法启动编辑器：{error}"),
    ("msg.setting_save_failed", "保存设置失败：{error}"),
    ("msg.setting_saved", "{key} = {value}（已保存）"),
    ("msg.session_not_persisted", "会话尚未持久化"),
    (
        "msg.share_needs_token",
        "/share 需要 GITHUB_TOKEN（私密 gist）",
    ),
    ("msg.share_read_failed", "无法读取会话文件"),
    ("msg.sharing", "正在通过 gist 分享…"),
    ("msg.shared", "已分享：{url}（链接已复制）"),
    ("msg.share_failed", "分享失败：{error}"),
    ("msg.usage_import", "用法：/import <session.jsonl>"),
    ("msg.session_imported", "会话已导入"),
    ("msg.import_failed", "导入失败：{error}"),
    ("msg.session_cloned", "会话已复制"),
    ("msg.clone_failed", "复制失败：{error}"),
    ("msg.usage_name", "用法：/name <name>"),
    ("msg.session_named", "会话已命名为 {name}"),
    ("msg.failed", "失败：{error}"),
    ("msg.copied_selection", "已复制选区"),
    ("msg.copied_last", "已复制最后一条助手消息"),
    ("msg.nothing_to_copy", "没有可复制的内容"),
    ("msg.copy_failed", "复制失败：{error}"),
    ("msg.debug_written", "✓ 调试日志已写入：{path}"),
    ("msg.debug_failed", "调试日志写入失败：{error}"),
    ("msg.exported", "已导出到 {path}"),
    ("msg.export_failed", "导出失败：{error}"),
    ("msg.reloaded", "已重新加载设置/技能/提示词模板"),
    ("msg.scoped_saved", "循环模型列表已保存"),
    ("msg.scoped_save_failed", "保存 scopedModels 失败：{error}"),
    ("msg.label_failed", "设置标签失败：{error}"),
    ("msg.default_model_saved", "默认模型已保存：{ref}"),
    ("msg.default_model_save_failed", "保存默认模型失败：{error}"),
    ("msg.thinking_expanded", "思考块已展开"),
    ("msg.thinking_hidden", "思考块已隐藏"),
    ("msg.thinking_collapsed", "思考块已折叠"),
    ("msg.no_queued", "没有排队消息"),
    ("msg.model_fallback", "模型回退：{from} → {to}（{reason}）"),
    (
        "msg.cache_miss",
        "缓存未命中：{count} 个输入 token 未命中缓存",
    ),
    (
        "msg.provider_locked",
        "提供商已被 managed settings 锁定为 {locked}",
    ),
    (
        "msg.model_locked",
        "模型已被 managed settings 锁定为 {locked}",
    ),
    ("msg.model_record_failed", "记录模型切换失败：{error}"),
    ("msg.model_set", "模型：{ref}"),
    ("msg.session_resumed", "会话已恢复"),
    ("msg.resume_failed", "恢复会话失败：{error}"),
    ("msg.forked", "已分叉到该位置"),
    ("msg.fork_failed", "分叉失败：{error}"),
    (
        "msg.theme_saved",
        "主题：{value}（已保存）。/help 查看命令——玩得开心！",
    ),
    ("msg.theme_save_failed", "保存主题失败：{error}"),
    (
        "msg.provider_registered",
        "提供商已注册：{id}（/model {id}/…）",
    ),
    (
        "msg.welcome",
        "tack {version}——{model}（{provider}）。/help 查看命令，/model 切换模型。",
    ),
    (
        "msg.onboarding_no_credentials",
        "**未找到任何提供商凭据——先来配置一个。**\n\n\
         1. 看看有哪些可用：`/providers`（或在终端里 `tack providers`）——列出每个提供商的模型数量和启用方式\n\
         2. 登录：`tack login --provider <id>`（anthropic/github-copilot/openrouter 等走 OAuth；其他从 stdin 读 API key）\n\
         3. 或设置环境变量，如 `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`GEMINI_API_KEY`\n\
         4. 浏览并选择模型：这里用 `/model`，或在终端用 `tack models [pattern]`\n\n\
         自定义端点写在 `~/.tack/agent/models.json`（见 docs/configuration.md）。",
    ),
    // -- 登录 / 登出 --
    ("msg.no_credentials", "没有已保存的凭据"),
    ("msg.logout_title", "登出提供商"),
    ("msg.logged_out", "已登出 {provider}"),
    ("msg.no_credential_for", "{provider} 没有已保存的凭据"),
    ("msg.logout_failed", "登出失败：{error}"),
    ("msg.usage_login", "用法：/login <provider>"),
    (
        "msg.no_oauth_flow",
        "{provider} 没有 OAuth 流程；请用以下方式保存 key：tack login --provider {provider} --api-key …",
    ),
    (
        "msg.oauth_starting",
        "正在为 {provider} 发起 OAuth 登录——请查看浏览器",
    ),
    // -- 压缩 --
    ("msg.compact_running", "压缩会自动进行；请等待当前运行结束"),
    ("msg.nav_running", "请等待当前响应或压缩结束后再浏览会话树"),
    ("msg.nothing_to_compact", "没有可压缩的内容（会话太短）"),
    ("msg.compact_auth_failed", "压缩认证失败：{error}"),
    ("msg.compact_persist_failed", "压缩结果写入失败：{error}"),
    ("msg.compact_failed", "压缩失败：{error}"),
    // -- bash / 分支总结 --
    ("msg.bash_persist_failed", "bash 结果写入会话失败：{error}"),
    ("msg.branch_auth_failed", "分支总结认证失败：{error}"),
    ("msg.branch_summarizing", "正在总结被放弃的分支…"),
    ("msg.branch_record_failed", "记录分支总结失败：{error}"),
    ("msg.branch_failed", "分支总结失败：{error}"),
    // -- /models 目录 --
    ("msg.catalog_refreshing", "正在刷新模型目录…"),
    (
        "msg.catalog_refreshed",
        "模型目录已从 @earendil-works/pi-ai {version} 刷新：{providers} 个提供商、{models} 个模型——立即生效",
    ),
    ("msg.catalog_refresh_failed", "模型目录刷新失败：{error}"),
    (
        "msg.catalog_reset",
        "缓存的模型目录已删除——重启后恢复使用内置目录",
    ),
    (
        "msg.catalog_refreshed_startup",
        "模型目录已从 @earendil-works/pi-ai {version} 刷新：{providers} 个提供商、{models} 个模型",
    ),
    (
        "catalog.status_cached",
        "**模型目录**：已从 `@earendil-works/pi-ai` {version} 刷新——{providers} 个提供商、{models} 个模型\n\n缓存于 `{path}`。`/models refresh` 拉取最新；`/models reset` 恢复内置目录。",
    ),
    (
        "catalog.status_embedded",
        "**模型目录**：内置（编译进本二进制，当前有 {count} 个提供商覆盖生效）。\n\n`/models refresh` 从已发布的 `@earendil-works/pi-ai` npm 包拉取最新目录。在 settings.json 里设置 `\"modelCatalogRefresh\": true` 可开启启动时自动刷新。",
    ),
    // -- /providers --
    ("providers.header", "**提供商**——✓ = 凭据可用\n\n"),
    (
        "providers.login_hint",
        "——用 `tack login --provider {id}` 或设置 `{env}`",
    ),
    ("providers.models_count", "{count} 个模型"),
    ("providers.custom_suffix", "（自定义，models.json）"),
    (
        "providers.footer",
        "\n{ready} 个提供商就绪。用 `/model` 浏览模型，`tack models [pattern]` 查看完整目录，`/models refresh` 刷新目录。",
    ),
    // -- /session --
    (
        "session.body",
        "**会话**\n- 文件：`{file}`\n- id：`{id}`\n- 上下文：{tokens} tokens（占 {window}k 的 {pct}%）\n- 总计：↑{input} ↓{output} R{cr} W{cw} — ${cost}\n",
    ),
    // -- /cost --
    ("cost.header", "**用量与费用**\n\n"),
    ("cost.no_usage", "（还没有用量记录）\n"),
    (
        "cost.table_header",
        "| 模型 | 输入 | 输出 | 缓存读 | 缓存写 | 费用 |\n|---|---|---|---|---|---|\n",
    ),
    (
        "cost.total",
        "\n**总计**：{tokens} tokens（输入 {input} / 输出 {output}）——**${cost}**\n",
    ),
    (
        "cost.budget",
        "\n预算：{used} / {budget} tokens（{pct}%）{extra}\n",
    ),
    ("cost.budget_exceeded", "——**已超支**"),
    // -- /todo --
    ("todo.cleared", "已清空 {count} 条任务"),
    ("todo.header", "**任务列表**\n\n"),
    ("todo.empty", "（空——由 agent 通过 todo 工具维护）\n"),
    ("todo.done_count", "\n已完成 {done}/{total}\n"),
    ("panel.todos", " 任务"),
    ("panel.ext_focused", "  [已聚焦 · ↑↓ 移动 · 回车选择 · esc]"),
    // -- /context --
    ("ctx.header", "**上下文用量**\n\n"),
    ("ctx.table_header", "| 分段 | ~tokens |\n|---|---|\n"),
    ("ctx.seg_system", "系统提示"),
    ("ctx.seg_tools", "工具 schema"),
    ("ctx.seg_user", "用户消息"),
    ("ctx.seg_assistant", "助手消息"),
    ("ctx.seg_thinking", "思考"),
    ("ctx.seg_tool_results", "工具结果"),
    ("ctx.sum", "| **合计（估算）** | **{total}** |\n"),
    (
        "ctx.provider_count",
        "\n提供商上次计数：{usage} tokens（此后估算 +~{trailing}）→ **共 {total}**\n",
    ),
    (
        "ctx.no_usage",
        "\n还没有提供商用量数据——以上均为 chars/4 估算。\n",
    ),
    ("ctx.window", "\n窗口：{used} / {window} tokens（{pct}%）\n"),
    (
        "ctx.autocompaction",
        "自动压缩约在 {trigger} tokens 触发（预留 {reserve}）；剩余 {remaining}。\n",
    ),
    ("ctx.by_size", "\n按体积排序的工具结果：\n"),
    ("ctx.by_size_row", "- {name}：约 {tokens} tokens\n"),
    (
        "ctx.cache",
        "\n提示缓存（会话累计）：读取 {read} tokens，写入 {write} · 命中率 {pct}%\n",
    ),
    (
        "ctx.budgets",
        "\n历史优化：工具结果上限 {cap} 字符 · 微压缩 {micro} 字符（最小收益门限 {savings}）· 重复读取掩码 {dedup} · 规则文件上限 {rules} 字符 · 目标复述 {recite}\n",
    ),
    ("ctx.flag_on", "开"),
    ("ctx.flag_off", "关"),
    // -- /rules --
    ("rules.context_files", "**上下文文件**\n"),
    ("rules.none", "-（无）\n"),
    ("rules.skills", "\n**技能**\n"),
    ("rules.file_row", "- `{path}`（{chars} 字符）\n"),
    // -- /trust 对话框 --
    ("trust.title", "信任项目文件夹？{path}"),
    ("trust.trust", "信任"),
    (
        "trust.trust_desc",
        "持久生效：加载本项目的 .pi 设置/资源和 MCP 服务器",
    ),
    ("trust.parent", "信任上级文件夹（{path}）"),
    ("trust.parent_desc", "持久生效：信任上级目录下的所有内容"),
    ("trust.trust_session", "信任（仅本次会话）"),
    ("trust.distrust", "不信任"),
    ("trust.distrust_desc", "持久生效：忽略项目 .pi 资源"),
    ("trust.distrust_session", "不信任（仅本次会话）"),
    (
        "trust.now_trusted",
        "项目已信任——.pi 设置/资源生效（项目 MCP 服务器下次运行时生效）",
    ),
    ("trust.now_untrusted", "项目未信任——项目 .pi 资源已忽略"),
    // -- 首次运行向导 --
    (
        "firstrun.title",
        "欢迎使用 tack！请选择主题——检测到系统外观：{detected}（随时可用 /theme 更改）",
    ),
    ("firstrun.dark", "深色"),
    ("firstrun.dark_desc", "深色背景的终端"),
    ("firstrun.light", "浅色"),
    ("firstrun.light_desc", "浅色背景的终端"),
    ("firstrun.auto", "自动（light/dark）"),
    ("firstrun.auto_desc", "通过 OSC 11 检测终端背景"),
    // -- /mcp --
    ("mcp.none_configured", "未配置 MCP 服务器（mcp.json）"),
    ("mcp.connecting", "正在连接 MCP 服务器…"),
    ("mcp.none_connected", "无法连接任何 MCP 服务器"),
    ("mcp.empty", "已连接的 MCP 服务器没有暴露资源或提示词"),
    ("mcp.title", "MCP 资源与提示词（插入编辑器）"),
    ("mcp.resource_inserted", "资源已插入：{id}"),
    ("mcp.read_failed", "read_resource 失败：{error}"),
    ("mcp.prompt_inserted", "提示词已插入：{id}"),
    ("mcp.prompt_failed", "get_prompt 失败：{error}"),
    ("mcp.image", "[图片]"),
    ("mcp.content", "[内容]"),
    // -- 自动补全 --
    ("auto.prompt_template", "提示词模板"),
    // -- mermaid 说明 --
    ("mermaid.full_resolution", "（完整分辨率：{path}）"),
    // -- 底栏 --
    ("footer.thinking_off", "关"),
    ("footer.update_available", "↑ v{version}"),
    // -- 更新检查提示 --
    (
        "msg.update_available",
        "发现新版本 tack：v{version}（当前 v{current}）——运行 `tack update` 升级。",
    ),
    // -- 历史反向搜索（ctrl+r） --
    (
        "search.history_hint",
        "输入以搜索历史 · ctrl+r/↑ 更旧 · ↓ 更新 · 回车接受 · Esc 取消",
    ),
    ("search.history_no_match", "无匹配"),
    // -- 桌面通知 --
    ("notify.permission_title", "需要权限确认"),
    ("notify.run_done", "agent 运行完成（{model}）"),
    ("notify.run_error", "agent 运行出错（{model}）"),
    ("notify.bg_task_title", "后台任务{status}"),
    // -- 全屏回到底部悬浮按钮 --
    ("fs.back_to_bottom", "↓ 回到底部 · End"),
];

/// Translate `key`, substituting `{name}` placeholders from `args`.
/// Lookup tables as hash maps, built once — the render loop calls `tr`
/// several times per frame and a linear scan over the sectioned tables
/// showed up in profiles. Missing keys still fall back to English and then
/// to the key itself, handled by `t`.
fn table_map(lang: Lang) -> &'static std::collections::HashMap<&'static str, &'static str> {
    use std::sync::OnceLock;
    static EN_MAP: OnceLock<std::collections::HashMap<&'static str, &'static str>> =
        OnceLock::new();
    static ZH_MAP: OnceLock<std::collections::HashMap<&'static str, &'static str>> =
        OnceLock::new();
    match lang {
        Lang::En => EN_MAP.get_or_init(|| EN.iter().copied().collect()),
        Lang::Zh => ZH_MAP.get_or_init(|| ZH.iter().copied().collect()),
    }
}

pub fn t(lang: Lang, key: &str, args: &[(&str, &str)]) -> String {
    let template = table_map(lang)
        .get(key)
        .copied()
        .or_else(|| table_map(Lang::En).get(key).copied())
        .unwrap_or(key);
    let mut out = template.to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn zh_translation_and_fallback() {
        assert_eq!(
            t(Lang::Zh, "notice.mode", &[("mode", "ask")]),
            "权限模式：ask"
        );
        assert_eq!(
            t(Lang::Zh, "permission.title", &[("what", "bash")]),
            "允许 bash？"
        );
        // Unknown key degrades to the key itself.
        assert_eq!(t(Lang::En, "no.such.key", &[]), "no.such.key");
    }

    #[test]
    fn lang_resolution() {
        assert_eq!(Lang::resolve(Some("zh")), Lang::Zh);
        assert_eq!(Lang::resolve(Some("en")), Lang::En);
        assert_eq!(Lang::resolve(Some("bogus")), Lang::En);
        // Case and locale suffixes must not matter.
        assert_eq!(Lang::resolve(Some("ZH")), Lang::Zh);
        assert_eq!(Lang::resolve(Some("zh-Hans")), Lang::Zh);
        assert_eq!(Lang::resolve(Some(" zh_CN ")), Lang::Zh);
    }

    #[test]
    fn lang_resolution_from_env() {
        // No setting: LANG decides.
        assert_eq!(Lang::resolve_with(None, Some("zh_CN.UTF-8")), Lang::Zh);
        assert_eq!(Lang::resolve_with(None, Some("en_US.UTF-8")), Lang::En);
        assert_eq!(Lang::resolve_with(None, None), Lang::En);
        // Setting wins over LANG.
        assert_eq!(
            Lang::resolve_with(Some("en"), Some("zh_CN.UTF-8")),
            Lang::En
        );
        assert_eq!(
            Lang::resolve_with(Some("zh"), Some("en_US.UTF-8")),
            Lang::Zh
        );
    }

    /// Every key exists in both tables, exactly once per table — a missing
    /// zh row would silently degrade to English.
    #[test]
    fn tables_are_paired() {
        let en_keys: std::collections::HashSet<&str> = EN.iter().map(|(k, _)| *k).collect();
        let zh_keys: std::collections::HashSet<&str> = ZH.iter().map(|(k, _)| *k).collect();
        assert_eq!(en_keys.len(), EN.len(), "duplicate keys in EN");
        assert_eq!(zh_keys.len(), ZH.len(), "duplicate keys in ZH");
        let missing_zh: Vec<&&str> = en_keys.difference(&zh_keys).collect();
        let missing_en: Vec<&&str> = zh_keys.difference(&en_keys).collect();
        assert!(
            missing_zh.is_empty() && missing_en.is_empty(),
            "unpaired keys — missing zh: {missing_zh:?}, missing en: {missing_en:?}"
        );
    }

    /// `{placeholder}` sets must match across languages, otherwise one
    /// language renders raw `{name}` while the other substitutes.
    #[test]
    fn placeholders_match_across_languages() {
        fn placeholders(template: &str) -> Vec<String> {
            let mut out = Vec::new();
            let mut rest = template;
            while let Some(start) = rest.find('{') {
                let Some(end) = rest[start..].find('}') else {
                    break;
                };
                out.push(rest[start + 1..start + end].to_string());
                rest = &rest[start + end + 1..];
            }
            out.sort();
            out
        }
        for (key, en_template) in EN {
            let zh_template = ZH
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| *v)
                .expect("paired by tables_are_paired");
            assert_eq!(
                placeholders(en_template),
                placeholders(zh_template),
                "placeholder mismatch for {key}"
            );
        }
    }

    /// The global-language convenience API used by components without a
    /// threaded `Lang`.
    #[test]
    fn global_current_language() {
        let _guard = TEST_LANG_LOCK.lock().unwrap();
        set_current(Lang::Zh);
        assert_eq!(tr("permission.yes"), "允许");
        assert_eq!(trf("chat.aborted", &[]), "◦ 已中止");
        assert_eq!(
            trf("status.retrying", &[("attempt", "2"), ("max", "5")]),
            "正在重试（第 2/5 次）"
        );
        set_current(Lang::En);
        assert_eq!(tr("permission.yes"), "Yes");
    }
}
