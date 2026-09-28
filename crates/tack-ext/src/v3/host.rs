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
    CommandInvokeParams, InitializeParams, InitializeResult, LifecycleEventParams,
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
