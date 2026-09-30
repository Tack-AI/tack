//! Google Generative AI (Gemini) adapter. Port of `google-generative-ai.ts`
//! and `google-shared.ts`, over raw HTTP (`streamGenerateContent?alt=sse`)
//! instead of the SDK.
//!
//! MVP cuts vs TS: no tool-search/validated modes (strict tool sampling is
//! reported but not enforced differently), no Vertex variant.

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::fail;
use crate::provider::StreamOptions;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use crate::transform::transform_messages;
use crate::types::{
    AssistantMessage, ContentBlock, Context, InputContentBlock, Message, Model, StopReason,
    ThinkingBudgets, ThinkingLevel, ToolDefinition, UserContent, calculate_cost,
};

const TACK_USER_AGENT: &str = concat!("tack/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, Default)]
pub struct GoogleGenerativeAiProvider;

impl crate::provider::Provider for GoogleGenerativeAiProvider {
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

// ---------------------------------------------------------------------------
// Message conversion (google-shared convertMessages)
// ---------------------------------------------------------------------------

fn gemini_major_version(model_id: &str) -> Option<u32> {
    let id = model_id.to_lowercase();
    let rest = id
        .strip_prefix("gemini-")
        .or_else(|| id.strip_prefix("gemini-live-"))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    gemini_major_version(model_id).is_none_or(|v| v >= 3)
}

fn is_valid_thought_signature(signature: &str) -> bool {
    !signature.is_empty()
        && signature.len().is_multiple_of(4)
        && signature
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
}

pub(crate) fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let needs_id = requires_tool_call_id(&model.id);
    let normalize_tool_call_id = |id: &str| -> String {
        if !needs_id {
            return id.to_string();
        }
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect()
    };
    let transformed = transform_messages(
        context.messages.as_slice(),
        model,
        Some(&normalize_tool_call_id),
    );

    let mut contents: Vec<Value> = Vec::new();

    for msg in &transformed {
        match msg {
            Message::User(u) => match &u.content {
                UserContent::Text(text) => {
                    contents.push(json!({ "role": "user", "parts": [{ "text": text }] }));
                }
                UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|b| match b {
                            InputContentBlock::Text { text, .. } => json!({ "text": text }),
                            InputContentBlock::Image { data, mime_type } => json!({
                                "inlineData": { "mimeType": mime_type, "data": data },
                            }),
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            },
            Message::Assistant(a) => {
                let mut parts: Vec<Value> = Vec::new();
                let is_same = a.provider == model.provider && a.model == model.id;
                let sig = |s: &Option<String>| -> Option<String> {
                    s.as_ref()
                        .filter(|sig| is_same && is_valid_thought_signature(sig))
                        .cloned()
                };

                for block in &a.content {
                    match block {
                        ContentBlock::Text {
                            text,
                            text_signature,
                        } => {
                            let signature = sig(text_signature);
                            // Keep empty text only when it carries a signature.
                            if text.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            let mut part = json!({ "text": text });
                            if let Some(s) = signature {
                                part["thoughtSignature"] = json!(s);
                            }
                            parts.push(part);
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            ..
                        } => {
                            if is_same {
                                let signature = sig(thinking_signature);
                                if thinking.trim().is_empty() && signature.is_none() {
                                    continue;
                                }
                                let mut part = json!({ "thought": true, "text": thinking });
                                if let Some(s) = signature {
                                    part["thoughtSignature"] = json!(s);
                                }
                                parts.push(part);
                            } else if !thinking.trim().is_empty() {
                                parts.push(json!({ "text": thinking }));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            thought_signature,
                            ..
                        } => {
                            let signature = sig(thought_signature);
                            let mut call = json!({ "name": name, "args": arguments });
                            if needs_id {
                                call["id"] = json!(id);
                            }
                            let mut part = json!({ "functionCall": call });
                            if let Some(s) = signature {
                                part["thoughtSignature"] = json!(s);
                            }
                            parts.push(part);
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            Message::ToolResult(t) => {
                let text_result: String = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        InputContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images: Vec<&InputContentBlock> = if model.supports_images() {
                    t.content
                        .iter()
                        .filter(|c| matches!(c, InputContentBlock::Image { .. }))
                        .collect()
                } else {
                    Vec::new()
                };
                let response_value = if !text_result.is_empty() {
                    text_result
                } else if !images.is_empty() {
                    "(see attached image)".to_string()
                } else {
                    String::new()
                };
                let multimodal = supports_multimodal_function_response(&model.id);

                // functionResponse: { name, response: { output|error }, parts?, id? }
                // (TS google-shared convertMessages) — the output/error payload
                // is nested under a `response` key, not flat in functionResponse.
                let mut function_response = serde_json::Map::new();
                function_response.insert("name".into(), json!(t.tool_name));
                let payload = if t.is_error {
                    json!({ "error": response_value })
                } else {
                    json!({ "output": response_value })
                };
                function_response.insert("response".into(), payload);
                if !images.is_empty() && multimodal {
                    let image_parts: Vec<Value> = images
                        .iter()
                        .map(|b| match b {
                            InputContentBlock::Image { data, mime_type } => json!({
                                "inlineData": { "mimeType": mime_type, "data": data },
                            }),
                            _ => unreachable!(),
                        })
                        .collect();
                    function_response.insert("parts".into(), Value::Array(image_parts));
                }
                if needs_id {
                    function_response.insert("id".into(), json!(t.tool_call_id));
                }

                let part = json!({ "functionResponse": Value::Object(function_response) });
                // Merge consecutive function responses into one user turn.
                let merged = match contents.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["parts"].as_array().is_some_and(|parts| {
                                parts.iter().any(|p| p.get("functionResponse").is_some())
                            }) =>
                    {
                        last["parts"]
                            .as_array_mut()
                            .expect("parts array")
                            .push(part.clone());
                        true
                    }
                    _ => false,
                };
                if !merged {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }

                if !images.is_empty() && !multimodal {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    for image in images {
                        if let InputContentBlock::Image { data, mime_type } = image {
                            parts.push(json!({
                                "inlineData": { "mimeType": mime_type, "data": data },
                            }));
                        }
                    }
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            // The caller collapses transcripts before building the Context;
            // skip system messages defensively.
            Message::System(_) => {}
        }
    }

    contents
}

const JSON_SCHEMA_META_DECLARATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    "definitions",
];

fn sanitize_for_openapi(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => map
            .iter()
            .filter(|(k, _)| !JSON_SCHEMA_META_DECLARATIONS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), sanitize_for_openapi(v)))
            .collect(),
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_for_openapi).collect()),
        other => other.clone(),
    }
}

fn convert_tools(tools: &[ToolDefinition]) -> Value {
    json!([{
        "functionDeclarations": tools.iter().map(|tool| json!({
            "name": tool.name,
            "description": tool.description,
            "parametersJsonSchema": sanitize_for_openapi(&tool.parameters),
        })).collect::<Vec<_>>()
    }])
}

// ---------------------------------------------------------------------------
// Thinking config
// ---------------------------------------------------------------------------

fn is_gemini3_pro(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    id.contains("gemini-3") && id.contains("-pro")
}

fn is_gemini3_flash(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    (id.contains("gemini-3") && id.contains("-flash"))
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
}

fn is_gemma4(model_id: &str) -> bool {
    model_id.to_lowercase().contains("gemma-4") || model_id.to_lowercase().contains("gemma4")
}

fn thinking_level_for(model: &Model, level: ThinkingLevel) -> &'static str {
    let mapped = model.thinking_level_value(level).cloned().flatten();
    let resolved = mapped.unwrap_or_else(|| level.clamped().as_str().to_string());
    match resolved.as_str() {
        "minimal" => {
            if is_gemini3_pro(model.id.as_str()) {
                "LOW"
            } else {
                "MINIMAL"
            }
        }
        "low" => "LOW",
        "medium" => {
            if is_gemini3_pro(model.id.as_str()) || is_gemma4(model.id.as_str()) {
                "HIGH"
            } else {
                "MEDIUM"
            }
        }
        _ => "HIGH",
    }
}

fn budget_for(model: &Model, level: ThinkingLevel, custom: Option<ThinkingBudgets>) -> i64 {
    if let Some(budgets) = custom {
        return budgets.for_level(level) as i64;
    }
    let id = model.id.as_str();
    let table = if id.contains("2.5-pro") {
        Some([128, 2048, 8192, 32768])
    } else if id.contains("2.5-flash-lite") {
        Some([512, 2048, 8192, 24576])
    } else if id.contains("2.5-flash") {
        Some([128, 2048, 8192, 24576])
    } else {
        None
    };
    match table {
        Some(t) => match level.clamped() {
            ThinkingLevel::Minimal => t[0],
            ThinkingLevel::Low => t[1],
            ThinkingLevel::Medium => t[2],
            _ => t[3],
        },
        None => -1, // dynamic
    }
}

pub(crate) fn build_params(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let contents = convert_messages(model, context);

    let mut params = json!({ "contents": contents });

    let mut config = json!({});
    if let Some(system_prompt) = &context.system_prompt {
        config["systemInstruction"] = json!(system_prompt);
    }
    if let Some(temperature) = options.temperature {
        config["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = options.max_tokens {
        config["maxOutputTokens"] = json!(max_tokens);
    }
    if !context.tools.is_empty() {
        config["tools"] = convert_tools(&context.tools);
        if let Some(choice) = options.tool_choice {
            config["toolConfig"] = json!({
                "functionCallingConfig": {
                    "mode": match choice {
                        crate::provider::ToolChoice::Auto => "AUTO",
                        crate::provider::ToolChoice::None => "NONE",
                    }
                }
            });
        }
    }

    if model.reasoning {
        match options.reasoning {
            Some(level) => {
                let mut thinking = json!({ "includeThoughts": true });
                if is_gemini3_pro(model.id.as_str())
                    || is_gemini3_flash(model.id.as_str())
                    || is_gemma4(model.id.as_str())
                {
                    thinking["thinkingLevel"] = json!(thinking_level_for(model, level));
                } else {
                    thinking["thinkingBudget"] =
                        json!(budget_for(model, level, options.thinking_budgets));
                }
                config["thinkingConfig"] = thinking;
            }
            None => {
                // Gemini 3 can't fully disable thinking; use the lowest level
                // without includeThoughts. Gemini 2.x: budget 0.
                let level = if is_gemini3_pro(model.id.as_str()) {
                    Some("LOW")
                } else if is_gemini3_flash(model.id.as_str()) || is_gemma4(model.id.as_str()) {
                    Some("MINIMAL")
                } else {
                    None
                };
                config["thinkingConfig"] = match level {
                    Some(level) => json!({ "thinkingLevel": level }),
                    None => json!({ "thinkingBudget": 0 }),
                };
            }
        }
    }

    params["generationConfig"] = config.clone();
    // The REST endpoint accepts config fields at top level of "generationConfig"
    // only for generation settings; system/tools/thinking live in the root.
    params
        .as_object_mut()
        .expect("params object")
        .remove("generationConfig");
    let root = params.as_object_mut().expect("params object");
    if let Some(obj) = config.as_object() {
        for (k, v) in obj {
            root.insert(k.clone(), v.clone());
        }
    }
    params
}

pub(crate) fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    let base = model.base_url.trim().trim_end_matches('/');
    let url = if base.is_empty() {
        // TS: without an explicit baseUrl the genai SDK falls back to
        // GOOGLE_GEMINI_BASE_URL (then its built-in host) plus the default
        // v1beta version path.
        let host = std::env::var("GOOGLE_GEMINI_BASE_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_string());
        format!(
            "{host}/v1beta/{}:streamGenerateContent?alt=sse",
            t_model(&model.id)
        )
    } else {
        // An explicit baseUrl is version-inclusive (TS sets apiVersion=""
        // so the SDK appends nothing).
        format!(
            "{base}/{}:streamGenerateContent?alt=sse",
            t_model(&model.id)
        )
    };
    let auth = match options.api_key.clone() {
        Some(key) => GoogleAuth::Header(key),
        None => {
            let mut output = AssistantMessage::pending(&model);
            fail!(
                output,
                sender,
                format!("No API key for provider: {}", model.provider),
                false
            );
        }
    };
    run_with_url(model, context, options, sender, url, auth).await;
}

/// How auth is attached to the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoogleAuth {
    /// `x-goog-api-key` header (Generative AI API and Vertex express mode —
    /// never a `?key=` query param: reqwest's error Display embeds the full
    /// URL and would leak the key into logs and in-band error events).
    Header(String),
    /// `Authorization: Bearer` (Vertex ADC OAuth2 access token).
    Bearer(String),
}

/// Shared runner for Google-protocol endpoints (Generative AI + Vertex).
pub(crate) async fn run_with_url(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
    url: String,
    auth: GoogleAuth,
) {
    let cancel: CancellationToken = options.cancel.clone();
    let mut output = AssistantMessage::pending(&model);
    let mut coalescer = crate::api::DeltaCoalescer::new();

    let params = build_params(&model, &context, &options);

    let client = crate::api::http_client();
    let build_request = || {
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("user-agent", TACK_USER_AGENT);
        match &auth {
            GoogleAuth::Header(key) => request = request.header("x-goog-api-key", key),
            GoogleAuth::Bearer(token) => {
                request = request.header("authorization", format!("Bearer {token}"));
            }
        }
        if let Some(headers) = &model.headers {
            for (k, v) in headers {
                request = request.header(k, v);
            }
        }
        for (k, v) in &options.headers {
            request = request.header(k, v);
        }
        request.body(params.to_string())
    };

    let response = match crate::api::send_with_retry(build_request, &cancel, &params).await {
        Ok(r) => r,
        Err(e) => {
            let aborted = cancel.is_cancelled() || e.is_aborted();
            coalescer.flush_into(&sender, &output);
            fail!(output, sender, format!("Google API error: {e}"), aborted);
        }
    };

    coalescer.push(
        &sender,
        &output,
        AssistantMessageEvent::Start {
            partial: output.clone(),
        },
    );

    // --- SSE consumption ---
    let mut sse = crate::api::SseStream::new(response.bytes_stream(), cancel.clone());
    // Current open block: index + whether it's thinking.
    let mut current_block: Option<(usize, bool)> = None;
    let mut stream_error: Option<crate::api::ApiError> = None;
    let mut tool_call_counter = 0u64;

    loop {
        let chunk: Value = match sse.next_json("Google SSE chunk").await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };

        if output.response_id.is_none()
            && let Some(id) = chunk.get("responseId").and_then(Value::as_str)
        {
            output.response_id = Some(id.to_string());
        }

        let candidate = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|a| a.first());
        if let Some(parts) = candidate
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    let is_thinking = part.get("thought").and_then(Value::as_bool) == Some(true);
                    let signature = part
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .map(str::to_string);

                    let needs_new = match current_block {
                        Some((_, thinking_now)) => thinking_now != is_thinking,
                        None => true,
                    };
                    if needs_new {
                        crate::api::close_current_block!(current_block, output, sender, coalescer);
                        let idx = output.content.len();
                        if is_thinking {
                            output.content.push(ContentBlock::Thinking {
                                thinking: String::new(),
                                thinking_signature: None,
                                redacted: None,
                            });
                            coalescer.push(
                                &sender,
                                &output,
                                AssistantMessageEvent::ThinkingStart {
                                    content_index: idx,
                                    partial: output.clone(),
                                },
                            );
                        } else {
                            output.content.push(ContentBlock::Text {
                                text: String::new(),
                                text_signature: None,
                            });
                            coalescer.push(
                                &sender,
                                &output,
                                AssistantMessageEvent::TextStart {
                                    content_index: idx,
                                    partial: output.clone(),
                                },
                            );
                        }
                        current_block = Some((idx, is_thinking));
                    }
                    let (idx, _) = current_block.expect("block just opened");
                    match output.content.get_mut(idx) {
                        Some(ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            ..
                        }) => {
                            thinking.push_str(text);
                            if let Some(sig) = signature
                                && !sig.is_empty()
                            {
                                *thinking_signature = Some(sig);
                            }
                            if let Some(ev) = coalescer.offer(
                                crate::api::DeltaKind::Thinking,
                                idx,
                                text.to_string(),
                                &output,
                            ) {
                                let _ = sender.push(ev);
                            }
                        }
                        Some(ContentBlock::Text {
                            text: t,
                            text_signature,
                        }) => {
                            t.push_str(text);
                            if let Some(sig) = signature
                                && !sig.is_empty()
                            {
                                *text_signature = Some(sig);
                            }
                            if let Some(ev) = coalescer.offer(
                                crate::api::DeltaKind::Text,
                                idx,
                                text.to_string(),
                                &output,
                            ) {
                                let _ = sender.push(ev);
                            }
                        }
                        _ => {}
                    }
                }

                if let Some(call) = part.get("functionCall") {
                    crate::api::close_current_block!(current_block, output, sender, coalescer);
                    let provided_id = call.get("id").and_then(Value::as_str);
                    let duplicate = provided_id.is_some_and(|id| {
                        output.tool_calls().any(|(existing, _, _)| existing == id)
                    });
                    tool_call_counter += 1;
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    // TS: `${name}_${Date.now()}_${++counter}` when the id is
                    // missing or a duplicate.
                    let id = match provided_id {
                        Some(id) if !duplicate => id.to_string(),
                        _ => format!("{name}_{}_{}", tack_ai_now_millis(), tool_call_counter),
                    };
                    let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    let block = ContentBlock::ToolCall {
                        id,
                        name,
                        arguments: arguments.clone(),
                        thought_signature: part
                            .get("thoughtSignature")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        namespace: None,
                    };
                    output.content.push(block);
                    let idx = output.content.len() - 1;
                    coalescer.push(
                        &sender,
                        &output,
                        AssistantMessageEvent::ToolCallStart {
                            content_index: idx,
                            partial: output.clone(),
                        },
                    );
                    // TS pushes a toolcall_delta with the full arguments JSON
                    // (Google delivers function calls atomically, not streamed).
                    coalescer.push(
                        &sender,
                        &output,
                        AssistantMessageEvent::ToolCallDelta {
                            content_index: idx,
                            delta: arguments.to_string(),
                            partial: output.clone(),
                        },
                    );
                    coalescer.push(
                        &sender,
                        &output,
                        AssistantMessageEvent::ToolCallEnd {
                            content_index: idx,
                            tool_call: output.content[idx].clone(),
                            partial: output.clone(),
                        },
                    );
                }
            }
        }

        if let Some(reason) = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
        {
            output.raw_stop_reason = Some(reason.to_string());
            output.stop_reason = map_stop_reason(reason);
            if output.has_tool_calls() && output.stop_reason == StopReason::Stop {
                output.stop_reason = StopReason::ToolUse;
            }
        }

        if let Some(usage) = chunk.get("usageMetadata") {
            let prompt = usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = usage
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let candidates = usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let thoughts = usage
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            output.usage.input = prompt.saturating_sub(cached);
            output.usage.output = candidates + thoughts;
            output.usage.cache_read = cached;
            output.usage.cache_write = 0;
            output.usage.reasoning = Some(thoughts);
            output.usage.total_tokens = usage
                .get("totalTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(output.usage.input + output.usage.output + cached);
            calculate_cost(&model, &mut output.usage);
        }
    }

    crate::api::close_current_block!(current_block, output, sender, coalescer);

    if let Some(error) = stream_error {
        let aborted = cancel.is_cancelled() || error.is_aborted();
        coalescer.flush_into(&sender, &output);
        fail!(output, sender, error.to_string(), aborted);
    }
    if output.stop_reason == StopReason::Pending {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            "Google stream ended without a finish reason".to_string(),
            false
        );
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        let message = output
            .raw_stop_reason
            .clone()
            .map(|r| format!("Provider stopped with: {r}"))
            .or_else(|| output.error_message.clone())
            .unwrap_or_else(|| "An unknown error occurred".into());
        let aborted = output.stop_reason == StopReason::Aborted;
        coalescer.flush_into(&sender, &output);
        fail!(output, sender, message, aborted);
    }

    coalescer.flush_into(&sender, &output);
    sender.finish(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output,
    });
}

fn tack_ai_now_millis() -> u64 {
    crate::types::now_millis()
}

/// TS genai SDK `tModel` (Gemini branch): resource-prefixed ids pass
/// through verbatim, bare ids get the `models/` prefix. Ids are
/// interpolated raw (no percent-encoding), matching the SDK.
pub(crate) fn t_model(id: &str) -> String {
    if id.starts_with("models/") || id.starts_with("tunedModels/") {
        id.to_string()
    } else {
        format!("models/{id}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::types::ToolResultMessage;

    fn model(id: &str) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: "google-generative-ai".to_string(),
            provider: "google".to_string(),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: crate::types::ModelCost::default(),
            context_window: 1_000_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn tool_result(id: &str, name: &str, text: &str, is_error: bool) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            content: vec![InputContentBlock::text(text)],
            details: None,
            usage: None,
            is_error,
            timestamp: 1,
        })
    }

    /// TS genai SDK `tModel` (Gemini branch): bare ids get `models/`,
    /// resource-prefixed ids pass through verbatim.
    #[test]
    fn t_model_prefixes_bare_ids() {
        assert_eq!(t_model("gemini-2.5-flash"), "models/gemini-2.5-flash");
        assert_eq!(
            t_model("models/gemini-2.5-flash"),
            "models/gemini-2.5-flash"
        );
        assert_eq!(t_model("tunedModels/x"), "tunedModels/x");
    }

    /// Regression: the output/error payload must be nested under a `response`
    /// key inside functionResponse (TS google-shared convertMessages), not
    /// flat alongside `name`.
    #[test]
    fn tool_result_payload_nests_under_response_key() {
        let context = Context {
            system_prompt: None,
            messages: vec![tool_result("call1", "read", "file contents", false)],
            tools: vec![],
        };
        let contents = convert_messages(&model("gemini-2.5-flash"), &context);
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "read");
        assert_eq!(fr["response"], json!({ "output": "file contents" }));
        assert!(fr.get("output").is_none(), "output must not be flat: {fr}");
        assert!(fr.get("id").is_none(), "gemini 2.x needs no id: {fr}");

        let context = Context {
            system_prompt: None,
            messages: vec![tool_result("call1", "read", "boom", true)],
            tools: vec![],
        };
        let contents = convert_messages(&model("gemini-2.5-flash"), &context);
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["response"], json!({ "error": "boom" }));
    }

    #[test]
    fn consecutive_tool_results_merge_and_gemini3_carries_id() {
        let context = Context {
            system_prompt: None,
            messages: vec![
                tool_result("id1", "read", "one", false),
                tool_result("id2", "read", "two", false),
            ],
            tools: vec![],
        };
        let contents = convert_messages(&model("gemini-3-pro"), &context);
        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["functionResponse"]["id"], "id1");
        assert_eq!(parts[1]["functionResponse"]["id"], "id2");
        assert_eq!(
            parts[0]["functionResponse"]["response"],
            json!({ "output": "one" })
        );
    }

    #[test]
    fn sanitize_strips_meta_declarations_recursively() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": { "x": { "$defs": {}, "type": "string" } },
        });
        let sanitized = sanitize_for_openapi(&schema);
        assert!(sanitized.get("$schema").is_none());
        assert!(sanitized["properties"]["x"].get("$defs").is_none());
    }
}
