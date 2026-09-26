//! Anthropic Messages API adapter. Lean port of
//! `packages/ai/src/api/anthropic-messages.ts` — request bodies are built as
//! raw `serde_json::Value` (mirroring the TS dynamic-object style) and SSE is
//! parsed via `eventsource-stream`.
//!
//! MVP scope vs the TS original: no deferred tools / tool_reference, no
//! server-side fallbacks, no Copilot headers, no onPayload/onResponse hooks.
//! OAuth Bearer tokens (sk-ant-oat*) are supported with Claude Code identity
//! headers.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::fail;
use crate::json_repair::parse_streaming_json;
use crate::provider::{CacheRetention, StreamOptions};
use crate::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use crate::transform::transform_messages;
use crate::types::{
    AssistantMessage, ContentBlock, Context, InputContentBlock, Message, Model, StopReason,
    ThinkingLevel, ToolDefinition, ToolResultMessage, UserContent, calculate_cost,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const CLAUDE_CODE_VERSION: &str = "2.1.75";
const TACK_USER_AGENT: &str = concat!("tack/", env!("CARGO_PKG_VERSION"));

/// Compatibility overrides for Anthropic-compatible APIs
/// (`AnthropicMessagesCompat` in TS). Unknown fields are ignored.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AnthropicCompat {
    pub supports_long_cache_retention: Option<bool>,
    pub supports_temperature: Option<bool>,
    pub allow_empty_signature: Option<bool>,
    pub force_adaptive_thinking: Option<bool>,
    pub supports_eager_tool_input_streaming: Option<bool>,
    pub supports_cache_control_on_tools: Option<bool>,
    pub supports_strict_tools: Option<bool>,
    /// Moonshot/Kimi-style caching: the server only honors a request-level
    /// `cache_control` field and ignores message-body markers. When set, the
    /// marker is emitted at the top level instead of on system/message/tool
    /// blocks (without it, prefixes are read-only — never written).
    pub top_level_cache_control: Option<bool>,
}

impl AnthropicCompat {
    fn for_model(model: &Model) -> Self {
        model
            .compat
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct AnthropicMessagesProvider;

impl crate::provider::Provider for AnthropicMessagesProvider {
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

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

fn has_auth_header(headers: &BTreeMap<String, String>) -> bool {
    headers.keys().any(|k| {
        let k = k.to_ascii_lowercase();
        k == "authorization" || k == "x-api-key" || k == "cf-aig-authorization"
    })
}

fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    if let Some(r) = options.cache_retention {
        return r;
    }
    if std::env::var("TACK_CACHE_RETENTION").is_ok_and(|v| v == "long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

fn cache_control(retention: CacheRetention, compat: &AnthropicCompat) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({ "type": "ephemeral" })),
        CacheRetention::Long => {
            if compat.supports_long_cache_retention.unwrap_or(true) {
                Some(json!({ "type": "ephemeral", "ttl": "1h" }))
            } else {
                Some(json!({ "type": "ephemeral" }))
            }
        }
    }
}

/// `mapThinkingLevelToEffort` from the TS adapter.
fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> &'static str {
    if let Some(Some(mapped)) = model.thinking_level_value(level) {
        // Caller-provided mapping; trust it matches an Anthropic effort value.
        return match mapped.as_str() {
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" => "xhigh",
            "max" => "max",
            _ => "high",
        };
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
}

/// Normalize tool call IDs to Anthropic's required pattern and length.
fn normalize_tool_call_id(id: &str) -> String {
    let normalized: String = id
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
    normalized
}

/// Convert text/image content blocks to an Anthropic content value: a plain
/// string when there are no images, otherwise a block array.
fn convert_content_blocks(content: &[InputContentBlock]) -> Value {
    let has_images = content
        .iter()
        .any(|c| matches!(c, InputContentBlock::Image { .. }));
    if !has_images {
        return Value::String(
            content
                .iter()
                .filter_map(|c| match c {
                    InputContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    let mut blocks: Vec<Value> = content
        .iter()
        .map(|block| match block {
            InputContentBlock::Text { text, .. } => json!({ "type": "text", "text": text }),
            InputContentBlock::Image { data, mime_type } => json!({
                "type": "image",
                "source": { "type": "base64", "media_type": mime_type, "data": data },
            }),
        })
        .collect();

    let has_text = blocks.iter().any(|b| b["type"] == "text");
    if !has_text {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    Value::Array(blocks)
}

fn convert_tool_result(msg: &ToolResultMessage) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": msg.tool_call_id,
        "content": convert_content_blocks(&msg.content),
        "is_error": msg.is_error,
    })
}

/// TS defaultSupportsToolReferences: first-party Anthropic models except
/// Haiku and models predating tool search (Claude 3.x, Opus/Sonnet 4.0/4.1).
fn supports_tool_references(model: &Model) -> bool {
    if model.provider != "anthropic" || model.id.contains("haiku") {
        return false;
    }
    let Some(version) = model.id.strip_prefix("claude-").and_then(|rest| {
        let mut parts = rest.split('-');
        let family = parts.next()?;
        if !matches!(family, "opus" | "sonnet" | "fable") {
            return None;
        }
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some((major, minor))
    }) else {
        return false;
    };
    version.0 > 4 || (version.0 == 4 && version.1 >= 5)
}

/// Port of `convertMessages`: converts transformed pi messages into Anthropic
/// `messages` params, grouping consecutive tool results into one user message.
fn convert_messages(
    transformed: &[Message],
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(u) => match &u.content {
                UserContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({ "role": "user", "content": text }));
                    }
                }
                UserContent::Blocks(blocks) => {
                    let filtered: Vec<Value> = blocks
                        .iter()
                        .filter(|b| match b {
                            InputContentBlock::Text { text, .. } => !text.trim().is_empty(),
                            _ => true,
                        })
                        .map(|b| match b {
                            InputContentBlock::Text { text, .. } => {
                                json!({ "type": "text", "text": text })
                            }
                            InputContentBlock::Image { data, mime_type } => json!({
                                "type": "image",
                                "source": { "type": "base64", "media_type": mime_type, "data": data },
                            }),
                        })
                        .collect();
                    if filtered.is_empty() {
                        i += 1;
                        continue;
                    }
                    params.push(json!({ "role": "user", "content": filtered }));
                }
            },
            Message::Assistant(a) => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in &a.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if text.trim().is_empty() {
                                continue;
                            }
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if *redacted == Some(true) {
                                if let Some(sig) = thinking_signature {
                                    blocks
                                        .push(json!({ "type": "redacted_thinking", "data": sig }));
                                }
                                continue;
                            }
                            let has_signature = thinking_signature
                                .as_ref()
                                .is_some_and(|s| !s.trim().is_empty());
                            if thinking.trim().is_empty() && !has_signature {
                                continue;
                            }
                            if !has_signature {
                                // Missing signature (e.g. aborted stream): convert
                                // to plain text unless the model opts into empty
                                // signatures.
                                if allow_empty_signature {
                                    blocks.push(json!({
                                        "type": "thinking",
                                        "thinking": thinking,
                                        "signature": "",
                                    }));
                                } else {
                                    blocks.push(json!({ "type": "text", "text": thinking }));
                                }
                            } else {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking,
                                    "signature": thinking_signature,
                                }));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments,
                            }));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if blocks.is_empty() {
                    i += 1;
                    continue;
                }
                params.push(json!({ "role": "assistant", "content": blocks }));
            }
            Message::ToolResult(_) => {
                // Collect all consecutive toolResult messages into one user
                // message (required by Anthropic, and z.ai's endpoint).
                let mut tool_results: Vec<Value> = Vec::new();
                while i < transformed.len() {
                    if let Message::ToolResult(t) = &transformed[i] {
                        tool_results.push(convert_tool_result(t));
                        i += 1;
                    } else {
                        break;
                    }
                }
                i = i.saturating_sub(1); // outer loop increments
                params.push(json!({ "role": "user", "content": tool_results }));
            }
            // The caller collapses transcripts before building the Context, so
            // system messages should not arrive; skip defensively (Anthropic
            // has no in-place system role in this port).
            Message::System(_) => {}
        }
        i += 1;
    }

    // Add cache_control to the last user message to cache conversation history.
    if let Some(cc) = cache_control
        && let Some(last) = params.last_mut()
        && last["role"] == "user"
    {
        match &mut last["content"] {
            Value::Array(blocks) => {
                if let Some(last_block) = blocks.last_mut() {
                    last_block["cache_control"] = cc.clone();
                }
            }
            Value::String(text) => {
                let text = text.clone();
                last["content"] = json!([{ "type": "text", "text": text, "cache_control": cc }]);
            }
            _ => {}
        }
    }

    params
}

fn convert_tools(
    tools: &[ToolDefinition],
    cache_control: Option<&Value>,
    defer_capable: bool,
    compat: &AnthropicCompat,
) -> Result<Vec<Value>, String> {
    let eager = compat.supports_eager_tool_input_streaming.unwrap_or(true);
    let cache_on_tools = compat.supports_cache_control_on_tools.unwrap_or(true);
    let supports_strict = compat.supports_strict_tools.unwrap_or(false);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            // TS convertTools: strict resolution ("require" violations throw)
            // and the strict schema merged into input_schema.
            let strict =
                crate::constrained_sampling::resolve_json_schema_strict(tool, supports_strict)?
                    == Some(true);
            let schema = if strict {
                crate::constrained_sampling::make_strict_json_schema(&tool.parameters)
                    .map_err(|e| format!("Tool \"{}\": {e}", tool.name))?
            } else {
                tool.parameters.clone()
            };
            let mut input_schema = json!({
                "type": "object",
                "properties": schema.get("properties").cloned().unwrap_or_else(|| json!({})),
                "required": schema.get("required").cloned().unwrap_or_else(|| json!([])),
            });
            if strict {
                // TS spreads the strict schema first, then the legacy fields.
                if let (Value::Object(strict_schema), Value::Object(out)) =
                    (&schema, &mut input_schema)
                {
                    for (k, v) in strict_schema {
                        if k != "properties" && k != "required" {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            let mut out = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": input_schema,
            });
            if eager {
                out["eager_input_streaming"] = json!(true);
            }
            if strict {
                out["strict"] = json!(true);
            }
            if tool.defer_loading && defer_capable {
                out["defer_loading"] = json!(true);
            }
            if cache_on_tools
                && let Some(cc) = cache_control
                && index == tools.len() - 1
            {
                out["cache_control"] = cc.clone();
            }
            Ok(out)
        })
        .collect()
}

fn build_params(
    model: &Model,
    context: &Context,
    is_oauth: bool,
    options: &StreamOptions,
    compat: &AnthropicCompat,
    retention: CacheRetention,
) -> Result<Value, String> {
    let cc = cache_control(retention, compat);
    // Top-level mode (Moonshot/Kimi): body markers are server-ignored, so
    // skip them and emit the request-level field at the end instead.
    let top_level_cc = compat.top_level_cache_control.unwrap_or(false);
    let block_cc = if top_level_cc { None } else { cc.as_ref() };
    let transformed = transform_messages(
        context.messages.as_slice(),
        model,
        Some(&normalize_tool_call_id),
    );

    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);

    // Deferred tools: only engaged when some tool opts in AND the model
    // supports tool references (defer_loading flag on declarations).
    let defer_capable = supports_tool_references(model);

    let mut params = json!({
        "model": model.id,
        "messages": convert_messages(&transformed, block_cc, compat.allow_empty_signature.unwrap_or(false)),
        "max_tokens": max_tokens,
        "stream": true,
    });

    // System prompt. OAuth tokens must present the Claude Code identity first.
    let mut system_blocks: Vec<Value> = Vec::new();
    if is_oauth {
        let mut block = json!({ "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." });
        if let Some(cc) = block_cc {
            block["cache_control"] = cc.clone();
        }
        system_blocks.push(block);
    }
    if let Some(system_prompt) = &context.system_prompt {
        let mut block = json!({ "type": "text", "text": system_prompt });
        if let Some(cc) = block_cc {
            block["cache_control"] = cc.clone();
        }
        system_blocks.push(block);
    }
    if !system_blocks.is_empty() {
        params["system"] = Value::Array(system_blocks);
    }

    // Temperature is incompatible with extended thinking and unsupported on
    // some newer models.
    let thinking_enabled = model.reasoning && options.reasoning.is_some();
    if let Some(temperature) = options.temperature
        && !thinking_enabled
        && compat.supports_temperature.unwrap_or(true)
    {
        params["temperature"] = json!(temperature);
    }

    if !context.tools.is_empty() {
        params["tools"] = Value::Array(convert_tools(
            &context.tools,
            block_cc,
            defer_capable,
            compat,
        )?);
    }

    if model.reasoning {
        match options.reasoning {
            Some(level) => {
                if compat.force_adaptive_thinking == Some(true) {
                    params["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
                    let effort = map_thinking_level_to_effort(model, level);
                    params["output_config"] = json!({ "effort": effort });
                } else {
                    let budgets = options.thinking_budgets.unwrap_or_default();
                    let mut budget = budgets.for_level(level) as u64;
                    let max_tokens = max_tokens as u64;
                    if max_tokens <= budget {
                        budget = max_tokens.saturating_sub(1024);
                    }
                    params["thinking"] = json!({
                        "type": "enabled",
                        "budget_tokens": budget,
                        "display": "summarized",
                    });
                }
            }
            None => {
                // thinking disabled: omit entirely only when the model marks
                // "off" as unsupported (thinkingLevelMap.off === null).
                let off_is_null = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get("off"))
                    .is_some_and(|v| v.is_none());
                if !off_is_null {
                    params["thinking"] = json!({ "type": "disabled" });
                }
            }
        }
    }

    if let Some(choice) = options.tool_choice {
        params["tool_choice"] = json!({
            "type": match choice {
                crate::provider::ToolChoice::Auto => "auto",
                crate::provider::ToolChoice::None => "none",
            }
        });
    }

    // Moonshot/Kimi top-level cache_control (body markers were skipped).
    if top_level_cc && let Some(cc) = &cc {
        params["cache_control"] = cc.clone();
    }

    Ok(params)
}

fn map_stop_reason(
    reason: &str,
    stop_details: Option<&Value>,
) -> Result<(StopReason, Option<String>), String> {
    Ok(match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => {
            let explanation = stop_details
                .and_then(|d| d.get("explanation"))
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "The model refused to complete the request".to_string());
            (StopReason::Error, Some(explanation))
        }
        "sensitive" => (
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_string()),
        ),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

/// Streaming block bookkeeping: maps Anthropic content-block indices to
/// positions in `output.content`, plus the scratch partial-JSON buffer.
#[derive(Debug)]
struct BlockState {
    api_index: usize,
    partial_json: String,
}

fn update_usage_from_json(usage_json: &Value, output: &mut AssistantMessage, model: &Model) {
    let get_u64 = |key: &str| usage_json.get(key).and_then(Value::as_u64);
    if let Some(v) = get_u64("input_tokens") {
        output.usage.input = v;
    }
    if let Some(v) = get_u64("output_tokens") {
        output.usage.output = v;
    }
    if let Some(v) = get_u64("cache_read_input_tokens") {
        output.usage.cache_read = v;
    }
    if let Some(v) = get_u64("cache_creation_input_tokens") {
        output.usage.cache_write = v;
    }
    if let Some(v) = usage_json
        .get("cache_creation")
        .and_then(|c| c.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_u64)
    {
        output.usage.cache_write_1h = Some(v);
    }
    if let Some(v) = usage_json
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(Value::as_u64)
    {
        output.usage.reasoning = Some(v);
    }
    output.usage.total_tokens = output.usage.input
        + output.usage.output
        + output.usage.cache_read
        + output.usage.cache_write;
    calculate_cost(model, &mut output.usage);
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    let cancel: CancellationToken = options.cancel.clone();
    let mut output = AssistantMessage::pending(&model);
    let mut coalescer = crate::api::DeltaCoalescer::new();

    // --- auth ---
    let api_key = options.api_key.clone();
    if api_key.is_none() && !has_auth_header(&options.headers) {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            format!("No API key for provider: {}", model.provider),
            false
        );
    }
    let is_oauth = api_key.as_ref().is_some_and(|k| is_oauth_token(k));

    let compat = AnthropicCompat::for_model(&model);
    let retention = resolve_cache_retention(&options);
    let params = match build_params(&model, &context, is_oauth, &options, &compat, retention) {
        Ok(p) => p,
        Err(e) => {
            coalescer.flush_into(&sender, &output);
            fail!(output, sender, e, false);
        }
    };

    // --- request ---
    let client = crate::api::http_client();
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));

    // Beta headers.
    let mut betas: Vec<String> = Vec::new();
    if is_oauth {
        betas.push("claude-code-20250219".to_string());
        betas.push("oauth-2025-04-20".to_string());
    }
    let thinking_enabled = model.reasoning && options.reasoning.is_some();
    if thinking_enabled && compat.force_adaptive_thinking != Some(true) {
        betas.push(INTERLEAVED_THINKING_BETA.to_string());
    }

    let build_request = || {
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION);
        if !betas.is_empty() {
            request = request.header("anthropic-beta", betas.join(","));
        }
        // Auth headers.
        if let Some(key) = &api_key {
            if is_oauth {
                request = request
                    .header("authorization", format!("Bearer {key}"))
                    .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
                    .header("x-app", "cli");
            } else {
                request = request
                    .header("x-api-key", key)
                    .header("user-agent", TACK_USER_AGENT);
            }
        } else {
            request = request.header("user-agent", TACK_USER_AGENT);
        }
        // Model headers, then per-request headers override.
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
            fail!(output, sender, format!("Anthropic API error: {e}"), aborted);
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
    let mut blocks: Vec<BlockState> = Vec::new();
    let mut stream_error: Option<crate::api::ApiError> = None;

    loop {
        let event = match sse.next_event().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };

        if event.event == "error" {
            stream_error = Some(crate::api::ApiError::Failed(event.data));
            break;
        }

        // Only message events carry JSON we understand.
        let data: Value = match crate::api::parse_sse_json(
            &format!("Anthropic SSE event {}", event.event),
            &event.data,
        ) {
            Ok(v) => v,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };
        let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "message_start" => {
                if let Some(message) = data.get("message") {
                    if let Some(id) = message.get("id").and_then(Value::as_str) {
                        output.response_id = Some(id.to_string());
                    }
                    // Keep the requested model ID on `output.model` so
                    // thinking replay through provider-renamed models stays
                    // consistent; record the reported model separately
                    // (TS #9188, same shape as openai_completions/stream.rs).
                    if let Some(m) = message.get("model").and_then(Value::as_str)
                        && !m.is_empty()
                        && m != model.id
                        && output.response_model.is_none()
                    {
                        output.response_model = Some(m.to_string());
                    }
                    if let Some(usage) = message.get("usage") {
                        update_usage_from_json(usage, &mut output, &model);
                    }
                }
            }
            "content_block_start" => {
                let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let Some(block) = data.get("content_block") else {
                    continue;
                };
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                match block_type {
                    "text" => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        output.content.push(ContentBlock::Text {
                            text,
                            text_signature: None,
                        });
                        blocks.push(BlockState {
                            api_index: index,
                            partial_json: String::new(),
                        });
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::TextStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            },
                        );
                    }
                    "thinking" => {
                        let thinking = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let signature = block
                            .get("signature")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        output.content.push(ContentBlock::Thinking {
                            thinking,
                            thinking_signature: Some(signature),
                            redacted: None,
                        });
                        blocks.push(BlockState {
                            api_index: index,
                            partial_json: String::new(),
                        });
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ThinkingStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            },
                        );
                    }
                    "redacted_thinking" => {
                        let data_payload = block
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        output.content.push(ContentBlock::Thinking {
                            thinking: "[Reasoning redacted]".to_string(),
                            thinking_signature: Some(data_payload),
                            redacted: Some(true),
                        });
                        blocks.push(BlockState {
                            api_index: index,
                            partial_json: String::new(),
                        });
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ThinkingStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            },
                        );
                    }
                    "tool_use" => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        output.content.push(ContentBlock::ToolCall {
                            id,
                            name,
                            arguments: input,
                            thought_signature: None,
                            namespace: None,
                        });
                        blocks.push(BlockState {
                            api_index: index,
                            partial_json: String::new(),
                        });
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ToolCallStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let Some(delta) = data.get("delta") else {
                    continue;
                };
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
                let Some(pos) = blocks.iter().position(|b| b.api_index == index) else {
                    continue;
                };
                match delta_type {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if let Some(ContentBlock::Text { text: t, .. }) =
                            output.content.get_mut(pos)
                        {
                            t.push_str(text);
                        }
                        if let Some(ev) = coalescer.offer(
                            crate::api::DeltaKind::Text,
                            pos,
                            text.to_string(),
                            &output,
                        ) {
                            let _ = sender.push(ev);
                        }
                    }
                    "thinking_delta" => {
                        let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if let Some(ContentBlock::Thinking { thinking, .. }) =
                            output.content.get_mut(pos)
                        {
                            thinking.push_str(text);
                        }
                        if let Some(ev) = coalescer.offer(
                            crate::api::DeltaKind::Thinking,
                            pos,
                            text.to_string(),
                            &output,
                        ) {
                            let _ = sender.push(ev);
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        blocks[pos].partial_json.push_str(partial);
                        // Defer the O(accumulated) streaming re-parse to the
                        // coalescer window: parse only when a merged delta
                        // event is about to go out (and once more at block
                        // stop below), not per tiny delta.
                        if coalescer.would_flush(
                            crate::api::DeltaKind::ToolCall,
                            pos,
                            partial.len(),
                        ) {
                            let parsed = parse_streaming_json(&blocks[pos].partial_json);
                            if let Some(ContentBlock::ToolCall { arguments, .. }) =
                                output.content.get_mut(pos)
                            {
                                *arguments = parsed;
                            }
                        }
                        if let Some(ev) = coalescer.offer(
                            crate::api::DeltaKind::ToolCall,
                            pos,
                            partial.to_string(),
                            &output,
                        ) {
                            let _ = sender.push(ev);
                        }
                    }
                    "signature_delta" => {
                        let sig = delta.get("signature").and_then(Value::as_str).unwrap_or("");
                        if let Some(ContentBlock::Thinking {
                            thinking_signature, ..
                        }) = output.content.get_mut(pos)
                        {
                            thinking_signature
                                .get_or_insert_with(String::new)
                                .push_str(sig);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let Some(pos) = blocks.iter().position(|b| b.api_index == index) else {
                    continue;
                };
                match output.content.get_mut(pos) {
                    Some(ContentBlock::Text { text, .. }) => {
                        let content = text.clone();
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::TextEnd {
                                content_index: pos,
                                content,
                                partial: output.clone(),
                            },
                        );
                    }
                    Some(ContentBlock::Thinking { thinking, .. }) => {
                        let content = thinking.clone();
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ThinkingEnd {
                                content_index: pos,
                                content,
                                partial: output.clone(),
                            },
                        );
                    }
                    Some(ContentBlock::ToolCall { arguments, .. }) => {
                        *arguments = parse_streaming_json(&blocks[pos].partial_json);
                        let tool_call = output.content[pos].clone();
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ToolCallEnd {
                                content_index: pos,
                                tool_call,
                                partial: output.clone(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(delta) = data.get("delta")
                    && let Some(reason) = delta.get("stop_reason").and_then(Value::as_str)
                {
                    output.raw_stop_reason = Some(reason.to_string());
                    match map_stop_reason(reason, delta.get("stop_details")) {
                        Ok((stop_reason, error_message)) => {
                            output.stop_reason = stop_reason;
                            output.error_message = error_message;
                        }
                        Err(e) => {
                            stream_error = Some(crate::api::ApiError::Failed(e));
                            break;
                        }
                    }
                }
                if let Some(usage) = data.get("usage") {
                    update_usage_from_json(usage, &mut output, &model);
                }
            }
            "message_stop" => {
                // Terminal marker; the stop reason from message_delta is what
                // matters (TS likewise does not require message_stop — some
                // Anthropic-compatible proxies omit it).
            }
            _ => {} // ping and unknown events
        }
    }

    // --- termination ---
    // Final parse fallback (F13): the per-delta re-parse was throttled to
    // coalescer flushes and block stop, so a proxy that omits
    // content_block_stop would leave the terminal message's tool
    // arguments short of the trailing deltas. Re-parse every accumulated
    // partial_json once here — covers clean, abort and error exits alike.
    for (pos, block) in blocks.iter().enumerate() {
        if !block.partial_json.is_empty()
            && let Some(ContentBlock::ToolCall { arguments, .. }) = output.content.get_mut(pos)
        {
            *arguments = parse_streaming_json(&block.partial_json);
        }
    }
    let aborted =
        cancel.is_cancelled() || matches!(stream_error, Some(crate::api::ApiError::Aborted));
    if let Some(error) = stream_error {
        coalescer.flush_into(&sender, &output);
        fail!(output, sender, error.to_string(), aborted);
    }
    if output.stop_reason == StopReason::Pending {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            "Anthropic stream ended without a stop reason".to_string(),
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
    use crate::types::InputContentBlock as ICB;

    fn model(id: &str) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: Default::default(),
            context_window: 200_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn tool_result(id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: "read".to_string(),
            content: vec![ICB::text("ok")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 1,
        })
    }

    #[test]
    fn consecutive_tool_results_group_into_one_user_message() {
        let messages = vec![Message::user("hi"), tool_result("t1"), tool_result("t2")];
        let out = convert_messages(&messages, None, false);
        assert_eq!(out.len(), 2);
        let content = out[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "t1");
        assert_eq!(content[1]["tool_use_id"], "t2");
    }

    #[test]
    fn thinking_without_signature_becomes_text_unless_allowed() {
        let m = model("claude-sonnet-4-5");
        let mut a = AssistantMessage::pending(&m);
        a.stop_reason = StopReason::Stop;
        a.content = vec![ContentBlock::Thinking {
            thinking: "hmm".to_string(),
            thinking_signature: None,
            redacted: None,
        }];
        let messages = vec![Message::Assistant(a.clone())];
        let out = convert_messages(&messages, None, false);
        assert_eq!(
            out[0]["content"][0],
            json!({ "type": "text", "text": "hmm" })
        );
        let out = convert_messages(&messages, None, true);
        assert_eq!(
            out[0]["content"][0],
            json!({ "type": "thinking", "thinking": "hmm", "signature": "" })
        );

        // Signed thinking replays as a thinking block.
        a.content = vec![ContentBlock::Thinking {
            thinking: "hmm".to_string(),
            thinking_signature: Some("sig".to_string()),
            redacted: None,
        }];
        let out = convert_messages(&[Message::Assistant(a)], None, false);
        assert_eq!(
            out[0]["content"][0],
            json!({ "type": "thinking", "thinking": "hmm", "signature": "sig" })
        );
    }

    #[test]
    fn cache_control_lands_on_last_block_of_last_user_message() {
        let cc = json!({ "type": "ephemeral" });
        let messages = vec![Message::user("hi")];
        let out = convert_messages(&messages, Some(&cc), false);
        // String content is upgraded to a block array to carry cache_control.
        assert_eq!(
            out[0]["content"],
            json!([{ "type": "text", "text": "hi", "cache_control": cc }])
        );

        // ...but never on an assistant message.
        let m = model("claude-sonnet-4-5");
        let mut a = AssistantMessage::pending(&m);
        a.stop_reason = StopReason::Stop;
        a.content = vec![ContentBlock::text("answer")];
        let out = convert_messages(&[Message::Assistant(a)], Some(&cc), false);
        assert_eq!(out[0]["role"], "assistant");
        assert!(out[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn convert_tools_honors_compat_gates() {
        let tools = vec![ToolDefinition {
            name: "read".to_string(),
            description: "d".to_string(),
            parameters: json!({ "type": "object", "properties": { "p": { "type": "string" } } }),
            defer_loading: false,
            constrained_sampling: None,
        }];
        // Defaults: eager on, cache_control on tools on.
        let cc = json!({ "type": "ephemeral" });
        let out = convert_tools(&tools, Some(&cc), false, &AnthropicCompat::default()).unwrap();
        assert_eq!(out[0]["eager_input_streaming"], json!(true));
        assert_eq!(out[0]["cache_control"], cc);

        // Compat disables both.
        let compat = AnthropicCompat {
            supports_eager_tool_input_streaming: Some(false),
            supports_cache_control_on_tools: Some(false),
            ..Default::default()
        };
        let out = convert_tools(&tools, Some(&cc), false, &compat).unwrap();
        assert!(out[0].get("eager_input_streaming").is_none());
        assert!(out[0].get("cache_control").is_none());
    }

    #[test]
    fn top_level_cache_control_moves_markers_out_of_body() {
        let m = model("kimi-for-coding");
        let context = Context {
            system_prompt: Some("sys".to_string()),
            messages: vec![Message::user("hi")],
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "d".to_string(),
                parameters: json!({ "type": "object", "properties": {} }),
                defer_loading: false,
                constrained_sampling: None,
            }],
        };
        let compat = AnthropicCompat {
            top_level_cache_control: Some(true),
            ..Default::default()
        };
        let options = StreamOptions::default();

        // Short retention: top-level ephemeral marker, no body markers.
        let params = build_params(
            &m,
            &context,
            false,
            &options,
            &compat,
            CacheRetention::Short,
        )
        .unwrap();
        assert_eq!(params["cache_control"], json!({ "type": "ephemeral" }));
        assert!(params["system"][0].get("cache_control").is_none());
        if let Value::Array(blocks) = &params["messages"][0]["content"] {
            assert!(blocks.iter().all(|b| b.get("cache_control").is_none()));
        }
        assert!(params["tools"][0].get("cache_control").is_none());

        // Long retention: ttl 1h at the top level.
        let params =
            build_params(&m, &context, false, &options, &compat, CacheRetention::Long).unwrap();
        assert_eq!(
            params["cache_control"],
            json!({ "type": "ephemeral", "ttl": "1h" })
        );

        // Retention none: no marker anywhere.
        let params =
            build_params(&m, &context, false, &options, &compat, CacheRetention::None).unwrap();
        assert!(params.get("cache_control").is_none());

        // Default message-level mode unchanged: marker stays in the body and
        // never appears at the top level.
        let params = build_params(
            &m,
            &context,
            false,
            &options,
            &AnthropicCompat::default(),
            CacheRetention::Short,
        )
        .unwrap();
        assert!(params.get("cache_control").is_none());
        assert_eq!(
            params["messages"][0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn convert_tools_applies_strict_schema_when_declared() {
        let tools = vec![ToolDefinition {
            name: "read".to_string(),
            description: "d".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "p": { "type": "string" } },
            }),
            defer_loading: false,
            constrained_sampling: Some(
                crate::constrained_sampling::ConstrainedSampling::JsonSchema { strict: None },
            ),
        }];
        let compat = AnthropicCompat {
            supports_strict_tools: Some(true),
            ..Default::default()
        };
        let out = convert_tools(&tools, None, false, &compat).unwrap();
        assert_eq!(out[0]["strict"], json!(true));
        // Strict conversion made the property required + nullable.
        assert_eq!(out[0]["input_schema"]["required"], json!(["p"]));
        assert_eq!(out[0]["input_schema"]["additionalProperties"], json!(false));

        // Without provider support, strict is not emitted.
        let out = convert_tools(&tools, None, false, &AnthropicCompat::default()).unwrap();
        assert!(out[0].get("strict").is_none());
    }

    #[test]
    fn thinking_budget_is_clamped_below_max_tokens() {
        let m = model("claude-sonnet-4-5");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
        };
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::High), // default budget 16384
            max_tokens: Some(4096),
            ..Default::default()
        };
        let params = build_params(
            &m,
            &context,
            false,
            &options,
            &AnthropicCompat::default(),
            CacheRetention::None,
        )
        .unwrap();
        // budget clamped to max_tokens - 1024, and temperature omitted while
        // thinking is enabled.
        assert_eq!(params["thinking"]["budget_tokens"], json!(4096 - 1024));
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            ..Default::default()
        };
        let params = build_params(
            &m,
            &context,
            false,
            &options,
            &AnthropicCompat::default(),
            CacheRetention::None,
        )
        .unwrap();
        assert!(params.get("temperature").is_none());
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(
            map_stop_reason("end_turn", None).unwrap().0,
            StopReason::Stop
        );
        assert_eq!(
            map_stop_reason("max_tokens", None).unwrap().0,
            StopReason::Length
        );
        assert_eq!(
            map_stop_reason("tool_use", None).unwrap().0,
            StopReason::ToolUse
        );
        let (reason, message) =
            map_stop_reason("refusal", Some(&json!({ "explanation": "no" }))).unwrap();
        assert_eq!(reason, StopReason::Error);
        assert_eq!(message.as_deref(), Some("no"));
        assert!(map_stop_reason("mystery", None).is_err());
    }
}
