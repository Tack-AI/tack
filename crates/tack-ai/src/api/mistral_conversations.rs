//! Mistral Conversations adapter. Port of `mistral-conversations.ts`
//! (Mistral's chat-completions-style SSE API with prompt-mode reasoning).

use std::collections::HashMap;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::fail;
use crate::json_repair::parse_streaming_json;
use crate::provider::StreamOptions;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use crate::transform::transform_messages;
use crate::types::{
    AssistantMessage, ContentBlock, Context, InputContentBlock, Message, Model, StopReason,
    UserContent, calculate_cost,
};

const TACK_USER_AGENT: &str = concat!("tack/", env!("CARGO_PKG_VERSION"));
const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;

#[derive(Clone, Debug, Default)]
pub struct MistralConversationsProvider;

impl crate::provider::Provider for MistralConversationsProvider {
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

fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Mistral requires 9-char alphanumeric tool call ids.
fn make_id_normalizer() -> impl Fn(&str) -> String {
    let state = std::cell::RefCell::new((
        HashMap::<String, String>::new(),
        HashMap::<String, String>::new(),
    ));
    move |id: &str| {
        let (map, reverse) = &mut *state.borrow_mut();
        if let Some(existing) = map.get(id) {
            return existing.clone();
        }
        let mut attempt = 0u32;
        loop {
            let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            let candidate = if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
                normalized
            } else {
                let seed = if attempt == 0 {
                    if normalized.is_empty() {
                        id.to_string()
                    } else {
                        normalized
                    }
                } else {
                    format!(
                        "{}:{attempt}",
                        if normalized.is_empty() {
                            id
                        } else {
                            &normalized
                        }
                    )
                };
                derive_tool_call_id(&seed)
            };
            let taken = reverse.get(&candidate).is_some_and(|owner| owner != id);
            if !taken {
                map.insert(id.to_string(), candidate.clone());
                reverse.insert(candidate.clone(), id.to_string());
                return candidate;
            }
            attempt += 1;
        }
    }
}

/// Deterministic 9-char alphanumeric id from a seed: 64-bit FNV-1a as
/// 16 hex digits (hex is alphanumeric), first 9. An 8-hex-digit u32 hash
/// can never reach the required 9 chars.
fn derive_tool_call_id(seed: &str) -> String {
    format!("{:016x}", fnv1a(seed))
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();

    for msg in messages {
        match msg {
            Message::User(u) => match &u.content {
                UserContent::Text(text) => {
                    result.push(json!({ "role": "user", "content": text }));
                }
                UserContent::Blocks(blocks) => {
                    let had_images = blocks
                        .iter()
                        .any(|b| matches!(b, InputContentBlock::Image { .. }));
                    let content: Vec<Value> = blocks
                        .iter()
                        .filter(|b| matches!(b, InputContentBlock::Text { .. }) || supports_images)
                        .map(|b| match b {
                            InputContentBlock::Text { text, .. } => {
                                json!({ "type": "text", "text": text })
                            }
                            InputContentBlock::Image { data, mime_type } => json!({
                                "type": "image_url",
                                "image_url": format!("data:{mime_type};base64,{data}"),
                            }),
                        })
                        .collect();
                    if !content.is_empty() {
                        result.push(json!({ "role": "user", "content": content }));
                    } else if had_images && !supports_images {
                        result.push(json!({
                            "role": "user",
                            "content": "(image omitted: model does not support images)",
                        }));
                    }
                }
            },
            Message::Assistant(a) => {
                let mut content_parts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in &a.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            content_parts.push(json!({ "type": "text", "text": text }));
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                            content_parts.push(json!({
                                "type": "thinking",
                                "thinking": [{ "type": "text", "text": thinking }],
                            }));
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": arguments.to_string() },
                                "index": 0,
                            }));
                        }
                        _ => {}
                    }
                }
                let has_content = !content_parts.is_empty();
                let has_tools = !tool_calls.is_empty();
                let mut message = json!({ "role": "assistant", "prefix": false });
                if has_content {
                    message["content"] = Value::Array(content_parts);
                }
                if has_tools {
                    message["tool_calls"] = Value::Array(tool_calls);
                }
                if has_content || has_tools {
                    result.push(message);
                }
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
                let has_images = t
                    .content
                    .iter()
                    .any(|c| matches!(c, InputContentBlock::Image { .. }));
                let tool_text = build_tool_result_text(
                    text_result.trim(),
                    has_images,
                    supports_images,
                    t.is_error,
                );
                let mut content = vec![json!({ "type": "text", "text": tool_text })];
                if supports_images {
                    for block in &t.content {
                        if let InputContentBlock::Image { data, mime_type } = block {
                            content.push(json!({
                                "type": "image_url",
                                "image_url": format!("data:{mime_type};base64,{data}"),
                            }));
                        }
                    }
                }
                result.push(json!({
                    "role": "tool",
                    "tool_call_id": t.tool_call_id,
                    "name": t.tool_name,
                    "content": content,
                }));
            }
            // The caller collapses transcripts before building the Context;
            // skip system messages defensively.
            Message::System(_) => {}
        }
    }

    result
}

fn build_tool_result_text(
    trimmed: &str,
    has_images: bool,
    supports_images: bool,
    is_error: bool,
) -> String {
    let error_prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let image_suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{image_suffix}");
    }
    if has_images {
        if supports_images {
            return format!("{error_prefix}(see attached image)");
        }
        return format!("{error_prefix}(image omitted: model does not support images)");
    }
    format!("{error_prefix}(no tool output)")
}

fn uses_reasoning_effort(model_id: &str) -> bool {
    model_id == "mistral-small-2603"
        || model_id == "mistral-small-latest"
        || model_id.starts_with("mistral-medium-")
        // GLM-5.2 ignores prompt_mode but honors reasoning_effort (TS #9375).
        || model_id == "zai-glm-5-2"
}

fn build_payload(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let normalize = make_id_normalizer();
    let transformed = transform_messages(context.messages.as_slice(), model, Some(&normalize));
    let mut messages = to_chat_messages(&transformed, model.supports_images());

    if let Some(system_prompt) = &context.system_prompt {
        messages.insert(0, json!({ "role": "system", "content": system_prompt }));
    }

    let mut payload = json!({
        "model": model.id,
        "stream": true,
        "messages": messages,
    });

    if !context.tools.is_empty() {
        payload["tools"] = Value::Array(
            context
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                            "strict": false,
                        },
                    })
                })
                .collect(),
        );
    }
    if let Some(temperature) = options.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = options.max_tokens {
        payload["max_tokens"] = json!(max_tokens);
    }
    if let Some(choice) = options.tool_choice {
        payload["tool_choice"] = json!(match choice {
            crate::provider::ToolChoice::Auto => "auto",
            crate::provider::ToolChoice::None => "none",
        });
    }
    if model.reasoning
        && let Some(level) = options.reasoning
    {
        if uses_reasoning_effort(&model.id) {
            let effort = model
                .thinking_level_value(level)
                .cloned()
                .flatten()
                .unwrap_or_else(|| "high".to_string());
            payload["reasoning_effort"] = json!(effort);
        } else {
            payload["prompt_mode"] = json!("reasoning");
        }
    }
    if !matches!(
        options.cache_retention,
        Some(crate::provider::CacheRetention::None)
    ) && let Some(session_id) = &options.session_id
    {
        payload["prompt_cache_key"] = json!(session_id);
    }
    for (k, v) in &options.sampling_params {
        payload[k.as_str()] = v.clone();
    }

    payload
}

fn cached_prompt_tokens(usage: &Value, prompt_tokens: u64) -> u64 {
    for path in [
        "/prompt_tokens_details/cached_tokens",
        "/promptTokensDetails/cachedTokens",
        "/prompt_token_details/cached_tokens",
        "/promptTokenDetails/cachedTokens",
        "/num_cached_tokens",
        "/numCachedTokens",
    ] {
        if let Some(v) = usage.pointer(path).and_then(Value::as_u64) {
            return v.min(prompt_tokens);
        }
    }
    0
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        other => crate::api::unknown_stop_reason(other),
    }
}

/// Accumulate one chunk's `delta.tool_calls` into the output message.
/// TS processMistralStream: chunks are keyed by `toolCall.index ?? callId`
/// because Mistral (like OpenAI) only sends the tool-call id on the FIRST
/// chunk of a call — later chunks carry just the index. Keying by id would
/// split one logical call into two blocks.
fn handle_tool_call_deltas(
    output: &mut AssistantMessage,
    tool_blocks: &mut HashMap<String, (usize, String)>,
    tool_calls: &[Value],
    sender: &AssistantMessageEventSender,
    coalescer: &mut crate::api::DeltaCoalescer,
) {
    for tool_call in tool_calls {
        let index = tool_call.get("index").and_then(Value::as_u64);
        let id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && *s != "null")
            .map(str::to_string)
            .unwrap_or_else(|| derive_tool_call_id(&format!("toolcall:{}", index.unwrap_or(0))));
        let key = index.map(|i| i.to_string()).unwrap_or_else(|| id.clone());

        if !tool_blocks.contains_key(&key) {
            output.content.push(ContentBlock::ToolCall {
                id: id.clone(),
                name: tool_call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            });
            let idx = output.content.len() - 1;
            tool_blocks.insert(key.clone(), (idx, String::new()));
            coalescer.push(
                sender,
                output,
                AssistantMessageEvent::ToolCallStart {
                    content_index: idx,
                    partial: output.clone(),
                },
            );
        }

        let args_delta = match tool_call.pointer("/function/arguments") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        };
        let (idx, partial) = tool_blocks.get_mut(&key).expect("block exists");
        partial.push_str(&args_delta);
        // Defer the O(accumulated) streaming re-parse to the coalescer
        // window: parse only when a merged delta event is about to go out
        // (the finish path below re-parses the final arguments), not per
        // tiny delta.
        if coalescer.would_flush(crate::api::DeltaKind::ToolCall, *idx, args_delta.len()) {
            let parsed = parse_streaming_json(partial);
            if let Some(ContentBlock::ToolCall { arguments, .. }) = output.content.get_mut(*idx) {
                *arguments = parsed;
            }
        }
        if let Some(ev) = coalescer.offer(crate::api::DeltaKind::ToolCall, *idx, args_delta, output)
        {
            let _ = sender.push(ev);
        }
    }
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

    let Some(api_key) = options.api_key.clone() else {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            format!("No API key for provider: {}", model.provider),
            false
        );
    };

    let url = format!(
        "{}/v1/chat/completions",
        model.base_url.trim_end_matches('/')
    );
    let payload = build_payload(&model, &context, &options);

    let client = crate::api::http_client();
    let build_request = || {
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {api_key}"))
            .header("user-agent", TACK_USER_AGENT);
        if let Some(headers) = &model.headers {
            for (k, v) in headers {
                request = request.header(k, v);
            }
        }
        for (k, v) in &options.headers {
            request = request.header(k, v);
        }
        request.body(payload.to_string())
    };

    let response = match crate::api::send_with_retry(build_request, &cancel, &payload).await {
        Ok(r) => r,
        Err(e) => {
            let aborted = cancel.is_cancelled() || e.is_aborted();
            coalescer.flush_into(&sender, &output);
            fail!(output, sender, format!("Mistral API error: {e}"), aborted);
        }
    };

    coalescer.push(
        &sender,
        &output,
        AssistantMessageEvent::Start {
            partial: output.clone(),
        },
    );

    let mut sse = crate::api::SseStream::new(response.bytes_stream(), cancel.clone());
    let mut current_block: Option<(usize, bool)> = None; // (index, is_thinking)
    let mut tool_blocks: HashMap<String, (usize, String)> = HashMap::new(); // key -> (index, partial_args)
    let mut stream_error: Option<crate::api::ApiError> = None;

    loop {
        let chunk: Value = match sse.next_json("Mistral SSE event").await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };

        if output.response_id.is_none()
            && let Some(id) = chunk.get("id").and_then(Value::as_str)
        {
            output.response_id = Some(id.to_string());
        }

        if let Some(usage) = chunk.get("usage")
            && !usage.is_null()
        {
            let prompt = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = cached_prompt_tokens(usage, prompt);
            output.usage.input = prompt.saturating_sub(cached);
            output.usage.output = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            output.usage.cache_read = cached;
            output.usage.total_tokens = usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(output.usage.input + output.usage.output + cached);
            calculate_cost(&model, &mut output.usage);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            continue;
        };

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            output.raw_stop_reason = Some(reason.to_string());
            let (stop_reason, error_message) = map_stop_reason(reason);
            output.stop_reason = stop_reason;
            output.error_message = error_message;
        }

        let Some(delta) = choice.get("delta") else {
            continue;
        };

        if let Some(content) = delta.get("content")
            && !content.is_null()
        {
            // Content is a string or an array of chunks.
            let items: Vec<(bool, String)> = match content {
                Value::String(s) => vec![(false, s.clone())],
                Value::Array(arr) => arr
                    .iter()
                    .filter_map(|item| {
                        let item_type = item.get("type").and_then(Value::as_str)?;
                        match item_type {
                            "thinking" => {
                                let text = item
                                    .get("thinking")
                                    .and_then(Value::as_array)
                                    .map(|parts| {
                                        parts
                                            .iter()
                                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                                            .collect::<Vec<_>>()
                                            .join("")
                                    })
                                    .unwrap_or_default();
                                if text.is_empty() {
                                    None
                                } else {
                                    Some((true, text))
                                }
                            }
                            "text" => item
                                .get("text")
                                .and_then(Value::as_str)
                                .map(|t| (false, t.to_string())),
                            _ => None,
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };

            for (is_thinking, text) in items {
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
                    Some(ContentBlock::Thinking { thinking, .. }) => {
                        thinking.push_str(&text);
                        if let Some(ev) =
                            coalescer.offer(crate::api::DeltaKind::Thinking, idx, text, &output)
                        {
                            let _ = sender.push(ev);
                        }
                    }
                    Some(ContentBlock::Text { text: t, .. }) => {
                        t.push_str(&text);
                        if let Some(ev) =
                            coalescer.offer(crate::api::DeltaKind::Text, idx, text, &output)
                        {
                            let _ = sender.push(ev);
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array)
            && !tool_calls.is_empty()
        {
            crate::api::close_current_block!(current_block, output, sender, coalescer);
            handle_tool_call_deltas(
                &mut output,
                &mut tool_blocks,
                tool_calls,
                &sender,
                &mut coalescer,
            );
        }
    }

    crate::api::close_current_block!(current_block, output, sender, coalescer);
    for (idx, partial) in tool_blocks.values() {
        if let Some(ContentBlock::ToolCall { arguments, .. }) = output.content.get_mut(*idx) {
            *arguments = parse_streaming_json(partial);
        }
        let tool_call = output.content[*idx].clone();
        coalescer.push(
            &sender,
            &output,
            AssistantMessageEvent::ToolCallEnd {
                content_index: *idx,
                tool_call,
                partial: output.clone(),
            },
        );
    }

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
            "Mistral stream ended without a finish reason".to_string(),
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

    fn test_model() -> Model {
        Model {
            id: "mistral-large-latest".into(),
            name: "Mistral Large".into(),
            api: "mistral-conversations".into(),
            provider: "mistral".into(),
            base_url: "https://api.mistral.ai".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: crate::types::ModelCost::default(),
            context_window: 128_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    /// TS pi #8700: all mistral-medium-* models use reasoning_effort.
    #[test]
    fn reasoning_effort_matches_mistral_medium_prefix() {
        assert!(uses_reasoning_effort("mistral-medium-3.5"));
        assert!(uses_reasoning_effort("mistral-medium-2508"));
        assert!(uses_reasoning_effort("mistral-small-2603"));
        assert!(uses_reasoning_effort("mistral-small-latest"));
        assert!(!uses_reasoning_effort("mistral-large-latest"));
        assert!(!uses_reasoning_effort("codestral-latest"));
        // TS #9375: GLM-5.2 ignores prompt_mode but honors reasoning_effort.
        assert!(uses_reasoning_effort("zai-glm-5-2"));
        assert!(!uses_reasoning_effort("zai-glm-5"));
    }

    /// Regression: Mistral only sends the tool-call id on the first chunk.
    /// Later chunks carry only `index`; keying the accumulator by id (as the
    /// old `{id}:{index}` key did) split one logical call into two blocks —
    /// one with the name and partial args, one anonymous with the rest.
    #[test]
    fn tool_call_chunks_without_id_merge_by_index() {
        let model = test_model();
        let mut output = AssistantMessage::pending(&model);
        let mut tool_blocks: HashMap<String, (usize, String)> = HashMap::new();
        let (sender, _stream) = crate::stream::event_stream();

        let chunks = [
            json!([{ "index": 0, "id": "abc123xyz", "function": { "name": "read", "arguments": "{\"path\":" } }]),
            json!([{ "index": 0, "function": { "arguments": "\"a.txt\"}" } }]),
        ];
        // Zero-interval coalescer: flush (and re-parse arguments) on every
        // delta so the intermediate `output` assertions below see the
        // throttled parse results (production parses at the 50ms window).
        let mut coalescer =
            crate::api::DeltaCoalescer::with_limits(std::time::Duration::ZERO, 1 << 20);
        for chunk in &chunks {
            handle_tool_call_deltas(
                &mut output,
                &mut tool_blocks,
                chunk.as_array().unwrap(),
                &sender,
                &mut coalescer,
            );
        }

        assert_eq!(
            output.content.len(),
            1,
            "chunks must merge into one tool call"
        );
        match &output.content[0] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "abc123xyz");
                assert_eq!(name, "read");
                assert_eq!(*arguments, json!({ "path": "a.txt" }));
            }
            other => panic!("expected tool call: {other:?}"),
        }
    }

    #[test]
    fn parallel_tool_calls_accumulate_independently() {
        let model = test_model();
        let mut output = AssistantMessage::pending(&model);
        let mut tool_blocks: HashMap<String, (usize, String)> = HashMap::new();
        let (sender, _stream) = crate::stream::event_stream();

        let chunks = [
            json!([
                { "index": 0, "id": "id0000001", "function": { "name": "a", "arguments": "{\"x\":" } },
                { "index": 1, "id": "id0000002", "function": { "name": "b", "arguments": "{\"y\":" } },
            ]),
            json!([
                { "index": 0, "function": { "arguments": "1}" } },
                { "index": 1, "function": { "arguments": "2}" } },
            ]),
        ];
        // Zero-interval coalescer: flush (and re-parse arguments) on every
        // delta so the intermediate `output` assertions below see the
        // throttled parse results (production parses at the 50ms window).
        let mut coalescer =
            crate::api::DeltaCoalescer::with_limits(std::time::Duration::ZERO, 1 << 20);
        for chunk in &chunks {
            handle_tool_call_deltas(
                &mut output,
                &mut tool_blocks,
                chunk.as_array().unwrap(),
                &sender,
                &mut coalescer,
            );
        }

        assert_eq!(output.content.len(), 2);
        let args: Vec<&Value> = output
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { arguments, .. } => Some(arguments),
                _ => None,
            })
            .collect();
        assert_eq!(*args[0], json!({ "x": 1 }));
        assert_eq!(*args[1], json!({ "y": 2 }));
    }

    /// Id-less chunks (some proxies omit both id and index on later chunks is
    /// not expected, but entirely id-less calls get a deterministic derived id).
    #[test]
    fn idless_tool_call_gets_deterministic_id() {
        let model = test_model();
        let mut output = AssistantMessage::pending(&model);
        let mut tool_blocks: HashMap<String, (usize, String)> = HashMap::new();
        let (sender, _stream) = crate::stream::event_stream();

        let chunks = [
            json!([{ "index": 0, "function": { "name": "a", "arguments": "{}" } }]),
            json!([{ "index": 0, "function": { "arguments": "" } }]),
        ];
        // Zero-interval coalescer: flush (and re-parse arguments) on every
        // delta so the intermediate `output` assertions below see the
        // throttled parse results (production parses at the 50ms window).
        let mut coalescer =
            crate::api::DeltaCoalescer::with_limits(std::time::Duration::ZERO, 1 << 20);
        for chunk in &chunks {
            handle_tool_call_deltas(
                &mut output,
                &mut tool_blocks,
                chunk.as_array().unwrap(),
                &sender,
                &mut coalescer,
            );
        }
        assert_eq!(output.content.len(), 1);
        match &output.content[0] {
            ContentBlock::ToolCall { id, .. } => {
                assert!(!id.is_empty() && id.len() <= MISTRAL_TOOL_CALL_ID_LENGTH);
                assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
            }
            other => panic!("expected tool call: {other:?}"),
        }
    }

    #[test]
    fn id_normalizer_is_stable_and_collision_free() {
        let normalize = make_id_normalizer();
        let a1 = normalize("call_abc:123");
        let a2 = normalize("call_abc:123");
        assert_eq!(a1, a2, "same input must map to the same id");
        assert_eq!(
            a1.len(),
            MISTRAL_TOOL_CALL_ID_LENGTH,
            "hashed ids must be exactly 9 chars (Mistral requirement), got {a1:?}"
        );
        assert!(a1.chars().all(|c| c.is_ascii_alphanumeric()));
        let nine = normalize("abcdefghi");
        assert_eq!(nine, "abcdefghi", "9-char alnum ids pass through");
        let b = normalize("call_abc:124");
        assert_ne!(a1, b);
    }
}
