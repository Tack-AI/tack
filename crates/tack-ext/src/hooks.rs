//! AgentHooks bridge: forwards tool_call interceptions to subscribed plugins
//! (`intercept.tool_call`). Plugins not subscribing pass through untouched.

use std::sync::Arc;

use tack_agent_core::{AgentHooks, BeforeToolCallContext, BeforeToolCallOutcome};

use crate::process::PluginPeer;
use crate::protocol::{ToolCallInterceptParams, ToolCallVerdict};

/// How `intercept.tool_call` failures (timeout, dead plugin, malformed
/// verdict) are treated. TS extensions crash-isolate (fail-open), which
/// stays the default for compatibility; security-sensitive deployments can
/// opt into fail-closed so a broken interceptor BLOCKS the tool call
/// instead of waving it through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FailMode {
    /// Intercept failure lets the tool call through (TS-compatible).
    #[default]
    Open,
    /// Intercept failure blocks the tool call.
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

/// One plugin's hook bridge (composed into the app's HooksChain).
pub struct ExtHooks {
    peer: Arc<PluginPeer>,
    intercept_tool_calls: bool,
    intercept_context: bool,
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
    pub fn new(peer: Arc<PluginPeer>, subscriptions: &[String]) -> Self {
        Self::with_fail_mode(peer, subscriptions, FailMode::default())
    }

    pub fn with_fail_mode(
        peer: Arc<PluginPeer>,
        subscriptions: &[String],
        fail_mode: FailMode,
    ) -> Self {
        ExtHooks {
            peer,
            intercept_tool_calls: subscriptions.iter().any(|s| s == "tool_call"),
            intercept_context: subscriptions.iter().any(|s| s == "context"),
            fail_mode,
        }
    }

    /// Intercept failure: always loud in the logs; fail-open lets the call
    /// through (TS compat), fail-closed blocks it.
    fn intercept_failed(&self, what: &str) -> BeforeToolCallOutcome {
        tracing::warn!(
            "intercept.tool_call failed ({what}); fail mode {:?}",
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
}

#[async_trait::async_trait]
impl AgentHooks for ExtHooks {
    async fn before_tool_call(&self, ctx: &BeforeToolCallContext<'_>) -> BeforeToolCallOutcome {
        if !self.intercept_tool_calls {
            return BeforeToolCallOutcome::Allow;
        }
        let params = match serde_json::to_value(ToolCallInterceptParams {
            tool_call_id: ctx.tool_call_id.to_string(),
            tool_name: ctx.tool_name.to_string(),
            arguments: ctx.args.clone(),
        }) {
            Ok(p) => p,
            Err(_) => return BeforeToolCallOutcome::Allow,
        };
        // Intercept failures are fail-open by default (TS extensions
        // crash-isolate the same way); fail-closed deployments block.
        let result = match self.peer.call("intercept.tool_call", params).await {
            Ok(result) => result,
            Err(e) => return self.intercept_failed(&e),
        };
        match serde_json::from_value::<ToolCallVerdict>(result) {
            Ok(ToolCallVerdict::Allow) => BeforeToolCallOutcome::Allow,
            Ok(ToolCallVerdict::Deny { reason }) => BeforeToolCallOutcome::Block {
                reason: Some(reason),
                terminate: false,
            },
            Ok(ToolCallVerdict::Rewrite { arguments }) => {
                BeforeToolCallOutcome::Rewrite { args: arguments }
            }
            Err(e) => self.intercept_failed(&format!("invalid verdict: {e}")),
        }
    }

    /// Context transform mutation point (`intercept.context`): plugins
    /// subscribing "context" receive the full message list before every LLM
    /// call and may return a replacement (`{"messages": [...]}`). Opt-in via
    /// subscription because the payload is the whole context — fail-open on
    /// any error (timeout/dead plugin/round-trip mismatch).
    async fn transform_context(
        &self,
        messages: &[tack_agent_core::AgentMessage],
    ) -> Option<Vec<tack_agent_core::AgentMessage>> {
        if !self.intercept_context || messages.is_empty() {
            return None;
        }
        let payload = match serde_json::to_value(messages) {
            Ok(value) => value,
            Err(_) => return None,
        };
        let params = serde_json::json!({ "messages": payload });
        let Ok(result) = self.peer.call("intercept.context", params).await else {
            return None;
        };
        let replacement = result.get("messages")?;
        match serde_json::from_value::<Vec<tack_agent_core::AgentMessage>>(replacement.clone()) {
            Ok(transformed) => Some(transformed),
            Err(e) => {
                tracing::warn!("intercept.context returned invalid messages: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::process::HostServices;
    use crate::protocol::Envelope;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct NoopServices;

    #[async_trait::async_trait]
    impl HostServices for NoopServices {
        async fn handle_request(&self, _method: &str, _params: Value) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn handle_event(&self, _event: &str, _payload: Value) {}
    }

    /// Fake plugin answering intercept.context with a one-message list.
    async fn run_transform(subscriptions: &[&str]) -> Option<Vec<tack_agent_core::AgentMessage>> {
        let (host_reader, mut plugin_writer) = tokio::io::duplex(8192);
        let (mut plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(NoopServices));
        let replacement =
            serde_json::to_value(vec![tack_agent_core::AgentMessage::user("replaced")]).unwrap();
        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut plugin_reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(Envelope::Request { id, method, .. }) = serde_json::from_str(&line) {
                    assert_eq!(method, "intercept.context");
                    let response = Envelope::result(id, json!({ "messages": replacement }));
                    let mut text = serde_json::to_string(&response).unwrap();
                    text.push('\n');
                    if plugin_writer.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });
        let hooks = ExtHooks::new(
            peer,
            &subscriptions
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        );
        hooks
            .transform_context(&[tack_agent_core::AgentMessage::user("original")])
            .await
    }

    #[tokio::test]
    async fn context_subscriber_can_replace_messages() {
        let messages = run_transform(&["context"])
            .await
            .expect("subscribed plugin replaces the context");
        assert_eq!(messages.len(), 1);
        let tack_agent_core::AgentMessage::User(user) = &messages[0] else {
            panic!("expected user message")
        };
        let tack_ai::UserContent::Text(text) = &user.content else {
            panic!("expected text")
        };
        assert_eq!(text, "replaced");
    }

    #[tokio::test]
    async fn unsubscribed_plugin_is_never_called() {
        // The fake would assert on the method; no call must arrive at all.
        let messages = run_transform(&["tool_call"]).await;
        assert!(messages.is_none(), "unsubscribed plugin passes through");
    }

    /// Hooks over a DEAD plugin (the other duplex half is dropped; the peer
    /// observes EOF and fails calls immediately).
    async fn dead_peer_hooks(fail_mode: FailMode) -> ExtHooks {
        let (host_reader, plugin_writer) = tokio::io::duplex(8192);
        let (_plugin_reader, host_writer) = tokio::io::duplex(8192);
        let peer = PluginPeer::new(host_reader, host_writer, Arc::new(NoopServices));
        drop(plugin_writer);
        peer.wait_dead().await;
        ExtHooks::with_fail_mode(peer, &["tool_call".to_string()], fail_mode)
    }

    fn tool_call_ctx<'a>(
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

    /// Default (TS-compatible) fail-open: a dead/slow interceptor must not
    /// stall the agent — but the failure is logged (see intercept_failed).
    #[tokio::test]
    async fn intercept_failure_fail_open_allows() {
        let hooks = dead_peer_hooks(FailMode::Open).await;
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = json!({"command": "ls"});
        let outcome = hooks
            .before_tool_call(&tool_call_ctx(&message, &args))
            .await;
        assert!(
            matches!(outcome, BeforeToolCallOutcome::Allow),
            "{outcome:?}"
        );
    }

    /// failMode: "closed" — the same failure BLOCKS the tool call.
    #[tokio::test]
    async fn intercept_failure_fail_closed_blocks() {
        let hooks = dead_peer_hooks(FailMode::Closed).await;
        let model = test_model();
        let message = tack_ai::AssistantMessage::pending(&model);
        let args = json!({"command": "ls"});
        let outcome = hooks
            .before_tool_call(&tool_call_ctx(&message, &args))
            .await;
        let BeforeToolCallOutcome::Block { reason, terminate } = outcome else {
            panic!("fail-closed must block, got {outcome:?}")
        };
        assert!(!terminate);
        assert!(reason.unwrap().contains("intercept unavailable"));
    }

    #[test]
    fn fail_mode_from_setting() {
        assert_eq!(FailMode::from_setting(Some("closed")), FailMode::Closed);
        assert_eq!(FailMode::from_setting(Some("open")), FailMode::Open);
        assert_eq!(FailMode::from_setting(None), FailMode::Open);
        assert_eq!(FailMode::from_setting(Some("garbage")), FailMode::Open);
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
}
