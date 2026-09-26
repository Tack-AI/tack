//! settings.json / bundle hooks: Claude-Code-compatible lifecycle hooks
//! (shell commands or LLM evaluations) around agent events — no extension
//! code required.
//!
//! ```json
//! "hooks": {
//!   "PreToolUse": [
//!     { "matcher": "bash|edit",
//!       "hooks": [ { "type": "command", "command": "check.sh", "timeout": 30 } ] }
//!   ],
//!   "UserPromptSubmit": [
//!     { "hooks": [ { "type": "prompt", "prompt": "Is this request safe?" } ] }
//!   ],
//!   "SessionStart":  [ { "hooks": [ { "type": "command", "command": "cat .pi/context.md" } ] } ],
//!   "Stop":          [ { "command": "notify.sh" } ]
//! }
//! ```
//!
//! - Events: PreToolUse, PermissionRequest, PostToolUse, PostToolUseFailure,
//!   PreCompact, PostCompact, SessionStart, SessionEnd, UserPromptSubmit,
//!   SubagentStart, SubagentStop, Stop, Interrupt, Notification. PostToolUse
//!   fires on success AND failure (`is_error` field); PostToolUseFailure
//!   only on failure (ZCode event split).
//! - Matchers: exact tool names with `|` alternation, `*`/empty for all,
//!   otherwise a regex.
//! - Handlers: `command` (JSON on stdin, verdict JSON on stdout, exit 2 =
//!   block), `prompt` / `agent` (LLM evaluation answering the same verdict
//!   JSON; `agent` may use read-only tools). Legacy flat entries
//!   (`{matcher, command}`) still parse.
//! - Verdicts: `decision: "block"` + `reason`; `hookSpecificOutput` with
//!   `permissionDecision` (allow/deny/ask — TUI permission hooks honor it),
//!   `updatedInput` (PreToolUse argument rewrite, merged over the original),
//!   `additionalContext` (injected context); universal `continue: false` /
//!   `systemMessage`. Hook failures are fail-open (logged, ignored).

pub mod agent_bridge;
pub mod config;
pub mod engine;
pub mod evaluate;

pub use agent_bridge::{HookDecisions, HookSessionInfo, ShellHooks};
pub use config::{HookConfig, HookEvent, HookGroup, HookHandler, parse_hooks, parse_hooks_file};
pub use engine::{HookEngine, HookOutcome, HookPermission, HookVerdict, matches_matcher};
pub use evaluate::{HookLlmEvaluator, LlmEvaluator};

/// Managed (enterprise) hooks live in a separate file next to settings,
/// administered out-of-band. `managedHooksOnly: true` in settings ignores
/// all user/project/session hooks while keeping managed ones (Codex's
/// `allow_managed_hooks_only` semantics).
pub const MANAGED_HOOKS_FILE: &str = "managed-hooks.json";

/// Load the effective hook config: settings `hooks.*` plus managed hooks,
/// honoring `managedHooksOnly`.
pub fn load_hooks_config(
    settings: &crate::settings::Settings,
    agent_dir: &std::path::Path,
) -> HookConfig {
    let managed = std::fs::read_to_string(agent_dir.join(MANAGED_HOOKS_FILE))
        .ok()
        .and_then(|content| match parse_hooks_file(&content) {
            Ok(config) => Some(config),
            Err(e) => {
                tracing::warn!("ignoring bad {MANAGED_HOOKS_FILE}: {e}");
                None
            }
        })
        .unwrap_or_default();
    let managed_only = settings
        .raw()
        .get("managedHooksOnly")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if managed_only {
        return managed;
    }
    let mut config = parse_hooks(settings.raw().get("hooks"));
    config.extend(managed);
    config
}
