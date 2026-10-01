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

fn allow_always_key(agent_dir: &std::path::Path, tool_name: &str, args: &Value) -> String {
    crate::permissions::allow_always_key(agent_dir, tool_name, args)
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
    /// Plugin approval chain (tack-RPC `approval/review`): reviewers get
    /// first crack at a decision that would otherwise be emitted as a
    /// `permission_request` event (see `crate::approval`).
    approval_chain: crate::approval::ApprovalChain,
    /// Prompt-injection defense: when untrusted external content (web/MCP)
    /// entered the context this run, mutating tools always prompt the
    /// client — allow rules, the allow-always cache AND the plugin approval
    /// chain are bypassed (parity with the TUI surface). Reset per run.
    untrusted_seen: Arc<std::sync::atomic::AtomicBool>,
    /// PreToolUse hook decisions recorded earlier in the chain by the
    /// ShellHooks bridge (allow => skip prompting, ask => force the client
    /// prompt past every fast path and the approval chain). Consumed per
    /// tool_call_id — shared with the ShellHooks instance in rpc/prompt.rs
    /// (a private map would drop the verdicts: a shell hook's "ask" must
    /// not be silently approved by the plugin chain).
    hook_decisions: crate::shell_hooks::HookDecisions,
    /// Agent dir (allow-always keys for ext__* tools bind the loaded
    /// plugin version via the extension load report — see
    /// `crate::permissions::allow_always_key`).
    agent_dir: std::path::PathBuf,
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
        // PreToolUse hook verdicts (Claude permissionDecision semantics),
        // consumed by tool_call_id — parity with the TUI surface:
        // "allow" approves without prompting; "ask" forces the CLIENT
        // prompt past every fast path below (mode gate, allow rules,
        // allow-always cache AND the plugin approval chain — a chain
        // claim must not approve the exact call a hook escalated).
        // Declarative deny already ran upstream (DenyRulesHooks) and
        // still wins.
        let hook_decision = self
            .hook_decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(ctx.tool_call_id);
        let mut force_human = false;
        match hook_decision {
            Some((crate::shell_hooks::HookPermission::Allow, _)) => {
                return Outcome::Allow;
            }
            Some((crate::shell_hooks::HookPermission::Deny, reason)) => {
                return Outcome::Block {
                    reason: Some(reason.unwrap_or_else(|| "denied by hook".into())),
                    terminate: false,
                };
            }
            Some((crate::shell_hooks::HookPermission::Ask, _)) => {
                force_human = true;
            }
            None => {}
        }
        let read_only = crate::permissions::is_read_only_tool(ctx.tool_name, ctx.args);
        let mode = self.state.lock().await.mode;
        if !force_human {
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
        }
        // Declarative allow rules + allow-always cache skip the prompt —
        // but not in an untrusted run: after web/MCP content entered the
        // context, mutating tools always ask (the injection chain
        // "webpage → allow-always bash" is the attack this defends
        // against).
        let untrusted = self
            .untrusted_seen
            .load(std::sync::atomic::Ordering::Relaxed)
            && !read_only;
        if !untrusted && !force_human {
            if self.rules.allow_match(ctx.tool_name, ctx.args).is_some() {
                return Outcome::Allow;
            }
            let key = allow_always_key(&self.agent_dir, ctx.tool_name, ctx.args);
            if self.state.lock().await.allow_always.contains(&key) {
                return Outcome::Allow;
            }
        }
        let key = allow_always_key(&self.agent_dir, ctx.tool_name, ctx.args);

        // Plugin approval chain: reviewers get first crack at the decision
        // that would otherwise be parked on a client answer — EXCEPT in an
        // untrusted run or on a hook "ask" verdict, where a chain claim
        // must not silently approve a call the human has to see.
        let policy = match mode {
            SessionMode::Ask => "ask",
            SessionMode::AcceptEdits => "acceptEdits",
            SessionMode::Plan => "plan",
            SessionMode::Bypass => "bypass",
        };
        if !untrusted
            && !force_human
            && self
                .approval_chain
                .claims_approval(
                    "rpc",
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn rpc_permission_hooks(
    session_id: String,
    state: Arc<Mutex<RpcPermissionState>>,
    rules: PermissionRules,
    events: EventSink,
    run_cancel: tokio_util::sync::CancellationToken,
    approval_chain: crate::approval::ApprovalChain,
    untrusted_seen: Arc<std::sync::atomic::AtomicBool>,
    hook_decisions: crate::shell_hooks::HookDecisions,
    agent_dir: std::path::PathBuf,
) -> Arc<RpcPermissionHooks> {
    Arc::new(RpcPermissionHooks {
        session_id,
        state,
        rules,
        events,
        run_cancel,
        approval_chain,
        untrusted_seen,
        hook_decisions,
        agent_dir,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Scripted approval-chain reviewer.
    #[derive(Debug)]
    struct ClaimReviewer(crate::approval::ChainAction);

    #[async_trait::async_trait]
    impl crate::approval::ApprovalReviewer for ClaimReviewer {
        async fn review(
            &self,
            _request: &crate::approval::ApprovalRequest,
        ) -> Option<crate::approval::ChainDecision> {
            Some(crate::approval::ChainDecision {
                action: self.0,
                reason: None,
            })
        }
    }

    fn hooks(
        untrusted: bool,
        chain: crate::approval::ApprovalChain,
    ) -> (
        Arc<RpcPermissionHooks>,
        Arc<Mutex<RpcPermissionState>>,
        mpsc::UnboundedReceiver<Value>,
    ) {
        hooks_with_decisions(
            untrusted,
            chain,
            crate::shell_hooks::HookDecisions::default(),
        )
    }

    fn hooks_with_decisions(
        untrusted: bool,
        chain: crate::approval::ApprovalChain,
        hook_decisions: crate::shell_hooks::HookDecisions,
    ) -> (
        Arc<RpcPermissionHooks>,
        Arc<Mutex<RpcPermissionState>>,
        mpsc::UnboundedReceiver<Value>,
    ) {
        let state = Arc::new(Mutex::new(RpcPermissionState {
            mode: SessionMode::Ask,
            ..RpcPermissionState::default()
        }));
        let (tx, rx) = mpsc::unbounded_channel();
        let hooks = rpc_permission_hooks(
            "s1".to_string(),
            state.clone(),
            PermissionRules {
                allow: vec![crate::permissions::Rule::parse("Bash(rm -rf build)").unwrap()],
                deny: vec![],
            },
            tx,
            tokio_util::sync::CancellationToken::new(),
            chain,
            Arc::new(std::sync::atomic::AtomicBool::new(untrusted)),
            hook_decisions,
            std::path::PathBuf::new(),
        );
        (hooks, state, rx)
    }

    fn ctx<'a>(
        message: &'a tack_ai::AssistantMessage,
        args: &'a Value,
    ) -> BeforeToolCallContext<'a> {
        BeforeToolCallContext {
            assistant_message: message,
            tool_call_id: "1",
            tool_name: "bash",
            args,
            context: &[],
        }
    }

    /// Answer the first permission_request event with AllowOnce.
    async fn answer_first_prompt(
        state: Arc<Mutex<RpcPermissionState>>,
        rx: &mut mpsc::UnboundedReceiver<Value>,
    ) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("permission_request event expected")
            .expect("event");
        assert_eq!(event["type"], "permission_request");
        let request_id = event["requestId"].as_str().unwrap().to_string();
        let tx = {
            let pending = state.lock().await.pending.clone();
            pending
                .lock()
                .await
                .remove(&request_id)
                .expect("pending prompt")
        };
        tx.send(PermissionAnswer {
            decision: PermissionDecision::AllowOnce,
            reason: None,
        })
        .unwrap();
    }

    /// Trusted run: a chain allow claim approves without any client prompt.
    #[tokio::test]
    async fn trusted_run_chain_claim_approves() {
        let message = tack_ai::AssistantMessage::pending(
            &crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
                .unwrap(),
        );
        let mut chain = crate::approval::ApprovalChain::empty();
        chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        let (hooks, _state, mut rx) = hooks(false, chain);
        let args = json!({ "command": "rm -rf build" });
        let outcome = hooks.before_tool_call(&ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        assert!(rx.try_recv().is_err(), "no prompt expected");
    }

    /// Untrusted run: the chain (and the allow rule) must be skipped — the
    /// client is prompted even though a reviewer claims allow.
    #[tokio::test]
    async fn untrusted_run_skips_chain_and_allow_rules() {
        let message = tack_ai::AssistantMessage::pending(
            &crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
                .unwrap(),
        );
        let mut chain = crate::approval::ApprovalChain::empty();
        chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        let (hooks, state, mut rx) = hooks(true, chain);
        let args = json!({ "command": "rm -rf build" });
        let c = ctx(&message, &args);
        let (outcome, _) = tokio::join!(
            hooks.before_tool_call(&c),
            answer_first_prompt(state, &mut rx)
        );
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
    }

    /// A PreToolUse "allow" verdict recorded by the shared ShellHooks
    /// bridge approves without any client prompt — and without consulting
    /// the allow rules or the chain.
    #[tokio::test]
    async fn hook_allow_verdict_skips_prompt() {
        let message = tack_ai::AssistantMessage::pending(
            &crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
                .unwrap(),
        );
        let decisions = crate::shell_hooks::HookDecisions::default();
        decisions.lock().unwrap().insert(
            "1".to_string(),
            (crate::shell_hooks::HookPermission::Allow, None),
        );
        let (hooks, _state, mut rx) =
            hooks_with_decisions(false, crate::approval::ApprovalChain::empty(), decisions);
        let args = json!({ "command": "something not rule-covered" });
        let outcome = hooks.before_tool_call(&ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        assert!(rx.try_recv().is_err(), "no prompt expected");
    }

    /// A PreToolUse "ask" verdict forces the CLIENT prompt past every fast
    /// path — the allow rule, the mode gate AND the plugin approval chain
    /// (a chain claim must not approve the exact call a hook escalated).
    #[tokio::test]
    async fn hook_ask_verdict_forces_prompt_past_chain_and_allow_rule() {
        let message = tack_ai::AssistantMessage::pending(
            &crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
                .unwrap(),
        );
        let decisions = crate::shell_hooks::HookDecisions::default();
        decisions.lock().unwrap().insert(
            "1".to_string(),
            (crate::shell_hooks::HookPermission::Ask, None),
        );
        let mut chain = crate::approval::ApprovalChain::empty();
        chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        let (hooks, state, mut rx) = hooks_with_decisions(false, chain, decisions);
        // Matches the allow rule "Bash(rm -rf build)": without the "ask"
        // verdict this would be approved with no prompt at all.
        let args = json!({ "command": "rm -rf build" });
        let c = ctx(&message, &args);
        let (outcome, _) = tokio::join!(
            hooks.before_tool_call(&c),
            answer_first_prompt(state, &mut rx)
        );
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        // The verdict is consumed: a second call with the same id follows
        // the normal flow (allow rule short-circuits, no NEW prompt — the
        // only queued event is the first prompt's resolution notice).
        let outcome = hooks.before_tool_call(&ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        let leftover = rx.try_recv().expect("resolution notice");
        assert_eq!(leftover["type"], "permission_resolved");
        assert!(rx.try_recv().is_err(), "no second prompt expected");
    }

    /// A PreToolUse "deny" verdict blocks outright (recorded for parity;
    /// ShellHooks also blocks on its own — defense in depth).
    #[tokio::test]
    async fn hook_deny_verdict_blocks() {
        let message = tack_ai::AssistantMessage::pending(
            &crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new("."))
                .unwrap(),
        );
        let decisions = crate::shell_hooks::HookDecisions::default();
        decisions.lock().unwrap().insert(
            "1".to_string(),
            (
                crate::shell_hooks::HookPermission::Deny,
                Some("hook says no".to_string()),
            ),
        );
        let (hooks, _state, mut rx) =
            hooks_with_decisions(false, crate::approval::ApprovalChain::empty(), decisions);
        let args = json!({ "command": "rm -rf build" });
        let outcome = hooks.before_tool_call(&ctx(&message, &args)).await;
        match outcome {
            BeforeToolCallOutcome::Block { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("hook says no"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no prompt expected");
    }
}
