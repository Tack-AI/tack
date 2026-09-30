//! Google Vertex AI adapter. Two auth modes (port of `google-vertex.ts`
//! plus the @google/genai SDK's URL rules):
//! - **Express / API key** (`GOOGLE_CLOUD_API_KEY` or --api-key): key sent as
//!   the `x-goog-api-key` header on the express endpoint (NOT a `?key=`
//!   query param — reqwest's error Display embeds the full URL and would
//!   leak the key into logs and in-band error events).
//! - **ADC** (no API key): OAuth2 access token from a service-account JSON
//!   (`GOOGLE_APPLICATION_CREDENTIALS`), the gcloud well-known ADC file, or
//!   the GCE metadata server as a last resort — see `vertex_adc.rs`. ADC
//!   requires `GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`; TS throws
//!   when either is missing.
//!
//! Endpoint resolution (TS): a custom `model.base_url` wins as a
//! collection-scope base (`{location}`-templated bases are ignored, matching
//! `resolveCustomBaseUrl`); otherwise `GOOGLE_VERTEX_BASE_URL` is honored;
//! otherwise the default host is derived from the location.
//!
//! Reuses the Google Generative AI message conversion and stream handling.

use std::sync::LazyLock;

use crate::provider::StreamOptions;
use crate::types::{Context, Model};

use super::google_generative_ai::{GoogleAuth, run_with_url};

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

/// `GOOGLE_VERTEX_BASE_URL`, honored by the TS genai SDK when no custom
/// httpOptions base URL is set.
fn env_base_url() -> Option<String> {
    std::env::var("GOOGLE_VERTEX_BASE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
}

/// Default ADC endpoint host for a location (SDK ApiClient rules): the
/// multi-regional `us`/`eu` locations use the rep.googleapis.com host.
fn default_adc_host(location: &str) -> String {
    if location == "global" {
        "https://aiplatform.googleapis.com".to_string()
    } else if location == "us" || location == "eu" {
        format!("https://aiplatform.{location}.rep.googleapis.com")
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    }
}

/// TS `resolveCustomBaseUrl`: empty or `{location}`-templated bases are not
/// custom bases at all — endpoint resolution falls back to the defaults.
fn resolve_custom_base_url(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.contains("{location}") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// TS `baseUrlIncludesApiVersion`: any path segment matching
/// `^v\d+(?:beta\d*)?$`.
fn base_url_includes_api_version(base_url: &str) -> bool {
    static SEGMENT_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^v\d+(?:beta\d*)?$").expect("valid regex"));
    if let Ok(url) = reqwest::Url::parse(base_url) {
        return url
            .path_segments()
            .is_some_and(|mut segments| segments.any(|part| SEGMENT_RE.is_match(part)));
    }
    // TS fallback when URL parsing fails: match the raw string.
    static RAW_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?:^|/)v\d+(?:beta\d*)?(?:/|$)").expect("valid regex")
    });
    RAW_RE.is_match(base_url)
}

/// Collection-scope URL (TS `baseUrlResourceScope = COLLECTION`): no
/// projects/locations prefix; `/v1` is appended only when the base carries
/// no version segment of its own.
fn collection_url(base: &str, model_id: &str) -> String {
    let base = if base_url_includes_api_version(base) {
        base.to_string()
    } else {
        format!("{base}/v1")
    };
    format!("{base}/{}:streamGenerateContent?alt=sse", t_model(model_id))
}

/// TS genai SDK `tModel` (Vertex branch): resource-rooted ids pass through
/// verbatim, `publisher/model` ids get expanded, bare ids default to the
/// google publisher. Ids are interpolated raw, matching the SDK.
fn t_model(id: &str) -> String {
    if id.starts_with("publishers/") || id.starts_with("projects/") || id.starts_with("models/") {
        id.to_string()
    } else if id.contains('/') {
        // JS `model.split('/', 2)` keeps only the first two segments.
        let mut parts = id.split('/');
        let publisher = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        format!("publishers/{publisher}/models/{name}")
    } else {
        format!("publishers/google/models/{id}")
    }
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: crate::stream::AssistantMessageEventSender,
) {
    let custom_base = resolve_custom_base_url(&model.base_url);

    // Mode A: API key (express). No project/location is involved, so the
    // model resource path is never project-prefixed (SDK ApiClient).
    if let Some(api_key) = options.api_key.clone() {
        let url = if let Some(base) = &custom_base {
            collection_url(base, &model.id)
        } else {
            let host =
                env_base_url().unwrap_or_else(|| "https://aiplatform.googleapis.com".to_string());
            format!(
                "{host}/v1/{}:streamGenerateContent?alt=sse",
                t_model(&model.id)
            )
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
            "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or use a service-account JSON with project_id.".to_string(),
        );
        return;
    };
    // TS `resolveLocation` throws without GOOGLE_CLOUD_LOCATION — no silent
    // default.
    let Some(location) = std::env::var("GOOGLE_CLOUD_LOCATION")
        .ok()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
    else {
        fail(
            sender,
            &model,
            "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION.".to_string(),
        );
        return;
    };

    let (token, _expiry) = match adc.source.access_token(&crate::api::http_client()).await {
        Ok(pair) => pair,
        Err(e) => {
            fail(sender, &model, format!("Vertex ADC token error: {e}"));
            return;
        }
    };

    let url = if let Some(base) = &custom_base {
        collection_url(base, &model.id)
    } else {
        let host = env_base_url().unwrap_or_else(|| default_adc_host(&location));
        let resource = t_model(&model.id);
        if resource.starts_with("projects/") {
            // SDK `shouldPrependVertexProjectPath`: already project-rooted.
            format!("{host}/v1/{resource}:streamGenerateContent?alt=sse")
        } else {
            format!(
                "{host}/v1/projects/{project}/locations/{location}/{resource}:streamGenerateContent?alt=sse"
            )
        }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn t_model_vertex_branch() {
        assert_eq!(
            t_model("gemini-2.5-pro"),
            "publishers/google/models/gemini-2.5-pro"
        );
        assert_eq!(
            t_model("meta/llama-3.3-70b-instruct-maas"),
            "publishers/meta/models/llama-3.3-70b-instruct-maas"
        );
        assert_eq!(
            t_model("publishers/meta/models/llama"),
            "publishers/meta/models/llama"
        );
        assert_eq!(
            t_model("projects/p/locations/l/models/m"),
            "projects/p/locations/l/models/m"
        );
        assert_eq!(t_model("models/gemini-2.5-pro"), "models/gemini-2.5-pro");
    }

    #[test]
    fn custom_base_url_resolution() {
        assert_eq!(resolve_custom_base_url(""), None);
        assert_eq!(resolve_custom_base_url("  "), None);
        assert_eq!(
            resolve_custom_base_url("https://{location}-aiplatform.googleapis.com"),
            None
        );
        assert_eq!(
            resolve_custom_base_url("https://proxy.example.com/").as_deref(),
            Some("https://proxy.example.com")
        );
    }

    #[test]
    fn api_version_detection() {
        assert!(base_url_includes_api_version(
            "https://proxy.example.com/v1"
        ));
        assert!(base_url_includes_api_version(
            "https://proxy.example.com/v1beta"
        ));
        assert!(base_url_includes_api_version(
            "https://proxy.example.com/v2beta1"
        ));
        assert!(!base_url_includes_api_version(
            "https://proxy.example.com/version1"
        ));
        assert!(!base_url_includes_api_version(
            "https://proxy.example.com/v"
        ));
        assert!(!base_url_includes_api_version("https://proxy.example.com"));
    }

    #[test]
    fn collection_url_appends_v1_only_without_version() {
        assert_eq!(
            collection_url("https://proxy.example.com", "gemini-2.5-pro"),
            "https://proxy.example.com/v1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            collection_url("https://proxy.example.com/v1beta", "gemini-2.5-pro"),
            "https://proxy.example.com/v1beta/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn default_adc_host_by_location() {
        assert_eq!(
            default_adc_host("global"),
            "https://aiplatform.googleapis.com"
        );
        assert_eq!(
            default_adc_host("us-central1"),
            "https://us-central1-aiplatform.googleapis.com"
        );
        assert_eq!(
            default_adc_host("us"),
            "https://aiplatform.us.rep.googleapis.com"
        );
        assert_eq!(
            default_adc_host("eu"),
            "https://aiplatform.eu.rep.googleapis.com"
        );
    }
}
