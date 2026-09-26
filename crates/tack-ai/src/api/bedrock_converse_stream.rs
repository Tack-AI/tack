//! Bedrock ConverseStream provider: `POST /model/{modelId}/converse-stream`
//! against `bedrock-runtime.{region}.amazonaws.com`, SigV4-signed per attempt
//! (fresh timestamp on every retry) or Bearer-authenticated.

use std::collections::HashMap;

use base64::Engine;
use futures_util::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::api::fail;
use crate::provider::StreamOptions;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use crate::types::{AssistantMessage, ContentBlock, Context, Model, StopReason, calculate_cost};

use super::bedrock::convert::build_request_body;
use super::bedrock::credentials::{self, BedrockAuth};
use super::bedrock::eventstream::{EventStreamDecoder, Frame};
use super::bedrock::sigv4;

const TACK_USER_AGENT: &str = concat!("tack/", env!("CARGO_PKG_VERSION"));

/// Matches the placeholder the Anthropic API path uses for redacted thinking.
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";

/// Upper bound for server-supplied content block indices; guards against
/// malformed/malicious `contentBlockIndex` values causing OOM fills.
const MAX_CONTENT_BLOCKS: usize = 4096;

#[derive(Clone, Debug, Default)]
pub struct BedrockConverseStreamProvider;

impl crate::provider::Provider for BedrockConverseStreamProvider {
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

/// Region resolution order (TS): ARN in model id > env > base_url-derived >
/// id-prefix heuristic > us-east-1.
fn resolve_region(model: &Model) -> String {
    // arn:aws[-...]:bedrock:{region}:...
    if model.id.starts_with("arn:") {
        let parts: Vec<&str> = model.id.split(':').collect();
        if parts.len() > 3 && !parts[3].is_empty() {
            return parts[3].to_string();
        }
    }
    for var in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(region) = std::env::var(var)
            && !region.is_empty()
        {
            return region;
        }
    }
    if let Some(region) = model
        .base_url
        .strip_prefix("https://bedrock-runtime.")
        .and_then(|rest| rest.strip_suffix(".amazonaws.com"))
        && !region.is_empty()
    {
        return region.to_string();
    }
    if model.id.starts_with("eu.") {
        return "eu-central-1".to_string();
    }
    "us-east-1".to_string()
}

fn resolve_endpoint(model: &Model, region: &str) -> String {
    let base = model.base_url.trim_end_matches('/');
    if base.is_empty() {
        format!("https://bedrock-runtime.{region}.amazonaws.com")
    } else {
        base.to_string()
    }
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        other => crate::api::unknown_stop_reason(other),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
}

struct StreamState {
    output: AssistantMessage,
    blocks: HashMap<usize, BlockKind>,
    tool_json: HashMap<usize, String>,
    thinking_signatures: HashMap<usize, String>,
    /// Scratch buffers for encrypted reasoning deltas (`redactedContent`),
    /// joined into `thinking_signature` when the block closes.
    redacted_chunks: HashMap<usize, Vec<Vec<u8>>>,
    /// Delta window (see api::DeltaCoalescer): frames arrive token-at-a-time
    /// and each used to clone the whole partial message into an event.
    coalescer: crate::api::DeltaCoalescer,
}

impl StreamState {
    fn new(model: &Model) -> Self {
        StreamState {
            output: AssistantMessage::pending(model),
            blocks: HashMap::new(),
            tool_json: HashMap::new(),
            thinking_signatures: HashMap::new(),
            redacted_chunks: HashMap::new(),
            coalescer: crate::api::DeltaCoalescer::new(),
        }
    }

    /// Buffer a provider delta; emits the merged window event when full.
    fn offer_delta(
        &mut self,
        events: &mut Vec<AssistantMessageEvent>,
        kind: crate::api::DeltaKind,
        index: usize,
        delta: String,
    ) {
        if let Some(ev) = self.coalescer.offer(kind, index, delta, &self.output) {
            events.push(ev);
        }
    }

    /// Flush the delta window before a structural/terminal event.
    fn flush_pending(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        if let Some(ev) = self.coalescer.flush(&self.output) {
            events.push(ev);
        }
    }

    fn open_block(&mut self, index: usize, kind: BlockKind) -> Option<AssistantMessageEvent> {
        // A malformed/malicious server could send a huge `contentBlockIndex`;
        // filling content up to it would exhaust memory. Reject instead.
        if index >= MAX_CONTENT_BLOCKS {
            tracing::warn!(index, "ignoring content block with out-of-range index");
            return None;
        }
        let block = match kind {
            BlockKind::Text => ContentBlock::Text {
                text: String::new(),
                text_signature: None,
            },
            BlockKind::Thinking => ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: None,
                redacted: None,
            },
            BlockKind::Tool => ContentBlock::ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: Value::Null,
                thought_signature: None,
                namespace: None,
            },
        };
        while self.output.content.len() <= index {
            self.output.content.push(ContentBlock::Text {
                text: String::new(),
                text_signature: None,
            });
        }
        self.output.content[index] = block;
        self.blocks.insert(index, kind);
        let partial = self.output.clone();
        Some(match kind {
            BlockKind::Text => AssistantMessageEvent::TextStart {
                content_index: index,
                partial,
            },
            BlockKind::Thinking => AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial,
            },
            BlockKind::Tool => AssistantMessageEvent::ToolCallStart {
                content_index: index,
                partial,
            },
        })
    }

    fn close_block(&mut self, index: usize) -> Option<AssistantMessageEvent> {
        let kind = self.blocks.remove(&index)?;
        match (kind, self.output.content.get_mut(index)) {
            (BlockKind::Text, Some(ContentBlock::Text { text, .. })) => {
                // Clone, don't take: the final message must keep the text.
                let content = text.clone();
                let partial = self.output.clone();
                Some(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial,
                })
            }
            (
                BlockKind::Thinking,
                Some(ContentBlock::Thinking {
                    thinking,
                    thinking_signature,
                    redacted,
                }),
            ) => {
                let content = thinking.clone();
                // Encrypted reasoning wins over any Anthropic-style signature:
                // mixing them would corrupt whichever arrived first.
                if let Some(chunks) = self.redacted_chunks.remove(&index) {
                    *redacted = Some(true);
                    *thinking_signature =
                        Some(base64::engine::general_purpose::STANDARD.encode(chunks.concat()));
                } else {
                    *thinking_signature = self.thinking_signatures.remove(&index);
                }
                let partial = self.output.clone();
                Some(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial,
                })
            }
            (BlockKind::Tool, Some(ContentBlock::ToolCall { arguments, .. })) => {
                if let Some(json) = self.tool_json.remove(&index) {
                    *arguments = crate::json_repair::parse_streaming_json(&json);
                }
                let tool_call = self.output.content[index].clone();
                let partial = self.output.clone();
                Some(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call,
                    partial,
                })
            }
            _ => None,
        }
    }

    /// Handle one ConverseStream event frame. Returns events to forward.
    fn handle_frame(&mut self, frame: &Frame) -> Result<Vec<AssistantMessageEvent>, String> {
        // Exception frames carry the error in headers.
        if frame.header_str(":message-type") == Some("exception") {
            let code = frame.header_str(":error-code").unwrap_or("unknown");
            let message = frame.header_str(":error-message").unwrap_or("");
            let payload: Value = serde_json::from_slice(&frame.payload).unwrap_or_default();
            let detail = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(message);
            return Err(format_exception(code, detail));
        }

        let payload: Value = serde_json::from_slice(&frame.payload)
            .map_err(|e| format!("bad event payload: {e}"))?;
        let mut events = Vec::new();
        match frame.event_type() {
            Some("messageStart") => {
                self.flush_pending(&mut events);
                events.push(AssistantMessageEvent::Start {
                    partial: self.output.clone(),
                });
            }
            Some("contentBlockStart") => {
                let index = payload
                    .get("contentBlockIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if let Some(tool_use) = payload.pointer("/start/toolUse") {
                    if let Some(event) = self.open_block(index, BlockKind::Tool) {
                        self.flush_pending(&mut events);
                        events.push(event);
                    }
                    if let Some(ContentBlock::ToolCall { id, name, .. }) =
                        self.output.content.get_mut(index)
                    {
                        *id = tool_use
                            .get("toolUseId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        *name = tool_use
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    self.tool_json.insert(index, String::new());
                }
            }
            Some("contentBlockDelta") => {
                let index = payload
                    .get("contentBlockIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let delta = payload.get("delta").cloned().unwrap_or_default();
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    if !self.blocks.contains_key(&index)
                        && let Some(event) = self.open_block(index, BlockKind::Text)
                    {
                        self.flush_pending(&mut events);
                        events.push(event);
                    }
                    if let Some(ContentBlock::Text { text: t, .. }) =
                        self.output.content.get_mut(index)
                    {
                        t.push_str(text);
                    }
                    self.offer_delta(
                        &mut events,
                        crate::api::DeltaKind::Text,
                        index,
                        text.to_string(),
                    );
                } else if let Some(input) = delta.pointer("/toolUse/input").and_then(Value::as_str)
                {
                    if !self.blocks.contains_key(&index)
                        && let Some(event) = self.open_block(index, BlockKind::Tool)
                    {
                        events.push(event);
                    }
                    self.tool_json.entry(index).or_default().push_str(input);
                    // Defer the O(accumulated) streaming re-parse to the
                    // coalescer window: parse only when a merged delta event
                    // is about to go out (close_block re-parses the final
                    // arguments), not per tiny delta.
                    if self.coalescer.would_flush(
                        crate::api::DeltaKind::ToolCall,
                        index,
                        input.len(),
                    ) && let Some(ContentBlock::ToolCall { arguments, .. }) =
                        self.output.content.get_mut(index)
                    {
                        *arguments = crate::json_repair::parse_streaming_json(
                            self.tool_json.get(&index).map(String::as_str).unwrap_or(""),
                        );
                    }
                    self.offer_delta(
                        &mut events,
                        crate::api::DeltaKind::ToolCall,
                        index,
                        input.to_string(),
                    );
                } else if let Some(reasoning) = delta.get("reasoningContent") {
                    if !self.blocks.contains_key(&index)
                        && let Some(event) = self.open_block(index, BlockKind::Thinking)
                    {
                        self.flush_pending(&mut events);
                        events.push(event);
                    }
                    if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                        if let Some(ContentBlock::Thinking { thinking, .. }) =
                            self.output.content.get_mut(index)
                        {
                            thinking.push_str(text);
                        }
                        self.offer_delta(
                            &mut events,
                            crate::api::DeltaKind::Thinking,
                            index,
                            text.to_string(),
                        );
                    }
                    if let Some(signature) = reasoning.get("signature").and_then(Value::as_str)
                        && !self.redacted_chunks.contains_key(&index)
                    {
                        self.thinking_signatures
                            .entry(index)
                            .or_default()
                            .push_str(signature);
                    }
                    // Encrypted reasoning from non-Anthropic models on Bedrock
                    // (e.g. OpenAI GPT-5.6): the payload is opaque, so keep it
                    // verbatim in `thinking_signature` the way the Anthropic
                    // path stores redacted thinking, and replay it next turn.
                    // On the wire the blob is a base64 string.
                    if let Some(redacted) = reasoning.get("redactedContent").and_then(Value::as_str)
                        && !redacted.is_empty()
                        && let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(redacted)
                        && !bytes.is_empty()
                    {
                        let first_chunk = !self.redacted_chunks.contains_key(&index);
                        self.redacted_chunks.entry(index).or_default().push(bytes);
                        if first_chunk {
                            self.thinking_signatures.remove(&index);
                            if let Some(ContentBlock::Thinking {
                                thinking,
                                redacted: flag,
                                ..
                            }) = self.output.content.get_mut(index)
                            {
                                *flag = Some(true);
                                thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                            }
                            self.offer_delta(
                                &mut events,
                                crate::api::DeltaKind::Thinking,
                                index,
                                REDACTED_THINKING_PLACEHOLDER.to_string(),
                            );
                        }
                    }
                }
            }
            Some("contentBlockStop") => {
                let index = payload
                    .get("contentBlockIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if let Some(event) = self.close_block(index) {
                    self.flush_pending(&mut events);
                    events.push(event);
                }
            }
            Some("messageStop") => {
                let reason = payload
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.output.raw_stop_reason = Some(reason.to_string());
                let (stop_reason, error_message) = map_stop_reason(reason);
                self.output.stop_reason = stop_reason;
                self.output.error_message = error_message;
            }
            Some("metadata") => {
                if let Some(usage) = payload.get("usage") {
                    self.output.usage.input = usage
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.output.usage.output = usage
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.output.usage.cache_read = usage
                        .get("cacheReadInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.output.usage.cache_write = usage
                        .get("cacheWriteInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    // One-hour cache writes price differently (TS #9457):
                    // sum cacheDetails entries with ttl == "1h".
                    let write_1h: u64 = usage
                        .get("cacheDetails")
                        .and_then(Value::as_array)
                        .map(|details| {
                            details
                                .iter()
                                .filter(|d| d.get("ttl").and_then(Value::as_str) == Some("1h"))
                                .map(|d| d.get("inputTokens").and_then(Value::as_u64).unwrap_or(0))
                                .sum()
                        })
                        .unwrap_or(0);
                    self.output.usage.cache_write_1h = (write_1h > 0).then_some(write_1h);
                    self.output.usage.total_tokens = usage
                        .get("totalTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| {
                            self.output.usage.input
                                + self.output.usage.output
                                + self.output.usage.cache_read
                                + self.output.usage.cache_write
                        });
                    // Cost is applied by the caller (needs the model).
                }
            }
            _ => {}
        }
        Ok(events)
    }
}

/// Map ConverseStream exception names to stable prefixes (retry classifier
/// matches these, per TS).
fn format_exception(code: &str, detail: &str) -> String {
    let prefix = match code {
        "internalServerException" => "Internal server error",
        "modelStreamErrorException" => "Model stream error",
        "validationException" => "Validation error",
        "throttlingException" => "Throttling error",
        "serviceUnavailableException" => "Service unavailable",
        other => other,
    };
    format!("Bedrock {prefix}: {detail}")
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    let cancel: CancellationToken = options.cancel.clone();
    let mut output = AssistantMessage::pending(&model);

    let auth = match credentials::resolve_from_process(options.api_key.as_deref()).await {
        Ok(auth) => auth,
        Err(e) => fail!(output, sender, e, false),
    };

    let region = resolve_region(&model);
    let endpoint = resolve_endpoint(&model, &region);
    let encoded_id = sigv4::aws_uri_encode(&model.id, true);
    let url = format!("{endpoint}/model/{encoded_id}/converse-stream");
    let host = reqwest::Url::parse(&endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let path = format!("/model/{encoded_id}/converse-stream");
    let body = build_request_body(&model, &context, &options);
    let body_bytes = body.to_string().into_bytes();

    let client = crate::api::http_client();
    let build_request = || {
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .header("user-agent", TACK_USER_AGENT);
        match &auth {
            BedrockAuth::Bearer(token) => {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            BedrockAuth::SigV4(creds) => {
                use sha2::Digest;
                let payload_hash = sha2::Sha256::digest(&body_bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let mut headers = std::collections::BTreeMap::new();
                headers.insert("content-type".to_string(), "application/json".to_string());
                headers.insert(
                    "accept".to_string(),
                    "application/vnd.amazon.eventstream".to_string(),
                );
                headers.insert("x-amz-content-sha256".to_string(), payload_hash);
                let params = sigv4::SignParams {
                    method: "POST".to_string(),
                    host: host.clone(),
                    path: path.clone(),
                    query: Vec::new(),
                    region: region.clone(),
                    service: "bedrock".to_string(),
                    headers,
                    payload: body_bytes.clone(),
                    timestamp: crate::types::now_millis() / 1000,
                };
                for (k, v) in sigv4::sign(&params, creds) {
                    if k != "host" {
                        request = request.header(k, v);
                    }
                }
            }
        }
        if let Some(model_headers) = &model.headers {
            for (k, v) in model_headers {
                request = request.header(k, v);
            }
        }
        for (k, v) in &options.headers {
            request = request.header(k, v);
        }
        request.body(body_bytes.clone())
    };

    let response = match crate::api::send_with_retry(build_request, &cancel, &body).await {
        Ok(r) => r,
        Err(e) => {
            let aborted = cancel.is_cancelled() || e.is_aborted();
            fail!(output, sender, format!("Bedrock API error: {e}"), aborted);
        }
    };

    let mut state = StreamState::new(&model);
    let mut decoder = EventStreamDecoder::new();
    let mut stream_error: Option<crate::api::ApiError> = None;
    let mut byte_stream = response.bytes_stream();

    'outer: loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                stream_error = Some(crate::api::ApiError::Aborted);
                None
            }
            item = byte_stream.next() => item,
        };
        let Some(item) = next else { break };
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                stream_error = Some(crate::api::ApiError::Failed(format!(
                    "Bedrock stream error: {e}"
                )));
                break;
            }
        };
        let frames = match decoder.feed(&chunk) {
            Ok(f) => f,
            Err(e) => {
                stream_error = Some(crate::api::ApiError::Failed(format!(
                    "Bedrock event-stream decode error: {e}"
                )));
                break;
            }
        };
        for frame in frames {
            match state.handle_frame(&frame) {
                Ok(events) => {
                    for event in events {
                        let _ = sender.push(event);
                    }
                }
                Err(e) => {
                    stream_error = Some(crate::api::ApiError::Failed(e));
                    break 'outer;
                }
            }
        }
    }

    // Close any open blocks before the terminal event.
    let open: Vec<usize> = state.blocks.keys().copied().collect();
    for index in open {
        if let Some(event) = state.close_block(index) {
            let mut flushed = Vec::new();
            state.flush_pending(&mut flushed);
            for ev in flushed {
                let _ = sender.push(ev);
            }
            let _ = sender.push(event);
        }
    }
    {
        let mut flushed = Vec::new();
        state.flush_pending(&mut flushed);
        for ev in flushed {
            let _ = sender.push(ev);
        }
    }
    output = state.output;
    calculate_cost(&model, &mut output.usage);

    if let Some(error) = stream_error {
        let aborted = cancel.is_cancelled() || error.is_aborted();
        fail!(output, sender, error.to_string(), aborted);
    }
    if output.stop_reason == StopReason::Pending {
        fail!(
            output,
            sender,
            "Bedrock stream ended without a messageStop event".to_string(),
            false
        );
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        let message = output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".into());
        let aborted = output.stop_reason == StopReason::Aborted;
        fail!(output, sender, message, aborted);
    }

    sender.finish(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output,
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::api::bedrock::eventstream::build_frame;

    fn test_model() -> Model {
        Model {
            id: "us.anthropic.claude-sonnet-4-5".into(),
            name: "Claude".into(),
            api: "bedrock-converse-stream".into(),
            provider: "amazon-bedrock".into(),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: crate::types::ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn event_frame(event_type: &str, payload: &str) -> Frame {
        let bytes = build_frame(
            &[
                (":message-type", "event"),
                (":event-type", event_type),
                (":content-type", "application/json"),
            ],
            payload.as_bytes(),
        );
        let mut decoder = EventStreamDecoder::new();
        decoder.feed(&bytes).unwrap().remove(0)
    }

    /// Regression: close_block used to `mem::take` text/thinking out of the
    /// accumulator, leaving the final message with empty content blocks.
    #[test]
    fn closed_blocks_keep_their_content() {
        let mut state = StreamState::new(&test_model());
        for frame in [
            event_frame("messageStart", r#"{"role":"assistant"}"#),
            event_frame(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
            ),
            event_frame(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"text":" world"}}"#,
            ),
            event_frame("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event_frame(
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"hmm"}}}"#,
            ),
            event_frame(
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"signature":"sig1"}}}"#,
            ),
            event_frame("contentBlockStop", r#"{"contentBlockIndex":1}"#),
            event_frame(
                "metadata",
                r#"{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15}}"#,
            ),
            event_frame("messageStop", r#"{"stopReason":"end_turn"}"#),
        ] {
            state.handle_frame(&frame).unwrap();
        }
        // Close anything still open (as run() does before the terminal event).
        let open: Vec<usize> = state.blocks.keys().copied().collect();
        for index in open {
            state.close_block(index);
        }
        let output = state.output;
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.content[0], ContentBlock::text("Hello world"));
        match &output.content[1] {
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                ..
            } => {
                assert_eq!(thinking, "hmm");
                assert_eq!(thinking_signature.as_deref(), Some("sig1"));
            }
            other => panic!("expected thinking block: {other:?}"),
        }
        assert_eq!(output.usage.input, 10);
        assert_eq!(output.usage.output, 5);
        assert_eq!(output.usage.total_tokens, 15);
    }

    #[test]
    fn tool_use_blocks_accumulate_and_parse_input() {
        let mut state = StreamState::new(&test_model());
        for frame in [
            event_frame(
                "contentBlockStart",
                r#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tu_1","name":"read"}}}"#,
            ),
            event_frame(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"path\":"}}}"#,
            ),
            event_frame(
                "contentBlockDelta",
                r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"\"a.txt\"}"}}}"#,
            ),
            event_frame("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        ] {
            state.handle_frame(&frame).unwrap();
        }
        match &state.output.content[0] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "read");
                assert_eq!(*arguments, serde_json::json!({ "path": "a.txt" }));
            }
            other => panic!("expected tool call: {other:?}"),
        }
    }

    /// Regression for TS #8314: encrypted reasoning from non-Anthropic
    /// models (e.g. OpenAI GPT-5.6) arrives as opaque
    /// `reasoningContent.redactedContent` and must be preserved in
    /// `thinking_signature` with `redacted: true`, not dropped.
    #[test]
    fn redacted_reasoning_content_is_preserved() {
        let redacted_base64 = "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF";
        // Split across deltas at a non-quantum boundary: chunks are decoded
        // and re-encoded, so the signature must equal the original base64.
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(redacted_base64)
            .unwrap();
        let (head, tail) = bytes.split_at(7);
        let head64 = base64::engine::general_purpose::STANDARD.encode(head);
        let tail64 = base64::engine::general_purpose::STANDARD.encode(tail);

        let mut state = StreamState::new(&test_model());
        for frame in [
            event_frame("messageStart", r#"{"role":"assistant"}"#),
            event_frame(
                "contentBlockDelta",
                &format!(
                    r#"{{"contentBlockIndex":0,"delta":{{"reasoningContent":{{"redactedContent":"{head64}"}}}}}}"#
                ),
            ),
            event_frame(
                "contentBlockDelta",
                &format!(
                    r#"{{"contentBlockIndex":0,"delta":{{"reasoningContent":{{"redactedContent":"{tail64}"}}}}}}"#
                ),
            ),
            event_frame("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event_frame(
                "contentBlockDelta",
                r#"{"contentBlockIndex":1,"delta":{"text":"done"}}"#,
            ),
            event_frame("contentBlockStop", r#"{"contentBlockIndex":1}"#),
            event_frame("messageStop", r#"{"stopReason":"end_turn"}"#),
        ] {
            state.handle_frame(&frame).unwrap();
        }
        let open: Vec<usize> = state.blocks.keys().copied().collect();
        for index in open {
            state.close_block(index);
        }
        let output = state.output;
        assert_eq!(output.stop_reason, StopReason::Stop);
        match &output.content[0] {
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                redacted,
            } => {
                assert_eq!(*redacted, Some(true));
                assert_eq!(thinking_signature.as_deref(), Some(redacted_base64));
                // The placeholder marks the block once, not once per delta.
                assert_eq!(thinking, "[Reasoning redacted]");
            }
            other => panic!("expected thinking block: {other:?}"),
        }
        assert_eq!(output.content[1], ContentBlock::text("done"));
        // Scratch state must not survive into the persisted message.
        assert!(state.redacted_chunks.is_empty());
    }

    /// A stream can settle without a contentBlockStop; close_block (run()
    /// closes all open blocks before the terminal event) must still flush.
    #[test]
    fn redacted_reasoning_flushes_without_block_stop() {
        let redacted_base64 = "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF";
        let mut state = StreamState::new(&test_model());
        for frame in [
            event_frame("messageStart", r#"{"role":"assistant"}"#),
            event_frame(
                "contentBlockDelta",
                &format!(
                    r#"{{"contentBlockIndex":0,"delta":{{"reasoningContent":{{"redactedContent":"{redacted_base64}"}}}}}}"#
                ),
            ),
            event_frame("messageStop", r#"{"stopReason":"end_turn"}"#),
        ] {
            state.handle_frame(&frame).unwrap();
        }
        let open: Vec<usize> = state.blocks.keys().copied().collect();
        for index in open {
            state.close_block(index);
        }
        match &state.output.content[0] {
            ContentBlock::Thinking {
                thinking_signature,
                redacted,
                ..
            } => {
                assert_eq!(*redacted, Some(true));
                assert_eq!(thinking_signature.as_deref(), Some(redacted_base64));
            }
            other => panic!("expected thinking block: {other:?}"),
        }
        assert!(state.redacted_chunks.is_empty());
    }

    #[test]
    fn exception_frames_become_errors() {
        let mut state = StreamState::new(&test_model());
        let bytes = build_frame(
            &[
                (":message-type", "exception"),
                (":error-code", "throttlingException"),
                (":error-message", "slow down"),
            ],
            b"{}",
        );
        let mut decoder = EventStreamDecoder::new();
        let frame = decoder.feed(&bytes).unwrap().remove(0);
        let err = state.handle_frame(&frame).unwrap_err();
        assert_eq!(err, "Bedrock Throttling error: slow down");
    }
}
