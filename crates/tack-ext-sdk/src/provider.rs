//! The provider bridge surface (P7): serve inference for registered
//! providers through `provider/stream`.
//!
//! Register [`crate::PluginBuilder::provider_stream`] to declare the
//! `provider.stream` capability, then call
//! [`crate::Host::register_provider`] with `bridge: true` (typically from
//! [`crate::PluginBuilder::on_ready`]) — the host starts sending
//! `provider/stream` requests for the provider's models and the turn's
//! events flow back through [`ProviderEvents`]. The SDK owns the plumbing:
//! streamId scoping, the ack/cancel wiring, and terminal enforcement
//! (exactly one terminal event, with an automatic `Error` when the handler
//! panics or returns without one).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::Error;
use crate::host::Cx;
use tack_ext::rpc3::{ProviderStreamEventParams, method};
use tack_ext::v3::JsonRpcPeer;
/// Event sink scoped to one `provider/stream` call: sends
/// `provider/streamEvent` notifications and enforces exactly one terminal
/// event.
#[derive(Clone, Debug)]
pub struct ProviderEvents {
    inner: Arc<ProviderEventsInner>,
}

#[derive(Debug)]
struct ProviderEventsInner {
    peer: Arc<JsonRpcPeer>,
    stream_id: String,
    /// The served model (for synthesized error messages).
    model: Value,
    terminal_sent: AtomicBool,
}

fn is_terminal(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("done" | "error")
    )
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A zeroed assistant message (valid `AssistantMessage` JSON) carrying an
/// error, built from the served model's ids.
fn error_message_json(model: &Value, error_message: &str) -> Value {
    let id_of = |key: &str| {
        model
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    serde_json::json!({
        "content": [],
        "api": id_of("api"),
        "provider": id_of("provider"),
        "model": id_of("id"),
        "usage": {
            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": 0,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
        },
        "stopReason": "error",
        "errorMessage": error_message,
        "timestamp": now_millis(),
    })
}

impl ProviderEvents {
    pub(crate) fn new(peer: Arc<JsonRpcPeer>, stream_id: String, model: Value) -> Self {
        ProviderEvents {
            inner: Arc::new(ProviderEventsInner {
                peer,
                stream_id,
                model,
                terminal_sent: AtomicBool::new(false),
            }),
        }
    }

    /// The stream these events belong to.
    pub fn stream_id(&self) -> &str {
        &self.inner.stream_id
    }

    /// Send one `AssistantMessageEvent`-shaped event. Terminal events
    /// (`done`/`error`) may be sent exactly once; a second one is an error.
    pub async fn send(&self, event: Value) -> Result<(), Error> {
        if is_terminal(&event) && self.inner.terminal_sent.swap(true, Ordering::SeqCst) {
            return Err(Error::internal("provider stream already terminated"));
        }
        self.notify(event).await
    }

    async fn notify(&self, event: Value) -> Result<(), Error> {
        let params = serde_json::to_value(ProviderStreamEventParams {
            stream_id: self.inner.stream_id.clone(),
            event,
        })
        .map_err(|e| Error::internal(e.to_string()))?;
        self.inner
            .peer
            .notify(method::PROVIDER_STREAM_EVENT, params)
            .await
            .map_err(Error::from)
    }

    /// `textDelta` convenience (`partial` is the accumulated message).
    pub async fn text_delta(
        &self,
        content_index: usize,
        delta: impl Into<String>,
        partial: Value,
    ) -> Result<(), Error> {
        self.send(serde_json::json!({
            "type": "textDelta",
            "contentIndex": content_index,
            "delta": delta.into(),
            "partial": partial,
        }))
        .await
    }

    /// `thinkingDelta` convenience (`partial` is the accumulated message).
    pub async fn thinking_delta(
        &self,
        content_index: usize,
        delta: impl Into<String>,
        partial: Value,
    ) -> Result<(), Error> {
        self.send(serde_json::json!({
            "type": "thinkingDelta",
            "contentIndex": content_index,
            "delta": delta.into(),
            "partial": partial,
        }))
        .await
    }

    /// Terminal `done` event; `reason` defaults to the message's
    /// `stopReason` (or `stop`).
    pub async fn done(&self, message: Value) -> Result<(), Error> {
        let reason = message
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("stop")
            .to_string();
        self.send(serde_json::json!({
            "type": "done",
            "reason": reason,
            "message": message,
        }))
        .await
    }

    /// Terminal `error` event. `message` defaults to a zeroed assistant
    /// message built from the served model carrying `error_message`.
    pub async fn error(
        &self,
        error_message: impl Into<String>,
        message: Option<Value>,
    ) -> Result<(), Error> {
        let error_message = error_message.into();
        let message =
            message.unwrap_or_else(|| error_message_json(&self.inner.model, &error_message));
        self.send(serde_json::json!({
            "type": "error",
            "reason": "error",
            "error": message,
        }))
        .await
    }

    /// Terminal enforcement: fire an automatic `Error` when the handler
    /// failed or returned without a terminal event. (The host's fail-open
    /// synthesis is the backstop, never the plan.)
    pub(crate) async fn enforce_terminal(&self, handler_result: Result<(), Error>) {
        if let Err(error) = handler_result {
            let _ = self.error(error.message, None).await;
            return;
        }
        if !self.inner.terminal_sent.load(Ordering::SeqCst) {
            let _ = self
                .error(
                    "provider stream handler returned without a terminal event",
                    None,
                )
                .await;
        }
    }
}

/// Stream-scoped context: the plugin's host environment plus the stream's
/// cancellation signal (`provider/streamCancel`).
#[derive(Clone, Debug)]
pub struct ProviderStreamCx {
    cx: Cx,
    stream_id: String,
    cancel: CancellationToken,
}

impl ProviderStreamCx {
    pub(crate) fn new(cx: Cx, stream_id: String, cancel: CancellationToken) -> Self {
        ProviderStreamCx {
            cx,
            stream_id,
            cancel,
        }
    }

    /// The plugin's host context (mode, trust, config, host client).
    pub fn cx(&self) -> &Cx {
        &self.cx
    }

    /// The host client (shortcut for `cx().host()`).
    pub fn host(&self) -> crate::Host {
        self.cx.host()
    }

    /// This stream's id.
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    /// The cancellation signal: poll ([`Self::is_cancelled`]) or await
    /// ([`Self::cancelled`]). Ignoring it is legal — the host synthesizes
    /// a terminal event after its grace period — but discouraged.
    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolve when the host cancels the stream (`provider/streamCancel`).
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }
}
