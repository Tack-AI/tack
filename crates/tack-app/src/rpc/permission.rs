//! Interactive permission prompts for rpc mode (`set_mode ask|acceptEdits|plan|bypass`).
//!
//! Mirrors the remote host's RemotePermissionHooks flow: deny rules are
//! already handled upstream in the hook chain (DenyRulesHooks); this hook
//! applies the session mode, allow rules and the session allow-always cache,
//! and for anything still gated emits a `permission_request` JSONL event and
//! parks the tool call until the client answers `permission_response`, the
//! run is cancelled, or the prompt times out (deny).
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

use tack_agent_core::hooks::{AgentHooks, BeforeToolCallContext, BeforeToolCallOutcome};
use tack_protocol::schemas::{PermissionDecision, SessionMode};

use crate::permissions::PermissionRules;

/// How long a permission prompt waits for an answer before denying (same
/// bound as the remote host's PERMISSION_PROMPT_TIMEOUT).
const PERMISSION_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Pending permission prompts: requestId -> answer channel.
pub(crate) type PendingPermissions = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionAnswer>>>>;

/// Out-of-band JSONL event sink (one line per frame, same atomic-write
/// discipline as the response/event writers).
pub(crate) type EventSink = mpsc::UnboundedSender<Value>;

#[derive(Debug)]
pub(crate) struct PermissionAnswer {
    pub decision: PermissionDecision,
    pub reason: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct RpcPermissionState {
    pub mode: SessionMode,
    pub allow_always: HashSet<String>,
    pub pending: PendingPermissions,
}

fn allow_always_key(tool_name: &str, args: &Value) -> String {
    let first = args
        .get("path")
        .or_else(|| args.get("command"))
        .or_else(|| args.get("pattern"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("{tool_name}:{first}")
}

/// Short human-readable summary of a tool call for the permission prompt.
fn permission_title(tool_name: &str, args: &Value) -> String {
    let first = args
        .get("path")
        .or_else(|| args.get("command"))
        .or_else(|| args.get("pattern"))
        .and_then(Value::as_str)
        .map(|s| {
            if s.chars().count() > 120 {
                let truncated: String = s.chars().take(120).collect();
                format!("{truncated}…")
            } else {
                s.to_string()
            }
        })
        .unwrap_or_default();
    if first.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name} {first}")
    }
}

fn next_permission_request_id(session_id: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("perm-{session_id}-{nanos:x}")
}

pub(crate) struct RpcPermissionHooks {
    session_id: String,
    state: Arc<Mutex<RpcPermissionState>>,
    rules: PermissionRules,
    events: EventSink,
    run_cancel: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for RpcPermissionHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcPermissionHooks")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl AgentHooks for RpcPermissionHooks {
    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        use tack_agent_core::hooks::BeforeToolCallOutcome as Outcome;
        let read_only = crate::permissions::is_read_only_tool(ctx.tool_name, ctx.args);
        let mode = self.state.lock().await.mode;
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
        // Declarative allow rules skip the prompt.
        if self.rules.allow_match(ctx.tool_name, ctx.args).is_some() {
            return Outcome::Allow;
        }
        let key = allow_always_key(ctx.tool_name, ctx.args);
        if self.state.lock().await.allow_always.contains(&key) {
            return Outcome::Allow;
        }

        // Prompt the client: park until answered, cancelled, or timed out.
        let request_id = next_permission_request_id(&self.session_id);
        let (tx, rx) = oneshot::channel::<PermissionAnswer>();
        self.state
            .lock()
            .await
            .pending
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let title = permission_title(ctx.tool_name, ctx.args);
        let _ = self.events.send(json!({
            "type": "permission_request",
            "requestId": request_id,
            "toolCallId": ctx.tool_call_id,
            "toolName": ctx.tool_name,
            "title": title,
            "input": ctx.args,
        }));
        let answer = {
            let cancel = self.run_cancel.clone();
            tokio::select! {
                answer = rx => Ok(answer),
                _ = cancel.cancelled() => Err("the run was cancelled"),
                _ = tokio::time::sleep(PERMISSION_PROMPT_TIMEOUT) => Err("timed out"),
            }
        };
        let (decision, reason) = match answer {
            Ok(Ok(answer)) => (answer.decision, answer.reason),
            Ok(Err(_closed)) => (PermissionDecision::Deny, Some("prompt closed".to_string())),
            Err(why) => (PermissionDecision::Deny, Some(why.to_string())),
        };
        // Drop the entry regardless of how we woke: a late answer must not
        // find a stale prompt (and the map must not leak it).
        self.state
            .lock()
            .await
            .pending
            .lock()
            .await
            .remove(&request_id);
        let _ = self.events.send(json!({
            "type": "permission_resolved",
            "requestId": request_id,
            "decision": match decision {
                PermissionDecision::AllowOnce => "allow",
                PermissionDecision::AllowAlways => "allowAlways",
                PermissionDecision::Deny => "deny",
            },
        }));
        match decision {
            PermissionDecision::AllowOnce => Outcome::Allow,
            PermissionDecision::AllowAlways => {
                self.state.lock().await.allow_always.insert(key);
                Outcome::Allow
            }
            PermissionDecision::Deny => Outcome::Block {
                reason: reason.or_else(|| Some("denied by user".to_string())),
                terminate: false,
            },
        }
    }
}

pub(crate) fn rpc_permission_hooks(
    session_id: String,
    state: Arc<Mutex<RpcPermissionState>>,
    rules: PermissionRules,
    events: EventSink,
    run_cancel: tokio_util::sync::CancellationToken,
) -> Arc<RpcPermissionHooks> {
    Arc::new(RpcPermissionHooks {
        session_id,
        state,
        rules,
        events,
        run_cancel,
    })
}
