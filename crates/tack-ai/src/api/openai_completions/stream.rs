use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::compat::{
    REASONING_FIELDS, append_openai_reasoning_detail, get_compat, is_openai_reasoning_detail,
};
use super::params::{
    build_params, grammar_constraint_map, has_auth_header, resolve_cache_retention,
};
use crate::api::fail;
use crate::json_repair::parse_streaming_json;
use crate::provider::StreamOptions;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use crate::types::{AssistantMessage, ContentBlock, Context, Model, StopReason, calculate_cost};

const TACK_USER_AGENT: &str = concat!("tack/", env!("CARGO_PKG_VERSION"));

fn parse_chunk_usage(raw: &Value, model: &Model) -> crate::types::Usage {
    let prompt_tokens = raw
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let details = raw.get("prompt_tokens_details");
    let cache_read = details
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| raw.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| raw.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = details
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input = prompt_tokens.saturating_sub(cache_read + cache_write);
    let output = raw
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = raw
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut usage = crate::types::Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: Some(reasoning),
        total_tokens: input + output + cache_read + cache_write,
        cost: Default::default(),
    };
    calculate_cost(model, &mut usage);
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

/// Streaming tool-call bookkeeping (by stream index and by id).
#[derive(Debug)]
struct ToolCallState {
    /// Position in `output.content`.
    content_pos: usize,
    partial_args: String,
    /// Grammar tool: the single string property the raw input maps to, and
    /// the input_json re-packager (TS GrammarToolInputJsonBuffer).
    grammar_property: Option<String>,
    grammar_buffer: crate::constrained_sampling::GrammarToolInputJsonBuffer,
}

impl ToolCallState {
    fn new(content_pos: usize) -> Self {
        ToolCallState {
            content_pos,
            partial_args: String::new(),
            grammar_property: None,
            grammar_buffer: Default::default(),
        }
    }
}

pub(crate) async fn run(
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

    let compat = get_compat(&model);
    let grammar_properties = grammar_constraint_map(&context, &compat)
        .into_iter()
        .map(|(name, g)| (name, g.input_property))
        .collect::<std::collections::HashMap<_, _>>();
    let retention = resolve_cache_retention(&options);
    let params = build_params(&model, &context, &options, &compat, retention);

    // --- request ---
    let client = crate::api::http_client();
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));

    let build_request = || {
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("user-agent", TACK_USER_AGENT);
        if let Some(key) = &api_key {
            request = request.header("authorization", format!("Bearer {key}"));
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
            fail!(
                output,
                sender,
                format!("OpenAI-compatible API error: {e}"),
                aborted,
            );
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
    let mut tool_calls_by_index: std::collections::HashMap<u64, ToolCallState> =
        std::collections::HashMap::new();
    let mut tool_calls_by_id: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new(); // id -> content_pos
    let mut has_finish_reason = false;
    let mut stream_error: Option<crate::api::ApiError> = None;
    // `reasoning_details` are replay metadata, not user-visible deltas. Keep
    // them in memory during streaming and serialize once when the stream
    // settles (TS streamedReasoningDetails).
    let mut streamed_reasoning_details: Vec<Value> = Vec::new();

    loop {
        let chunk: Value = match sse.next_json("OpenAI SSE chunk").await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };

        if let Some(id) = chunk.get("id").and_then(Value::as_str)
            && output.response_id.is_none()
        {
            output.response_id = Some(id.to_string());
        }
        if let Some(m) = chunk.get("model").and_then(Value::as_str)
            && !m.is_empty()
            && m != model.id
            && output.response_model.is_none()
        {
            output.response_model = Some(m.to_string());
        }
        if let Some(usage) = chunk.get("usage")
            && !usage.is_null()
        {
            output.usage = parse_chunk_usage(usage, &model);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            continue;
        };

        // Some providers (e.g. Moonshot) return usage in choice.usage.
        if (chunk.get("usage").is_none() || chunk.get("usage").is_some_and(Value::is_null))
            && let Some(usage) = choice.get("usage")
            && !usage.is_null()
        {
            output.usage = parse_chunk_usage(usage, &model);
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            output.raw_stop_reason = Some(reason.to_string());
            let (stop_reason, error_message) = map_stop_reason(reason);
            output.stop_reason = stop_reason;
            output.error_message = error_message;
            has_finish_reason = true;
        }

        let Some(delta) = choice.get("delta") else {
            continue;
        };

        // Text content.
        if let Some(content) = delta.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            // Start a text block if the last content block isn't text.
            let pos = match output.content.last() {
                Some(ContentBlock::Text { .. }) => output.content.len() - 1,
                _ => {
                    output.content.push(ContentBlock::Text {
                        text: String::new(),
                        text_signature: None,
                    });
                    let pos = output.content.len() - 1;
                    coalescer.push(
                        &sender,
                        &output,
                        AssistantMessageEvent::TextStart {
                            content_index: pos,
                            partial: output.clone(),
                        },
                    );
                    pos
                }
            };
            if let Some(ContentBlock::Text { text, .. }) = output.content.get_mut(pos) {
                text.push_str(content);
            }
            if let Some(ev) = coalescer.offer(
                crate::api::DeltaKind::Text,
                pos,
                content.to_string(),
                &output,
            ) {
                let _ = sender.push(ev);
            }
        }

        // Reasoning fields (first non-empty wins).
        let reasoning_field = REASONING_FIELDS.iter().find(|f| {
            delta
                .get(**f)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        });
        if let Some(field) = reasoning_field {
            let text = delta[*field].as_str().unwrap_or("");
            let pos = match output.content.last() {
                Some(ContentBlock::Thinking { .. }) => output.content.len() - 1,
                _ => {
                    output.content.push(ContentBlock::Thinking {
                        thinking: String::new(),
                        thinking_signature: Some((*field).to_string()),
                        redacted: None,
                    });
                    let pos = output.content.len() - 1;
                    coalescer.push(
                        &sender,
                        &output,
                        AssistantMessageEvent::ThinkingStart {
                            content_index: pos,
                            partial: output.clone(),
                        },
                    );
                    pos
                }
            };
            if let Some(ContentBlock::Thinking { thinking, .. }) = output.content.get_mut(pos) {
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

        // Structured reasoning_details (OpenRouter et al.): replay metadata
        // only; merge consecutive text/summary deltas, keep encrypted
        // entries discrete.
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details {
                if !is_openai_reasoning_detail(detail) {
                    continue;
                }
                // ensureThinkingBlock("")
                if !matches!(output.content.last(), Some(ContentBlock::Thinking { .. })) {
                    output.content.push(ContentBlock::Thinking {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    });
                    let pos = output.content.len() - 1;
                    coalescer.push(
                        &sender,
                        &output,
                        AssistantMessageEvent::ThinkingStart {
                            content_index: pos,
                            partial: output.clone(),
                        },
                    );
                }
                append_openai_reasoning_detail(&mut streamed_reasoning_details, detail.clone());
            }
        }

        // Tool calls.
        if let Some(tool_call_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tool_call_deltas {
                let stream_index = tc.get("index").and_then(Value::as_u64);
                let id = tc.get("id").and_then(Value::as_str);
                // Grammar tools stream via `custom` (raw grammar text instead
                // of JSON arguments).
                let custom = tc.get("custom");
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .or_else(|| custom.and_then(|c| c.get("name")).and_then(Value::as_str));
                let args_delta = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .or_else(|| custom.and_then(|c| c.get("input")).and_then(Value::as_str));

                // Find or create the block.
                let content_pos = if let Some(idx) = stream_index {
                    if let Some(state) = tool_calls_by_index.get(&idx) {
                        Some(state.content_pos)
                    } else if let Some(id) = id {
                        tool_calls_by_id.get(id).copied()
                    } else {
                        None
                    }
                } else {
                    id.and_then(|id| tool_calls_by_id.get(id).copied())
                };

                let content_pos = match content_pos {
                    Some(pos) => pos,
                    None => {
                        output.content.push(ContentBlock::ToolCall {
                            id: id.unwrap_or("").to_string(),
                            name: name.unwrap_or("").to_string(),
                            arguments: json!({}),
                            thought_signature: None,
                            namespace: None,
                        });
                        let pos = output.content.len() - 1;
                        if let Some(idx) = stream_index {
                            tool_calls_by_index.insert(idx, ToolCallState::new(pos));
                        }
                        if let Some(id) = id {
                            tool_calls_by_id.insert(id.to_string(), pos);
                        }
                        coalescer.push(
                            &sender,
                            &output,
                            AssistantMessageEvent::ToolCallStart {
                                content_index: pos,
                                partial: output.clone(),
                            },
                        );
                        pos
                    }
                };

                // Late-arriving id/name.
                if let Some(ContentBlock::ToolCall {
                    id: bid,
                    name: bname,
                    ..
                }) = output.content.get_mut(content_pos)
                {
                    if bid.is_empty()
                        && let Some(id) = id
                    {
                        *bid = id.to_string();
                        tool_calls_by_id.insert(id.to_string(), content_pos);
                    }
                    if bname.is_empty()
                        && let Some(name) = name
                    {
                        *bname = name.to_string();
                    }
                }

                if let Some(args) = args_delta
                    && !args.is_empty()
                {
                    if let Some(idx) = stream_index {
                        let property = name.and_then(|n| grammar_properties.get(n)).cloned();
                        let state = tool_calls_by_index
                            .entry(idx)
                            .or_insert_with(|| ToolCallState::new(content_pos));
                        if state.grammar_property.is_none() {
                            state.grammar_property = property;
                        }
                        state.partial_args.push_str(args);
                        if let Some(property) = state.grammar_property.clone() {
                            // Grammar tool: raw input becomes the single
                            // string property; deltas are re-packaged as
                            // input_json deltas.
                            let delta = state
                                .grammar_buffer
                                .append_delta(&property, &state.partial_args, false)
                                .ok()
                                .flatten()
                                .unwrap_or_default();
                            if !delta.is_empty() {
                                // Defer the full-arguments rebuild to the
                                // coalescer window (see F13 note below).
                                if coalescer.would_flush(
                                    crate::api::DeltaKind::ToolCall,
                                    content_pos,
                                    delta.len(),
                                ) && let Some(ContentBlock::ToolCall { arguments, .. }) =
                                    output.content.get_mut(content_pos)
                                {
                                    *arguments =
                                        json!({ property.clone(): state.partial_args.clone() });
                                }
                                if let Some(ev) = coalescer.offer(
                                    crate::api::DeltaKind::ToolCall,
                                    content_pos,
                                    delta,
                                    &output,
                                ) {
                                    let _ = sender.push(ev);
                                }
                            }
                        } else {
                            // Defer the O(accumulated) streaming re-parse to
                            // the coalescer window: parse only when a merged
                            // delta event is about to go out (and once more
                            // at stream end below), not per tiny delta.
                            if coalescer.would_flush(
                                crate::api::DeltaKind::ToolCall,
                                content_pos,
                                args.len(),
                            ) {
                                let parsed = parse_streaming_json(&state.partial_args);
                                if let Some(ContentBlock::ToolCall { arguments, .. }) =
                                    output.content.get_mut(content_pos)
                                {
                                    *arguments = parsed;
                                }
                            }
                            if let Some(ev) = coalescer.offer(
                                crate::api::DeltaKind::ToolCall,
                                content_pos,
                                args.to_string(),
                                &output,
                            ) {
                                let _ = sender.push(ev);
                            }
                        }
                    } else {
                        let parsed = parse_streaming_json(args);
                        if let Some(ContentBlock::ToolCall { arguments, .. }) =
                            output.content.get_mut(content_pos)
                        {
                            *arguments = parsed;
                        }
                        if let Some(ev) = coalescer.offer(
                            crate::api::DeltaKind::ToolCall,
                            content_pos,
                            args.to_string(),
                            &output,
                        ) {
                            let _ = sender.push(ev);
                        }
                    }
                }
            }
        }
    }

    // --- termination ---
    // Serialize streamed reasoning_details once, onto the thinking block(s)
    // (TS applyStreamedReasoningDetails at thinking_end).
    if !streamed_reasoning_details.is_empty()
        && let Ok(signature) = serde_json::to_string(&streamed_reasoning_details)
    {
        for block in &mut output.content {
            if let ContentBlock::Thinking {
                thinking_signature, ..
            } = block
            {
                *thinking_signature = Some(signature.clone());
            }
        }
    }

    if let Some(error) = stream_error {
        let aborted = cancel.is_cancelled() || error.is_aborted();
        coalescer.flush_into(&sender, &output);
        fail!(output, sender, error.to_string(), aborted);
    }

    // Finalize all tool call blocks (parse final arguments + emit end events).
    // TS finishBlock parity: text/thinking blocks also get explicit
    // text_end/thinking_end events at stream end — openai-completions has no
    // per-block stop frames, so the adapter must synthesize them here rather
    // than letting them end implicitly.
    let content_len = output.content.len();
    for pos in 0..content_len {
        match output.content.get(pos) {
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
            _ => {}
        }
        if matches!(output.content.get(pos), Some(ContentBlock::ToolCall { .. })) {
            let state = tool_calls_by_index.values().find(|s| s.content_pos == pos);
            let final_args = state.map(|s| match &s.grammar_property {
                Some(property) => json!({ property: s.partial_args }),
                None => parse_streaming_json(&s.partial_args),
            });
            if let Some(ContentBlock::ToolCall { arguments, .. }) = output.content.get_mut(pos)
                && let Some(parsed) = final_args
            {
                *arguments = parsed;
            }
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
    }

    if !has_finish_reason && !compat.supports_finish_reason {
        output.stop_reason = if output.has_tool_calls() {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
    }
    if output.stop_reason == StopReason::Error {
        let message = output
            .error_message
            .clone()
            .unwrap_or_else(|| "Provider returned an error stop reason".into());
        coalescer.flush_into(&sender, &output);
        fail!(output, sender, message, false);
    }
    if (compat.supports_finish_reason && !has_finish_reason)
        || output.stop_reason == StopReason::Pending
    {
        coalescer.flush_into(&sender, &output);
        fail!(
            output,
            sender,
            "Stream ended without finish_reason".to_string(),
            false,
        );
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
    use super::super::generic_model;
    use super::*;

    /// Kimi reports top-level `usage.cached_tokens` (TS #8075): it must
    /// count as cache reads, not normal input tokens.
    #[test]
    fn top_level_cached_tokens_count_as_cache_reads() {
        let model = generic_model("kimi-coding", "https://api.kimi.com/coding/v1");
        let usage = parse_chunk_usage(
            &json!({
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "cached_tokens": 40,
            }),
            &model,
        );
        assert_eq!(usage.cache_read, 40);
        assert_eq!(usage.input, 60);
        assert_eq!(usage.total_tokens, 110);
    }
}
