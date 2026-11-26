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
use crate::v3::PluginConnection;
use std::sync::Arc;

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
    pub(crate) client: Arc<dyn PluginConnection>,
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
    pub fn new(client: Arc<dyn PluginConnection>, capabilities: HookCapabilities) -> Self {
        Self::with_fail_mode(client, capabilities, FailMode::default())
    }

    pub fn with_fail_mode(
        client: Arc<dyn PluginConnection>,
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

    fn wants_transform_with_system(&self) -> bool {
        self.capabilities
            .transform_context_with_system
            .unwrap_or(false)
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

/// Serialize a complete hook input or none. Silently skipping one bad
/// message could let a replacement accidentally delete history.
fn context_params(
    messages: &[tack_agent_core::AgentMessage],
    method: &str,
) -> Option<TransformContextParams> {
    let mut payload = Vec::with_capacity(messages.len());
    for message in messages {
        match serde_json::to_value(message) {
            Ok(value) => payload.push(value),
            Err(e) => {
                tracing::warn!("{method} skipped: a message failed to serialize: {e}");
                return None;
            }
        }
    }
    Some(TransformContextParams { messages: payload })
}

fn context_result(
    result: crate::rpc3::TransformContextResult,
    method: &str,
) -> Option<Vec<tack_agent_core::AgentMessage>> {
    match serde_json::from_value::<Vec<tack_agent_core::AgentMessage>>(serde_json::Value::Array(
        result.messages,
    )) {
        Ok(transformed) => Some(transformed),
        Err(e) => {
            tracing::warn!("{method} returned invalid messages: {e}");
            None
        }
    }
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

    /// Ordinary context handlers see only conversation messages. A changed
    /// result gets one replayed leading system head; unchanged/null results
    /// preserve the original transcript, including mid-conversation systems.
    async fn transform_context(
        &self,
        messages: &[tack_agent_core::AgentMessage],
    ) -> Option<Vec<tack_agent_core::AgentMessage>> {
        if !self.wants_transform() || messages.is_empty() {
            return None;
        }
        let visible = messages
            .iter()
            .filter(|message| !matches!(message, tack_agent_core::AgentMessage::System(_)))
            .cloned()
            .collect::<Vec<_>>();
        let params = context_params(&visible, "hooks/transformContext")?;
        let result = self.client.transform_context(&params).await.ok()??;
        let returned = context_result(result, "hooks/transformContext")?;

        // JSON-RPC cannot preserve JS object identity. Structural equality is
        // the closest approximation to upstream's same-element unchanged
        // check, and retains mid-system placement for pass-through handlers.
        if returned == visible {
            return None;
        }
        let head = tack_ai::transcript::get_current_system_message(messages);
        let mut restored = Vec::with_capacity(returned.len() + usize::from(head.is_some()));
        if let Some(head) = head {
            restored.push(tack_agent_core::AgentMessage::System(head));
        }
        restored.extend(returned);
        Some(restored)
    }

    /// Full handlers run in the chain's second phase and own the complete
    /// transcript. In particular, deleting system messages is honored.
    async fn transform_context_with_system(
        &self,
        messages: &[tack_agent_core::AgentMessage],
    ) -> Option<Vec<tack_agent_core::AgentMessage>> {
        if !self.wants_transform_with_system() {
            return None;
        }
        let had_leading_system = matches!(
            messages.first(),
            Some(tack_agent_core::AgentMessage::System(_))
        );
        let params = context_params(messages, "hooks/transformContextWithSystem")?;
        let result = self
            .client
            .transform_context_with_system(&params)
            .await
            .ok()??;
        let transformed = context_result(result, "hooks/transformContextWithSystem")?;
        if had_leading_system
            && !matches!(
                transformed.first(),
                Some(tack_agent_core::AgentMessage::System(_))
            )
        {
            tracing::warn!(
                "hooks/transformContextWithSystem removed the leading system message; honoring its authoritative output"
            );
        }
        Some(transformed)
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
    use crate::v3::{HostClient, JsonRpcPeer, PeerHandler};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    struct Noop;

    #[async_trait::async_trait]
    impl PeerHandler for Noop {}

    struct ContextScript {
        name: &'static str,
        ordinary: Value,
        full: Value,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        order: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl PeerHandler for ContextScript {
        async fn handle_request(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            match method {
                crate::rpc3::method::HOOKS_TRANSFORM_CONTEXT => {
                    self.order
                        .lock()
                        .unwrap()
                        .push(format!("ordinary-{}", self.name));
                    Ok(self.ordinary.clone())
                }
                crate::rpc3::method::HOOKS_TRANSFORM_CONTEXT_WITH_SYSTEM => {
                    self.order
                        .lock()
                        .unwrap()
                        .push(format!("full-{}", self.name));
                    Ok(self.full.clone())
                }
                _ => unreachable!("unexpected method {method}"),
            }
        }
    }

    fn context_hooks_pair(
        name: &'static str,
        ordinary: Value,
        full: Value,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        order: Arc<Mutex<Vec<String>>>,
    ) -> (ExtHooks, Arc<JsonRpcPeer>) {
        let (s1, s2) = tokio::io::duplex(8192);
        let (r1, w1) = tokio::io::split(s1);
        let (r2, w2) = tokio::io::split(s2);
        let client = HostClient::new(JsonRpcPeer::new(r1, w1, Arc::new(Noop)));
        let plugin = JsonRpcPeer::new(
            r2,
            w2,
            Arc::new(ContextScript {
                name,
                ordinary,
                full,
                calls,
                order,
            }),
        );
        let capabilities = HookCapabilities {
            transform_context: Some(true),
            transform_context_with_system: Some(true),
            ..Default::default()
        };
        (ExtHooks::new(Arc::new(client), capabilities), plugin)
    }

    fn context_reply(messages: &[tack_agent_core::AgentMessage]) -> Value {
        json!({"messages": messages})
    }

    fn declared_system(content: &str) -> tack_agent_core::AgentMessage {
        tack_agent_core::AgentMessage::System(tack_ai::SystemMessage {
            content: tack_ai::UserContent::Text(content.to_string()),
            sections: None,
            tools_added: Some(vec![tack_ai::ToolDefinition {
                name: "declared".to_string(),
                description: "declared tool".to_string(),
                parameters: json!({"type": "object"}),
                defer_loading: false,
                constrained_sampling: None,
            }]),
            tools_removed: None,
            timestamp: 0,
        })
    }

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
        (ExtHooks::new(Arc::new(client), capabilities), plugin)
    }

    fn caps(before: bool, transform: bool, after: bool) -> HookCapabilities {
        HookCapabilities {
            before_tool_call: Some(before),
            transform_context: Some(transform),
            transform_context_with_system: Some(false),
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
    async fn ordinary_context_excludes_system_and_full_phase_runs_after_every_ordinary_hook() {
        use tack_agent_core::{AgentHooks, AgentMessage, HooksChain};

        struct Internal {
            order: Arc<Mutex<Vec<String>>>,
            seen: Arc<Mutex<Vec<AgentMessage>>>,
        }
        #[async_trait::async_trait]
        impl AgentHooks for Internal {
            async fn transform_context(
                &self,
                messages: &[AgentMessage],
            ) -> Option<Vec<AgentMessage>> {
                self.order.lock().unwrap().push("internal".to_string());
                *self.seen.lock().unwrap() = messages.to_vec();
                None
            }
        }

        let original = vec![
            declared_system("base"),
            AgentMessage::user("one"),
            AgentMessage::System(tack_ai::SystemMessage {
                content: tack_ai::UserContent::Text("mid".to_string()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
            }),
            AgentMessage::user("two"),
        ];
        let canonical = original.clone();
        let order = Arc::new(Mutex::new(Vec::new()));
        let calls_a = Arc::new(Mutex::new(Vec::new()));
        let calls_b = Arc::new(Mutex::new(Vec::new()));
        let (a, _peer_a) = context_hooks_pair(
            "a",
            context_reply(&[AgentMessage::user("from-a")]),
            Value::Null,
            calls_a.clone(),
            order.clone(),
        );
        let (b, _peer_b) = context_hooks_pair(
            "b",
            context_reply(&[AgentMessage::user("from-b")]),
            Value::Null,
            calls_b.clone(),
            order.clone(),
        );
        let internal_seen = Arc::new(Mutex::new(Vec::new()));
        let internal = Internal {
            order: order.clone(),
            seen: internal_seen.clone(),
        };
        let chain = HooksChain::new(vec![Arc::new(a), Arc::new(internal), Arc::new(b)]);

        let ordinary = chain
            .transform_context(&original)
            .await
            .expect("ordinary rewrite");
        let full = chain.transform_context_with_system(&ordinary).await;
        assert!(full.is_none(), "both full handlers returned null");
        assert_eq!(
            *order.lock().unwrap(),
            ["ordinary-a", "internal", "ordinary-b", "full-a", "full-b"]
        );
        assert_eq!(
            original, canonical,
            "request transforms never mutate canonical input"
        );

        for calls in [&calls_a, &calls_b] {
            for (method, params) in calls.lock().unwrap().iter() {
                let roles = params["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|message| message["role"].as_str().unwrap())
                    .collect::<Vec<_>>();
                if method == crate::rpc3::method::HOOKS_TRANSFORM_CONTEXT {
                    assert!(
                        !roles.contains(&"system"),
                        "ordinary input hid system state"
                    );
                } else {
                    assert!(
                        roles.contains(&"system"),
                        "full input includes system state"
                    );
                }
            }
        }
        assert!(
            internal_seen
                .lock()
                .unwrap()
                .iter()
                .any(|message| matches!(message, AgentMessage::System(_))),
            "internal compaction/token-style hooks still receive the full transcript"
        );
        assert!(matches!(ordinary.first(), Some(AgentMessage::System(_))));
        assert!(
            matches!(ordinary.get(1), Some(AgentMessage::User(user)) if user.content == tack_ai::UserContent::Text("from-b".into()))
        );
        let prompt = tack_ai::transcript::get_current_system_prompt(&ordinary);
        assert!(
            prompt.contains("base") && prompt.contains("mid"),
            "{prompt}"
        );
        assert_eq!(
            tack_ai::transcript::get_current_tools(&ordinary)[0].name,
            "declared"
        );
    }

    #[tokio::test]
    async fn unchanged_ordinary_context_preserves_mid_system_positions() {
        use tack_agent_core::AgentMessage;

        let visible = vec![AgentMessage::user("one"), AgentMessage::user("two")];
        let original = vec![
            declared_system("base"),
            visible[0].clone(),
            AgentMessage::System(tack_ai::SystemMessage::update(1)),
            visible[1].clone(),
        ];
        for reply in [context_reply(&visible), Value::Null] {
            let (hooks, _plugin) = hooks_pair(reply, caps(false, true, false));
            let output = hooks.transform_context(&original).await;
            assert!(
                output.is_none(),
                "unchanged/null must preserve the original list"
            );
            assert!(matches!(original.get(2), Some(AgentMessage::System(_))));
        }
    }

    #[tokio::test]
    async fn full_context_output_is_authoritative_when_it_deletes_system_state() {
        use tack_agent_core::AgentMessage;

        let input = vec![declared_system("base"), AgentMessage::user("original")];
        let calls = Arc::new(Mutex::new(Vec::new()));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (hooks, _plugin) = context_hooks_pair(
            "full",
            Value::Null,
            context_reply(&[AgentMessage::user("full-only")]),
            calls,
            order,
        );
        let output = hooks
            .transform_context_with_system(&input)
            .await
            .expect("full rewrite");
        assert_eq!(output.len(), 1);
        assert!(
            matches!(&output[0], AgentMessage::User(user) if user.content == tack_ai::UserContent::Text("full-only".into()))
        );
        assert!(tack_ai::transcript::get_current_system_message(&output).is_none());
        assert!(tack_ai::transcript::get_current_tools(&output).is_empty());
        assert!(matches!(input.first(), Some(AgentMessage::System(_))));
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
            let hooks =
                ExtHooks::with_fail_mode(Arc::new(client), caps(true, false, false), fail_mode);
            hooks.client.wait_dead().await;
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
