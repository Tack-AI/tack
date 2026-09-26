//! AgentHooks bridge: runs PreToolUse / PostToolUse hook groups inside the
//! agent loop's tool-call gates, and records permission decisions for the
//! TUI permission hooks to consult.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tack_agent_core::{
    AfterToolCallContext, AfterToolCallPatch, AgentHooks, BeforeToolCallContext,
    BeforeToolCallOutcome,
};

use super::config::{HookEvent, HookGroup};
use super::engine::{HookEngine, HookPermission};

/// Per-tool-call permission decisions recorded by PreToolUse hooks,
/// consulted (and consumed) by the permission hooks later in the chain.
/// Maps tool_call_id → (permission, reason).
pub type HookDecisions = Arc<Mutex<HashMap<String, (HookPermission, Option<String>)>>>;

/// Shared context threaded into hook input payloads.
#[derive(Clone, Debug, Default)]
pub struct HookSessionInfo {
    pub session_id: String,
    pub model: String,
    pub permission_mode: String,
}

/// The loop-facing bridge (PreToolUse/PostToolUse). Other events are run
/// directly through [`HookEngine`] by the app.
pub struct ShellHooks {
    engine: HookEngine,
    pre: Vec<HookGroup>,
    post: Vec<HookGroup>,
    /// PostToolUseFailure groups: run only when the tool call errored
    /// (ZCode event split; `post` still fires on both with `is_error`).
    post_failure: Vec<HookGroup>,
    info: HookSessionInfo,
    decisions: HookDecisions,
}

impl std::fmt::Debug for ShellHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellHooks")
            .field("pre", &self.pre.len())
            .field("post", &self.post.len())
            .finish()
    }
}

impl ShellHooks {
    pub fn new(
        engine: HookEngine,
        pre: Vec<HookGroup>,
        post: Vec<HookGroup>,
        info: HookSessionInfo,
        decisions: HookDecisions,
    ) -> Self {
        ShellHooks {
            engine,
            pre,
            post,
            post_failure: Vec::new(),
            info,
            decisions,
        }
    }

    pub fn with_post_failure(mut self, post_failure: Vec<HookGroup>) -> Self {
        self.post_failure = post_failure;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty() && self.post_failure.is_empty()
    }
}

#[async_trait]
impl AgentHooks for ShellHooks {
    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        if self.pre.is_empty() {
            return BeforeToolCallOutcome::Allow;
        }
        let input = json!({
            "session_id": self.info.session_id,
            "transcript_path": serde_json::Value::Null,
            "cwd": self.engine.cwd(),
            "hook_event_name": HookEvent::PreToolUse.as_str(),
            "model": self.info.model,
            "permission_mode": self.info.permission_mode,
            "tool_name": ctx.tool_name,
            "tool_input": ctx.args,
            "tool_use_id": ctx.tool_call_id,
        });
        let verdict = self
            .engine
            .run(&self.pre, Some(ctx.tool_name), &input)
            .await;
        // Record the permission decision for TuiPermissionHooks (consumed
        // there by tool_call_id) BEFORE returning, so a Rewrite still
        // carries the permission verdict.
        if let Some(permission) = verdict.permission
            && let Ok(mut decisions) = self.decisions.lock()
        {
            decisions.insert(
                ctx.tool_call_id.to_string(),
                (permission, verdict.permission_reason.clone()),
            );
        }
        if let Some(reason) = verdict.blocked {
            return BeforeToolCallOutcome::Block {
                reason: Some(reason),
                terminate: false,
            };
        }
        if let Some(HookPermission::Deny) = verdict.permission {
            return BeforeToolCallOutcome::Block {
                reason: Some(
                    verdict
                        .permission_reason
                        .unwrap_or_else(|| "denied by PreToolUse hook".to_string()),
                ),
                terminate: false,
            };
        }
        if let Some(updated) = verdict.updated_input {
            // Claude semantics: updatedInput is a partial merge over the
            // original arguments, not a wholesale replacement.
            let mut merged = ctx.args.clone();
            if let (Some(base), Some(patch)) = (merged.as_object_mut(), updated.as_object()) {
                for (key, value) in patch {
                    base.insert(key.clone(), value.clone());
                }
            } else {
                merged = updated;
            }
            return BeforeToolCallOutcome::Rewrite { args: merged };
        }
        BeforeToolCallOutcome::Allow
    }

    async fn after_tool_call(
        &self,
        ctx: &AfterToolCallContext<'_>,
        result: &tack_agent_core::AgentToolResult,
        is_error: bool,
    ) -> Option<AfterToolCallPatch> {
        let run_post = !self.post.is_empty();
        let run_failure = is_error && !self.post_failure.is_empty();
        if !run_post && !run_failure {
            return None;
        }
        let result_text: String = result
            .content
            .iter()
            .filter_map(|b| match b {
                tack_ai::InputContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let base_input = |event_name: &str| {
            json!({
                "session_id": self.info.session_id,
                "transcript_path": serde_json::Value::Null,
                "cwd": self.engine.cwd(),
                "hook_event_name": event_name,
                "model": self.info.model,
                "permission_mode": self.info.permission_mode,
                "tool_name": ctx.tool_name,
                "tool_input": ctx.args,
                "tool_response": result_text.chars().take(10_000).collect::<String>(),
                "tool_use_id": ctx.tool_call_id,
                "is_error": is_error,
            })
        };
        let mut content: Option<Vec<tack_ai::InputContentBlock>> = None;
        let mut mark_error = false;
        // Claude: a "block" verdict feeds the reason back to the model as an
        // error; additionalContext is appended to the tool result.
        let mut apply_verdict = |verdict: super::engine::HookVerdict| {
            if let Some(reason) = verdict.blocked {
                let mut blocks = content.take().unwrap_or_else(|| result.content.clone());
                blocks.push(tack_ai::InputContentBlock::text(format!(
                    "PostToolUse hook feedback: {reason}"
                )));
                content = Some(blocks);
                mark_error = true;
            }
            if !verdict.additional_context.is_empty() {
                let mut blocks = content.take().unwrap_or_else(|| result.content.clone());
                for context in &verdict.additional_context {
                    blocks.push(tack_ai::InputContentBlock::text(context.clone()));
                }
                content = Some(blocks);
            }
        };
        if run_post {
            let input = base_input(HookEvent::PostToolUse.as_str());
            let verdict = self
                .engine
                .run(&self.post, Some(ctx.tool_name), &input)
                .await;
            apply_verdict(verdict);
        }
        if run_failure {
            let mut input = base_input(HookEvent::PostToolUseFailure.as_str());
            // ZCode compat aliases: error_details object + error message.
            let message = result_text.chars().take(2_000).collect::<String>();
            if let Some(obj) = input.as_object_mut() {
                obj.insert("error".to_string(), json!(message));
                obj.insert(
                    "error_details".to_string(),
                    json!({ "message": message, "type": "ToolError" }),
                );
                obj.insert("is_interrupt".to_string(), json!(false));
            }
            let verdict = self
                .engine
                .run(&self.post_failure, Some(ctx.tool_name), &input)
                .await;
            apply_verdict(verdict);
        }
        content.map(|content| AfterToolCallPatch {
            content: Some(content),
            is_error: if mark_error { Some(true) } else { None },
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::shell_hooks::config::{HookGroup, HookHandler};
    use crate::shell_hooks::engine::HookEngine;

    fn test_ctx<'a>(
        message: &'a tack_ai::AssistantMessage,
        args: &'a serde_json::Value,
    ) -> BeforeToolCallContext<'a> {
        BeforeToolCallContext {
            assistant_message: message,
            tool_call_id: "call-1",
            tool_name: "bash",
            args,
            context: &[],
        }
    }

    fn test_message() -> tack_ai::AssistantMessage {
        tack_ai::AssistantMessage::pending(&tack_ai::Model {
            provider: "anthropic".into(),
            id: "k3".into(),
            name: "k3".into(),
            api: "anthropic-messages".into(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: tack_ai::ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            sampling_params: None,
            headers: None,
            compat: None,
        })
    }

    fn hooks_with(command: &str) -> (ShellHooks, HookDecisions) {
        let shell = tack_tools::shell::resolve_shell(None).ok().map(Arc::new);
        let engine = HookEngine::new(shell, std::env::current_dir().unwrap());
        let decisions = HookDecisions::default();
        let hooks = ShellHooks::new(
            engine,
            vec![HookGroup {
                matcher: None,
                hooks: vec![HookHandler::Command {
                    command: command.to_string(),
                    timeout_sec: Some(10),
                    run_async: false,
                    status_message: None,
                }],
            }],
            vec![],
            HookSessionInfo::default(),
            decisions.clone(),
        );
        (hooks, decisions)
    }

    #[tokio::test]
    async fn exit_2_blocks_with_stderr_reason() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let message = test_message();
        let args = serde_json::json!({"command": "rm -rf /"});
        let (hooks, _) = hooks_with("echo denied-reason 1>&2; exit 2");
        match hooks.before_tool_call(&test_ctx(&message, &args)).await {
            BeforeToolCallOutcome::Block { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("denied-reason"));
            }
            _ => panic!("expected block"),
        }
    }

    #[tokio::test]
    async fn json_deny_permission_blocks() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let message = test_message();
        let args = serde_json::json!({});
        let (hooks, _) = hooks_with(
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"policy says no"}}'"#,
        );
        match hooks.before_tool_call(&test_ctx(&message, &args)).await {
            BeforeToolCallOutcome::Block { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("policy says no"));
            }
            _ => panic!("expected block"),
        }
    }

    #[tokio::test]
    async fn updated_input_merges_over_original_args() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let message = test_message();
        let args = serde_json::json!({"command": "rm -rf /tmp/x", "timeout": 30});
        let (hooks, _) =
            hooks_with(r#"echo '{"hookSpecificOutput":{"updatedInput":{"command":"ls /tmp/x"}}}'"#);
        match hooks.before_tool_call(&test_ctx(&message, &args)).await {
            BeforeToolCallOutcome::Rewrite { args } => {
                // updatedInput merges: command replaced, timeout preserved.
                assert_eq!(args["command"], "ls /tmp/x");
                assert_eq!(args["timeout"], 30);
            }
            other => panic!("expected rewrite, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_decision_is_recorded_for_permission_hooks() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let message = test_message();
        let args = serde_json::json!({});
        let (hooks, decisions) =
            hooks_with(r#"echo '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#);
        let outcome = hooks.before_tool_call(&test_ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        let recorded = decisions.lock().unwrap().remove("call-1");
        assert!(matches!(recorded, Some((HookPermission::Allow, _))));
    }

    #[tokio::test]
    async fn post_tool_use_failure_fires_only_on_error() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("marker");
        // Spell the redirect target for the hook shell (see
        // tack_tools::shell::shell_quote).
        let command = format!(
            "echo failure >> {}",
            tack_tools::shell::shell_quote(&marker)
        );
        let shell = tack_tools::shell::resolve_shell(None).ok().map(Arc::new);
        let engine = HookEngine::new(shell, std::env::current_dir().unwrap());
        let hooks = ShellHooks::new(
            engine,
            vec![],
            vec![],
            HookSessionInfo::default(),
            HookDecisions::default(),
        )
        .with_post_failure(vec![HookGroup {
            matcher: None,
            hooks: vec![HookHandler::Command {
                command,
                timeout_sec: Some(10),
                run_async: false,
                status_message: None,
            }],
        }]);
        assert!(!hooks.is_empty());
        let message = test_message();
        let args = serde_json::json!({});
        let ctx = AfterToolCallContext {
            assistant_message: &message,
            tool_call_id: "call-1",
            tool_name: "bash",
            args: &args,
            context: &[],
        };
        let result = tack_agent_core::AgentToolResult::error("boom");
        // Success: the failure group must not fire.
        hooks.after_tool_call(&ctx, &result, false).await;
        assert!(!marker.exists(), "PostToolUseFailure fired on success");
        // Error: fires.
        hooks.after_tool_call(&ctx, &result, true).await;
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "failure");
    }

    #[tokio::test]
    async fn matcher_skips_non_matching_tools() {
        if tack_tools::shell::resolve_shell(None).is_err() {
            return;
        }
        let shell = tack_tools::shell::resolve_shell(None).ok().map(Arc::new);
        let engine = HookEngine::new(shell, std::env::current_dir().unwrap());
        let hooks = ShellHooks::new(
            engine,
            vec![HookGroup {
                matcher: Some("edit|write".to_string()),
                hooks: vec![HookHandler::Command {
                    command: "exit 2".to_string(),
                    timeout_sec: Some(5),
                    run_async: false,
                    status_message: None,
                }],
            }],
            vec![],
            HookSessionInfo::default(),
            HookDecisions::default(),
        );
        let message = test_message();
        let args = serde_json::json!({});
        // Tool "bash" does not match "edit|write": hook never runs (would
        // have blocked via exit 2).
        let outcome = hooks.before_tool_call(&test_ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
    }
}
