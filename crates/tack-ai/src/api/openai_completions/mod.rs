//! OpenAI Chat Completions API adapter. Lean port of
//! `packages/ai/src/api/openai-completions.ts`, including the `detectCompat`
//! URL/provider auto-detection matrix. Request bodies are built as raw
//! `serde_json::Value`.
//!
//! MVP scope vs the TS original: no grammar/custom tools, no Kimi deferred
mod compat;
mod params;
mod stream;

pub use compat::{OpenAiCompat, ResolvedCompat};

use stream::run;

use crate::provider::StreamOptions;
use crate::types::{Context, Model};

#[derive(Clone, Debug, Default)]
pub struct OpenAiCompletionsProvider;

impl crate::provider::Provider for OpenAiCompletionsProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> crate::stream::AssistantMessageEventStream {
        let (sender, stream) = crate::stream::event_stream();
        let model = model.clone();
        let context = context.clone();
        tokio::spawn(async move {
            run(model, context, options, sender).await;
        });
        stream
    }
}

#[cfg(test)]
pub(crate) fn generic_model(provider: &str, base_url: &str) -> Model {
    Model {
        id: "test-model".into(),
        name: "test-model".into(),
        api: "openai-completions".into(),
        provider: provider.into(),
        base_url: base_url.into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}
