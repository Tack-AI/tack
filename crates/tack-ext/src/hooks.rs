//! AgentHooks bridge over tack-RPC v3: forwards tool-call interception,
//! context transform, and result patching to the plugin's `hooks/*`
//! handlers. Plugins that did not declare a hook pass through untouched
//! (the host never calls undeclared capabilities).

use tack_agent_core::{
    AfterToolCallContext, AfterToolCallPatch, AgentHooks, AgentToolResult, BeforeToolCallContext,
    BeforeToolCallOutcome,
};

use crate::rpc3::{
    AfterToolCallParams, BeforeToolCallParams, ContentBlock, ContentBlockKind, HookCapabilities,
    ToolCall, ToolOutput, TransformContextParams, VerdictAction,
};
use crate::v3::HostClient;

/// How hook failures (timeout, dead plugin, malformed reply) are treated.
/// TS extensions crash-isolate (fail-open), which stays the default;
/// security-sensitive deployments can opt into fail-closed so a broken
/// interceptor BLOCKS the tool call instead of waving it through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FailMode {
    /// Hook failure lets the tool call through (TS-compatible).
    #[default]
    Open,
    /// Hook failure blocks the tool call.
    Closed,
}

impl FailMode {
    /// From a manifest/settings value (`failMode: "closed"`).
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("closed") => FailMode::Closed,
            _ => FailMode::Open,
        }
    }
}

/// One v3 plugin's hook bridge (composed into the app's HooksChain).
pub struct ExtHooks {
    pub(crate) client: HostClient,
    capabilities: HookCapabilities,
    fail_mode: FailMode,
}

impl std::fmt::Debug for ExtHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtHooks")
            .field("fail_mode", &self.fail_mode)
            .finish()
    }
}

impl ExtHooks {
    pub fn new(client: HostClient, capabilities: HookCapabilities) -> Self {
        Self::with_fail_mode(client, capabilities, FailMode::default())
    }

    pub fn with_fail_mode(
        client: HostClient,
        capabilities: HookCapabilities,
        fail_mode: FailMode,
    ) -> Self {
        ExtHooks {
            client,
            capabilities,
            fail_mode,
        }
    }

    /// Intercept failure: always loud in the logs; fail-open lets the call
    /// through (TS compat), fail-closed blocks it.
    fn intercept_failed(&self, what: &str) -> BeforeToolCallOutcome {
        tracing::warn!(
            "hooks/beforeToolCall failed ({what}); fail mode {:?}",
            self.fail_mode
        );
        match self.fail_mode {
            FailMode::Open => BeforeToolCallOutcome::Allow,
            FailMode::Closed => BeforeToolCallOutcome::Block {
                reason: Some(format!(
                    "tool call blocked: extension intercept unavailable ({what})"
                )),
                terminate: false,
            },
        }
    }

    fn wants_before(&self) -> bool {
        self.capabilities.before_tool_call.unwrap_or(false)
    }

    fn wants_transform(&self) -> bool {
        self.capabilities.transform_context.unwrap_or(false)
    }

    fn wants_after(&self) -> bool {
        self.capabilities.after_tool_call.unwrap_or(false)
    }
}

/// AgentToolResult → the rpc3 ToolOutput wire shape (best-effort block
/// mapping; non-text/image detail is dropped).
fn to_rpc3_output(result: &AgentToolResult) -> ToolOutput {
    let content = result
        .content
        .iter()
        .map(|block| match block {
            tack_ai::InputContentBlock::Text { text, .. } => ContentBlock {
                r#type: ContentBlockKind::Text,
                text: Some(text.clone()),
                mime_type: None,
                data: None,
            },
            tack_ai::InputContentBlock::Image { data, mime_type } => ContentBlock {
                r#type: ContentBlockKind::Image,
                text: None,
                mime_type: Some(mime_type.clone()),
                data: Some(data.clone()),
            },
        })
        .collect();
    ToolOutput {
        content,
        details: Some(result.details.clone()),
        is_error: None,
    }
}

/// rpc3 content blocks → agent input blocks (unknown kinds are skipped).
fn from_rpc3_blocks(blocks: Vec<ContentBlock>) -> Vec<tack_ai::InputContentBlock> {
    blocks
        .into_iter()
        .map(|block| match block.r#type {
            ContentBlockKind::Text => {
                tack_ai::InputContentBlock::text(block.text.unwrap_or_default())
            }
            ContentBlockKind::Image => tack_ai::InputContentBlock::Image {
                data: block.data.unwrap_or_default(),
                mime_type: block.mime_type.unwrap_or_else(|| "image/png".to_string()),
            },
        })
        .collect()
}

#[async_trait::async_trait]
impl AgentHooks for ExtHooks {
    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        if !self.wants_before() {
            return BeforeToolCallOutcome::Allow;
        }
        let params = BeforeToolCallParams {
            tool_call: ToolCall {
                tool_call_id: ctx.tool_call_id.to_string(),
                tool_name: ctx.tool_name.to_string(),
                arguments: ctx.args.clone(),
            },
            assistant_message: serde_json::to_value(ctx.assistant_message).ok(),
        };
        // Intercept failures are fail-open by default (TS extensions
        // crash-isolate the same way); fail-closed deployments block.
        let verdict = match self.client.before_tool_call(&params).await {
            Ok(verdict) => verdict,
            Err(e) => return self.intercept_failed(&e.to_string()),
        };
        match verdict.action {
            VerdictAction::Allow => BeforeToolCallOutcome::Allow,
            VerdictAction::Deny => BeforeToolCallOutcome::Block {
                reason: Some(
                    verdict
                        .reason
                        .unwrap_or_else(|| "denied by extension".to_string()),
                ),
                terminate: false,
            },
            VerdictAction::Rewrite => match verdict.arguments {
                Some(args) => BeforeToolCallOutcome::Rewrite { args },
                None => self.intercept_failed("rewrite verdict without arguments"),
            },
        }
    }

    /// Context transform mutation point (`hooks/transformContext`): the
    /// plugin receives the full message list before every LLM call and may
    /// return a replacement. Opt-in via capabilities — fail-open on any
    /// error (timeout/dead plugin/round-trip mismatch).
    async fn transform_context(
        &self,
        messages: &[tack_agent_core::AgentMessage],
    ) -> Option<Vec<tack_agent_core::AgentMessage>> {
        if !self.wants_transform() || messages.is_empty() {
            return None;
        }
        let payload: Vec<serde_json::Value> = messages
            .iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();
        let params = TransformContextParams { messages: payload };
        let result = self.client.transform_context(&params).await.ok()??;
        match serde_json::from_value::<Vec<tack_agent_core::AgentMessage>>(
            serde_json::Value::Array(result.messages),
        ) {
            Ok(transformed) => Some(transformed),
            Err(e) => {
                tracing::warn!("hooks/transformContext returned invalid messages: {e}");
                None
            }
        }
    }

    /// Result patch mutation point (`hooks/afterToolCall`): per-field
    /// patch, fail-open (errors drop the patch, never the result).
    async fn after_tool_call(
        &self,
        ctx: &AfterToolCallContext<'_>,
        result: &AgentToolResult,
        is_error: bool,
    ) -> Option<AfterToolCallPatch> {
        if !self.wants_after() {
            return None;
        }
        let params = AfterToolCallParams {
            tool_call: ToolCall {
                tool_call_id: ctx.tool_call_id.to_string(),
                tool_name: ctx.tool_name.to_string(),
                arguments: ctx.args.clone(),
            },
            result: to_rpc3_output(result),
            is_error,
        };
        let patch = self.client.after_tool_call(&params).await.ok()??;
        let usage = patch
            .usage
            .and_then(|u| serde_json::from_value::<tack_ai::Usage>(u).ok());
        Some(AfterToolCallPatch {
            content: patch.content.map(from_rpc3_blocks),
            details: patch.details,
            usage,
            terminate: patch.terminate,
            is_error: patch.is_error,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::rpc3::ErrorObject;
    use crate::v3::{JsonRpcPeer, PeerHandler};
    use serde_json::{Value, json};
    use std::sync::Arc;

    struct Noop;

    #[async_trait::async_trait]
    impl PeerHandler for Noop {}

    /// Fake plugin replying to every request with a fixed value.
    struct Scripted(Value);

    #[async_trait::async_trait]
    impl PeerHandler for Scripted {
        async fn handle_request(
            &self,
            _method: &str,
            _params: Value,
        ) -> Result<Value, ErrorObject> {
            Ok(self.0.clone())
        }
    }

    /// (ExtHooks, plugin peer guard) connected over an in-memory duplex.
    fn hooks_pair(reply: Value, capabilities: HookCapabilities) -> (ExtHooks, Arc<JsonRpcPeer>) {
        let (s1, s2) = tokio::io::duplex(8192);
        let (r1, w1) = tokio::io::split(s1);
        let (r2, w2) = tokio::io::split(s2);
        let client = HostClient::new(JsonRpcPeer::new(r1, w1, Arc::new(Noop)));
        let plugin = JsonRpcPeer::new(r2, w2, Arc::new(Scripted(reply)));
        (ExtHooks::new(client, capabilities), plugin)
    }

    fn caps(before: bool, transform: bool, after: bool) -> HookCapabilities {
        HookCapabilities {
            before_tool_call: Some(before),
            transform_context: Some(transform),
            after_tool_call: Some(after),
            approval_review: None,
        }
    }

    fn before_ctx<'a>(
        message: &'a tack_ai::AssistantMessage,
        args: &'a Value,
    ) -> BeforeToolCallContext<'a> {
        BeforeToolCallContext {
            assistant_message: message,
            tool_call_id: "call-1",
            tool_name: "bash",
            args,
            context: &[],
        }
    }

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

    #[tokio::test]
    async fn undeclared_hook_passes_through_without_call() {
        // The scripted plugin would reply to anything; with the capability
        // unset the host must not call at all.
        let (hooks, _plugin) = hooks_pair(json!({"action": "deny"}), caps(false, false, false));
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = json!({});
        let outcome = hooks.before_tool_call(&before_ctx(&message, &args)).await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
    }

    #[tokio::test]
    async fn deny_verdict_blocks_with_reason() {
        let (hooks, _plugin) = hooks_pair(
            json!({"action": "deny", "reason": "nope"}),
            caps(true, false, false),
        );
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = json!({});
        let outcome = hooks.before_tool_call(&before_ctx(&message, &args)).await;
        let BeforeToolCallOutcome::Block { reason, .. } = outcome else {
            panic!("expected Block, got {outcome:?}")
        };
        assert_eq!(reason.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn rewrite_verdict_replaces_arguments() {
        let (hooks, _plugin) = hooks_pair(
            json!({"action": "rewrite", "arguments": {"command": "ls -la"}}),
            caps(true, false, false),
        );
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = json!({"command": "ls"});
        let outcome = hooks.before_tool_call(&before_ctx(&message, &args)).await;
        let BeforeToolCallOutcome::Rewrite { args } = outcome else {
            panic!("expected Rewrite, got {outcome:?}")
        };
        assert_eq!(args, json!({"command": "ls -la"}));
    }

    #[tokio::test]
    async fn transform_context_replaces_messages() {
        let replacement =
            serde_json::to_value(vec![tack_agent_core::AgentMessage::user("replaced")]).unwrap();
        let (hooks, _plugin) = hooks_pair(
            json!({"messages": [replacement[0].clone()]}),
            caps(false, true, false),
        );
        let out = hooks
            .transform_context(&[tack_agent_core::AgentMessage::user("original")])
            .await
            .expect("declared hook rewrites");
        assert_eq!(out.len(), 1);
        let tack_agent_core::AgentMessage::User(user) = &out[0] else {
            panic!("expected user message")
        };
        let tack_ai::UserContent::Text(text) = &user.content else {
            panic!("expected text")
        };
        assert_eq!(text, "replaced");
    }

    #[tokio::test]
    async fn after_tool_call_patch_maps_fields() {
        let (hooks, _plugin) = hooks_pair(
            json!({"details": {"patched": true}, "isError": true}),
            caps(false, false, true),
        );
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = json!({});
        let ctx = AfterToolCallContext {
            assistant_message: &message,
            tool_call_id: "call-1",
            tool_name: "bash",
            args: &args,
            context: &[],
        };
        let result = AgentToolResult::text("original");
        let patch = hooks
            .after_tool_call(&ctx, &result, false)
            .await
            .expect("patch");
        assert_eq!(patch.details, Some(json!({"patched": true})));
        assert_eq!(patch.is_error, Some(true));
        assert!(patch.content.is_none());
    }

    #[tokio::test]
    async fn dead_plugin_fail_open_allows_fail_closed_blocks() {
        async fn dead_hooks(fail_mode: FailMode) -> ExtHooks {
            let (s1, s2) = tokio::io::duplex(1024);
            let (r1, w1) = tokio::io::split(s1);
            let client = HostClient::new(JsonRpcPeer::new(r1, w1, Arc::new(Noop)));
            drop(s2);
            let hooks = ExtHooks::with_fail_mode(client, caps(true, false, false), fail_mode);
            hooks.client.peer().wait_dead().await;
            hooks
        }
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = json!({});
        let outcome = dead_hooks(FailMode::Open)
            .await
            .before_tool_call(&before_ctx(&message, &args))
            .await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Allow));
        let outcome = dead_hooks(FailMode::Closed)
            .await
            .before_tool_call(&before_ctx(&message, &args))
            .await;
        assert!(matches!(outcome, BeforeToolCallOutcome::Block { .. }));
    }

    #[test]
    fn fail_mode_from_setting() {
        assert_eq!(FailMode::from_setting(Some("closed")), FailMode::Closed);
        assert_eq!(FailMode::from_setting(Some("open")), FailMode::Open);
        assert_eq!(FailMode::from_setting(None), FailMode::Open);
        assert_eq!(FailMode::from_setting(Some("garbage")), FailMode::Open);
    }
}
