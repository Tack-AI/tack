//! Plugin-served provider bridges (tack-RPC v3 `provider/stream`).
//!
//! A plugin that declares `capabilities.provider.stream` and registers a
//! provider with `bridge: true` serves inference itself: the host sends
//! `(model, context, options)` and the plugin streams `AssistantMessage`
//! events back — no HTTP hop. This module is the tack-ai side of that
//! bridge: the reserved api kind, the trait-erased serving endpoint, the
//! process-global bridge registry, and the [`Provider`] impl adapting a
//! bridge to the agent loop. The dependency direction is preserved by
//! trait erasure (the same pattern as the approval chain): tack-app
//! implements [`ProviderStreamBridge`] over a plugin connection, and
//! tack-ai never sees the plugin protocol.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::provider::{CacheRetention, Provider, StreamOptions, ToolChoice};
use crate::stream::{
    AssistantMessageEvent, AssistantMessageEventSender, AssistantMessageEventStream, event_stream,
};
use crate::types::{AssistantMessage, Context, Model, StopReason, ThinkingBudgets, ThinkingLevel};

/// Reserved api kind for plugin-served providers. The `ext-` prefix matches
/// the `ext__` tool naming convention; no built-in adapter uses it, and a
/// `models.json` entry naming it resolves to a bridge that was never
/// registered (an in-band error at stream time).
pub const EXT_PROVIDER_BRIDGE_API: &str = "ext-provider-bridge";

/// The serializable subset of [`StreamOptions`] sent to a bridge provider.
///
/// Deliberately excluded (see `docs/plugin-provider-bridge.md` §4.1):
///
/// - `api_key` — a bridge provider manages its own credentials (the
///   CLI-login model: "if the tool works in your terminal, it works here").
///   The host never brokers keys for bridges.
/// - `cancel` / `retry_cancel` — transport-level concerns. Cancellation
///   rides `provider/streamCancel`; retry policy belongs to the plugin.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sampling_params: BTreeMap<String, Value>,
}

impl BridgeStreamOptions {
    /// Project full [`StreamOptions`] down to the wire subset.
    pub fn from_stream_options(options: &StreamOptions) -> Self {
        BridgeStreamOptions {
            max_tokens: options.max_tokens,
            temperature: options.temperature,
            reasoning: options.reasoning,
            thinking_budgets: options.thinking_budgets,
            tool_choice: options.tool_choice,
            cache_retention: options.cache_retention,
            session_id: options.session_id.clone(),
            headers: options.headers.clone(),
            sampling_params: options.sampling_params.clone(),
        }
    }
}

/// One inference stream request: the typed mirror of the v3
/// `ProviderStreamParams` envelope (`model`/`context`/`options` cross the
/// wire as provider-shaped JSON and are parsed back with these types).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStreamParams {
    /// Host-generated stream id, unique per connection.
    pub stream_id: String,
    pub model: Model,
    pub context: Context,
    pub options: BridgeStreamOptions,
}

/// One serving endpoint for a bridged provider id. Implemented in tack-app
/// over the plugin connection (`ExtProviderBridge`).
pub trait ProviderStreamBridge: Send + Sync + std::fmt::Debug {
    /// Start a stream; events are delivered to `sink` until a terminal
    /// event finishes it. `cancel` is the host's cancellation token: the
    /// implementation forwards it as `provider/streamCancel` and
    /// synthesizes a terminal event after a grace period when the plugin
    /// goes silent. An `Err` return means the implementation did **not**
    /// consume the sink; the caller finishes it as an in-band error.
    fn stream(
        &self,
        params: BridgeStreamParams,
        cancel: CancellationToken,
        sink: AssistantMessageEventSender,
    ) -> Result<(), String>;
    /// Best-effort abort of an in-flight stream (host cancelled).
    fn cancel(&self, stream_id: &str);
}

/// The process-global bridge registry: provider id -> serving endpoint.
/// Same process-global model as the runtime provider registry
/// ([`crate::providers::runtime_providers`]): bridge registration and model
/// registration are separate writes, resolved together at stream time so a
/// re-registration after plugin reload picks up the new connection.
static PROVIDER_BRIDGES: RwLock<BTreeMap<String, Arc<dyn ProviderStreamBridge>>> =
    RwLock::new(BTreeMap::new());

/// Register (replace-by-id) the serving endpoint for a bridged provider.
pub fn register_provider_bridge(id: &str, bridge: Arc<dyn ProviderStreamBridge>) {
    PROVIDER_BRIDGES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string(), bridge);
}

/// Remove the serving endpoint for a provider id (plugin gone). Models
/// registered for the provider remain until unregistered separately and
/// fail with an in-band error at stream time.
pub fn unregister_provider_bridge(id: &str) {
    PROVIDER_BRIDGES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
}

/// The serving endpoint currently registered for a provider id.
pub fn provider_bridge(id: &str) -> Option<Arc<dyn ProviderStreamBridge>> {
    PROVIDER_BRIDGES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
}

/// Stream-id sequence: process-unique, which is unique per connection.
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// The [`Provider`] adapter for [`EXT_PROVIDER_BRIDGE_API`] models. Looks
/// the bridge up in the registry at `stream()` time and converts every
/// failure into an in-band `Error` event, preserving the `Provider`
/// contract — the agent loop treats a bridged provider exactly like a
/// native one.
#[derive(Debug)]
pub struct BridgedProvider {
    provider_id: String,
}

impl BridgedProvider {
    pub fn new(provider_id: impl Into<String>) -> Self {
        BridgedProvider {
            provider_id: provider_id.into(),
        }
    }

    /// A stream that is just one terminal in-band error (native adapters
    /// emit the same shape for early failures, before any `Start` event).
    fn failed_stream(
        model: &Model,
        reason: StopReason,
        message: impl Into<String>,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = event_stream();
        let mut error = AssistantMessage::pending(model);
        error.stop_reason = reason;
        error.error_message = Some(message.into());
        sender.finish(AssistantMessageEvent::Error { reason, error });
        stream
    }
}

impl Provider for BridgedProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        if options.cancel.is_cancelled() {
            return Self::failed_stream(
                model,
                StopReason::Aborted,
                "stream cancelled before it started",
            );
        }
        let Some(bridge) = provider_bridge(&self.provider_id) else {
            return Self::failed_stream(
                model,
                StopReason::Error,
                format!(
                    "provider {} is served by a plugin that is not running",
                    self.provider_id
                ),
            );
        };
        let (sender, stream) = event_stream();
        let params = BridgeStreamParams {
            stream_id: format!("ps-{}", NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed)),
            model: model.clone(),
            context: context.clone(),
            options: BridgeStreamOptions::from_stream_options(&options),
        };
        if let Err(message) = bridge.stream(params, options.cancel.clone(), sender) {
            return Self::failed_stream(model, StopReason::Error, message);
        }
        stream
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::types::{InputKind, ModelCost};

    fn bare_model(provider: &str) -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: EXT_PROVIDER_BRIDGE_API.to_string(),
            provider: provider.into(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputKind::Text],
            cost: ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn bare_context() -> Context {
        Context {
            system_prompt: None,
            messages: vec![crate::types::Message::user("hi")],
            tools: vec![],
        }
    }

    /// A bridge that answers `stream` by pushing one scripted event
    /// sequence into the sink.
    #[derive(Debug)]
    struct FakeBridge {
        events: Vec<AssistantMessageEvent>,
        fail_stream: Option<String>,
        seen_cancel: std::sync::Mutex<Option<String>>,
    }

    impl ProviderStreamBridge for FakeBridge {
        fn stream(
            &self,
            params: BridgeStreamParams,
            _cancel: CancellationToken,
            sink: AssistantMessageEventSender,
        ) -> Result<(), String> {
            if let Some(message) = &self.fail_stream {
                return Err(message.clone());
            }
            assert_eq!(params.model.api, EXT_PROVIDER_BRIDGE_API);
            for event in &self.events {
                if event.is_terminal() {
                    sink.finish(event.clone());
                    return Ok(());
                }
                assert!(sink.push(event.clone()));
            }
            Ok(())
        }
        fn cancel(&self, stream_id: &str) {
            *self.seen_cancel.lock().unwrap() = Some(stream_id.to_string());
        }
    }

    fn done_event(model: &Model) -> AssistantMessageEvent {
        let mut message = AssistantMessage::pending(model);
        message.stop_reason = StopReason::Stop;
        message.content = vec![crate::types::ContentBlock::text("answer")];
        AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message,
        }
    }

    #[tokio::test]
    async fn provider_for_resolves_bridge_api_kind() {
        let model = bare_model("acme");
        let provider = crate::provider::provider_for(&model).expect("bridge provider");
        let mut stream = provider.stream(&model, &bare_context(), StreamOptions::default());
        // No bridge registered: in-band error, not a panic or a hang.
        let event = stream.next().await.expect("terminal event");
        let AssistantMessageEvent::Error { reason, error } = event else {
            panic!("expected in-band error, got {event:?}");
        };
        assert_eq!(reason, StopReason::Error);
        assert!(error.error_message.unwrap().contains("acme"));
    }

    #[tokio::test]
    async fn registered_bridge_streams_events_to_terminal() {
        let model = bare_model("acme-events");
        register_provider_bridge(
            "acme-events",
            Arc::new(FakeBridge {
                events: vec![
                    AssistantMessageEvent::Start {
                        partial: AssistantMessage::pending(&model),
                    },
                    done_event(&model),
                ],
                fail_stream: None,
                seen_cancel: std::sync::Mutex::new(None),
            }),
        );
        let provider = BridgedProvider::new("acme-events");
        let mut stream = provider.stream(&model, &bare_context(), StreamOptions::default());
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        let terminal = stream.next().await.expect("terminal");
        assert!(terminal.is_terminal());
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.text(), "answer");
        unregister_provider_bridge("acme-events");
    }

    #[tokio::test]
    async fn pre_ack_error_is_an_in_band_error_event() {
        let model = bare_model("acme-fail");
        register_provider_bridge(
            "acme-fail",
            Arc::new(FakeBridge {
                events: vec![],
                fail_stream: Some("bridge exploded".to_string()),
                seen_cancel: std::sync::Mutex::new(None),
            }),
        );
        let provider = BridgedProvider::new("acme-fail");
        let message = provider
            .stream(&model, &bare_context(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("bridge exploded"));
        unregister_provider_bridge("acme-fail");
    }

    #[tokio::test]
    async fn pre_cancelled_stream_aborts_without_calling_the_bridge() {
        let model = bare_model("acme-cancel");
        let bridge = Arc::new(FakeBridge {
            events: vec![done_event(&model)],
            fail_stream: None,
            seen_cancel: std::sync::Mutex::new(None),
        });
        register_provider_bridge("acme-cancel", bridge.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let options = StreamOptions {
            cancel: cancel.clone(),
            ..Default::default()
        };
        let provider = BridgedProvider::new("acme-cancel");
        let message = provider
            .stream(&model, &bare_context(), options)
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert!(
            bridge.seen_cancel.lock().unwrap().is_none(),
            "a pre-cancelled stream never reaches the bridge"
        );
        unregister_provider_bridge("acme-cancel");
    }

    #[tokio::test]
    async fn re_registration_replaces_the_serving_bridge() {
        let model = bare_model("acme-re");
        let first = Arc::new(FakeBridge {
            events: vec![],
            fail_stream: Some("old".to_string()),
            seen_cancel: std::sync::Mutex::new(None),
        });
        register_provider_bridge("acme-re", first);
        let second = Arc::new(FakeBridge {
            events: vec![done_event(&model)],
            fail_stream: None,
            seen_cancel: std::sync::Mutex::new(None),
        });
        register_provider_bridge("acme-re", second);
        let provider = BridgedProvider::new("acme-re");
        let message = provider
            .stream(&model, &bare_context(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        unregister_provider_bridge("acme-re");
    }

    #[test]
    fn bridge_stream_options_project_the_wire_subset() {
        let options = StreamOptions {
            api_key: Some("secret".to_string()),
            max_tokens: Some(1024),
            reasoning: Some(ThinkingLevel::High),
            session_id: Some("s".to_string()),
            ..Default::default()
        };
        let wire = BridgeStreamOptions::from_stream_options(&options);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["maxTokens"], 1024);
        assert_eq!(json["reasoning"], "high");
        assert_eq!(json["sessionId"], "s");
        assert!(
            json.get("apiKey").is_none(),
            "bridge providers manage their own credentials: {json}"
        );
        let round: BridgeStreamOptions = serde_json::from_value(json).unwrap();
        assert_eq!(round.max_tokens, Some(1024));
    }

    #[test]
    fn assistant_message_event_wire_shape_roundtrips() {
        let model = bare_model("acme");
        let mut partial = AssistantMessage::pending(&model);
        partial.content = vec![crate::types::ContentBlock::text("he")];
        let event = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "he".to_string(),
            partial,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "textDelta");
        assert_eq!(json["contentIndex"], 0);
        assert_eq!(json["delta"], "he");
        assert!(json["partial"]["usage"]["totalTokens"].is_number());
        let parsed: AssistantMessageEvent = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
        // Terminal events roundtrip too.
        let done = done_event(&model);
        let json = serde_json::to_value(&done).unwrap();
        assert_eq!(json["type"], "done");
        let parsed: AssistantMessageEvent = serde_json::from_value(json).unwrap();
        assert!(parsed.is_terminal());
    }
}
