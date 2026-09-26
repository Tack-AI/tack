//! Images API (port of `packages/ai/src/providers/images/` +
//! `api/openrouter-images.ts`): image generation via chat completions with
//! `modalities: ["image","text"]`, non-streaming. No built-in tool calls it
//! (same as TS pi's coding agent); it exists for API parity and extensions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::types::{Model, Usage, UsageCost};

/// Input block for image generation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImagesInputBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesContext {
    pub input: Vec<ImagesInputBlock>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImagesOutputBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub provider: String,
    pub model: String,
    pub output: Vec<ImagesOutputBlock>,
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ImagesOptions {
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Generate images with a model (api kind must be `openrouter-images`).
pub async fn generate_images(
    model: &Model,
    context: &ImagesContext,
    options: ImagesOptions,
) -> AssistantImages {
    let mut output = AssistantImages {
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        stop_reason: "stop".to_string(),
        error_message: None,
        usage: None,
        response_id: None,
    };
    let Some(api_key) = options.api_key.clone() else {
        output.stop_reason = "error".to_string();
        output.error_message = Some(format!("No API key for provider: {}", model.provider));
        return output;
    };

    let content: Vec<Value> = context
        .input
        .iter()
        .map(|block| match block {
            ImagesInputBlock::Text { text } => json!({ "type": "text", "text": text }),
            ImagesInputBlock::Image { data, mime_type } => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
            }),
        })
        .collect();
    let payload = json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": ["image", "text"],
    });

    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let client = crate::api::http_client();
    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {api_key}"));
    if let Some(headers) = &model.headers {
        for (k, v) in headers {
            request = request.header(k, v);
        }
    }
    for (k, v) in &options.headers {
        request = request.header(k, v);
    }
    let request = request.body(payload.to_string());

    let response = tokio::select! {
        _ = options.cancel.cancelled() => {
            output.stop_reason = "aborted".to_string();
            return output;
        }
        r = request.send() => r,
    };
    let response = match response {
        Ok(r) => r,
        Err(e) => {
            output.stop_reason = "error".to_string();
            output.error_message = Some(format!("{} API error: {e}", model.provider));
            return output;
        }
    };
    let status = response.status();
    let body: Value = match response.json().await {
        Ok(b) => b,
        Err(e) => {
            output.stop_reason = "error".to_string();
            output.error_message = Some(format!("{} API error: {e}", model.provider));
            return output;
        }
    };
    if !status.is_success() {
        output.stop_reason = "error".to_string();
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| crate::api::truncate_for_error(&body.to_string()));
        output.error_message = Some(format!("{} API error: {status}: {message}", model.provider));
        return output;
    }

    output.response_id = body.get("id").and_then(Value::as_str).map(str::to_string);
    if let Some(usage) = body.get("usage") {
        output.usage = Some(parse_usage(usage, model));
    }

    if let Some(choice) = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    {
        let message = choice.get("message").cloned().unwrap_or(Value::Null);
        if let Some(text) = message.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            output.output.push(ImagesOutputBlock::Text {
                text: text.to_string(),
            });
        }
        for image in message
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let url = image.get("image_url").and_then(|u| {
                u.as_str()
                    .map(str::to_string)
                    .or_else(|| u.get("url").and_then(Value::as_str).map(str::to_string))
            });
            let Some(url) = url else { continue };
            let Some(data) = url.strip_prefix("data:") else {
                continue;
            };
            let Some((mime, base64)) = data.split_once(";base64,") else {
                continue;
            };
            output.output.push(ImagesOutputBlock::Image {
                data: base64.to_string(),
                mime_type: mime.to_string(),
            });
        }
    }

    output
}

fn parse_usage(raw: &Value, model: &Model) -> Usage {
    let prompt = raw
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = raw
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = raw
        .pointer("/prompt_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = if cache_write > 0 {
        cached.saturating_sub(cache_write)
    } else {
        cached
    };
    let input = prompt.saturating_sub(cache_read + cache_write);
    let output = raw
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let rates = &model.cost;
    let cost = UsageCost {
        input: rates.input / 1_000_000.0 * input as f64,
        output: rates.output / 1_000_000.0 * output as f64,
        cache_read: rates.cache_read / 1_000_000.0 * cache_read as f64,
        cache_write: rates.cache_write / 1_000_000.0 * cache_write as f64,
        total: 0.0,
    };
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost {
            total: cost.input + cost.output + cost.cache_read + cost.cache_write,
            ..cost
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn model(base_url: &str) -> Model {
        Model {
            id: "gemini-image".to_string(),
            name: "Gemini Image".to_string(),
            api: "openrouter-images".to_string(),
            provider: "openrouter".to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: crate::types::ModelCost {
                input: 2.0,
                output: 8.0,
                cache_read: 0.5,
                cache_write: 2.0,
                tiers: None,
            },
            context_window: 128_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    #[tokio::test]
    async fn generates_and_parses_images() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 65536];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.contains("\"modalities\":[\"image\",\"text\"]"),
                "{request}"
            );
            let body = serde_json::json!({
                "id": "gen-1",
                "choices": [{
                    "message": {
                        "content": "here you go",
                        "images": [{ "image_url": { "url": "data:image/png;base64,QUJD" } }],
                    }
                }],
                "usage": { "prompt_tokens": 20, "completion_tokens": 5 },
            });
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.to_string().len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let context = ImagesContext {
            input: vec![ImagesInputBlock::Text {
                text: "a cat".to_string(),
            }],
        };
        let result = generate_images(
            &model(&format!("http://{addr}")),
            &context,
            ImagesOptions {
                api_key: Some("k".to_string()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(result.stop_reason, "stop");
        assert_eq!(result.response_id.as_deref(), Some("gen-1"));
        assert_eq!(result.output.len(), 2);
        let ImagesOutputBlock::Image { data, mime_type } = &result.output[1] else {
            panic!("expected image: {:?}", result.output);
        };
        assert_eq!(data, "QUJD");
        assert_eq!(mime_type, "image/png");
        let usage = result.usage.unwrap();
        assert_eq!(usage.input, 20);
        assert_eq!(usage.output, 5);
    }
}
