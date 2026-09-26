//! Hook execution engine: runs matcher-selected handlers (shell commands,
//! LLM evaluations) with the Claude hook wire contract and merges their
//! verdicts.
//!
//! Wire contract (Claude Code compatible):
//! - Input: one JSON object on stdin — `{session_id, cwd, hook_event_name,
//!   ...event fields}` (snake_case).
//! - Output (exit 0): stdout may be a JSON object with
//!   `{continue, stopReason, systemMessage, decision, reason,
//!   hookSpecificOutput: {permissionDecision, updatedInput,
//!   additionalContext}}` (unknown fields — e.g. Claude's `suppressOutput` —
//!   are ignored by the lenient parser).
//! - Exit 2: block; stderr is the reason.
//! - Other non-zero exits: non-blocking warning.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::config::{HookEvent, HookGroup, HookHandler};
use super::evaluate::HookLlmEvaluator;

/// Default per-hook timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Permission verdict from a PreToolUse / PermissionRequest hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookPermission {
    Allow,
    Deny,
    Ask,
}

/// Merged outcome of all handlers that ran for one event.
#[derive(Clone, Debug, Default)]
pub struct HookVerdict {
    /// `decision: "block"` / exit 2 — first block wins.
    pub blocked: Option<String>,
    /// PreToolUse `updatedInput` — later handlers override earlier ones.
    pub updated_input: Option<Value>,
    /// PreToolUse/PermissionRequest `permissionDecision` — the most
    /// restrictive wins (deny > ask > allow).
    pub permission: Option<HookPermission>,
    pub permission_reason: Option<String>,
    /// `additionalContext` fragments, in handler order.
    pub additional_context: Vec<String>,
    /// `systemMessage` fragments + engine warnings (for the transcript).
    pub system_messages: Vec<String>,
    /// Universal `continue: false` — stop processing entirely.
    pub stop_reason: Option<String>,
}

impl HookVerdict {
    fn absorb_output(&mut self, output: HookOutput) {
        if let Some(reason) = output.block_reason
            && self.blocked.is_none()
        {
            self.blocked = Some(reason);
        }
        if let Some(input) = output.updated_input {
            self.updated_input = Some(input);
        }
        if let Some(permission) = output.permission {
            let more_restrictive = match (self.permission, permission) {
                (Some(HookPermission::Deny), _) => false,
                (_, HookPermission::Deny) => true,
                (Some(HookPermission::Ask), _) => false,
                (_, HookPermission::Ask) => true,
                _ => true,
            };
            if more_restrictive {
                self.permission = Some(permission);
                self.permission_reason = output.permission_reason;
            }
        }
        self.additional_context.extend(output.additional_context);
        self.system_messages.extend(output.system_messages);
        if self.stop_reason.is_none() {
            self.stop_reason = output.stop_reason;
        }
    }
}

/// One handler's parsed contribution.
#[derive(Default)]
struct HookOutput {
    block_reason: Option<String>,
    updated_input: Option<Value>,
    permission: Option<HookPermission>,
    permission_reason: Option<String>,
    additional_context: Vec<String>,
    system_messages: Vec<String>,
    stop_reason: Option<String>,
}

/// The shared hook executor: matcher filtering, handler dispatch, verdict
/// parsing/merging. Clone-cheap (Arc internals).
#[derive(Clone)]
pub struct HookEngine {
    shell: Option<Arc<tack_tools::shell::ShellConfig>>,
    cwd: PathBuf,
    evaluator: Option<Arc<dyn HookLlmEvaluator>>,
}

impl std::fmt::Debug for HookEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookEngine")
            .field("shell", &self.shell.is_some())
            .field("evaluator", &self.evaluator.is_some())
            .finish()
    }
}

impl HookEngine {
    pub fn new(shell: Option<Arc<tack_tools::shell::ShellConfig>>, cwd: PathBuf) -> Self {
        HookEngine {
            shell,
            cwd,
            evaluator: None,
        }
    }

    pub fn with_evaluator(mut self, evaluator: Arc<dyn HookLlmEvaluator>) -> Self {
        self.evaluator = Some(evaluator);
        self
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Run all handlers in `groups` matching `matcher_input`, merging
    /// verdicts in declaration order. Never fails: handler errors degrade
    /// to warnings (`system_messages`) — a broken hook must not stall the
    /// agent (fail-open, same contract as tack-ext intercepts).
    pub async fn run(
        &self,
        groups: &[HookGroup],
        matcher_input: Option<&str>,
        input: &Value,
    ) -> HookVerdict {
        let mut verdict = HookVerdict::default();
        for group in groups {
            if !matches_matcher(group.matcher.as_deref(), matcher_input) {
                continue;
            }
            for handler in &group.hooks {
                match handler {
                    HookHandler::Command {
                        command,
                        timeout_sec,
                        run_async,
                        ..
                    } => {
                        let timeout = timeout_sec
                            .map(|s| Duration::from_secs(s.max(1)))
                            .unwrap_or(DEFAULT_TIMEOUT);
                        if *run_async {
                            let shell = self.shell.clone();
                            let cwd = self.cwd.clone();
                            let command = command.clone();
                            let input = input.clone();
                            tokio::spawn(async move {
                                let _ =
                                    run_command(&command, shell.as_ref(), &cwd, &input, timeout)
                                        .await;
                            });
                            continue;
                        }
                        match run_command(command, self.shell.as_ref(), &self.cwd, input, timeout)
                            .await
                        {
                            Ok(outcome) => verdict.absorb_output(parse_command_output(&outcome)),
                            Err(e) => {
                                tracing::warn!("hook {command:?} failed: {e}");
                                verdict
                                    .system_messages
                                    .push(format!("hook {command} failed: {e}"));
                            }
                        }
                    }
                    HookHandler::Prompt {
                        prompt,
                        model,
                        timeout_sec,
                    }
                    | HookHandler::Agent {
                        prompt,
                        model,
                        timeout_sec,
                    } => {
                        let use_tools = matches!(handler, HookHandler::Agent { .. });
                        let Some(evaluator) = &self.evaluator else {
                            tracing::warn!("prompt/agent hook without an LLM evaluator; skipping");
                            continue;
                        };
                        let timeout = timeout_sec
                            .map(|s| Duration::from_secs(s.max(1)))
                            .unwrap_or(DEFAULT_TIMEOUT);
                        match evaluator
                            .evaluate(prompt, model.as_deref(), use_tools, input, timeout)
                            .await
                        {
                            Ok(output) => verdict.absorb_output(parse_verdict_json(&output)),
                            Err(e) => {
                                tracing::warn!("prompt hook failed: {e}");
                            }
                        }
                    }
                }
            }
        }
        verdict
    }
}

/// Raw result of one hook command.
#[derive(Clone, Debug)]
pub struct HookOutcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Spawn + feed stdin + collect output with a timeout; the child is killed
/// on timeout (kill_on_drop), never leaked.
pub async fn run_command(
    command: &str,
    shell: Option<&Arc<tack_tools::shell::ShellConfig>>,
    cwd: &Path,
    input: &Value,
    timeout: Duration,
) -> Result<HookOutcome, String> {
    let shell = shell.ok_or_else(|| "no shell configured for hooks".to_string())?;
    let body = input.to_string();
    let run = async move {
        let mut process = tokio::process::Command::new(&shell.shell);
        process
            .args(&shell.args)
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // On timeout the future is dropped; without this the child would
            // keep running detached (leaked process per timed-out hook).
            .kill_on_drop(true);
        let mut child = process
            .spawn()
            .map_err(|e| format!("failed to spawn hook: {e}"))?;
        let stdin = child.stdin.take();
        // Feed stdin CONCURRENTLY with output collection (F31): writing
        // stdin to completion first deadlocks when the input exceeds the
        // pipe buffer and the child fills its stdout pipe before reading
        // stdin to EOF (each side waits on the other; only the 60s
        // timeout unwound it).
        let write_stdin = async move {
            if let Some(mut stdin) = stdin {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(body.as_bytes()).await;
                // Dropping stdin (or an explicit shutdown) signals EOF to
                // hooks that read stdin to completion.
                let _ = stdin.shutdown().await;
            }
        };
        let ((), output) = tokio::join!(write_stdin, child.wait_with_output());
        let output = output.map_err(|e| format!("hook wait failed: {e}"))?;
        Ok::<HookOutcome, String>(HookOutcome {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(result) => result,
        Err(_) => Err(format!("hook timed out ({}s)", timeout.as_secs().max(1))),
    }
}

/// Parse one command's outcome into a HookOutput per the Claude contract.
fn parse_command_output(outcome: &HookOutcome) -> HookOutput {
    match outcome.code {
        0 => {
            if outcome.stdout.is_empty() {
                return HookOutput::default();
            }
            match serde_json::from_str::<Value>(&outcome.stdout) {
                Ok(json) => parse_verdict_json(&json),
                Err(_) => {
                    // Plain stdout is not a verdict; surface it as context
                    // (keeps tack's original SessionStart stdout-injection
                    // behavior working without a JSON envelope).
                    HookOutput {
                        additional_context: vec![outcome.stdout.clone()],
                        ..Default::default()
                    }
                }
            }
        }
        2 => HookOutput {
            block_reason: Some(if outcome.stderr.is_empty() {
                "blocked by hook".to_string()
            } else {
                outcome.stderr.clone()
            }),
            ..Default::default()
        },
        code => {
            tracing::warn!("hook exited {code}: {}", outcome.stderr);
            HookOutput::default()
        }
    }
}

/// Parse a verdict JSON object (Claude output schema) into a HookOutput.
/// Tolerant: unknown fields are ignored (Claude evolves the schema).
fn parse_verdict_json(json: &Value) -> HookOutput {
    let mut output = HookOutput::default();
    if let Some(system) = json.get("systemMessage").and_then(Value::as_str) {
        output.system_messages.push(system.to_string());
    }
    if json.get("continue").and_then(Value::as_bool) == Some(false) {
        output.stop_reason = json
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some("stopped by hook".to_string()));
    }
    if json.get("decision").and_then(Value::as_str) == Some("block") {
        output.block_reason = json
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some("blocked by hook".to_string()));
    }
    if let Some(specific) = json.get("hookSpecificOutput") {
        if let Some(context) = specific.get("additionalContext").and_then(Value::as_str)
            && !context.is_empty()
        {
            output.additional_context.push(context.to_string());
        }
        if let Some(input) = specific.get("updatedInput")
            && !input.is_null()
        {
            output.updated_input = Some(input.clone());
        }
        if let Some(decision) = specific.get("permissionDecision").and_then(Value::as_str) {
            output.permission = match decision {
                "allow" => Some(HookPermission::Allow),
                "deny" => Some(HookPermission::Deny),
                "ask" => Some(HookPermission::Ask),
                other => {
                    tracing::warn!("unknown permissionDecision {other:?}");
                    None
                }
            };
            output.permission_reason = specific
                .get("permissionDecisionReason")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    output
}

/// Matcher semantics (Claude/Codex): absent/empty/"*" matches everything;
/// a string without regex metacharacters is an exact match with `|`
/// alternation; anything else compiles as a regex.
pub fn matches_matcher(matcher: Option<&str>, input: Option<&str>) -> bool {
    match matcher {
        None => true,
        Some(m) if m.trim().is_empty() || m.trim() == "*" => true,
        Some(m) if is_exact_matcher(m) => input
            .map(|input| m.split('|').any(|candidate| candidate.trim() == input))
            .unwrap_or(false),
        Some(m) => input
            .and_then(|input| cached_matcher_regex(m).map(|re| re.is_match(input)))
            .unwrap_or(false),
    }
}

/// Compiled-regex cache for hook matchers. Matchers come from a small,
/// static config set but `matches_matcher` runs on every hook trigger —
/// compiling per evaluation was the hot path. Caches compile failures
/// too (`None`), so an invalid pattern stays a consistent non-match
/// without recompiling on every evaluation.
static MATCHER_REGEX_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Option<regex::Regex>>>,
> = std::sync::OnceLock::new();

fn cached_matcher_regex(pattern: &str) -> Option<regex::Regex> {
    let cache =
        MATCHER_REGEX_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(pattern.to_string())
        .or_insert_with(|| regex::Regex::new(pattern).ok())
        .clone()
}

fn is_exact_matcher(matcher: &str) -> bool {
    matcher
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '|' | ' ' | '.'))
}

/// Build the common hook input fields (snake_case, Claude wire names).
pub fn base_input(event: HookEvent, cwd: &Path, session_id: &str) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "transcript_path": Value::Null,
        "cwd": cwd,
        "hook_event_name": event.as_str(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn matcher_semantics() {
        assert!(matches_matcher(None, Some("bash")));
        assert!(matches_matcher(Some("*"), Some("bash")));
        assert!(matches_matcher(Some("bash|edit"), Some("edit")));
        assert!(!matches_matcher(Some("bash|edit"), Some("write")));
        // Regex fallback.
        assert!(matches_matcher(Some("mcp__.*"), Some("mcp__github__get")));
        assert!(!matches_matcher(Some("^edit$"), Some("editor")));
        // Exact matcher does not substring-match.
        assert!(!matches_matcher(Some("edit"), Some("editor")));
    }

    #[test]
    fn matcher_regex_cache_is_consistent() {
        // Valid patterns compile once and stay match-stable across calls.
        let first = cached_matcher_regex("^mcp__cache_test__.*$");
        let second = cached_matcher_regex("^mcp__cache_test__.*$");
        assert!(first.is_some());
        assert_eq!(
            first.map(|re| re.is_match("mcp__cache_test__tool")),
            second.map(|re| re.is_match("mcp__cache_test__tool"))
        );
        // Compile failures are cached too: an invalid pattern is a
        // non-match on every evaluation (never a panic, never recompiled
        // into a different verdict).
        assert!(cached_matcher_regex("(unclosed").is_none());
        assert!(!matches_matcher(Some("(unclosed"), Some("anything")));
        assert!(!matches_matcher(Some("(unclosed"), Some("anything")));
    }

    #[test]
    fn parses_claude_pre_tool_use_output() {
        let json = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "rm -rf is not allowed",
                "updatedInput": {"command": "ls"},
                "additionalContext": "be careful"
            },
            "systemMessage": "hook ran"
        });
        let output = parse_verdict_json(&json);
        assert_eq!(output.permission, Some(HookPermission::Deny));
        assert_eq!(
            output.permission_reason.as_deref(),
            Some("rm -rf is not allowed")
        );
        assert_eq!(output.updated_input.unwrap()["command"], "ls");
        assert_eq!(output.additional_context, vec!["be careful"]);
        assert_eq!(output.system_messages, vec!["hook ran"]);
    }

    #[test]
    fn parses_block_and_continue_false() {
        let json = serde_json::json!({
            "decision": "block",
            "reason": "nope",
            "continue": false,
            "stopReason": "halt everything"
        });
        let output = parse_verdict_json(&json);
        assert_eq!(output.block_reason.as_deref(), Some("nope"));
        assert_eq!(output.stop_reason.as_deref(), Some("halt everything"));
    }

    #[test]
    fn exit_2_blocks_with_stderr() {
        let output = parse_command_output(&HookOutcome {
            code: 2,
            stdout: String::new(),
            stderr: "denied-reason".into(),
        });
        assert_eq!(output.block_reason.as_deref(), Some("denied-reason"));
    }

    #[test]
    fn plain_stdout_becomes_additional_context() {
        let output = parse_command_output(&HookOutcome {
            code: 0,
            stdout: "remember to run tests".into(),
            stderr: String::new(),
        });
        assert_eq!(
            output.additional_context,
            vec!["remember to run tests".to_string()]
        );
        assert!(output.block_reason.is_none());
    }

    #[test]
    fn deny_wins_over_allow_when_merging() {
        let mut verdict = HookVerdict::default();
        verdict.absorb_output(HookOutput {
            permission: Some(HookPermission::Allow),
            ..Default::default()
        });
        verdict.absorb_output(HookOutput {
            permission: Some(HookPermission::Deny),
            permission_reason: Some("no".into()),
            ..Default::default()
        });
        assert_eq!(verdict.permission, Some(HookPermission::Deny));
        verdict.absorb_output(HookOutput {
            permission: Some(HookPermission::Allow),
            ..Default::default()
        });
        assert_eq!(verdict.permission, Some(HookPermission::Deny));
    }

    /// F31 regression: feeding stdin BEFORE collecting output deadlocked
    /// when the input exceeded the pipe buffer and the child filled its
    /// stdout pipe without reading stdin to EOF (each side blocked on the
    /// other; only the timeout unwound it). stdin is now written
    /// concurrently with wait_with_output.
    #[tokio::test]
    async fn large_stdin_and_stdout_do_not_deadlock() {
        let shell = tack_tools::shell::resolve_shell(None).ok().map(Arc::new);
        let Some(shell) = shell else { return }; // no shell on this machine
        let tmp = tempfile::tempdir().unwrap();
        // ~1MiB of hook input (>> 64KiB pipe buffer); the child reads one
        // byte of stdin, then emits ~1MiB of stdout.
        let input = serde_json::json!({ "data": "x".repeat(1024 * 1024) });
        let outcome = run_command(
            "head -c 1 >/dev/null; head -c 1048576 /dev/zero | tr '\\0' 'y'",
            Some(&shell),
            tmp.path(),
            &input,
            Duration::from_secs(20),
        )
        .await
        .expect("must not deadlock (the timeout would produce an Err)");
        assert_eq!(outcome.code, 0);
        assert_eq!(outcome.stdout.len(), 1048576);
    }

    #[tokio::test]
    async fn timed_out_hook_process_is_killed_not_leaked() {
        let shell = tack_tools::shell::resolve_shell(None).ok().map(Arc::new);
        let Some(shell) = shell else { return }; // no shell on this machine
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("hook-survived");
        let result = run_command(
            &format!("sleep 1 && touch {}", marker.display()),
            Some(&shell),
            tmp.path(),
            &serde_json::json!({}),
            Duration::from_millis(100),
        )
        .await;
        assert!(result.unwrap_err().contains("timed out"));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "timed-out hook kept running (leaked process)"
        );
    }
}
