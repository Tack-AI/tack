//! Plugin approval chain (tack-RPC `approval/review`): when the built-in
//! permission flow is about to prompt a human, active plugins that declared
//! `capabilities.hooks.approvalReview` get first crack at the decision —
//! every reviewer is consulted in load order, the first `allow`/`reviewed`
//! claim wins, and a pass (null) or `askUser` moves to the next reviewer.
//!
//! Composition with the built-in permission modes (the roadmap's "approval
//! chain scope" question, resolved here):
//!
//! ```text
//! deny rules → PreToolUse hook decisions → mode gate (plan/acceptEdits/bypass)
//!   → allow rules + allow-always cache → PLUGIN APPROVAL CHAIN
//!   → PermissionRequest hooks → user prompt (dialog / RPC event)
//! ```
//!
//! The chain is consulted exactly at the point where the flow would ask a
//! human — never for calls an allow rule or mode already approved (plugins
//! observe those via `hooks/beforeToolCall` instead), and never in bypass
//! mode or headless print runs (no approval is needed there). Two more
//! guards sit in front of it:
//!
//! - Prompt-injection defense: once untrusted web/MCP content entered the
//!   context this run, mutating (non-read-only) calls skip the chain — the
//!   human must be asked; a chain claim must not silently approve.
//! - A PreToolUse `permissionDecision: "ask"` verdict forces the human
//!   dialog past every fast path, the chain included.
//!
//! Plugin `hooks/beforeToolCall` bridges run BEFORE the permission layer in
//! every surface's hook chain, so the reviewers (and the dialog) see the
//! FINAL, post-rewrite arguments.
//!
//! Claimed decisions map to the wire actions: `allow`/`reviewed` both approve
//! the call (one-shot; nothing is persisted into allow-always state) —
//! `reviewed` exists so a reviewer that performed its own vetting (e.g. an
//! LLM pass or its own UI) is distinguishable from a blanket auto-allow in
//! the audit event. `askUser` claims nothing: it is logged for audit and
//! iteration continues — an early cautious reviewer must not wedge a later
//! auto-approver (and vice versa, no reviewer can veto the human's option:
//! if nobody claims `allow`/`reviewed`, the built-in prompt still runs).
//!
//! Failure discipline: a reviewer error (including `unsupported_capability`
//! from carriers that do not implement `approval/review`, and the standard
//! 30s request timeout) degrades to "pass" — a broken reviewer must not
//! wedge every prompt. Claims and failures are structured tracing events
//! (target `plugin_approval`), so managed `auditSink` deployments see them
//! like any policy decision.

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

/// One approval question, built at the point the built-in flow would prompt.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    /// Unique id for this approval (the triggering tool call's id — stable
    /// across surfaces and correlatable with `hooks/beforeToolCall`).
    pub approval_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    /// The session's active approval policy name ("ask" | "acceptEdits" |
    /// "plan" | "bypass") — the wire `approvalPolicy`.
    pub approval_policy: String,
    /// Host-gathered permission evidence (untrusted-content state, surface,
    /// read-only classification) — the wire `evidence` object.
    pub evidence: Value,
}

/// The action half of a claimed decision; mirrors the rpc3
/// `ApprovalDecisionAction` enum without naming tack-ext types (this module
/// compiles without the `ext` feature).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainAction {
    /// Auto-approve (one-shot).
    Allow,
    /// Approve after the reviewer's own vetting (audit-distinguishable).
    Reviewed,
    /// Defer to the built-in human prompt.
    AskUser,
}

impl ChainAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ChainAction::Allow => "allow",
            ChainAction::Reviewed => "reviewed",
            ChainAction::AskUser => "askUser",
        }
    }
}

/// A reviewer's claimed decision (`None` from the reviewer = pass).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainDecision {
    pub action: ChainAction,
    pub reason: Option<String>,
}

/// One chain participant. Implemented in `extension_host` (cfg `ext`) over
/// `Arc<dyn PluginConnection>`; tests use fakes. Returning `None` passes to
/// the next reviewer — reviewer implementations map errors to `None`
/// themselves (fail-open, see module docs).
#[async_trait::async_trait]
pub trait ApprovalReviewer: fmt::Debug + Send + Sync {
    async fn review(&self, request: &ApprovalRequest) -> Option<ChainDecision>;
}

/// The ordered reviewer chain for a session. Empty chain = zero-cost fast
/// path (`is_empty` short-circuits before any params are built).
#[derive(Clone, Default)]
pub struct ApprovalChain {
    /// (plugin id, reviewer) pairs — the id feeds the audit event.
    reviewers: Vec<(String, Arc<dyn ApprovalReviewer>)>,
}

impl fmt::Debug for ApprovalChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovalChain")
            .field("reviewers", &self.reviewers.len())
            .finish()
    }
}

impl ApprovalChain {
    pub fn empty() -> Self {
        ApprovalChain::default()
    }

    pub fn is_empty(&self) -> bool {
        self.reviewers.is_empty()
    }

    pub fn push(&mut self, plugin_id: String, reviewer: Arc<dyn ApprovalReviewer>) {
        self.reviewers.push((plugin_id, reviewer));
    }

    /// Offer the request to EVERY reviewer in load order; the first
    /// `allow`/`reviewed` claim wins. `askUser` is logged (audit) but does
    /// not terminate the chain — it claims nothing. `None` means no
    /// reviewer claimed approval (built-in prompt decides).
    pub async fn review(&self, request: &ApprovalRequest) -> Option<ChainDecision> {
        for (plugin_id, reviewer) in &self.reviewers {
            let Some(decision) = reviewer.review(request).await else {
                continue;
            };
            if decision.action == ChainAction::AskUser {
                // Deferred to the human, but the chain continues: a later
                // reviewer may still claim allow/reviewed.
                tracing::info!(
                    target: "plugin_approval",
                    decision = "deferred",
                    plugin = plugin_id.as_str(),
                    action = decision.action.as_str(),
                    continues = true,
                    approval_id = request.approval_id.as_str(),
                    tool = request.tool_name.as_str(),
                    policy = request.approval_policy.as_str(),
                    reason = decision.reason.as_deref().unwrap_or(""),
                );
                continue;
            }
            tracing::info!(
                target: "plugin_approval",
                decision = "claimed",
                plugin = plugin_id.as_str(),
                action = decision.action.as_str(),
                approval_id = request.approval_id.as_str(),
                tool = request.tool_name.as_str(),
                policy = request.approval_policy.as_str(),
                reason = decision.reason.as_deref().unwrap_or(""),
            );
            return Some(decision);
        }
        tracing::debug!(
            target: "plugin_approval",
            decision = "pass",
            approval_id = request.approval_id.as_str(),
            tool = request.tool_name.as_str(),
            reviewers = self.reviewers.len(),
        );
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fake reviewer: scripted response + call recording.
    #[derive(Debug)]
    struct FakeReviewer {
        response: Option<ChainDecision>,
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
    }

    impl FakeReviewer {
        fn claiming(action: ChainAction) -> Self {
            FakeReviewer {
                response: Some(ChainDecision {
                    action,
                    reason: Some("because".into()),
                }),
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn passing() -> Self {
            FakeReviewer {
                response: None,
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ApprovalReviewer for FakeReviewer {
        async fn review(&self, request: &ApprovalRequest) -> Option<ChainDecision> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.seen.lock().unwrap().push(request.approval_id.clone());
            self.response.clone()
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            approval_id: "call-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: serde_json::json!({"command": "rm -rf build"}),
            approval_policy: "ask".into(),
            evidence: serde_json::json!({"surface": "test"}),
        }
    }

    #[tokio::test]
    async fn empty_chain_passes() {
        let chain = ApprovalChain::empty();
        assert!(chain.is_empty());
        assert_eq!(chain.review(&request()).await, None);
    }

    #[tokio::test]
    async fn all_pass_returns_none() {
        let mut chain = ApprovalChain::empty();
        let a = Arc::new(FakeReviewer::passing());
        let b = Arc::new(FakeReviewer::passing());
        chain.push("a".into(), a.clone());
        chain.push("b".into(), b.clone());
        assert_eq!(chain.review(&request()).await, None);
        assert_eq!(a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(b.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn first_claim_wins_and_short_circuits() {
        let mut chain = ApprovalChain::empty();
        let a = Arc::new(FakeReviewer::passing());
        let b = Arc::new(FakeReviewer::claiming(ChainAction::Allow));
        let c = Arc::new(FakeReviewer::claiming(ChainAction::AskUser));
        chain.push("a".into(), a.clone());
        chain.push("b".into(), b.clone());
        chain.push("c".into(), c.clone());
        let decision = chain.review(&request()).await.expect("b claims");
        assert_eq!(decision.action, ChainAction::Allow);
        assert_eq!(decision.reason.as_deref(), Some("because"));
        assert_eq!(a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(b.calls.load(Ordering::Relaxed), 1);
        // c sits behind the winning claim: never consulted.
        assert_eq!(c.calls.load(Ordering::Relaxed), 0);
    }

    /// askUser claims nothing: it does NOT terminate the chain. A lone
    /// askUser reviewer leaves the chain undecided (None => built-in
    /// prompt), and a later reviewer's allow/reviewed claim still wins.
    #[tokio::test]
    async fn ask_user_is_not_a_claim() {
        let mut chain = ApprovalChain::empty();
        let a = Arc::new(FakeReviewer::claiming(ChainAction::AskUser));
        chain.push("a".into(), a.clone());
        assert_eq!(chain.review(&request()).await, None);
        assert_eq!(a.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ask_user_defers_and_later_claim_wins() {
        let mut chain = ApprovalChain::empty();
        let a = Arc::new(FakeReviewer::claiming(ChainAction::AskUser));
        let b = Arc::new(FakeReviewer::claiming(ChainAction::Allow));
        chain.push("a".into(), a.clone());
        chain.push("b".into(), b.clone());
        let decision = chain.review(&request()).await.expect("b claims");
        assert_eq!(decision.action, ChainAction::Allow);
        // Both reviewers were consulted: the early askUser did not wedge
        // the later auto-approver.
        assert_eq!(a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(b.calls.load(Ordering::Relaxed), 1);
    }
}
