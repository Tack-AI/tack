//! Hook configuration model: Claude-Code-compatible declarations with
//! backward-compatible parsing of tack's original flat format.
//!
//! Claude format (settings.json `hooks` key, or a bundle `hooks.json`):
//!
//! ```json
//! {
//!   "hooks": {
//!     "PreToolUse": [
//!       { "matcher": "bash|edit",
//!         "hooks": [ { "type": "command", "command": "check.sh", "timeout": 30 } ] }
//!     ],
//!     "UserPromptSubmit": [
//!       { "hooks": [ { "type": "prompt", "prompt": "Is this prompt safe?" } ] }
//!     ]
//!   }
//! }
//! ```
//!
//! Legacy tack flat entries (`{ "matcher": "bash", "command": "check.sh" }`)
//! are normalized to a one-handler group, so old settings keep working.

use serde_json::Value;

/// Lifecycle events hooks can attach to (Claude's set plus tack's
/// `Notification`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    /// Tool call ended in an error (ZCode/Claude split: PostToolUse fires
    /// on success AND failure with `is_error`, PostToolUseFailure only on
    /// failure).
    PostToolUseFailure,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    SubagentStart,
    SubagentStop,
    Stop,
    Interrupt,
    /// tack extension: agent wants user attention (Claude also has this).
    Notification,
}

impl HookEvent {
    /// Wire name as it appears in config and `hook_event_name` input fields.
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::Stop => "Stop",
            HookEvent::Interrupt => "Interrupt",
            HookEvent::Notification => "Notification",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "PreToolUse" => HookEvent::PreToolUse,
            "PermissionRequest" => HookEvent::PermissionRequest,
            "PostToolUse" => HookEvent::PostToolUse,
            "PostToolUseFailure" => HookEvent::PostToolUseFailure,
            "PreCompact" => HookEvent::PreCompact,
            "PostCompact" => HookEvent::PostCompact,
            "SessionStart" => HookEvent::SessionStart,
            "SessionEnd" => HookEvent::SessionEnd,
            "UserPromptSubmit" => HookEvent::UserPromptSubmit,
            "SubagentStart" => HookEvent::SubagentStart,
            "SubagentStop" => HookEvent::SubagentStop,
            "Stop" => HookEvent::Stop,
            "Interrupt" => HookEvent::Interrupt,
            "Notification" => HookEvent::Notification,
            _ => return None,
        })
    }

    /// All events, in a stable order.
    pub fn all() -> &'static [HookEvent] {
        &[
            HookEvent::PreToolUse,
            HookEvent::PermissionRequest,
            HookEvent::PostToolUse,
            HookEvent::PreCompact,
            HookEvent::PostCompact,
            HookEvent::SessionStart,
            HookEvent::SessionEnd,
            HookEvent::UserPromptSubmit,
            HookEvent::SubagentStart,
            HookEvent::SubagentStop,
            HookEvent::Stop,
            HookEvent::Interrupt,
            HookEvent::Notification,
        ]
    }
}

/// One hook handler inside a matcher group.
#[derive(Clone, Debug, PartialEq)]
pub enum HookHandler {
    /// Run a shell command; hook input JSON on stdin, verdict JSON on
    /// stdout. Exit 2 = block (stderr is the reason).
    Command {
        command: String,
        /// Per-hook timeout in seconds (default 60).
        timeout_sec: Option<u64>,
        /// Fire-and-forget: the verdict is ignored.
        run_async: bool,
        status_message: Option<String>,
    },
    /// Ask an LLM to evaluate the hook input and return a verdict JSON
    /// (Claude prompt-hook semantics). `prompt` is the evaluation
    /// instruction; the hook input JSON is appended.
    Prompt {
        prompt: String,
        /// `provider/id`; defaults to the session model.
        model: Option<String>,
        timeout_sec: Option<u64>,
    },
    /// Like `Prompt` but the evaluator may use read-only tools
    /// (read/grep/find/ls) over multiple turns before answering.
    Agent {
        prompt: String,
        model: Option<String>,
        timeout_sec: Option<u64>,
    },
}

/// A matcher group: one optional matcher plus its handlers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HookGroup {
    pub matcher: Option<String>,
    pub hooks: Vec<HookHandler>,
}

/// Parsed hook configuration: event → matcher groups.
#[derive(Clone, Debug, Default)]
pub struct HookConfig {
    pub groups: Vec<(HookEvent, HookGroup)>,
}

impl HookConfig {
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|(_, g)| g.hooks.is_empty())
    }

    pub fn groups_for(&self, event: HookEvent) -> Vec<&HookGroup> {
        self.groups
            .iter()
            .filter(|(e, _)| *e == event)
            .map(|(_, g)| g)
            .collect()
    }

    /// Groups for one event, cloned out (callers that spawn need owned).
    pub fn take_groups(&self, event: HookEvent) -> Vec<HookGroup> {
        self.groups
            .iter()
            .filter(|(e, _)| *e == event)
            .map(|(_, g)| g.clone())
            .collect()
    }

    /// Merge another config (bundle hooks) after this one.
    pub fn extend(&mut self, other: HookConfig) {
        self.groups.extend(other.groups);
    }
}

/// Parse one handler object (`{"type": "command", ...}`).
fn parse_handler(raw: &Value) -> Option<HookHandler> {
    let kind = raw.get("type").and_then(Value::as_str).unwrap_or("command");
    let timeout_sec = raw
        .get("timeout")
        .or_else(|| raw.get("timeoutSec"))
        .and_then(Value::as_u64);
    match kind {
        "command" => {
            let command = raw.get("command").and_then(Value::as_str)?.to_string();
            if command.trim().is_empty() {
                return None;
            }
            Some(HookHandler::Command {
                command,
                timeout_sec,
                run_async: raw.get("async").and_then(Value::as_bool).unwrap_or(false),
                status_message: raw
                    .get("statusMessage")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        "prompt" => {
            let prompt = raw
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_EVAL_PROMPT)
                .to_string();
            Some(HookHandler::Prompt {
                prompt,
                model: raw.get("model").and_then(Value::as_str).map(str::to_string),
                timeout_sec,
            })
        }
        "agent" => {
            let prompt = raw
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_EVAL_PROMPT)
                .to_string();
            Some(HookHandler::Agent {
                prompt,
                model: raw.get("model").and_then(Value::as_str).map(str::to_string),
                timeout_sec,
            })
        }
        other => {
            tracing::warn!("unknown hook handler type {other:?}; ignoring");
            None
        }
    }
}

/// Default instruction for prompt/agent handlers without an explicit prompt.
pub const DEFAULT_EVAL_PROMPT: &str =
    "Evaluate whether this operation should be allowed to proceed.";

/// Parse one matcher-group entry, accepting both the Claude nested shape
/// (`{matcher, hooks: [...]}`) and the legacy tack flat shape
/// (`{matcher, command}`).
fn parse_group(raw: &Value) -> HookGroup {
    let matcher = raw
        .get("matcher")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(handlers) = raw.get("hooks").and_then(Value::as_array) {
        let hooks = handlers.iter().filter_map(parse_handler).collect();
        return HookGroup { matcher, hooks };
    }
    // Legacy flat: {matcher, command} → one command handler.
    if let Some(command) = raw.get("command").and_then(Value::as_str)
        && !command.trim().is_empty()
    {
        return HookGroup {
            matcher,
            hooks: vec![HookHandler::Command {
                command: command.to_string(),
                timeout_sec: raw.get("timeout").and_then(Value::as_u64),
                run_async: false,
                status_message: None,
            }],
        };
    }
    HookGroup {
        matcher,
        hooks: Vec::new(),
    }
}

/// Parse a `hooks` object (`{"PreToolUse": [...], ...}`) into a HookConfig.
/// Unknown event names are ignored with a warning.
pub fn parse_hooks(raw: Option<&Value>) -> HookConfig {
    let mut config = HookConfig::default();
    let Some(raw) = raw.and_then(Value::as_object) else {
        return config;
    };
    for (name, entries) in raw {
        let Some(event) = HookEvent::parse(name) else {
            tracing::warn!("unknown hook event {name:?}; ignoring");
            continue;
        };
        let Some(entries) = entries.as_array() else {
            tracing::warn!("hooks.{name} is not an array; ignoring");
            continue;
        };
        for entry in entries {
            let group = parse_group(entry);
            if !group.hooks.is_empty() {
                config.groups.push((event, group));
            }
        }
    }
    config
}

/// Parse a Claude-format hooks file (`{"description": ..., "hooks": {...}}`
/// or a bare `{"PreToolUse": [...]}` object) — bundle `hooks.json` support.
pub fn parse_hooks_file(content: &str) -> Result<HookConfig, String> {
    let value: Value = serde_json::from_str(content).map_err(|e| format!("bad hooks JSON: {e}"))?;
    let hooks = value.get("hooks").unwrap_or(&value);
    Ok(parse_hooks(Some(hooks)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn claude_nested_format_parses() {
        let raw = serde_json::json!({
            "PreToolUse": [
                { "matcher": "bash|edit",
                  "hooks": [
                    { "type": "command", "command": "check.sh", "timeout": 30 },
                    { "type": "prompt", "prompt": "safe?" }
                  ] }
            ],
            "SessionStart": [ { "hooks": [ { "type": "command", "command": "ctx.sh" } ] } ]
        });
        let cfg = parse_hooks(Some(&raw));
        let pre = cfg.groups_for(HookEvent::PreToolUse);
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].matcher.as_deref(), Some("bash|edit"));
        assert_eq!(pre[0].hooks.len(), 2);
        assert!(matches!(
            &pre[0].hooks[0],
            HookHandler::Command {
                timeout_sec: Some(30),
                ..
            }
        ));
        assert!(matches!(&pre[0].hooks[1], HookHandler::Prompt { .. }));
        assert_eq!(cfg.groups_for(HookEvent::SessionStart).len(), 1);
    }

    #[test]
    fn legacy_flat_format_still_parses() {
        let raw = serde_json::json!({
            "PreToolUse": [
                { "matcher": "bash", "command": "check.sh" },
                { "command": "always.sh" },
                { "matcher": "x" }
            ],
            "Stop": [ { "command": "notify.sh" } ]
        });
        let cfg = parse_hooks(Some(&raw));
        let pre = cfg.groups_for(HookEvent::PreToolUse);
        assert_eq!(pre.len(), 2);
        assert!(matches!(&pre[0].hooks[0], HookHandler::Command { .. }));
        assert_eq!(cfg.groups_for(HookEvent::Stop).len(), 1);
    }

    #[test]
    fn hooks_file_accepts_wrapped_and_bare_shapes() {
        let wrapped = r#"{"description": "d", "hooks": {"Stop": [{"command": "a.sh"}]}}"#;
        let cfg = parse_hooks_file(wrapped).unwrap();
        assert_eq!(cfg.groups_for(HookEvent::Stop).len(), 1);
        let bare = r#"{"Stop": [{"command": "a.sh"}]}"#;
        let cfg = parse_hooks_file(bare).unwrap();
        assert_eq!(cfg.groups_for(HookEvent::Stop).len(), 1);
    }

    #[test]
    fn subagent_start_event_parses_with_matcher() {
        let raw = serde_json::json!({
            "SubagentStart": [
                { "matcher": "reviewer|explore",
                  "hooks": [ { "type": "command", "command": "gate.sh" } ] }
            ]
        });
        let cfg = parse_hooks(Some(&raw));
        let groups = cfg.groups_for(HookEvent::SubagentStart);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].matcher.as_deref(), Some("reviewer|explore"));
        assert!(matches!(&groups[0].hooks[0], HookHandler::Command { .. }));
    }

    #[test]
    fn unknown_events_and_handler_types_are_skipped() {
        let raw = serde_json::json!({
            "BogusEvent": [ { "command": "x.sh" } ],
            "Stop": [ { "hooks": [ { "type": "teleport", "command": "x.sh" } ] } ]
        });
        let cfg = parse_hooks(Some(&raw));
        assert!(cfg.is_empty());
    }
}
