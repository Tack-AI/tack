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
    InitializeResult, LifecycleEventParams, ToolExecuteParams, ToolOutput, TransformContextParams,
    TransformContextResult, Verdict, WidgetActionParams, method,
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
