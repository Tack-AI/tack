//! OpenAI Responses API adapter (+ Azure OpenAI Responses and OpenAI Codex
//! Responses variants). Port of `openai-responses.ts` +
//! `openai-responses-shared.ts` and `azure-openai-responses.ts`.
//!
//! MVP cuts vs TS: no grammar/custom tools, no deferred tools (additional
//! tools / tool search), no Copilot dynamic headers, no service-tier pricing.

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::fail;
use crate::json_repair::parse_streaming_json;
use crate::provider::{CacheRetention, StreamOptions};
use crate::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use crate::transform::transform_messages_with_source;
use crate::types::{
    AssistantMessage, ContentBlock, Context, InputContentBlock, Message, Model, StopReason,
    ToolDefinition, UserContent, calculate_cost,
};

const TACK_USER_AGENT: &str = concat!("tack/", env!("CARGO_PKG_VERSION"));
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u32 = 16;

/// Variant-specific behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponsesFlavor {
    OpenAi,
    Azure,
    Codex,
}

impl ResponsesFlavor {
    fn allowed_tool_call_providers(self) -> &'static [&'static str] {
        match self {
            ResponsesFlavor::OpenAi | ResponsesFlavor::Codex => {
                &["openai", "openai-codex", "opencode"]
            }
            ResponsesFlavor::Azure => &[
                "openai",
                "openai-codex",
                "opencode",
                "azure-openai-responses",
            ],
        }
    }
}

/// `OpenAIResponsesCompat` subset.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResponsesCompat {
    pub supports_developer_role: Option<bool>,
    pub supports_long_cache_retention: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    pub supports_explicit_prompt_cache_mode: Option<bool>,
}

#[derive(Clone, Debug)]
struct ResolvedResponsesCompat {
    supports_developer_role: bool,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    supports_explicit_prompt_cache_mode: bool,
}

fn resolve_compat(model: &Model) -> ResolvedResponsesCompat {
    let overrides: ResponsesCompat = model
        .compat
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    ResolvedResponsesCompat {
        supports_developer_role: overrides.supports_developer_role.unwrap_or(true),
        supports_long_cache_retention: overrides.supports_long_cache_retention.unwrap_or(true),
        supports_strict_mode: overrides.supports_strict_mode.unwrap_or(false),
        supports_explicit_prompt_cache_mode: overrides
            .supports_explicit_prompt_cache_mode
            .unwrap_or(false),
    }
}

/// The Responses provider; `flavor` selects OpenAI / Azure / Codex behavior.
#[derive(Clone, Copy, Debug)]
pub struct OpenAiResponsesProvider {
    pub flavor: ResponsesFlavor,
}

impl crate::provider::Provider for OpenAiResponsesProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> crate::stream::AssistantMessageEventStream {
        let (sender, stream) = crate::stream::event_stream();
        let model = model.clone();
        let context = context.clone();
        let flavor = self.flavor;
        tokio::spawn(async move {
            run(model, context, options, sender, flavor).await;
        });
        stream
    }
}

// ---------------------------------------------------------------------------
// Message conversion (convertResponsesMessages)
// ---------------------------------------------------------------------------

fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for b in s.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

fn short_hash(s: &str) -> String {
    format!("{:08x}", fnv1a(s))
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    sanitized.trim_end_matches('_').to_string()
}

/// Text signatures: TextSignatureV1 JSON or a legacy plain id.
fn encode_text_signature_v1(id: &str, phase: Option<&str>) -> String {
    match phase {
        Some(phase) => json!({ "v": 1, "id": id, "phase": phase }).to_string(),
        None => json!({ "v": 1, "id": id }).to_string(),
    }
}

fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature?;
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = parsed.get("id").and_then(Value::as_str)
    {
        let phase = parsed
            .get("phase")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Some((id.to_string(), phase));
    }
    Some((signature.to_string(), None))
}

fn convert_tool_result_output(model: &Model, content: &[InputContentBlock]) -> Value {
    let text: String = content
        .iter()
        .filter_map(|c| match c {
            InputContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&InputContentBlock> = content
        .iter()
        .filter(|c| matches!(c, InputContentBlock::Image { .. }))
        .collect();

    if images.is_empty() || !model.supports_images() {
        return Value::String(if !text.is_empty() {
            text
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        });
    }

    let mut output: Vec<Value> = Vec::new();
    if !text.is_empty() {
        output.push(json!({ "type": "input_text", "text": text }));
    }
    for image in images {
        if let InputContentBlock::Image { data, mime_type } = image {
            output.push(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{mime_type};base64,{data}"),
            }));
        }
    }
    Value::Array(output)
}

fn convert_responses_messages(
    model: &Model,
    context: &Context,
    flavor: ResponsesFlavor,
) -> Vec<Value> {
    let compat = resolve_compat(model);
    let allowed: std::collections::HashSet<&str> = flavor
        .allowed_tool_call_providers()
        .iter()
        .copied()
        .collect();

    let normalize_tool_call_id = |id: &str, source: &crate::types::AssistantMessage| -> String {
        if !allowed.contains(model.provider.as_str()) {
            return normalize_id_part(id);
        }
        // TS: const [callId, itemId] = id.split("|") — extra segments drop.
        let mut parts = id.split('|');
        let call_part = parts.next().unwrap_or("");
        let Some(item_part) = parts.next() else {
            return normalize_id_part(id);
        };
        let call_id = normalize_id_part(call_part);
        let is_foreign = source.provider != model.provider || source.api != model.api;
        let mut normalized_item = if is_foreign {
            format!("fc_{}", short_hash(item_part))
        } else {
            normalize_id_part(item_part)
        };
        if !normalized_item.starts_with("fc_") {
            normalized_item = normalize_id_part(&format!("fc_{normalized_item}"));
        }
        format!("{call_id}|{normalized_item}")
    };

    let transformed = transform_messages_with_source(
        context.messages.as_slice(),
        model,
        Some(&normalize_tool_call_id),
    );

    let mut messages: Vec<Value> = Vec::new();

    if let Some(system_prompt) = &context.system_prompt {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        messages.push(json!({ "role": role, "content": system_prompt }));
    }

    for (msg_index, msg) in transformed.iter().enumerate() {
        match msg {
            Message::User(u) => match &u.content {
                UserContent::Text(text) => {
                    messages.push(json!({
                        "role": "user",
                        "content": [{ "type": "input_text", "text": text }],
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|b| match b {
                            InputContentBlock::Text { text, .. } => {
                                json!({ "type": "input_text", "text": text })
                            }
                            InputContentBlock::Image { data, mime_type } => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{mime_type};base64,{data}"),
                            }),
                        })
                        .collect();
                    if content.is_empty() {
                        continue;
                    }
                    messages.push(json!({ "role": "user", "content": content }));
                }
            },
            Message::Assistant(a) => {
                let mut output: Vec<Value> = Vec::new();
                let is_same_provider_api = a.provider == model.provider && a.api == model.api;
                let is_same_model = is_same_provider_api && a.model == model.id;
                let is_different_model = is_same_provider_api && a.model != model.id;
                let mut text_block_index = 0usize;

                for block in &a.content {
                    match block {
                        ContentBlock::Thinking {
                            thinking_signature: Some(sig),
                            ..
                        } => {
                            // Replayed reasoning item (JSON snapshot).
                            if let Ok(item) = serde_json::from_str::<Value>(sig) {
                                output.push(item);
                            }
                        }
                        ContentBlock::Thinking { .. } => {}
                        ContentBlock::Text {
                            text,
                            text_signature,
                        } => {
                            let parsed = parse_text_signature(text_signature.as_deref());
                            let fallback = if text_block_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            let mut msg_id = parsed
                                .as_ref()
                                .map(|(id, _)| id.clone())
                                .unwrap_or(fallback);
                            if msg_id.len() > 64 {
                                msg_id = format!("msg_{}", short_hash(&msg_id));
                            }
                            let phase = parsed.and_then(|(_, p)| p);
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                                "status": "completed",
                                "id": msg_id,
                            });
                            if let Some(phase) = phase {
                                item["phase"] = json!(phase);
                            }
                            output.push(item);
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            namespace,
                            ..
                        } => {
                            // transform_messages already normalized the id
                            // (source-aware); split it back into call/item ids.
                            let mut id_parts = id.split('|');
                            let call_id = id_parts.next().unwrap_or("").to_string();
                            let mut item_id = id_parts.next().map(str::to_string);
                            // For different-model messages, drop the id to avoid
                            // pairing validation; function_call item ids must be
                            // fc_* (TS convertResponsesMessages).
                            if (is_different_model
                                && item_id.as_deref().is_some_and(|i| i.starts_with("fc_")))
                                || item_id.as_deref().is_none_or(|i| !i.starts_with("fc_"))
                            {
                                item_id = None;
                            }

                            let mut item = json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments.to_string(),
                            });
                            if let Some(item_id) = item_id {
                                item["id"] = json!(item_id);
                            }
                            if is_same_model && let Some(ns) = namespace {
                                item["namespace"] = json!(ns);
                            }
                            output.push(item);
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if output.is_empty() {
                    continue;
                }
                messages.extend(output);
            }
            Message::ToolResult(t) => {
                let call_id = t.tool_call_id.split('|').next().unwrap_or("").to_string();
                messages.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": convert_tool_result_output(model, &t.content),
                }));
            }
            // The caller collapses transcripts before building the Context;
            // skip system messages defensively.
            Message::System(_) => {}
        }
    }

    messages
}

fn convert_responses_tools(
    tools: &[ToolDefinition],
    supports_strict_mode: bool,
) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            // TS convertResponsesTools: strict = resolveJsonSchemaStrictSampling
            // (tool, supportsStrictMode) ?? false; parameters become the strict
            // schema only when the tool opts in and the schema converts.
            let resolved = crate::constrained_sampling::resolve_json_schema_strict(
                tool,
                supports_strict_mode,
            )?;
            let strict = resolved == Some(true);
            let parameters = if strict {
                crate::constrained_sampling::make_strict_json_schema(&tool.parameters)
                    .map_err(|e| format!("Tool \"{}\": {e}", tool.name))?
            } else {
                tool.parameters.clone()
            };
            let mut function = json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": parameters,
            });
            if supports_strict_mode {
                function["strict"] = json!(strict);
            }
            Ok(function)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    if let Some(r) = options.cache_retention {
        return r;
    }
    if std::env::var("TACK_CACHE_RETENTION").is_ok_and(|v| v == "long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

fn normalize_azure_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/').to_string();
    let is_azure_host = trimmed.contains(".openai.azure.com")
        || trimmed.contains(".cognitiveservices.azure.com")
        || trimmed.contains(".ai.azure.com");
    if !is_azure_host {
        return trimmed;
    }
    // Azure hosts need /openai/v1 as the base path so /responses and
    // ?api-version=v1 resolve correctly.
    let path = trimmed
        .find("://")
        .and_then(|i| {
            trimmed[i + 3..]
                .find('/')
                .map(|p| trimmed[i + 3 + p..].to_string())
        })
        .unwrap_or_default();
    if path.is_empty() || path == "/openai" || path == "/openai/v1/responses" {
        let host_end = trimmed
            .find("://")
            .map(|i| i + 3 + trimmed[i + 3..].find('/').unwrap_or(trimmed.len() - i - 3))
            .unwrap_or(trimmed.len());
        return format!("{}/openai/v1", &trimmed[..host_end]);
    }
    trimmed
}

fn resolve_azure_base_url(model: &Model) -> Result<String, String> {
    let base = std::env::var("AZURE_OPENAI_BASE_URL")
        .ok()
        .map(|v| v.trim().to_string());
    let resource = std::env::var("AZURE_OPENAI_RESOURCE_NAME").ok();
    let resolved = base
        .or_else(|| resource.map(|r| format!("https://{r}.openai.azure.com/openai/v1")))
        .or_else(|| (!model.base_url.is_empty()).then(|| model.base_url.clone()));
    match resolved {
        Some(url) => Ok(normalize_azure_base_url(&url)),
        None => Err(
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or model.baseUrl."
                .to_string(),
        ),
    }
}

fn resolve_azure_deployment_name(model: &Model) -> String {
    if let Ok(map) = std::env::var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP") {
        for entry in map.split(',') {
            if let Some((id, deployment)) = entry.trim().split_once('=')
                && id.trim() == model.id
            {
                return deployment.trim().to_string();
            }
        }
    }
    model.id.clone()
}

fn build_params(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    flavor: ResponsesFlavor,
) -> Result<Value, String> {
    let compat = resolve_compat(model);
    let messages = convert_responses_messages(model, context, flavor);
    let retention = resolve_cache_retention(options);

    let model_field = if flavor == ResponsesFlavor::Azure {
        resolve_azure_deployment_name(model)
    } else {
        model.id.clone()
    };

    let mut params = json!({
        "model": model_field,
        "input": messages,
        "stream": true,
        "store": false,
    });

    if flavor != ResponsesFlavor::Azure {
        if retention != CacheRetention::None {
            if let Some(session_id) = &options.session_id {
                params["prompt_cache_key"] = json!(session_id.chars().take(64).collect::<String>());
            }
        } else if compat.supports_explicit_prompt_cache_mode {
            params["prompt_cache_options"] = json!({ "mode": "explicit" });
        }
        if retention == CacheRetention::Long && compat.supports_long_cache_retention {
            params["prompt_cache_retention"] = json!("24h");
        }
    } else if let Some(session_id) = &options.session_id {
        params["prompt_cache_key"] = json!(session_id.chars().take(64).collect::<String>());
    }

    if let Some(max_tokens) = options.max_tokens {
        params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.temperature {
        params["temperature"] = json!(temperature);
    }

    if !context.tools.is_empty() {
        params["tools"] = Value::Array(convert_responses_tools(
            &context.tools,
            compat.supports_strict_mode,
        )?);
    }
    if let Some(choice) = options.tool_choice {
        params["tool_choice"] = json!(match choice {
            crate::provider::ToolChoice::Auto => "auto",
            crate::provider::ToolChoice::None => "none",
        });
    }

    if model.reasoning {
        match options.reasoning {
            Some(level) => {
                let effort = model
                    .thinking_level_value(level)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| level.as_str().to_string());
                params["reasoning"] = json!({ "effort": effort, "summary": "auto" });
                params["include"] = json!(["reasoning.encrypted_content"]);
            }
            None => {
                let off_is_null = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get("off"))
                    .is_some_and(|v| v.is_none());
                if model.provider != "github-copilot" && !off_is_null {
                    let off = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get("off"))
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| "none".to_string());
                    params["reasoning"] = json!({ "effort": off });
                }
            }
        }
        if model.provider == "xai" {
            params["include"] = json!(["reasoning.encrypted_content"]);
        }
    }

    if let Some(sampling) = &model.sampling_params {
        for (k, v) in sampling {
            params[k.as_str()] = v.clone();
        }
    }
    for (k, v) in &options.sampling_params {
        params[k.as_str()] = v.clone();
    }

    Ok(params)
}

// ---------------------------------------------------------------------------
// Stream processing (processResponsesStream)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum SlotKind {
    Thinking,
    Text,
    ToolCall,
}

#[derive(Debug)]
struct Slot {
    kind: SlotKind,
    content_index: usize,
    /// Scratch partial JSON for tool calls.
    partial_json: String,
}

/// Create a streaming slot for an output item (TS createSlot). Also used to
/// backfill slots when `response.output_item.done` arrives without a prior
/// `response.output_item.added` (TS getOrCreateSlot).
fn create_slot(
    output: &mut AssistantMessage,
    slots: &mut std::collections::HashMap<u64, Slot>,
    sender: &AssistantMessageEventSender,
    coalescer: &mut crate::api::DeltaCoalescer,
    index: u64,
    item: &Value,
) {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "reasoning" => {
            output.content.push(ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: None,
                redacted: None,
            });
            let idx = output.content.len() - 1;
            slots.insert(
                index,
                Slot {
                    kind: SlotKind::Thinking,
                    content_index: idx,
                    partial_json: String::new(),
                },
            );
            coalescer.push(
                sender,
                output,
                AssistantMessageEvent::ThinkingStart {
                    content_index: idx,
                    partial: output.clone(),
                },
            );
        }
        "message" => {
            if item.get("phase").and_then(Value::as_str) == Some("final_answer") {
                output.stop_reason = StopReason::Stop;
            }
            output.content.push(ContentBlock::Text {
                text: String::new(),
                text_signature: None,
            });
            let idx = output.content.len() - 1;
            slots.insert(
                index,
                Slot {
                    kind: SlotKind::Text,
                    content_index: idx,
                    partial_json: String::new(),
                },
            );
            coalescer.push(
                sender,
                output,
                AssistantMessageEvent::TextStart {
                    content_index: idx,
                    partial: output.clone(),
                },
            );
        }
        "function_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            output.content.push(ContentBlock::ToolCall {
                id: format!("{call_id}|{item_id}"),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                arguments: json!({}),
                thought_signature: None,
                namespace: item
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
            let idx = output.content.len() - 1;
            let partial = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            slots.insert(
                index,
                Slot {
                    kind: SlotKind::ToolCall,
                    content_index: idx,
                    partial_json: partial,
                },
            );
            coalescer.push(
                sender,
                output,
                AssistantMessageEvent::ToolCallStart {
                    content_index: idx,
                    partial: output.clone(),
                },
            );
        }
        _ => {}
    }
}

fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> (StopReason, Option<String>) {
    match status {
        None | Some("completed") | Some("in_progress") | Some("queued") => (StopReason::Stop, None),
        Some("incomplete") => {
            if incomplete_reason == Some("max_output_tokens") {
                (StopReason::Length, None)
            } else {
                (
                    StopReason::Error,
                    Some(
                        incomplete_reason
                            .map(|r| format!("Response incomplete: {r}"))
                            .unwrap_or_else(|| {
                                "Response incomplete without a provider reason".into()
                            }),
                    ),
                )
            }
        }
        Some("failed") | Some("cancelled") => (StopReason::Error, None),
        Some(other) => (
            StopReason::Error,
            Some(format!("Unhandled response status: {other}")),
        ),
    }
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
    flavor: ResponsesFlavor,
) {
    let cancel: CancellationToken = options.cancel.clone();
    let mut output = AssistantMessage::pending(&model);
    let mut coalescer = crate::api::DeltaCoalescer::new();

    // --- auth + URL ---
    let api_key = options.api_key.clone();
    let has_auth_header = options.headers.keys().any(|k| {
        let k = k.to_ascii_lowercase();
        k == "authorization" || k == "cf-aig-authorization"
    });
    if api_key.is_none() && !has_auth_header {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            format!("No API key for provider: {}", model.provider),
            false
        );
    }

    let base_url = match flavor {
        ResponsesFlavor::Azure => match resolve_azure_base_url(&model) {
            Ok(u) => u,
            Err(e) => fail!(output, sender, e, false),
        },
        _ => model.base_url.trim_end_matches('/').to_string(),
    };
    let mut url = format!("{base_url}/responses");
    if flavor == ResponsesFlavor::Azure {
        let api_version =
            std::env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "v1".to_string());
        url = format!("{url}?api-version={api_version}");
    }

    let params = match build_params(&model, &context, &options, flavor) {
        Ok(p) => p,
        Err(e) => fail!(output, sender, e, false),
    };

    // --- transport: Codex WebSocket (with SSE fallback) or HTTP SSE ---
    use tokio::sync::mpsc;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Result<String, String>>();
    let mut ws_active = false;
    if crate::api::codex_ws::wants_websocket(flavor, options.session_id.as_deref()) {
        match crate::api::codex_ws::spawn_producer(
            &base_url,
            api_key.as_deref(),
            model.headers.as_ref(),
            options.session_id.as_deref(),
            params.clone(),
            cancel.clone(),
        )
        .await
        {
            Ok(rx) => {
                event_rx = rx;
                ws_active = true;
            }
            Err(e) => {
                crate::api::codex_ws::record_ws_fallback(options.session_id.as_deref());
                tracing::info!("codex websocket unavailable, falling back to SSE: {e}");
            }
        }
    }
    if !ws_active {
        let client = crate::api::http_client();
        let build_request = || {
            let mut request = client
                .post(&url)
                .header("content-type", "application/json")
                .header("user-agent", TACK_USER_AGENT);
            if let Some(key) = &api_key {
                // Azure accepts api-key; Bearer also works with Entra tokens.
                if flavor == ResponsesFlavor::Azure {
                    request = request.header("api-key", key);
                } else {
                    request = request.header("authorization", format!("Bearer {key}"));
                }
            }
            if flavor == ResponsesFlavor::Codex {
                // ChatGPT Codex backend requirements.
                request = request.header("OpenAI-Beta", "responses=experimental");
                if let Some(session_id) = &options.session_id {
                    request = request.header("session_id", session_id);
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
                // Identify the actual provider in the error (TS #9298) —
                // azure-openai-responses/openai-codex/... are not "OpenAI".
                let provider = if model.provider == "openai" {
                    "OpenAI"
                } else {
                    model.provider.as_str()
                };
                fail!(
                    output,
                    sender,
                    format!("{provider} API error: {e}"),
                    aborted
                );
            }
        };

        // SSE feeds the same unified channel as the WS producer, so the
        // transport runs in a producer task and JSON parsing stays in the
        // consumer loop below (WS frames also arrive as raw JSON strings).
        // SseStream supplies the shared skeleton: [DONE] handling,
        // cancellation, and the "SSE stream error: ..." format the other
        // adapters use.
        let mut sse = crate::api::SseStream::new(response.bytes_stream(), cancel.clone());
        let tx = event_tx;
        tokio::spawn(async move {
            loop {
                match sse.next_event().await {
                    Ok(Some(event)) => {
                        if tx.send(Ok(event.data)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // Aborted is handled by the consumer loop's own
                        // cancel select — don't inject a spurious error.
                        if !e.is_aborted() {
                            let _ = tx.send(Err(e.to_string()));
                        }
                        break;
                    }
                }
            }
        });
    }

    coalescer.push(
        &sender,
        &output,
        AssistantMessageEvent::Start {
            partial: output.clone(),
        },
    );

    // --- event consumption (WS or SSE, unified channel) ---
    let mut slots: std::collections::HashMap<u64, Slot> = std::collections::HashMap::new();
    let mut saw_terminal_event = false;
    let mut stream_error: Option<crate::api::ApiError> = None;

    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                stream_error = Some(crate::api::ApiError::Aborted);
                None
            }
            item = event_rx.recv() => item,
        };
        let Some(item) = next else { break };
        let data_str = match item {
            Ok(d) => d,
            Err(e) => {
                stream_error = Some(crate::api::ApiError::Failed(e));
                break;
            }
        };
        let data: Value = match crate::api::parse_sse_json("Responses event", &data_str) {
            Ok(v) => v,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };
        let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "response.created" => {
                if let Some(id) = data.pointer("/response/id").and_then(Value::as_str) {
                    output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let index = data
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if let Some(item) = data.get("item") {
                    create_slot(
                        &mut output,
                        &mut slots,
                        &sender,
                        &mut coalescer,
                        index,
                        item,
                    );
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_part.done" => {
                let index = data
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let delta = if event_type == "response.reasoning_summary_part.done" {
                    "\n\n"
                } else {
                    data.get("delta").and_then(Value::as_str).unwrap_or("")
                };
                if let Some(slot) = slots.get(&index)
                    && matches!(slot.kind, SlotKind::Thinking)
                {
                    if let Some(ContentBlock::Thinking { thinking, .. }) =
                        output.content.get_mut(slot.content_index)
                    {
                        thinking.push_str(delta);
                    }
                    if let Some(ev) = coalescer.offer(
                        crate::api::DeltaKind::Thinking,
                        slot.content_index,
                        delta.to_string(),
                        &output,
                    ) {
                        let _ = sender.push(ev);
                    }
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let index = data
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
                if let Some(slot) = slots.get(&index)
                    && matches!(slot.kind, SlotKind::Text)
                {
                    if let Some(ContentBlock::Text { text, .. }) =
                        output.content.get_mut(slot.content_index)
                    {
                        text.push_str(delta);
                    }
                    if let Some(ev) = coalescer.offer(
                        crate::api::DeltaKind::Text,
                        slot.content_index,
                        delta.to_string(),
                        &output,
                    ) {
                        let _ = sender.push(ev);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let index = data
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
                if let Some(slot) = slots.get_mut(&index)
                    && matches!(slot.kind, SlotKind::ToolCall)
                {
                    slot.partial_json.push_str(delta);
                    // Defer the O(accumulated) streaming re-parse to the
                    // coalescer window: parse only when a merged delta event
                    // is about to go out (the ".done" event below re-parses
                    // the final arguments), not per tiny delta.
                    if coalescer.would_flush(
                        crate::api::DeltaKind::ToolCall,
                        slot.content_index,
                        delta.len(),
                    ) {
                        let parsed = parse_streaming_json(&slot.partial_json);
                        if let Some(ContentBlock::ToolCall { arguments, .. }) =
                            output.content.get_mut(slot.content_index)
                        {
                            *arguments = parsed;
                        }
                    }
                    if let Some(ev) = coalescer.offer(
                        crate::api::DeltaKind::ToolCall,
                        slot.content_index,
                        delta.to_string(),
                        &output,
                    ) {
                        let _ = sender.push(ev);
                    }
                }
            }
            "response.function_call_arguments.done" => {
                let index = data
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let arguments = data.get("arguments").and_then(Value::as_str).unwrap_or("");
                if let Some(slot) = slots.get_mut(&index) {
                    let previous = slot.partial_json.clone();
                    slot.partial_json = arguments.to_string();
                    let parsed = parse_streaming_json(&slot.partial_json);
                    if let Some(ContentBlock::ToolCall {
                        arguments: args, ..
                    }) = output.content.get_mut(slot.content_index)
                    {
                        *args = parsed;
                    }
                    if arguments.starts_with(&previous) && arguments.len() > previous.len() {
                        let delta = &arguments[previous.len()..];
                        if let Some(ev) = coalescer.offer(
                            crate::api::DeltaKind::ToolCall,
                            slot.content_index,
                            delta.to_string(),
                            &output,
                        ) {
                            let _ = sender.push(ev);
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let index = data
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some(item) = data.get("item") else {
                    continue;
                };
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                if item.get("phase").and_then(Value::as_str) == Some("final_answer") {
                    output.stop_reason = StopReason::Stop;
                }
                // getOrCreateSlot: a done event can arrive without a matching
                // added event (reconnects, provider quirks) — materialize the
                // slot instead of dropping the block.
                if !slots.contains_key(&index) {
                    create_slot(
                        &mut output,
                        &mut slots,
                        &sender,
                        &mut coalescer,
                        index,
                        item,
                    );
                }
                let Some(slot) = slots.get(&index) else {
                    continue;
                };
                let content_index = slot.content_index;
                match item_type {
                    "reasoning" if matches!(slot.kind, SlotKind::Thinking) => {
                        let summary_text = item
                            .get("summary")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        let content_text = item
                            .get("content")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        let thinking = if !summary_text.is_empty() {
                            summary_text
                        } else if !content_text.is_empty() {
                            content_text
                        } else if let Some(ContentBlock::Thinking { thinking, .. }) =
                            output.content.get(content_index)
                        {
                            thinking.clone()
                        } else {
                            String::new()
                        };
                        if let Some(ContentBlock::Thinking {
                            thinking: t,
                            thinking_signature,
                            ..
                        }) = output.content.get_mut(content_index)
                        {
                            *t = thinking.clone();
                            *thinking_signature = Some(item.to_string());
                        }
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ThinkingEnd {
                                content_index,
                                content: thinking,
                                partial: output.clone(),
                            },
                        );
                        slots.remove(&index);
                    }
                    "message" if matches!(slot.kind, SlotKind::Text) => {
                        let text = item
                            .get("content")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .map(|p| {
                                        p.get("text")
                                            .or_else(|| p.get("refusal"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                    })
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        let signature = encode_text_signature_v1(
                            item.get("id").and_then(Value::as_str).unwrap_or(""),
                            item.get("phase").and_then(Value::as_str),
                        );
                        if let Some(ContentBlock::Text {
                            text: t,
                            text_signature,
                        }) = output.content.get_mut(content_index)
                        {
                            *t = text.clone();
                            *text_signature = Some(signature);
                        }
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::TextEnd {
                                content_index,
                                content: text,
                                partial: output.clone(),
                            },
                        );
                        slots.remove(&index);
                    }
                    "function_call" if matches!(slot.kind, SlotKind::ToolCall) => {
                        let args_str = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| slot.partial_json.clone());
                        let parsed = parse_streaming_json(if args_str.is_empty() {
                            "{}"
                        } else {
                            &args_str
                        });
                        if let Some(ContentBlock::ToolCall {
                            arguments,
                            namespace,
                            ..
                        }) = output.content.get_mut(content_index)
                        {
                            *arguments = parsed;
                            if let Some(ns) = item.get("namespace").and_then(Value::as_str) {
                                *namespace = Some(ns.to_string());
                            }
                        }
                        let tool_call = output.content[content_index].clone();
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call,
                                partial: output.clone(),
                            },
                        );
                        slots.remove(&index);
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                saw_terminal_event = true;
                if let Some(resp) = data.get("response") {
                    if let Some(id) = resp.get("id").and_then(Value::as_str) {
                        output.response_id = Some(id.to_string());
                    }
                    if let Some(usage) = resp.get("usage") {
                        let input_tokens = usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cached = usage
                            .pointer("/input_tokens_details/cached_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cache_write = usage
                            .pointer("/input_tokens_details/cache_write_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        output.usage.input = input_tokens.saturating_sub(cached + cache_write);
                        output.usage.output = usage
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        output.usage.cache_read = cached;
                        output.usage.cache_write = cache_write;
                        output.usage.reasoning = usage
                            .pointer("/output_tokens_details/reasoning_tokens")
                            .and_then(Value::as_u64);
                        output.usage.total_tokens =
                            usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(
                                output.usage.input
                                    + output.usage.output
                                    + output.usage.cache_read
                                    + output.usage.cache_write,
                            );
                        calculate_cost(&model, &mut output.usage);
                    }
                    let status = resp.get("status").and_then(Value::as_str);
                    let incomplete_reason = resp
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str);
                    output.raw_stop_reason = Some(match incomplete_reason {
                        Some(r) => format!("{}.{r}", status.unwrap_or("")),
                        None => status.unwrap_or("").to_string(),
                    });
                    let (stop_reason, error_message) = map_stop_reason(status, incomplete_reason);
                    output.stop_reason = stop_reason;
                    output.error_message = error_message;
                    if output.has_tool_calls() && output.stop_reason == StopReason::Stop {
                        output.stop_reason = StopReason::ToolUse;
                    }
                }
            }
            "response.failed" => {
                saw_terminal_event = true;
                let msg = data
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| "Unknown error (response.failed)".to_string());
                stream_error = Some(crate::api::ApiError::Failed(msg));
                break;
            }
            "error" => {
                let code = data
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = data.get("message").and_then(Value::as_str).unwrap_or("");
                stream_error = Some(crate::api::ApiError::Failed(format!(
                    "Error Code {code}: {message}"
                )));
                break;
            }
            _ => {}
        }
    }

    // --- termination ---
    // Final parse fallback (F13): the per-delta re-parse was throttled to
    // coalescer flushes and ".done" events, so a proxy that omits
    // function_call_arguments.done would leave the terminal message's
    // tool arguments short of the trailing deltas. Re-parse every
    // accumulated partial_json once here — covers clean, abort and error
    // exits alike.
    for slot in slots.values() {
        if matches!(slot.kind, SlotKind::ToolCall)
            && !slot.partial_json.is_empty()
            && let Some(ContentBlock::ToolCall { arguments, .. }) =
                output.content.get_mut(slot.content_index)
        {
            *arguments = parse_streaming_json(&slot.partial_json);
        }
    }
    if let Some(error) = stream_error {
        let aborted = cancel.is_cancelled() || error.is_aborted();
        coalescer.flush_into(&sender, &output);
        fail!(output, sender, error.to_string(), aborted);
    }
    if !saw_terminal_event {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            "OpenAI Responses stream ended before a terminal response event".to_string(),
            cancel.is_cancelled()
        );
    }
    if output.stop_reason == StopReason::Pending {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            "OpenAI Responses stream ended without a stop reason".to_string(),
            false
        );
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        let message = output
            .error_message
            .clone()
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn model(id: &str, provider: &str, api: &str) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: Default::default(),
            context_window: 400_000,
            max_tokens: 128_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn assistant_from(model: &Model, content: Vec<ContentBlock>) -> Message {
        let mut a = AssistantMessage::pending(model);
        a.stop_reason = StopReason::Stop;
        a.content = content;
        Message::Assistant(a)
    }

    fn tool_call(id: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: json!({ "path": "a.txt" }),
            thought_signature: None,
            namespace: None,
        }
    }

    /// Same-provider replay: the original fc_* item id is preserved verbatim
    /// (previously the source-unaware transform normalizer re-hashed it).
    #[test]
    fn same_provider_tool_call_keeps_item_id() {
        let m = model("gpt-5", "openai", "openai-responses");
        let context = Context {
            system_prompt: None,
            messages: vec![assistant_from(&m, vec![tool_call("call_abc|fc_xyz123")])],
            tools: vec![],
        };
        let out = convert_responses_messages(&m, &context, ResponsesFlavor::OpenAi);
        let fc = out.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["call_id"], "call_abc");
        assert_eq!(fc["id"], "fc_xyz123");
        assert_eq!(
            fc["arguments"],
            json!(json!({ "path": "a.txt" }).to_string())
        );
    }

    /// Foreign (cross-provider) tool calls get a normalized fc_ item id, and
    /// the tool result keeps the full normalized call_id mapping.
    #[test]
    fn foreign_tool_call_item_id_is_normalized() {
        let m = model("gpt-5", "openai", "openai-responses");
        let foreign = model("claude-sonnet-4-5", "anthropic", "anthropic-messages");
        let context = Context {
            system_prompt: None,
            messages: vec![
                assistant_from(&foreign, vec![tool_call("toolu_123|rs_longitem")]),
                Message::ToolResult(crate::types::ToolResultMessage {
                    tool_call_id: "toolu_123|rs_longitem".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![InputContentBlock::text("ok")],
                    details: None,
                    usage: None,
                    is_error: false,
                    timestamp: 1,
                }),
            ],
            tools: vec![],
        };
        let out = convert_responses_messages(&m, &context, ResponsesFlavor::OpenAi);
        let fc = out.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["call_id"], "toolu_123");
        assert!(fc["id"].as_str().unwrap().starts_with("fc_"), "{fc}");
        let fco = out
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(fco["call_id"], "toolu_123");
        assert_eq!(fco["output"], "ok");
    }

    /// A different model from the same provider/api: fc_ item ids are dropped
    /// to avoid pairing validation (TS convertResponsesMessages).
    #[test]
    fn different_model_drops_item_id() {
        let m = model("gpt-5.1", "openai", "openai-responses");
        let older = model("gpt-5", "openai", "openai-responses");
        let context = Context {
            system_prompt: None,
            messages: vec![assistant_from(
                &older,
                vec![tool_call("call_abc|fc_xyz123")],
            )],
            tools: vec![],
        };
        let out = convert_responses_messages(&m, &context, ResponsesFlavor::OpenAi);
        let fc = out.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["call_id"], "call_abc");
        assert!(fc.get("id").is_none(), "{fc}");
    }

    /// Text signatures round-trip into message ids (and survive transform
    /// for same-model replay).
    #[test]
    fn text_signature_becomes_message_id() {
        let m = model("gpt-5", "openai", "openai-responses");
        let sig = json!({ "v": 1, "id": "msg_abc", "phase": "final_answer" }).to_string();
        let context = Context {
            system_prompt: None,
            messages: vec![assistant_from(
                &m,
                vec![ContentBlock::Text {
                    text: "hi".to_string(),
                    text_signature: Some(sig),
                }],
            )],
            tools: vec![],
        };
        let out = convert_responses_messages(&m, &context, ResponsesFlavor::OpenAi);
        let msg = out.iter().find(|i| i["type"] == "message").unwrap();
        assert_eq!(msg["id"], "msg_abc");
        assert_eq!(msg["phase"], "final_answer");
    }

    #[test]
    fn parse_text_signature_handles_v1_and_legacy() {
        let (id, phase) =
            parse_text_signature(Some(r#"{"v":1,"id":"msg_1","phase":"commentary"}"#)).unwrap();
        assert_eq!(
            (id.as_str(), phase.as_deref()),
            ("msg_1", Some("commentary"))
        );
        let (id, phase) = parse_text_signature(Some("msg_legacy")).unwrap();
        assert_eq!((id.as_str(), phase), ("msg_legacy", None));
        assert!(parse_text_signature(None).is_none());
    }

    /// Azure Responses must forward provider-specific tool choice while
    /// preserving tool definitions (TS #8614-adjacent, 0.84.3 fixed list).
    #[test]
    fn azure_forwards_tool_choice() {
        let m = model(
            "test-deployment",
            "azure-openai-responses",
            "azure-openai-responses",
        );
        let context = Context {
            system_prompt: None,
            messages: vec![Message::user("Summarize this")],
            tools: vec![crate::types::ToolDefinition {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
                defer_loading: false,
                constrained_sampling: None,
            }],
        };
        let options = crate::provider::StreamOptions {
            tool_choice: Some(crate::provider::ToolChoice::Auto),
            ..Default::default()
        };
        let params = build_params(&m, &context, &options, ResponsesFlavor::Azure).unwrap();
        assert_eq!(params["tool_choice"], json!("auto"));
        assert_eq!(params["tools"].as_array().unwrap().len(), 1);
    }
}
