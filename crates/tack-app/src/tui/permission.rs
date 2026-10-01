//! Permission dialog + mode state machine (port of the ACP ask/acceptEdits/
//! plan/bypass logic from acp/agent.rs, UI-side).

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tack_agent_core::hooks::{BeforeToolCallContext, BeforeToolCallOutcome};
use tack_tui::components::select_list::{SelectItem, SelectList};
use tack_tui::{Component, Line, Span, Style};
use tokio::sync::oneshot;

use super::lock_recover;
use super::theme::Theme;
use super::tool_render::tool_title;

/// Permission mode (TS pi / ACP session modes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// Prompt for edits/commands; read-only tools free.
    #[default]
    Ask,
    /// File edits free, commands prompt.
    AcceptEdits,
    /// Read-only; bash/edit/write and MCP tools blocked.
    Plan,
    /// No prompts at all.
    Bypass,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::Bypass => "bypass",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            PermissionMode::Ask => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Bypass,
            PermissionMode::Bypass => PermissionMode::Ask,
        }
    }
}

pub use crate::permissions::is_read_only_tool;

/// A permission question from a running tool call.
#[derive(Debug)]
pub struct PermissionQuery {
    pub tool_call_id: String,
    pub tool_name: String,
    pub title: String,
    pub raw_input: Value,
    pub respond: oneshot::Sender<PermissionChoice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionChoice {
    AllowOnce,
    AllowAlways,
    Deny,
}

/// Key for allow-always caching: tool + first identifying arg — with the
/// loaded plugin version embedded for ext__* tools so `tack ext upgrade`
/// (which swaps the code behind the tool name) invalidates stale
/// approvals (see `crate::permissions::allow_always_key`).
fn allow_always_key(agent_dir: &std::path::Path, tool_name: &str, args: &Value) -> String {
    crate::permissions::allow_always_key(agent_dir, tool_name, args)
}

/// Hooks-side permission gate: decides from mode/rules/cache, else forwards
/// a query to the UI and awaits the answer.
#[derive(Debug)]
pub struct TuiPermissionHooks {
    pub mode: Arc<std::sync::Mutex<PermissionMode>>,
    pub allow_always: Arc<std::sync::Mutex<HashSet<String>>>,
    pub queries: tokio::sync::mpsc::UnboundedSender<PermissionQuery>,
    /// Declarative rules (settings permissions.* + persisted allow-always).
    pub rules: crate::permissions::PermissionRules,
    /// Where allow-always answers persist.
    pub agent_dir: std::path::PathBuf,
    /// Managed policy: bypass mode unavailable (treated as acceptEdits).
    pub disable_bypass: bool,
    /// Prompt-injection defense: when untrusted external content (web/MCP)
    /// entered the context this run, mutating tools always prompt — allow
    /// rules and the allow-always cache are bypassed. Reset per run.
    pub untrusted_seen: Arc<std::sync::atomic::AtomicBool>,
    /// PreToolUse hook decisions recorded earlier in the chain (allow =>
    /// skip prompting, ask => force prompting). Consumed per tool_call_id.
    pub hook_decisions: crate::shell_hooks::HookDecisions,
    /// PermissionRequest hooks (engine, groups, session_id): run when the
    /// user would be prompted; an allow/deny verdict replaces the dialog.
    pub permission_request: Option<(
        crate::shell_hooks::HookEngine,
        Vec<crate::shell_hooks::HookGroup>,
        String,
    )>,
    /// Plugin approval chain (tack-RPC `approval/review`): reviewers get
    /// first crack at a decision that would otherwise reach the dialog
    /// (see `crate::approval` for the composition contract). Empty chain
    /// = no plugin participation.
    pub approval_chain: crate::approval::ApprovalChain,
}

#[async_trait::async_trait]
impl tack_agent_core::AgentHooks for TuiPermissionHooks {
    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        // Declarative deny wins over every mode (including bypass).
        if let Some(rule) = self.rules.deny_match(ctx.tool_name, ctx.args) {
            return BeforeToolCallOutcome::Block {
                reason: Some(format!(
                    "denied by permissions.deny rule \"{}\"",
                    rule.render()
                )),
                terminate: false,
            };
        }
        // PreToolUse hook verdicts (Claude permissionDecision semantics):
        // "allow" approves without prompting; "ask" forces the dialog past
        // every fast path below. Declarative deny (above) still wins.
        let hook_decision = lock_recover(&self.hook_decisions).remove(ctx.tool_call_id);
        match hook_decision {
            Some((crate::shell_hooks::HookPermission::Allow, _)) => {
                return BeforeToolCallOutcome::Allow;
            }
            Some((crate::shell_hooks::HookPermission::Deny, reason)) => {
                return BeforeToolCallOutcome::Block {
                    reason: Some(reason.unwrap_or_else(|| "denied by hook".into())),
                    terminate: false,
                };
            }
            Some((crate::shell_hooks::HookPermission::Ask, _)) => {
                // A hook "ask" forces the HUMAN dialog: skip the plugin
                // approval chain too, or a chain claim would approve the
                // call the hook explicitly escalated.
                return self.prompt_user(ctx, true).await;
            }
            None => {}
        }
        let mut mode = *lock_recover(&self.mode);
        // Managed policy defense-in-depth: bypass degrades to acceptEdits.
        if self.disable_bypass && mode == PermissionMode::Bypass {
            mode = PermissionMode::AcceptEdits;
        }
        let read_only = is_read_only_tool(ctx.tool_name, ctx.args);
        match mode {
            PermissionMode::Bypass => return BeforeToolCallOutcome::Allow,
            PermissionMode::Plan => {
                // exit_plan_mode self-gates (its own approval dialog).
                if read_only || ctx.tool_name == "exit_plan_mode" {
                    return BeforeToolCallOutcome::Allow;
                }
                return BeforeToolCallOutcome::Block {
                    reason: Some("plan mode: edits and commands are disabled".into()),
                    terminate: false,
                };
            }
            PermissionMode::AcceptEdits => {
                if read_only || matches!(ctx.tool_name, "edit" | "write") {
                    return BeforeToolCallOutcome::Allow;
                }
            }
            PermissionMode::Ask => {
                if read_only {
                    return BeforeToolCallOutcome::Allow;
                }
            }
        }
        // Declarative allow skips the prompt — but not in an untrusted run:
        // after web/MCP content entered the context, mutating tools always
        // ask (the injection chain "webpage → allow-always bash" is the
        // attack this defends against).
        let untrusted = self
            .untrusted_seen
            .load(std::sync::atomic::Ordering::Relaxed)
            && !read_only;
        if !untrusted {
            if self.rules.allow_match(ctx.tool_name, ctx.args).is_some() {
                return BeforeToolCallOutcome::Allow;
            }
            let key = allow_always_key(&self.agent_dir, ctx.tool_name, ctx.args);
            if lock_recover(&self.allow_always).contains(&key) {
                return BeforeToolCallOutcome::Allow;
            }
        }
        self.prompt_user(ctx, untrusted).await
    }
}

impl TuiPermissionHooks {
    /// The dialog path: unless `force_human` (PreToolUse "ask" verdict, or
    /// an untrusted run with a mutating call — the prompt-injection defense
    /// where a chain claim must not silently approve), the plugin approval
    /// chain gets first crack at the decision; then PermissionRequest hooks
    /// may answer in place of the user (allow/deny); otherwise the overlay
    /// prompt decides.
    async fn prompt_user(
        &self,
        ctx: &BeforeToolCallContext<'_>,
        force_human: bool,
    ) -> BeforeToolCallOutcome {
        if !force_human {
            let policy = lock_recover(&self.mode).as_str().to_string();
            if self
                .approval_chain
                .claims_approval(
                    "tui",
                    ctx.tool_call_id,
                    ctx.tool_name,
                    ctx.args,
                    &policy,
                    self.untrusted_seen
                        .load(std::sync::atomic::Ordering::Relaxed),
                    is_read_only_tool(ctx.tool_name, ctx.args),
                )
                .await
            {
                // allow/reviewed both approve one-shot (nothing persists
                // into the allow-always cache or permissions.json); askUser
                // (explicit defer) and all-pass both reach the built-in
                // prompt below.
                return BeforeToolCallOutcome::Allow;
            }
        }
        if let Some((engine, groups, session_id)) = &self.permission_request
            && !groups.is_empty()
        {
            let input = serde_json::json!({
                "session_id": session_id,
                "transcript_path": serde_json::Value::Null,
                "cwd": engine.cwd(),
                "hook_event_name": "PermissionRequest",
                "tool_name": ctx.tool_name,
                "tool_input": ctx.args,
            });
            let verdict = engine.run(groups, Some(ctx.tool_name), &input).await;
            match verdict.permission {
                Some(crate::shell_hooks::HookPermission::Allow) => {
                    return BeforeToolCallOutcome::Allow;
                }
                Some(crate::shell_hooks::HookPermission::Deny) => {
                    return BeforeToolCallOutcome::Block {
                        reason: Some(
                            verdict
                                .permission_reason
                                .unwrap_or_else(|| "denied by PermissionRequest hook".into()),
                        ),
                        terminate: false,
                    };
                }
                _ => {}
            }
        }
        let key = allow_always_key(&self.agent_dir, ctx.tool_name, ctx.args);
        let (tx, rx) = oneshot::channel();
        let query = PermissionQuery {
            tool_call_id: ctx.tool_call_id.to_string(),
            tool_name: ctx.tool_name.to_string(),
            title: tool_title(ctx.tool_name, ctx.args, 120),
            raw_input: ctx.args.clone(),
            respond: tx,
        };
        if self.queries.send(query).is_err() {
            return BeforeToolCallOutcome::Block {
                reason: Some("UI closed".into()),
                terminate: false,
            };
        }
        match rx.await {
            Ok(PermissionChoice::AllowOnce) => BeforeToolCallOutcome::Allow,
            Ok(PermissionChoice::AllowAlways) => {
                lock_recover(&self.allow_always).insert(key);
                // Persist: the answer survives restarts (permissions.json).
                crate::permissions::persist_allow_always(&self.agent_dir, ctx.tool_name, ctx.args);
                BeforeToolCallOutcome::Allow
            }
            Ok(PermissionChoice::Deny) | Err(_) => BeforeToolCallOutcome::Block {
                reason: Some("denied by user".into()),
                terminate: false,
            },
        }
    }
}

/// The overlay dialog for one permission query.
#[derive(Debug)]
pub struct PermissionDialog {
    pub tool_call_id: String,
    title: String,
    options: SelectList,
    respond: Option<oneshot::Sender<PermissionChoice>>,
    theme: Theme,
    lang: crate::i18n::Lang,
}

impl PermissionDialog {
    pub fn new(query: PermissionQuery, theme: Theme) -> Self {
        Self::with_lang(query, theme, crate::i18n::Lang::default())
    }

    pub fn with_lang(query: PermissionQuery, theme: Theme, lang: crate::i18n::Lang) -> Self {
        let options = SelectList::new(vec![
            SelectItem::new(crate::i18n::t(lang, "permission.yes", &[]), "once"),
            SelectItem::new(crate::i18n::t(lang, "permission.always", &[]), "always"),
            SelectItem::new(crate::i18n::t(lang, "permission.no", &[]), "deny"),
        ]);
        PermissionDialog {
            tool_call_id: query.tool_call_id,
            title: crate::i18n::t(lang, "permission.title", &[("what", &query.title)]),
            options,
            respond: Some(query.respond),
            theme,
            lang,
        }
    }

    fn answer(&mut self, choice: PermissionChoice) {
        if let Some(respond) = self.respond.take() {
            let _ = respond.send(choice);
        }
    }

    /// True when the dialog has answered (the app hides it).
    pub fn resolved(&self) -> bool {
        self.respond.is_none()
    }
}

impl Component for PermissionDialog {
    fn render(&mut self, width: u16) -> Vec<Line> {
        let w = width as usize;
        let mut lines = Vec::new();
        let mut title = Line::new();
        title.push(Span::styled(
            format!(" {} ", self.title),
            self.theme.warning.bold(),
        ));
        title.pad_right(w, Style::default());
        lines.push(title);
        lines.extend(self.options.render(width));
        lines.push(Line::styled(
            crate::i18n::t(self.lang, "permission.hint", &[]),
            self.theme.dim,
        ));
        lines
    }

    fn handle_input(&mut self, event: &tack_tui::input::InputEvent) -> bool {
        let handled = self.options.handle_input(event);
        if let Some(value) = self.options.on_confirm.take() {
            self.answer(match value.as_str() {
                "once" => PermissionChoice::AllowOnce,
                "always" => PermissionChoice::AllowAlways,
                _ => PermissionChoice::Deny,
            });
        }
        if self.options.cancelled {
            self.answer(PermissionChoice::Deny);
        }
        handled
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tack_agent_core::AgentHooks as _;

    fn test_model() -> tack_ai::Model {
        crate::model::resolve_model("anthropic", Some("k3"), std::path::Path::new(".")).unwrap()
    }

    fn hooks(
        mode: PermissionMode,
        untrusted: Arc<std::sync::atomic::AtomicBool>,
    ) -> (
        TuiPermissionHooks,
        tokio::sync::mpsc::UnboundedReceiver<PermissionQuery>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rules = crate::permissions::PermissionRules {
            allow: vec![crate::permissions::Rule::parse("Bash(cargo test)").unwrap()],
            deny: vec![],
        };
        (
            TuiPermissionHooks {
                mode: Arc::new(std::sync::Mutex::new(mode)),
                allow_always: Arc::new(std::sync::Mutex::new(
                    ["bash:cargo build".to_string()].into_iter().collect(),
                )),
                queries: tx,
                rules,
                agent_dir: std::path::PathBuf::new(),
                disable_bypass: false,
                untrusted_seen: untrusted,
                hook_decisions: crate::shell_hooks::HookDecisions::default(),
                permission_request: None,
                approval_chain: crate::approval::ApprovalChain::empty(),
            },
            rx,
        )
    }

    fn ctx<'a>(
        message: &'a tack_ai::AssistantMessage,
        args: &'a serde_json::Value,
    ) -> BeforeToolCallContext<'a> {
        BeforeToolCallContext {
            assistant_message: message,
            tool_call_id: "1",
            tool_name: "bash",
            args,
            context: &[],
        }
    }

    #[tokio::test]
    async fn untrusted_run_forces_prompt_past_allow_rules() {
        let message = tack_ai::AssistantMessage::pending(&test_model());
        let untrusted = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Trusted run: allow rule short-circuits (no UI query).
        let (hooks, mut rx) = hooks(PermissionMode::AcceptEdits, untrusted.clone());
        let args = serde_json::json!({ "command": "cargo test" });
        let outcome = hooks.before_tool_call(&ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        assert!(rx.try_recv().is_err(), "no prompt expected");

        // allow-always cache hit also short-circuits.
        let args = serde_json::json!({ "command": "cargo build" });
        let outcome = hooks.before_tool_call(&ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));

        // Untrusted run: both paths must PROMPT (query arrives).
        untrusted.store(true, std::sync::atomic::Ordering::Relaxed);
        let args = serde_json::json!({ "command": "cargo test" });
        let c = ctx(&message, &args);
        // join!: the hook future only makes progress when polled.
        let (outcome, answered) = tokio::join!(hooks.before_tool_call(&c), async {
            let query = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("prompt expected")
                .expect("query");
            query.respond.send(PermissionChoice::AllowOnce).unwrap();
            true
        });
        assert!(answered);
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
    }

    /// Approval-chain reviewer stub: claims with a scripted action.
    #[derive(Debug)]
    struct ClaimReviewer(crate::approval::ChainAction);

    #[async_trait::async_trait]
    impl crate::approval::ApprovalReviewer for ClaimReviewer {
        async fn review(
            &self,
            request: &crate::approval::ApprovalRequest,
        ) -> Option<crate::approval::ChainDecision> {
            // The params the plugin would see: policy + tool identity.
            assert_eq!(request.approval_policy, "ask");
            assert_eq!(request.tool_name, "bash");
            Some(crate::approval::ChainDecision {
                action: self.0,
                reason: None,
            })
        }
    }

    /// A chain claim (allow) approves the call without any UI prompt —
    /// and persists nothing into the allow-always cache.
    #[tokio::test]
    async fn approval_chain_claim_skips_dialog() {
        let message = tack_ai::AssistantMessage::pending(&test_model());
        let untrusted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut hooks, mut rx) = hooks(PermissionMode::Ask, untrusted);
        hooks.approval_chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        // Matches neither the allow rule nor the allow-always cache: the
        // built-in flow WOULD prompt.
        let args = serde_json::json!({ "command": "rm -rf build" });
        let outcome = hooks.before_tool_call(&ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        assert!(rx.try_recv().is_err(), "no prompt expected");
        // One-shot approve: nothing new persisted into the cache.
        assert_eq!(lock_recover(&hooks.allow_always).len(), 1);
    }

    /// askUser defers to the built-in dialog (the chain claims nothing, so
    /// the human still decides).
    #[tokio::test]
    async fn approval_chain_ask_user_falls_through_to_dialog() {
        let message = tack_ai::AssistantMessage::pending(&test_model());
        let untrusted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut hooks, mut rx) = hooks(PermissionMode::Ask, untrusted);
        hooks.approval_chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::AskUser)),
        );
        let args = serde_json::json!({ "command": "rm -rf build" });
        let c = ctx(&message, &args);
        let (outcome, answered) = tokio::join!(hooks.before_tool_call(&c), async {
            let query = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("prompt expected")
                .expect("query");
            query.respond.send(PermissionChoice::Deny).unwrap();
            true
        });
        assert!(answered);
        assert!(matches!(outcome, BeforeToolCallOutcome::Block { .. }));
    }

    /// Prompt-injection defense: in an untrusted run (web/MCP content in
    /// context), a mutating call must reach the HUMAN even when a plugin
    /// approval-chain reviewer claims allow — the chain is skipped.
    #[tokio::test]
    async fn untrusted_run_skips_approval_chain() {
        let message = tack_ai::AssistantMessage::pending(&test_model());
        let untrusted = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (mut hooks, mut rx) = hooks(PermissionMode::Ask, untrusted);
        hooks.approval_chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        let args = serde_json::json!({ "command": "rm -rf build" });
        let c = ctx(&message, &args);
        let (outcome, answered) = tokio::join!(hooks.before_tool_call(&c), async {
            let query = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("prompt expected")
                .expect("query");
            query.respond.send(PermissionChoice::AllowOnce).unwrap();
            true
        });
        assert!(answered);
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
    }

    /// A PreToolUse "ask" verdict forces the HUMAN dialog past every fast
    /// path — including the plugin approval chain (a chain claim must not
    /// approve a call the hook explicitly escalated).
    #[tokio::test]
    async fn hook_ask_verdict_forces_dialog_past_approval_chain() {
        let message = tack_ai::AssistantMessage::pending(&test_model());
        let untrusted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut hooks, mut rx) = hooks(PermissionMode::Ask, untrusted);
        hooks.approval_chain.push(
            "fake@user".into(),
            Arc::new(ClaimReviewer(crate::approval::ChainAction::Allow)),
        );
        lock_recover(&hooks.hook_decisions).insert(
            "1".to_string(),
            (crate::shell_hooks::HookPermission::Ask, None),
        );
        let args = serde_json::json!({ "command": "rm -rf build" });
        let c = ctx(&message, &args);
        let (outcome, answered) = tokio::join!(hooks.before_tool_call(&c), async {
            let query = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("prompt expected")
                .expect("query");
            query.respond.send(PermissionChoice::Deny).unwrap();
            true
        });
        assert!(answered);
        assert!(matches!(outcome, BeforeToolCallOutcome::Block { .. }));
    }
}
