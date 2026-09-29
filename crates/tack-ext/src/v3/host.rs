//! Host-side typed client for tack-RPC v3: the handshake plus one method
//! per capability namespace, over the generated [`crate::rpc3`] types.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::peer::{JsonRpcPeer, PeerError};
use super::{PROTOCOL_VERSION, protocol_compatible};
use crate::rpc3::{
    AfterToolCallParams, AfterToolCallPatch, ApprovalDecision, ApprovalReviewParams,
    AutocompleteProvideParams, AutocompleteProvideResult, BeforeToolCallParams,
    CommandInvokeParams, ERR_CAPABILITY_NOT_GRANTED, ErrorObject, InitializeParams,
    InitializeResult, LifecycleEventParams, ProviderStreamCancelParams, ProviderStreamParams,
    ToolExecuteParams, ToolOutput, TransformContextParams, TransformContextResult, Verdict,
    WidgetActionParams, method,
};

/// Handshake bound (a plugin that cannot answer initialize quickly is
/// not worth waiting for).
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// The host's view of one v3 plugin: typed calls host → plugin.
#[derive(Clone)]
pub struct HostClient {
    peer: Arc<JsonRpcPeer>,
}

impl std::fmt::Debug for HostClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostClient")
            .field("peer", &self.peer)
            .finish()
    }
}

impl HostClient {
    pub fn new(peer: Arc<JsonRpcPeer>) -> Self {
        HostClient { peer }
    }

    pub fn peer(&self) -> &Arc<JsonRpcPeer> {
        &self.peer
    }

    async fn call<P, R>(&self, rpc_method: &str, params: &P) -> Result<R, PeerError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let params =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        let result = self.peer.call(rpc_method, params).await?;
        serde_json::from_value(result).map_err(|e| PeerError::Transport(e.to_string()))
    }

    /// The initialize handshake. Validates the plugin's protocol version
    /// (same major, minor not newer than the host's).
    pub async fn initialize(
        &self,
        params: &InitializeParams,
    ) -> Result<InitializeResult, PeerError> {
        let payload =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        let result = self
            .peer
            .call_with_timeout(method::INITIALIZE, payload, INITIALIZE_TIMEOUT)
            .await?;
        let result: InitializeResult =
            serde_json::from_value(result).map_err(|e| PeerError::Transport(e.to_string()))?;
        if !protocol_compatible(&result.protocol_version) {
            return Err(PeerError::Transport(format!(
                "plugin {:?} speaks protocol {}, but this host speaks {PROTOCOL_VERSION}",
                result.plugin.name, result.protocol_version
            )));
        }
        Ok(result)
    }

    /// `tools/execute`.
    pub async fn tool_execute(&self, params: &ToolExecuteParams) -> Result<ToolOutput, PeerError> {
        self.call(method::TOOLS_EXECUTE, params).await
    }

    /// `commands/invoke` (free-form result).
    pub async fn command_invoke(&self, params: &CommandInvokeParams) -> Result<Value, PeerError> {
        self.call(method::COMMANDS_INVOKE, params).await
    }

    /// `hooks/beforeToolCall` → allow / deny / rewrite.
    pub async fn before_tool_call(
        &self,
        params: &BeforeToolCallParams,
    ) -> Result<Verdict, PeerError> {
        self.call(method::HOOKS_BEFORE_TOOL_CALL, params).await
    }

    /// `hooks/transformContext`; `None` means "context unchanged".
    pub async fn transform_context(
        &self,
        params: &TransformContextParams,
    ) -> Result<Option<TransformContextResult>, PeerError> {
        self.call(method::HOOKS_TRANSFORM_CONTEXT, params).await
    }

    /// `hooks/afterToolCall`; `None` means "no patch".
    pub async fn after_tool_call(
        &self,
        params: &AfterToolCallParams,
    ) -> Result<Option<AfterToolCallPatch>, PeerError> {
        self.call(method::HOOKS_AFTER_TOOL_CALL, params).await
    }

    /// `approval/review`; `None` passes to the next reviewer.
    pub async fn approval_review(
        &self,
        params: &ApprovalReviewParams,
    ) -> Result<Option<ApprovalDecision>, PeerError> {
        self.call(method::APPROVAL_REVIEW, params).await
    }

    /// `autocomplete/provide`.
    pub async fn autocomplete_provide(
        &self,
        params: &AutocompleteProvideParams,
    ) -> Result<AutocompleteProvideResult, PeerError> {
        self.call(method::AUTOCOMPLETE_PROVIDE, params).await
    }

    /// `shutdown` (graceful stop request; the caller still enforces the
    /// carrier teardown after a grace period).
    pub async fn shutdown(&self) -> Result<(), PeerError> {
        self.peer.call(method::SHUTDOWN, Value::Null).await?;
        Ok(())
    }

    /// `provider/stream`: start one inference stream on a bridge provider.
    /// The answer is a fast ack (synchronous validation only); the turn's
    /// events then flow back as `provider/streamEvent` notifications
    /// demuxed by `streamId`.
    pub async fn provider_stream(&self, params: &ProviderStreamParams) -> Result<(), PeerError> {
        let params =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer.call(method::PROVIDER_STREAM, params).await?;
        Ok(())
    }

    /// `provider/streamCancel` notification (best-effort abort of an
    /// in-flight stream).
    pub async fn provider_stream_cancel(&self, stream_id: &str) -> Result<(), PeerError> {
        let params = ProviderStreamCancelParams {
            stream_id: stream_id.to_string(),
        };
        let params =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer
            .notify(method::PROVIDER_STREAM_CANCEL, params)
            .await
    }

    /// `events/lifecycle` notification (fire-and-forget).
    pub async fn lifecycle_event(&self, event: &str, payload: Value) -> Result<(), PeerError> {
        let params = LifecycleEventParams {
            event: event.to_string(),
            payload,
        };
        let params =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer.notify(method::EVENTS_LIFECYCLE, params).await
    }

    /// `widgets/action` notification (owning plugin only).
    pub async fn widget_action(&self, params: &WidgetActionParams) -> Result<(), PeerError> {
        let params =
            serde_json::to_value(params).map_err(|e| PeerError::Transport(e.to_string()))?;
        self.peer.notify(method::WIDGETS_ACTION, params).await
    }
}

/// The error a [`PluginConnection`] returns when the caller invokes a
/// capability namespace its carrier does not implement (Level-2 MCP
/// plugins and the component carrier implement a subset of the v3
/// surface; the manager only calls declared capabilities, so reaching
/// this is a host bug, not a plugin failure).
pub fn unsupported_capability(namespace: &str) -> PeerError {
    PeerError::Remote(ErrorObject {
        code: ERR_CAPABILITY_NOT_GRANTED,
        message: format!("carrier does not implement {namespace}"),
        data: None,
    })
}

/// The host→plugin call surface, abstract over the carrier.
///
/// [`HostClient`] implements this for the JSON-RPC carriers (process and
/// WASI-stdio WASM); Level-2 MCP server plugins and the WIT component
/// carrier provide their own implementations in the crates that own those
/// clients (`tack-app`, `tack-ext-wasm`). The method set mirrors
/// [`HostClient`] exactly; consumers (tools, hooks, widgets, dev tooling)
/// code against the trait so every carrier plugs in uniformly.
#[async_trait::async_trait]
pub trait PluginConnection: Send + Sync + std::fmt::Debug {
    /// `initialize` handshake (version-checked).
    async fn initialize(&self, params: &InitializeParams) -> Result<InitializeResult, PeerError>;
    /// `tools/execute`.
    async fn tool_execute(&self, params: &ToolExecuteParams) -> Result<ToolOutput, PeerError>;
    /// `commands/invoke`.
    async fn command_invoke(&self, params: &CommandInvokeParams) -> Result<Value, PeerError>;
    /// `hooks/beforeToolCall`.
    async fn before_tool_call(&self, params: &BeforeToolCallParams) -> Result<Verdict, PeerError>;
    /// `hooks/transformContext`.
    async fn transform_context(
        &self,
        params: &TransformContextParams,
    ) -> Result<Option<TransformContextResult>, PeerError>;
    /// `hooks/afterToolCall`.
    async fn after_tool_call(
        &self,
        params: &AfterToolCallParams,
    ) -> Result<Option<AfterToolCallPatch>, PeerError>;
    /// `approval/review`.
    async fn approval_review(
        &self,
        params: &ApprovalReviewParams,
    ) -> Result<Option<ApprovalDecision>, PeerError>;
    /// `autocomplete/provide`.
    async fn autocomplete_provide(
        &self,
        params: &AutocompleteProvideParams,
    ) -> Result<AutocompleteProvideResult, PeerError>;
    /// `events/lifecycle` notification (fire-and-forget).
    async fn lifecycle_event(&self, event: &str, payload: Value) -> Result<(), PeerError>;
    /// `widgets/action` notification.
    async fn widget_action(&self, params: &WidgetActionParams) -> Result<(), PeerError>;
    /// `provider/stream` (bridge provider inference). Default: the carrier
    /// does not serve inference — the process and WASI-stdio carriers
    /// override this through [`HostClient`]; the WIT component and MCP
    /// carriers structurally cannot (see the carrier matrix in
    /// `docs/plugin-provider-bridge.md` §4.4).
    async fn provider_stream(&self, _params: &ProviderStreamParams) -> Result<(), PeerError> {
        Err(unsupported_capability("provider/stream"))
    }
    /// `provider/streamCancel` notification. Default: the carrier does not
    /// serve inference (see [`Self::provider_stream`]).
    async fn provider_stream_cancel(&self, _stream_id: &str) -> Result<(), PeerError> {
        Err(unsupported_capability("provider/stream"))
    }
    /// Untyped request escape hatch for dev tooling (`ext dev`, `ext
    /// inspect` script arbitrary methods).
    async fn call_raw(&self, rpc_method: &str, params: Value) -> Result<Value, PeerError>;
    /// Untyped notification escape hatch (see [`Self::call_raw`]).
    async fn notify_raw(&self, rpc_method: &str, params: Value) -> Result<(), PeerError>;
    /// `shutdown` (graceful stop request; the caller still enforces the
    /// carrier teardown after a grace period).
    async fn shutdown(&self) -> Result<(), PeerError>;
    /// Carrier liveness (process alive / connection open / store running).
    fn is_alive(&self) -> bool;
    /// Resolve once the carrier is gone (EOF, trap, connection close).
    async fn wait_dead(&self);
}

#[async_trait::async_trait]
impl PluginConnection for HostClient {
    async fn initialize(&self, params: &InitializeParams) -> Result<InitializeResult, PeerError> {
        HostClient::initialize(self, params).await
    }

    async fn tool_execute(&self, params: &ToolExecuteParams) -> Result<ToolOutput, PeerError> {
        HostClient::tool_execute(self, params).await
    }

    async fn command_invoke(&self, params: &CommandInvokeParams) -> Result<Value, PeerError> {
        HostClient::command_invoke(self, params).await
    }

    async fn before_tool_call(&self, params: &BeforeToolCallParams) -> Result<Verdict, PeerError> {
        HostClient::before_tool_call(self, params).await
    }

    async fn transform_context(
        &self,
        params: &TransformContextParams,
    ) -> Result<Option<TransformContextResult>, PeerError> {
        HostClient::transform_context(self, params).await
    }

    async fn after_tool_call(
        &self,
        params: &AfterToolCallParams,
    ) -> Result<Option<AfterToolCallPatch>, PeerError> {
        HostClient::after_tool_call(self, params).await
    }

    async fn approval_review(
        &self,
        params: &ApprovalReviewParams,
    ) -> Result<Option<ApprovalDecision>, PeerError> {
        HostClient::approval_review(self, params).await
    }

    async fn autocomplete_provide(
        &self,
        params: &AutocompleteProvideParams,
    ) -> Result<AutocompleteProvideResult, PeerError> {
        HostClient::autocomplete_provide(self, params).await
    }

    async fn lifecycle_event(&self, event: &str, payload: Value) -> Result<(), PeerError> {
        HostClient::lifecycle_event(self, event, payload).await
    }

    async fn widget_action(&self, params: &WidgetActionParams) -> Result<(), PeerError> {
        HostClient::widget_action(self, params).await
    }

    async fn provider_stream(&self, params: &ProviderStreamParams) -> Result<(), PeerError> {
        HostClient::provider_stream(self, params).await
    }

    async fn provider_stream_cancel(&self, stream_id: &str) -> Result<(), PeerError> {
        HostClient::provider_stream_cancel(self, stream_id).await
    }

    async fn call_raw(&self, rpc_method: &str, params: Value) -> Result<Value, PeerError> {
        self.peer.call(rpc_method, params).await
    }

    async fn notify_raw(&self, rpc_method: &str, params: Value) -> Result<(), PeerError> {
        self.peer.notify(rpc_method, params).await
    }

    async fn shutdown(&self) -> Result<(), PeerError> {
        HostClient::shutdown(self).await
    }

    fn is_alive(&self) -> bool {
        self.peer.is_alive()
    }

    async fn wait_dead(&self) {
        self.peer.wait_dead().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Mutex;

    use super::*;
    use crate::rpc3::{ERR_METHOD_NOT_FOUND, ProviderStreamEventParams, method};
    use crate::v3::peer::PeerHandler;

    /// A scripted plugin side: answers `provider/stream` with a fast ack,
    /// records cancels, and pushes scripted `provider/streamEvent`
    /// notifications back over its own peer.
    struct ProviderPlugin {
        cancelled: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl PeerHandler for ProviderPlugin {
        async fn handle_request(
            &self,
            rpc_method: &str,
            _params: Value,
        ) -> Result<Value, ErrorObject> {
            match rpc_method {
                method::PROVIDER_STREAM => Ok(Value::Null),
                _ => Err(ErrorObject {
                    code: ERR_METHOD_NOT_FOUND,
                    message: format!("unknown method {rpc_method}"),
                    data: None,
                }),
            }
        }
        async fn handle_notification(&self, rpc_method: &str, params: Value) {
            if rpc_method == method::PROVIDER_STREAM_CANCEL {
                let stream_id = params
                    .get("streamId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.cancelled.lock().unwrap().push(stream_id);
            }
        }
    }

    struct HostSide {
        received: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait::async_trait]
    impl PeerHandler for HostSide {
        async fn handle_notification(&self, rpc_method: &str, params: Value) {
            if rpc_method == method::PROVIDER_STREAM_EVENT {
                let stream_id = params
                    .get("streamId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let event = params.get("event").cloned().unwrap_or(Value::Null);
                self.received.lock().unwrap().push((stream_id, event));
            }
        }
    }

    fn duplex_pair(
        plugin: Arc<ProviderPlugin>,
        host: Arc<HostSide>,
    ) -> (HostClient, Arc<crate::v3::peer::JsonRpcPeer>) {
        let (s1, s2) = tokio::io::duplex(64 * 1024);
        let (r1, w1) = tokio::io::split(s1);
        let (r2, w2) = tokio::io::split(s2);
        let host_peer = crate::v3::peer::JsonRpcPeer::new(r1, w1, host);
        let plugin_peer = crate::v3::peer::JsonRpcPeer::new(r2, w2, plugin);
        (HostClient::new(host_peer), plugin_peer)
    }

    #[tokio::test]
    async fn provider_stream_ack_events_and_cancel_flow() {
        let plugin = Arc::new(ProviderPlugin {
            cancelled: Mutex::new(vec![]),
        });
        let host = Arc::new(HostSide {
            received: Mutex::new(vec![]),
        });
        let (client, plugin_peer) = duplex_pair(plugin.clone(), host.clone());

        let params = ProviderStreamParams {
            stream_id: "ps-1".to_string(),
            model: serde_json::json!({"id": "m"}),
            context: serde_json::json!({"messages": []}),
            options: serde_json::json!({}),
        };
        // The ack is synchronous validation only: Ok(()).
        PluginConnection::provider_stream(&client, &params)
            .await
            .unwrap();

        // Events then ride plugin->host notifications.
        for event in [
            serde_json::json!({"type": "start"}),
            serde_json::json!({"type": "done"}),
        ] {
            let note = ProviderStreamEventParams {
                stream_id: "ps-1".to_string(),
                event,
            };
            plugin_peer
                .notify(
                    method::PROVIDER_STREAM_EVENT,
                    serde_json::to_value(note).unwrap(),
                )
                .await
                .unwrap();
        }
        for _ in 0..50 {
            if host.received.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let received = host.received.lock().unwrap().clone();
        assert_eq!(received.len(), 2, "stream events reached the host side");
        assert!(received.iter().all(|(id, _)| id == "ps-1"));
        assert_eq!(received[1].1["type"], "done");
        drop(received);

        // Cancel rides a host->plugin notification.
        PluginConnection::provider_stream_cancel(&client, "ps-1")
            .await
            .unwrap();
        for _ in 0..50 {
            if !plugin.cancelled.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(plugin.cancelled.lock().unwrap().as_slice(), ["ps-1"]);
    }

    #[tokio::test]
    async fn default_trait_methods_are_unsupported_capability() {
        #[derive(Debug)]
        struct Bare;
        #[async_trait::async_trait]
        impl PluginConnection for Bare {
            async fn initialize(
                &self,
                _params: &InitializeParams,
            ) -> Result<InitializeResult, PeerError> {
                unreachable!()
            }
            async fn tool_execute(
                &self,
                _params: &crate::rpc3::ToolExecuteParams,
            ) -> Result<crate::rpc3::ToolOutput, PeerError> {
                unreachable!()
            }
            async fn command_invoke(
                &self,
                _params: &crate::rpc3::CommandInvokeParams,
            ) -> Result<Value, PeerError> {
                unreachable!()
            }
            async fn before_tool_call(
                &self,
                _params: &crate::rpc3::BeforeToolCallParams,
            ) -> Result<crate::rpc3::Verdict, PeerError> {
                unreachable!()
            }
            async fn transform_context(
                &self,
                _params: &crate::rpc3::TransformContextParams,
            ) -> Result<Option<crate::rpc3::TransformContextResult>, PeerError> {
                unreachable!()
            }
            async fn after_tool_call(
                &self,
                _params: &crate::rpc3::AfterToolCallParams,
            ) -> Result<Option<crate::rpc3::AfterToolCallPatch>, PeerError> {
                unreachable!()
            }
            async fn approval_review(
                &self,
                _params: &crate::rpc3::ApprovalReviewParams,
            ) -> Result<Option<crate::rpc3::ApprovalDecision>, PeerError> {
                unreachable!()
            }
            async fn autocomplete_provide(
                &self,
                _params: &crate::rpc3::AutocompleteProvideParams,
            ) -> Result<crate::rpc3::AutocompleteProvideResult, PeerError> {
                unreachable!()
            }
            async fn lifecycle_event(
                &self,
                _event: &str,
                _payload: Value,
            ) -> Result<(), PeerError> {
                unreachable!()
            }
            async fn widget_action(
                &self,
                _params: &crate::rpc3::WidgetActionParams,
            ) -> Result<(), PeerError> {
                unreachable!()
            }
            async fn call_raw(&self, _m: &str, _p: Value) -> Result<Value, PeerError> {
                unreachable!()
            }
            async fn notify_raw(&self, _m: &str, _p: Value) -> Result<(), PeerError> {
                unreachable!()
            }
            async fn shutdown(&self) -> Result<(), PeerError> {
                unreachable!()
            }
            fn is_alive(&self) -> bool {
                false
            }
            async fn wait_dead(&self) {}
        }
        let bare = Bare;
        let err = PluginConnection::provider_stream(&bare, &ProviderStreamParams::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), ERR_CAPABILITY_NOT_GRANTED);
        let err = PluginConnection::provider_stream_cancel(&bare, "x")
            .await
            .unwrap_err();
        assert_eq!(err.code(), ERR_CAPABILITY_NOT_GRANTED);
    }
}
