//! In-process extension API. Replaces pi's TS-module extensions for the Rust
//! port (dynamic loading is explicitly deferred — see the tack plan M8).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tack_ai::Message;

use crate::hooks::{
    AfterToolCallContext, AfterToolCallPatch, AgentHooks, BeforeToolCallContext,
    BeforeToolCallOutcome, NextTurnUpdate, TurnContext,
};
use crate::message::AgentMessage;
use crate::tool::{AgentTool, AgentToolResult};

/// A tack extension: contributes hooks and/or tools.
#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;

    /// Hooks this extension wants in the loop.
    fn hooks(&self) -> Option<Arc<dyn AgentHooks>> {
        None
    }

    /// Extra tools contributed by this extension.
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        Vec::new()
    }

    /// Custom session entries to append at session start
    /// (`(custom_type, data)` pairs).
    fn session_entries(&self) -> Vec<(String, Value)> {
        Vec::new()
    }
}

/// Compose several hook sets into one. Rules:
/// - `before_tool_call`: first `Block` wins.
/// - `transform_context`: applied in order (pipeline).
/// - `after_tool_call`: patches merged in order (later wins per field).
/// - steering/follow-up: concatenated.
/// - `should_stop_after_turn`: any true wins.
/// - `prepare_next_turn`: first `Some` wins.
pub struct HooksChain {
    pub chain: Vec<Arc<dyn AgentHooks>>,
}

impl HooksChain {
    pub fn new(chain: Vec<Arc<dyn AgentHooks>>) -> Self {
        HooksChain { chain }
    }
}

impl std::fmt::Debug for HooksChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HooksChain")
            .field("len", &self.chain.len())
            .finish()
    }
}

#[async_trait]
impl AgentHooks for HooksChain {
    /// `transform_context`: applied in order (COW pipeline): each hook
    /// sees the previous hook's rewrite (or the original borrow when no
    /// hook rewrote yet); `None` means "unchanged" at every stage.
    async fn transform_context(&self, messages: &[AgentMessage]) -> Option<Vec<AgentMessage>> {
        let mut current: Option<Vec<AgentMessage>> = None;
        for hooks in &self.chain {
            let input: &[AgentMessage] = current.as_deref().unwrap_or(messages);
            if let Some(rewritten) = hooks.transform_context(input).await {
                current = Some(rewritten);
            }
        }
        current
    }

    fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        // The first hooks in the chain owns conversion (usually the app's
        // session-aware converter).
        self.chain.first().map_or_else(
            || AgentMessage::default_convert_to_llm(messages),
            |h| h.convert_to_llm(messages),
        )
    }

    /// `compact_for_overflow`: first hook able to recover wins (the
    /// compaction-capable session hooks sit early in the chain).
    async fn compact_for_overflow(&self) -> Option<Vec<AgentMessage>> {
        for hooks in &self.chain {
            if let Some(rebuilt) = hooks.compact_for_overflow().await {
                return Some(rebuilt);
            }
        }
        None
    }

    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        // Chain semantics: first `Block` wins; a `Rewrite` replaces the
        // arguments seen by every LATER hook (and ultimately the tool),
        // mirroring how chained extensions each observe the previous
        // extension's mutation.
        let mut current_args: Option<Value> = None;
        for hooks in &self.chain {
            let outcome = match &current_args {
                None => hooks.before_tool_call(ctx).await,
                Some(args) => {
                    let rewritten = BeforeToolCallContext {
                        assistant_message: ctx.assistant_message,
                        tool_call_id: ctx.tool_call_id,
                        tool_name: ctx.tool_name,
                        args,
                        context: ctx.context,
                    };
                    hooks.before_tool_call(&rewritten).await
                }
            };
            match outcome {
                BeforeToolCallOutcome::Block { .. } => return outcome,
                BeforeToolCallOutcome::Rewrite { args } => current_args = Some(args),
                BeforeToolCallOutcome::Allow => {}
            }
        }
        match current_args {
            Some(args) => BeforeToolCallOutcome::Rewrite { args },
            None => BeforeToolCallOutcome::Allow,
        }
    }

    async fn after_tool_call(
        &self,
        ctx: &AfterToolCallContext<'_>,
        result: &AgentToolResult,
        is_error: bool,
    ) -> Option<AfterToolCallPatch> {
        let mut merged: Option<AfterToolCallPatch> = None;
        for hooks in &self.chain {
            if let Some(patch) = hooks.after_tool_call(ctx, result, is_error).await {
                let entry = merged.get_or_insert_with(AfterToolCallPatch::default);
                if patch.content.is_some() {
                    entry.content = patch.content;
                }
                if patch.details.is_some() {
                    entry.details = patch.details;
                }
                if patch.usage.is_some() {
                    entry.usage = patch.usage;
                }
                if patch.terminate.is_some() {
                    entry.terminate = patch.terminate;
                }
                if patch.is_error.is_some() {
                    entry.is_error = patch.is_error;
                }
            }
        }
        merged
    }

    async fn prepare_next_turn(&self, ctx: &TurnContext<'_>) -> Option<NextTurnUpdate> {
        for hooks in &self.chain {
            if let Some(update) = hooks.prepare_next_turn(ctx).await {
                return Some(update);
            }
        }
        None
    }

    async fn should_stop_after_turn(&self, ctx: &TurnContext<'_>) -> bool {
        for hooks in &self.chain {
            if hooks.should_stop_after_turn(ctx).await {
                return true;
            }
        }
        false
    }

    async fn steering_messages(&self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        for hooks in &self.chain {
            out.extend(hooks.steering_messages().await);
        }
        out
    }

    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        for hooks in &self.chain {
            out.extend(hooks.follow_up_messages().await);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::hooks::AfterToolCallContext;
    use std::sync::Mutex;

    fn test_model() -> tack_ai::Model {
        tack_ai::Model {
            id: "mock".to_string(),
            name: "Mock".to_string(),
            api: "mock".to_string(),
            provider: "mock".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![tack_ai::InputKind::Text],
            cost: tack_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn before_ctx<'a>(
        message: &'a tack_ai::AssistantMessage,
        args: &'a Value,
    ) -> BeforeToolCallContext<'a> {
        BeforeToolCallContext {
            assistant_message: message,
            tool_call_id: "call-1",
            tool_name: "tool",
            args,
            context: &[],
        }
    }

    /// Hook returning a fixed before_tool_call outcome; records the args it
    /// saw and how often it was called.
    struct FixedBeforeHook {
        outcome: BeforeToolCallOutcome,
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl AgentHooks for FixedBeforeHook {
        async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
            self.calls.lock().unwrap().push(ctx.args.clone());
            self.outcome.clone()
        }
    }

    /// `before_tool_call`: the first `Block` short-circuits — later hooks
    /// must not even run (a policy deny is final).
    #[tokio::test]
    async fn before_tool_call_block_short_circuits() {
        let blocker_calls = Arc::new(Mutex::new(Vec::new()));
        let later_calls = Arc::new(Mutex::new(Vec::new()));
        let chain = HooksChain::new(vec![
            Arc::new(FixedBeforeHook {
                outcome: BeforeToolCallOutcome::Block {
                    reason: Some("denied".to_string()),
                    terminate: false,
                },
                calls: blocker_calls,
            }),
            Arc::new(FixedBeforeHook {
                outcome: BeforeToolCallOutcome::Allow,
                calls: later_calls.clone(),
            }),
        ]);
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = serde_json::json!({"x": 0});
        let outcome = chain.before_tool_call(&before_ctx(&message, &args)).await;
        assert!(
            matches!(outcome, BeforeToolCallOutcome::Block { ref reason, .. } if reason.as_deref() == Some("denied")),
            "{outcome:?}"
        );
        assert!(
            later_calls.lock().unwrap().is_empty(),
            "later hook must not run after a Block"
        );
    }

    /// `before_tool_call`: a `Rewrite` replaces the args seen by every
    /// LATER hook, and the final rewrite is what the chain reports.
    #[tokio::test]
    async fn before_tool_call_rewrite_chains_through_later_hooks() {
        let first_calls = Arc::new(Mutex::new(Vec::new()));
        let second_calls = Arc::new(Mutex::new(Vec::new()));
        let chain = HooksChain::new(vec![
            Arc::new(FixedBeforeHook {
                outcome: BeforeToolCallOutcome::Rewrite {
                    args: serde_json::json!({"x": 1}),
                },
                calls: first_calls,
            }),
            Arc::new(FixedBeforeHook {
                outcome: BeforeToolCallOutcome::Rewrite {
                    args: serde_json::json!({"x": 2}),
                },
                calls: second_calls.clone(),
            }),
            Arc::new(FixedBeforeHook {
                outcome: BeforeToolCallOutcome::Allow,
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
        ]);
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = serde_json::json!({"x": 0});
        let outcome = chain.before_tool_call(&before_ctx(&message, &args)).await;
        // The second hook saw the FIRST hook's rewrite, not the original.
        assert_eq!(
            second_calls.lock().unwrap().as_slice(),
            &[serde_json::json!({"x": 1})]
        );
        let BeforeToolCallOutcome::Rewrite { args } = outcome else {
            panic!("expected final Rewrite, got {outcome:?}")
        };
        assert_eq!(args, serde_json::json!({"x": 2}), "last rewrite wins");
    }

    /// `after_tool_call`: patches merge in order; later hooks win per field
    /// and fields they don't touch survive from earlier hooks.
    #[tokio::test]
    async fn after_tool_call_patches_merge_later_wins_per_field() {
        struct PatchHook(AfterToolCallPatch);
        #[async_trait]
        impl AgentHooks for PatchHook {
            async fn after_tool_call(
                &self,
                _ctx: &AfterToolCallContext<'_>,
                _result: &AgentToolResult,
                _is_error: bool,
            ) -> Option<AfterToolCallPatch> {
                Some(self.0.clone())
            }
        }

        let chain = HooksChain::new(vec![
            Arc::new(PatchHook(AfterToolCallPatch {
                content: Some(vec![tack_ai::InputContentBlock::text("from A")]),
                details: Some(serde_json::json!({"a": 1})),
                is_error: Some(false),
                ..Default::default()
            })),
            Arc::new(PatchHook(AfterToolCallPatch {
                details: Some(serde_json::json!({"b": 2})),
                is_error: Some(true),
                ..Default::default()
            })),
        ]);
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = serde_json::json!({});
        let ctx = AfterToolCallContext {
            assistant_message: &message,
            tool_call_id: "call-1",
            tool_name: "tool",
            args: &args,
            context: &[],
        };
        let result = AgentToolResult::text("original");
        let merged = chain
            .after_tool_call(&ctx, &result, false)
            .await
            .expect("merged patch");
        // Content only set by A survives; B's details/is_error win.
        let content = merged.content.expect("content from A");
        assert_eq!(content.len(), 1);
        assert_eq!(merged.details, Some(serde_json::json!({"b": 2})));
        assert_eq!(merged.is_error, Some(true));
        assert!(merged.usage.is_none());
        assert!(merged.terminate.is_none());
    }

    /// `transform_context`: pipeline — each hook receives the previous
    /// hook's output, in chain order.
    #[tokio::test]
    async fn transform_context_pipelines_in_order() {
        struct AppendHook(&'static str);
        #[async_trait]
        impl AgentHooks for AppendHook {
            async fn transform_context(
                &self,
                messages: &[AgentMessage],
            ) -> Option<Vec<AgentMessage>> {
                let mut messages = messages.to_vec();
                messages.push(AgentMessage::user(self.0));
                Some(messages)
            }
        }
        let chain = HooksChain::new(vec![Arc::new(AppendHook("a")), Arc::new(AppendHook("b"))]);
        let out = chain
            .transform_context(&[AgentMessage::user("orig")])
            .await
            .expect("append hooks rewrite the context");
        let texts: Vec<String> = out
            .iter()
            .map(|m| match m {
                AgentMessage::User(u) => match &u.content {
                    tack_ai::UserContent::Text(t) => t.clone(),
                    _ => panic!(),
                },
                _ => panic!(),
            })
            .collect();
        assert_eq!(texts, vec!["orig", "a", "b"]);
    }

    /// `transform_context` COW contract: a chain where no hook rewrites
    /// anything returns `None` — the loop then uses the borrowed context
    /// with zero clones.
    #[tokio::test]
    async fn transform_context_pass_through_returns_none() {
        let chain = HooksChain::new(vec![Arc::new(crate::hooks::NoopHooks)]);
        let out = chain.transform_context(&[AgentMessage::user("orig")]).await;
        assert!(out.is_none());
    }

    /// `prepare_next_turn`: the first `Some` wins; `should_stop_after_turn`:
    /// any true wins.
    #[tokio::test]
    async fn prepare_next_turn_first_some_wins_and_stop_any_true() {
        struct PrepareHook(Option<&'static str>, bool);
        #[async_trait]
        impl AgentHooks for PrepareHook {
            async fn prepare_next_turn(
                &self,
                _ctx: &crate::hooks::TurnContext<'_>,
            ) -> Option<NextTurnUpdate> {
                self.0.map(|id| {
                    let mut model = test_model();
                    model.id = id.to_string();
                    NextTurnUpdate {
                        model: Some(model),
                        thinking_level: None,
                    }
                })
            }
            async fn should_stop_after_turn(&self, _ctx: &crate::hooks::TurnContext<'_>) -> bool {
                self.1
            }
        }

        let chain = HooksChain::new(vec![
            Arc::new(PrepareHook(None, false)),
            Arc::new(PrepareHook(Some("first"), false)),
            Arc::new(PrepareHook(Some("second"), true)),
        ]);
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let ctx = crate::hooks::TurnContext {
            message: &message,
            tool_results: &[],
            new_messages: &[],
        };
        let update = chain.prepare_next_turn(&ctx).await.expect("update");
        assert_eq!(update.model.unwrap().id, "first");
        assert!(chain.should_stop_after_turn(&ctx).await);
    }
}
