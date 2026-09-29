//! Provider trait and stream options.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::stream::AssistantMessageEventStream;
use crate::types::{Context, Model, ThinkingBudgets, ThinkingLevel};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
}

/// Unified options for streaming requests. Mirrors pi's `SimpleStreamOptions`:
/// the reasoning fields are provider-neutral and each adapter maps them to its
/// wire format (like pi's `streamSimple` does).
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub tool_choice: Option<ToolChoice>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    /// Extra headers merged over provider defaults.
    pub headers: BTreeMap<String, String>,
    /// Retry-scope cancellation: aborts an in-progress retry backoff without
    /// cancelling the whole stream (TS abortRetry).
    pub retry_cancel: Option<CancellationToken>,
    /// Arbitrary sampling parameters merged into the request body last
    /// (OpenAI-compatible adapters only).
    pub sampling_params: BTreeMap<String, Value>,
    pub cancel: CancellationToken,
}

/// A provider adapter: one per wire protocol (`anthropic-messages`,
/// `openai-completions`, ...).
///
/// CONTRACT (mirrors pi): `stream` never fails fatally for request/model/
/// runtime errors — they are encoded in-band as an `Error` event plus a final
/// `AssistantMessage` with `stop_reason: Error`/`Aborted`. `stream` is
/// non-async: it spawns internally and returns the stream immediately.
pub trait Provider: Send + Sync + std::fmt::Debug {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream;

    /// Non-streaming convenience: collect the final message.
    fn complete<'a>(
        &'a self,
        model: &'a Model,
        context: &'a Context,
        options: StreamOptions,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::types::AssistantMessage> + Send + 'a>> {
        Box::pin(async move { self.stream(model, context, options).result().await })
    }
}

/// Canonical api kind for the pi-messages wire protocol (Radius gateway /
/// generic proxy). Renamed from upstream's `pi-messages` in the pi→Tack cut.
pub const TACK_MESSAGES_API: &str = "tack-messages";
/// Legacy alias accepted for TS pi config parity: models.json entries and
/// runtime provider registrations with `api: "pi-messages"` are normalized
/// to `tack-messages` at load, and `provider_for` still resolves the alias
/// directly for any model that bypasses normalization.
pub const LEGACY_PI_MESSAGES_API: &str = "pi-messages";

/// Map an api-kind alias to its canonical name (identity for everything
/// else).
pub fn canonical_api_kind(api: &str) -> &str {
    match api {
        LEGACY_PI_MESSAGES_API => TACK_MESSAGES_API,
        other => other,
    }
}

/// Resolve the provider adapter for a model's `api` kind.
pub fn provider_for(model: &Model) -> Option<Arc<dyn Provider>> {
    use crate::api::ResponsesFlavor;
    match model.api.as_str() {
        "anthropic-messages" => Some(Arc::new(crate::api::AnthropicMessagesProvider)),
        "openai-completions" => Some(Arc::new(crate::api::OpenAiCompletionsProvider)),
        "openai-responses" => Some(Arc::new(crate::api::OpenAiResponsesProvider {
            flavor: ResponsesFlavor::OpenAi,
        })),
        "azure-openai-responses" => Some(Arc::new(crate::api::OpenAiResponsesProvider {
            flavor: ResponsesFlavor::Azure,
        })),
        "openai-codex-responses" => Some(Arc::new(crate::api::OpenAiResponsesProvider {
            flavor: ResponsesFlavor::Codex,
        })),
        "google-generative-ai" => Some(Arc::new(crate::api::GoogleGenerativeAiProvider)),
        "google-vertex" => Some(Arc::new(crate::api::GoogleVertexProvider)),
        "mistral-conversations" => Some(Arc::new(crate::api::MistralConversationsProvider)),
        // Canonical + legacy alias (defense in depth; loads normalize anyway).
        "tack-messages" | "pi-messages" => {
            Some(Arc::new(crate::api::TackMessagesProvider::radius()))
        }
        // Generic proxy stream function (agent/proxy.ts): routes LLM calls
        // through a server that manages auth upstream.
        "proxy-stream" => Some(Arc::new(crate::api::TackMessagesProvider::proxy())),
        "bedrock-converse-stream" => Some(Arc::new(crate::api::BedrockConverseStreamProvider)),
        crate::codebuddy::CODEBUDDY_API => {
            Some(Arc::new(crate::codebuddy::CodeBuddyStreamProvider))
        }
        // Plugin-served provider bridge (tack-RPC v3 `provider/stream`):
        // resolves the serving plugin connection from the bridge registry
        // at stream time.
        crate::provider_bridge::EXT_PROVIDER_BRIDGE_API => Some(Arc::new(
            crate::provider_bridge::BridgedProvider::new(model.provider.clone()),
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Provider events (rate limits, warnings) — P7c
// ---------------------------------------------------------------------------

/// Kind of a provider-scoped out-of-band event. `RateLimited` is the native
/// codebuddy path's kind; bridge providers map their `provider/event`
/// notification kinds onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderEventKind {
    RateLimited,
    Warning,
    Info,
}

/// A provider-scoped out-of-band event (rate limit, warning, info).
/// Generalizes the old codebuddy-only rate-limit hook into one channel for
/// native providers and plugin bridge providers alike: the TUI shows an
/// inline notice plus a (settings-gated) desktop notification; headless
/// modes log.
#[derive(Clone, Debug)]
pub struct ProviderEvent {
    pub kind: ProviderEventKind,
    /// The provider id the event belongs to (e.g. `codebuddy`, or a
    /// plugin-registered provider id).
    pub provider: String,
    pub message: String,
}

/// Provider event handler installed by tack-app (the TUI installs one;
/// headless modes leave it unset and events degrade to log lines).
type ProviderEventNotifier = Arc<dyn Fn(ProviderEvent) + Send + Sync>;

static PROVIDER_EVENT_NOTIFIER: OnceLock<RwLock<Option<ProviderEventNotifier>>> = OnceLock::new();

/// Install (or clear) the provider event handler.
pub fn set_provider_event_notifier(handler: Option<ProviderEventNotifier>) {
    let slot = PROVIDER_EVENT_NOTIFIER.get_or_init(|| RwLock::new(None));
    *slot.write().unwrap_or_else(|e| e.into_inner()) = handler;
}

/// Emit a provider event: to the installed handler, or to the log when no
/// handler is installed (headless modes).
pub fn emit_provider_event(event: ProviderEvent) {
    let handler = PROVIDER_EVENT_NOTIFIER
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match handler {
        Some(handler) => handler(event),
        None => match event.kind {
            ProviderEventKind::Info => {
                tracing::info!("{}: {}", event.provider, event.message)
            }
            ProviderEventKind::RateLimited | ProviderEventKind::Warning => {
                tracing::warn!("{}: {}", event.provider, event.message)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn bare_model(api: &str) -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: api.to_string(),
            provider: "radius".into(),
            base_url: "https://example.com".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: Default::default(),
            context_window: 1,
            max_tokens: 1,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn canonical_api_kind_maps_legacy_alias() {
        assert_eq!(canonical_api_kind("pi-messages"), "tack-messages");
        assert_eq!(canonical_api_kind("tack-messages"), "tack-messages");
        assert_eq!(
            canonical_api_kind("anthropic-messages"),
            "anthropic-messages"
        );
    }

    #[test]
    fn provider_for_resolves_canonical_and_legacy_alias() {
        assert!(provider_for(&bare_model(TACK_MESSAGES_API)).is_some());
        assert!(provider_for(&bare_model(LEGACY_PI_MESSAGES_API)).is_some());
    }
}
