//! Google Vertex AI adapter. Two auth modes (port of `google-vertex.ts`):
//! - **Express / API key** (`GOOGLE_CLOUD_API_KEY` or --api-key): key sent as
//!   the `x-goog-api-key` header on the express endpoint (NOT a `?key=`
//!   query param — reqwest's error Display embeds the full URL and would
//!   leak the key into logs and in-band error events).
//! - **ADC** (no API key): OAuth2 access token from a service-account JSON
//!   (`GOOGLE_APPLICATION_CREDENTIALS`), the gcloud well-known ADC file, or
//!   the GCE metadata server as a last resort — see `vertex_adc.rs`.
//!
//! Reuses the Google Generative AI message conversion and stream handling.

use crate::provider::StreamOptions;
use crate::types::{Context, Model};

use super::google_generative_ai::{GoogleAuth, run_with_url, urlencoding};

#[derive(Clone, Debug, Default)]
pub struct GoogleVertexProvider;

impl crate::provider::Provider for GoogleVertexProvider {
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

fn fail(sender: crate::stream::AssistantMessageEventSender, model: &Model, message: String) {
    let mut output = crate::types::AssistantMessage::pending(model);
    output.stop_reason = crate::types::StopReason::Error;
    output.error_message = Some(message);
    sender.finish(crate::stream::AssistantMessageEvent::Error {
        reason: crate::types::StopReason::Error,
        error: output,
    });
}

/// Build the Vertex streamGenerateContent URL.
///
/// - Express mode (API key): `https://aiplatform.googleapis.com/v1/publishers/google/models/<model>:streamGenerateContent?alt=sse`
/// - ADC mode: `https://{location}-aiplatform.googleapis.com/v1/projects/<project>/locations/<location>/publishers/google/models/<model>:streamGenerateContent?alt=sse`
/// - Custom `model.base_url` with `{location}`: substitute GOOGLE_CLOUD_LOCATION
///   (default "global") and append the publisher path.
async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: crate::stream::AssistantMessageEventSender,
) {
    let base = model.base_url.trim();

    // Mode A: API key (express). Also honored with a custom base_url.
    if let Some(api_key) = options.api_key.clone() {
        let location =
            std::env::var("GOOGLE_CLOUD_LOCATION").unwrap_or_else(|_| "global".to_string());
        let url = if base.is_empty() {
            format!(
                "https://aiplatform.googleapis.com/v1/publishers/google/models/{}:streamGenerateContent?alt=sse",
                urlencoding(&model.id)
            )
        } else {
            custom_base_url(base, &model.id, &location)
        };
        run_with_url(
            model,
            context,
            options,
            sender,
            url,
            GoogleAuth::Header(api_key),
        )
        .await;
        return;
    }

    // Mode B: ADC (env file → gcloud ADC → GCE metadata server).
    let adc = match super::vertex_adc::adc_with_metadata_fallback().await {
        Ok(Some(adc)) => adc,
        Ok(None) => {
            fail(
                sender,
                &model,
                format!(
                    "No credentials for provider: {}. Set GOOGLE_CLOUD_API_KEY (express) or GOOGLE_APPLICATION_CREDENTIALS / `gcloud auth application-default login` (ADC).",
                    model.provider
                ),
            );
            return;
        }
        Err(e) => {
            fail(sender, &model, format!("Vertex ADC error: {e}"));
            return;
        }
    };
    let Some(project) = super::vertex_adc::resolve_project(Some(&adc)) else {
        fail(
            sender,
            &model,
            "Vertex ADC needs a GCP project: set GOOGLE_CLOUD_PROJECT (or use a service-account JSON with project_id).".to_string(),
        );
        return;
    };
    // Location: GOOGLE_CLOUD_LOCATION wins; fall back to the region derived
    // from the instance zone (GCE metadata server), then "global".
    let location = std::env::var("GOOGLE_CLOUD_LOCATION")
        .ok()
        .filter(|l| !l.is_empty())
        .or_else(|| adc.location.clone())
        .unwrap_or_else(|| "global".to_string());

    let (token, _expiry) = match adc.source.access_token(&crate::api::http_client()).await {
        Ok(pair) => pair,
        Err(e) => {
            fail(sender, &model, format!("Vertex ADC token error: {e}"));
            return;
        }
    };

    let url = if !base.is_empty() && !base.contains("aiplatform.googleapis.com") {
        custom_base_url(base, &model.id, &location)
    } else {
        let host = if location == "global" {
            "https://aiplatform.googleapis.com".to_string()
        } else {
            format!("https://{location}-aiplatform.googleapis.com")
        };
        format!(
            "{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
            urlencoding(&model.id)
        )
    };

    run_with_url(
        model,
        context,
        options,
        sender,
        url,
        GoogleAuth::Bearer(token),
    )
    .await;
}

fn custom_base_url(base: &str, model_id: &str, location: &str) -> String {
    let base = base
        .replace("{location}", location)
        .trim_end_matches('/')
        .to_string();
    // If the custom base already contains an api-version segment, append the
    // publisher path directly; otherwise add /v1.
    let has_version = base.split('/').any(|part| {
        part.starts_with('v')
            && part[1..].chars().all(|c| c.is_ascii_alphanumeric())
            && part.len() > 1
            && part[1..2].chars().all(|c| c.is_ascii_digit())
    });
    let base = if has_version {
        base
    } else {
        format!("{base}/v1")
    };
    format!(
        "{base}/publishers/google/models/{}:streamGenerateContent?alt=sse",
        urlencoding(model_id)
    )
}
