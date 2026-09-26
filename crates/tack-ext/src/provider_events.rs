//! Provider event wrapper: emits `before_provider_request` and
//! `after_provider_response` lifecycle events around any provider's stream
//! (sanitized payloads — no message bodies, so plugins can't exfiltrate
//! conversation content by default).

use std::sync::Arc;

use tack_ai::provider::{Provider, StreamOptions};
use tack_ai::stream::{AssistantMessageEventStream, event_stream};
use tack_ai::types::{Context, Model};

/// Anything that accepts lifecycle events (ExtensionManager implements it).
pub trait EventSink: Send + Sync + std::fmt::Debug {
    fn notify(&self, event: &str, payload: serde_json::Value);
}

/// Wraps a provider and fires provider-boundary lifecycle events.
#[derive(Debug)]
pub struct ExtNotifyProvider {
    inner: Arc<dyn Provider>,
    sink: Arc<dyn EventSink>,
}

impl ExtNotifyProvider {
    pub fn new(inner: Arc<dyn Provider>, sink: Arc<dyn EventSink>) -> Self {
        ExtNotifyProvider { inner, sink }
    }
}

impl Provider for ExtNotifyProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let started = std::time::Instant::now();
        self.sink.notify(
            "before_provider_request",
            serde_json::json!({
                "provider": model.provider,
                "model": model.id,
                "messageCount": context.messages.len(),
                "toolCount": context.tools.len(),
                "hasSystemPrompt": context.system_prompt.is_some(),
            }),
        );

        let mut inner = self.inner.stream(model, context, options);
        let (sender, stream) = event_stream();
        let sink = self.sink.clone();
        let provider = model.provider.clone();
        let model_id = model.id.clone();
        tokio::spawn(async move {
            while let Some(event) = inner.next().await {
                if !sender.push(event) {
                    return; // consumer gone
                }
            }
            let message = inner.result().await;
            sink.notify(
                "after_provider_response",
                serde_json::json!({
                    "provider": provider,
                    "model": model_id,
                    "stopReason": format!("{:?}", message.stop_reason).to_lowercase(),
                    "durationMs": started.elapsed().as_millis() as u64,
                    "usage": message.usage,
                    "errorMessage": message.error_message,
                }),
            );
            sender.end(message);
        });
        stream
    }
}
