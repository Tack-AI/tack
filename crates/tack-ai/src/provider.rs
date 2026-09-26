//! Provider trait and stream options.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::stream::AssistantMessageEventStream;
use crate::types::{Context, Model, ThinkingBudgets, ThinkingLevel};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
        _ => None,
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
