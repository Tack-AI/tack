//! tack-messages adapter. Port of upstream's `pi-messages.ts` (the protocol's
//! original name): streams pi's own message
//! protocol to a backend — the request is a single POST of
//! `{ model, context, options }` to `<baseUrl>/messages`, the response is an
//! SSE stream of serialized assistant-message events plus a terminal
//! `done`/`error` event. This is the wire protocol spoken by the Radius
//! gateway, but any backend implementing it can be used via a models.json
//! custom provider with `"api": "tack-messages"` (canonical; the legacy
//! `"pi-messages"` alias is accepted and normalized at load).

use std::collections::HashMap;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::fail;
use crate::json_repair::parse_streaming_json;
use crate::provider::StreamOptions;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventSender};
use crate::types::{AssistantMessage, ContentBlock, Context, Model, StopReason, Usage};

const TACK_USER_AGENT: &str = concat!("tack/", env!("CARGO_PKG_VERSION"));
/// Default Radius gateway when neither the model nor the env specifies one.
const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

#[derive(Clone, Debug, Default)]
pub struct TackMessagesProvider {
    /// Endpoint path appended to the base URL: "/messages" for the radius
    /// gateway, "/api/stream" for generic proxy servers (agent/proxy.ts).
    pub endpoint: &'static str,
}

impl TackMessagesProvider {
    /// Radius gateway flavor (with /v1/config base-url discovery).
    pub fn radius() -> Self {
        TackMessagesProvider {
            endpoint: "/messages",
        }
    }

    /// Generic proxy flavor (agent/proxy.ts; baseUrl required on the model).
    pub fn proxy() -> Self {
        TackMessagesProvider {
            endpoint: "/api/stream",
        }
    }
}

impl crate::provider::Provider for TackMessagesProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> crate::stream::AssistantMessageEventStream {
        let (sender, stream) = crate::stream::event_stream();
        let model = model.clone();
        let context = context.clone();
        let endpoint = self.endpoint;
        tokio::spawn(async move {
            run(model, context, options, sender, endpoint).await;
        });
        stream
    }
}

/// Radius registry entries ship an empty base_url; discover the serving
/// baseUrl from the gateway's `/v1/config` (cached per gateway).
async fn resolve_base_url(model: &Model, api_key: Option<&str>) -> Result<String, String> {
    let base = model.base_url.trim_end_matches('/');
    if !base.is_empty() {
        return Ok(base.to_string());
    }
    let gateway = std::env::var("RADIUS_GATEWAY_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RADIUS_GATEWAY.to_string());
    let gateway = gateway.trim_end_matches('/').to_string();

    // Only successful discoveries are cached: caching a transient Err
    // (gateway restart, network hiccup) would poison base-url resolution
    // for the rest of the process.
    static CACHE: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    if let Some(cached) = cache.lock().await.get(&gateway) {
        return Ok(cached.clone());
    }

    let client = crate::api::http_client();
    let mut request = client
        .get(format!("{gateway}/v1/config"))
        .header("accept", "application/json")
        .header("user-agent", TACK_USER_AGENT);
    if let Some(key) = api_key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let result = async {
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{}: gateway /v1/config failed", response.status()));
        }
        let body: Value = response.json().await.map_err(|e| e.to_string())?;
        body.get("baseUrl")
            .and_then(Value::as_str)
            .map(|s| s.trim_end_matches('/').to_string())
            .ok_or_else(|| "gateway /v1/config has no baseUrl".to_string())
    }
    .await;
    if let Ok(base_url) = &result {
        cache.lock().await.insert(gateway, base_url.clone());
    }
    result
}

/// Format a non-2xx body per the pi-messages error contract:
/// `{status}: {error.message | body} ({error.code})`.
fn format_error_body(status: &str, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .filter(|e| e.is_object());
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(body);
    let code = error.and_then(|e| e.get("code")).and_then(Value::as_str);
    match code {
        Some(code) => format!("{status}: {message} ({code})"),
        None => format!("{status}: {message}"),
    }
}

fn build_payload(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut opts = serde_json::Map::new();
    if let Some(temperature) = options.temperature {
        opts.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        opts.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(reasoning) = options.reasoning {
        opts.insert("reasoning".into(), json!(reasoning.as_str()));
    }
    // Backend defaults apply when unset; an explicit retention always passes
    // through, and the legacy env opt-in maps to "long".
    let cache_retention = match options.cache_retention {
        Some(crate::provider::CacheRetention::Long) => Some("long"),
        Some(crate::provider::CacheRetention::Short) => Some("short"),
        Some(crate::provider::CacheRetention::None) => Some("none"),
        None => {
            (std::env::var("TACK_CACHE_RETENTION").is_ok_and(|v| v == "long")).then_some("long")
        }
    };
    if let Some(retention) = cache_retention {
        opts.insert("cacheRetention".into(), json!(retention));
    }
    if let Some(session_id) = &options.session_id {
        opts.insert("sessionId".into(), json!(session_id));
    }
    if let Some(choice) = options.tool_choice {
        opts.insert(
            "toolChoice".into(),
            json!(match choice {
                crate::provider::ToolChoice::Auto => "auto",
                crate::provider::ToolChoice::None => "none",
            }),
        );
    }
    json!({
        "model": model.id,
        "context": context,
        "options": Value::Object(opts),
    })
}

fn map_done_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Stop,
        "length" => StopReason::Length,
        "toolUse" => StopReason::ToolUse,
        "aborted" => StopReason::Aborted,
        _ => StopReason::Error,
    }
}

/// Event converter: the SSE vocabulary IS pi's AssistantMessageEvent
/// protocol, so this mostly mutates the partial message.
struct Converter {
    partial: AssistantMessage,
    tool_json: HashMap<usize, String>,
    coalescer: crate::api::DeltaCoalescer,
}

impl Converter {
    fn new(model: &Model) -> Self {
        Converter {
            partial: AssistantMessage::pending(model),
            tool_json: HashMap::new(),
            coalescer: crate::api::DeltaCoalescer::new(),
        }
    }

    /// Forward a structural/terminal event, flushing any buffered delta
    /// window first so block boundaries keep their order.
    fn emit(&mut self, event: AssistantMessageEvent) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();
        if let Some(ev) = self.coalescer.flush(&self.partial) {
            out.push(ev);
        }
        out.push(event);
        out
    }

    /// Flush a trailing delta window (end of stream / error paths).
    fn drain(&mut self) -> Option<AssistantMessageEvent> {
        self.coalescer.flush(&self.partial)
    }

    /// Returns the events to forward (0..=2: a flushed delta window plus
    /// the structural event). Deltas are coalesced — see DeltaCoalescer.
    fn convert(&mut self, event: &Value) -> Vec<AssistantMessageEvent> {
        let Some(ty) = event.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        let index = |key: &str| event.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
        match ty {
            "done" | "error" => {
                let reason = map_done_reason(
                    event
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("error"),
                );
                if let Ok(usage) =
                    serde_json::from_value::<Usage>(event.get("usage").cloned().unwrap_or_default())
                {
                    self.partial.usage = usage;
                }
                self.partial.response_id = event
                    .get("responseId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.partial.stop_reason = reason;
                if ty == "error" {
                    self.partial.error_message = event
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    return self.emit(AssistantMessageEvent::Error {
                        reason,
                        error: self.partial.clone(),
                    });
                }
                self.emit(AssistantMessageEvent::Done {
                    reason,
                    message: self.partial.clone(),
                })
            }
            "start" => self.emit(AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            }),
            "text_start" => {
                let i = index("contentIndex");
                set_block(
                    &mut self.partial,
                    i,
                    ContentBlock::Text {
                        text: String::new(),
                        text_signature: None,
                    },
                );
                self.emit(AssistantMessageEvent::TextStart {
                    content_index: i,
                    partial: self.partial.clone(),
                })
            }
            "text_delta" => {
                let i = index("contentIndex");
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(ContentBlock::Text { text, .. }) = self.partial.content.get_mut(i) {
                    text.push_str(&delta);
                }
                self.coalescer
                    .offer(crate::api::DeltaKind::Text, i, delta, &self.partial)
                    .into_iter()
                    .collect()
            }
            "text_end" => {
                let i = index("contentIndex");
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(ContentBlock::Text {
                    text,
                    text_signature,
                }) = self.partial.content.get_mut(i)
                {
                    *text = content.clone();
                    *text_signature = event
                        .get("contentSignature")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                self.emit(AssistantMessageEvent::TextEnd {
                    content_index: i,
                    content,
                    partial: self.partial.clone(),
                })
            }
            "thinking_start" => {
                let i = index("contentIndex");
                set_block(
                    &mut self.partial,
                    i,
                    ContentBlock::Thinking {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    },
                );
                self.emit(AssistantMessageEvent::ThinkingStart {
                    content_index: i,
                    partial: self.partial.clone(),
                })
            }
            "thinking_delta" => {
                let i = index("contentIndex");
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(ContentBlock::Thinking { thinking, .. }) =
                    self.partial.content.get_mut(i)
                {
                    thinking.push_str(&delta);
                }
                self.coalescer
                    .offer(crate::api::DeltaKind::Thinking, i, delta, &self.partial)
                    .into_iter()
                    .collect()
            }
            "thinking_end" => {
                let i = index("contentIndex");
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(ContentBlock::Thinking {
                    thinking,
                    thinking_signature,
                    redacted,
                }) = self.partial.content.get_mut(i)
                {
                    *thinking = content.clone();
                    *thinking_signature = event
                        .get("contentSignature")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    *redacted = event.get("redacted").and_then(Value::as_bool);
                }
                self.emit(AssistantMessageEvent::ThinkingEnd {
                    content_index: i,
                    content,
                    partial: self.partial.clone(),
                })
            }
            "toolcall_start" => {
                let i = index("contentIndex");
                set_block(
                    &mut self.partial,
                    i,
                    ContentBlock::ToolCall {
                        id: event
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: event
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                        namespace: None,
                    },
                );
                self.tool_json.insert(i, String::new());
                self.emit(AssistantMessageEvent::ToolCallStart {
                    content_index: i,
                    partial: self.partial.clone(),
                })
            }
            "toolcall_delta" => {
                let i = index("contentIndex");
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let accumulated = self.tool_json.entry(i).or_default();
                accumulated.push_str(&delta);
                // Defer the O(accumulated) streaming re-parse to the
                // coalescer window: parse only when a merged delta event is
                // about to go out (toolcall_end supplies the final
                // arguments), not per tiny delta.
                if self
                    .coalescer
                    .would_flush(crate::api::DeltaKind::ToolCall, i, delta.len())
                {
                    let parsed = parse_streaming_json(accumulated);
                    if let Some(ContentBlock::ToolCall { arguments, .. }) =
                        self.partial.content.get_mut(i)
                    {
                        *arguments = parsed;
                    }
                }
                self.coalescer
                    .offer(crate::api::DeltaKind::ToolCall, i, delta, &self.partial)
                    .into_iter()
                    .collect()
            }
            "toolcall_end" => {
                let i = index("contentIndex");
                if let Some(tool_call) = event.get("toolCall") {
                    let id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = tool_call.get("name").and_then(Value::as_str).unwrap_or("");
                    let arguments = tool_call
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    if let Some(ContentBlock::ToolCall {
                        id: bid,
                        name: bname,
                        arguments: bargs,
                        ..
                    }) = self.partial.content.get_mut(i)
                    {
                        *bid = id.to_string();
                        *bname = name.to_string();
                        *bargs = arguments;
                    }
                }
                self.tool_json.remove(&i);
                let Some(block) = self.partial.content.get(i).cloned() else {
                    return Vec::new();
                };
                self.emit(AssistantMessageEvent::ToolCallEnd {
                    content_index: i,
                    tool_call: block,
                    partial: self.partial.clone(),
                })
            }
            _ => Vec::new(),
        }
    }
}

/// Upper bound for server-supplied content block indices. A malformed or
/// malicious peer could send a huge `contentIndex`; without a cap,
/// `resize_with(index + 1, ...)` would panic (capacity overflow) or OOM.
const MAX_CONTENT_BLOCKS: usize = 4096;

fn set_block(message: &mut AssistantMessage, index: usize, block: ContentBlock) {
    if index >= MAX_CONTENT_BLOCKS {
        tracing::warn!(index, "ignoring content block with out-of-range index");
        return;
    }
    if index >= message.content.len() {
        message
            .content
            .resize_with(index + 1, || ContentBlock::Text {
                text: String::new(),
                text_signature: None,
            });
    }
    message.content[index] = block;
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
    endpoint: &str,
) {
    let cancel: CancellationToken = options.cancel.clone();
    let mut output = AssistantMessage::pending(&model);

    let Some(api_key) = options.api_key.clone() else {
        fail!(
            output,
            sender,
            format!("No API key for provider: {}", model.provider),
            false
        );
    };

    let base_url = if endpoint == "/api/stream" {
        // Generic proxy (agent/proxy.ts): the model's baseUrl is the proxy
        // server itself — no gateway config discovery.
        let base = model.base_url.trim_end_matches('/');
        if base.is_empty() {
            fail!(
                output,
                sender,
                format!("proxy provider {} needs a baseUrl", model.provider),
                false
            );
        }
        base.to_string()
    } else {
        match resolve_base_url(&model, Some(&api_key)).await {
            Ok(url) => url,
            Err(e) => {
                fail!(
                    output,
                    sender,
                    format!("{} base URL resolution failed: {e}", model.provider),
                    false
                );
            }
        }
    };
    let url = format!("{base_url}{endpoint}");
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
            // Reformat per the pi-messages error contract when the body is a
            // structured error ("{status}: {body}" from send_with_retry).
            let e = e.to_string();
            let message = match e.split_once(": ") {
                Some((status, body))
                    if status
                        .split(' ')
                        .next()
                        .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()))
                        && body.trim_start().starts_with('{') =>
                {
                    format!(
                        "{} API error: {}",
                        model.provider,
                        format_error_body(status, body)
                    )
                }
                _ => format!("{} API error: {e}", model.provider),
            };
            fail!(output, sender, message, aborted);
        }
    };

    let mut converter = Converter::new(&model);
    let mut stream_error: Option<crate::api::ApiError> = None;

    let mut sse = crate::api::SseStream::new(response.bytes_stream(), cancel.clone());
    loop {
        let chunk: Value = match sse.next_json("tack-messages SSE event").await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };
        for out_event in converter.convert(&chunk) {
            if matches!(
                out_event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ) {
                sender.finish(out_event);
                return;
            }
            let _ = sender.push(out_event);
        }
    }
    if let Some(ev) = converter.drain() {
        let _ = sender.push(ev);
    }

    // Mid-stream failures must carry everything accumulated so far: the
    // local `output` was never mutated (the converter owns the partial
    // message), so hand it over before failing like the peer adapters do.
    output = converter.partial;

    if let Some(error) = stream_error {
        let aborted = cancel.is_cancelled() || error.is_aborted();
        fail!(output, sender, error.to_string(), aborted);
    }
    fail!(
        output,
        sender,
        format!("{} stream ended without a terminal event", model.provider),
        false
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::provider::CacheRetention;

    fn test_model() -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "tack-messages".into(),
            provider: "radius".into(),
            base_url: "https://example.com".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::types::InputKind::Text],
            cost: Default::default(),
            context_window: 1,
            max_tokens: 1,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn context() -> Context {
        Context {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        }
    }

    /// Explicit cache retention always reaches the backend (TS passes the
    /// value through verbatim); only an unset option consults the legacy env.
    #[test]
    fn explicit_cache_retention_passes_through() {
        let model = test_model();
        for (retention, expected) in [
            (CacheRetention::None, "none"),
            (CacheRetention::Short, "short"),
            (CacheRetention::Long, "long"),
        ] {
            let options = StreamOptions {
                cache_retention: Some(retention),
                ..Default::default()
            };
            let payload = build_payload(&model, &context(), &options);
            assert_eq!(payload["options"]["cacheRetention"], json!(expected));
        }
        let options = StreamOptions::default();
        let payload = build_payload(&model, &context(), &options);
        assert!(payload["options"].get("cacheRetention").is_none());
    }

    #[test]
    fn converter_accumulates_text_and_tool_calls() {
        let model = test_model();
        let mut converter = Converter::new(&model);
        let events = [
            json!({ "type": "start" }),
            json!({ "type": "text_start", "contentIndex": 0 }),
            json!({ "type": "text_delta", "contentIndex": 0, "delta": "Hel" }),
            json!({ "type": "text_delta", "contentIndex": 0, "delta": "lo" }),
            json!({ "type": "text_end", "contentIndex": 0, "content": "Hello", "contentSignature": "sig" }),
            json!({ "type": "toolcall_start", "contentIndex": 1, "id": "c1", "toolName": "read" }),
            json!({ "type": "toolcall_delta", "contentIndex": 1, "delta": "{\"path\":" }),
            json!({ "type": "toolcall_delta", "contentIndex": 1, "delta": "\"x\"}" }),
            json!({ "type": "toolcall_end", "contentIndex": 1, "toolCall": { "id": "c1", "name": "read", "arguments": { "path": "x" } } }),
            json!({
                "type": "done",
                "reason": "toolUse",
                "usage": {
                    "input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 3,
                    "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
                },
                "responseId": "r1"
            }),
        ];
        let mut terminal = None;
        for event in &events {
            for e in converter.convert(event) {
                if e.is_terminal() {
                    terminal = Some(e);
                }
            }
        }
        let Some(AssistantMessageEvent::Done { reason, message }) = terminal else {
            panic!("expected done: {terminal:?}")
        };
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(message.response_id.as_deref(), Some("r1"));
        assert_eq!(message.usage.input, 1);
        assert_eq!(message.text(), "Hello");
        match &message.content[0] {
            ContentBlock::Text { text_signature, .. } => {
                assert_eq!(text_signature.as_deref(), Some("sig"))
            }
            other => panic!("text: {other:?}"),
        }
        match &message.content[1] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!((id.as_str(), name.as_str()), ("c1", "read"));
                assert_eq!(*arguments, json!({ "path": "x" }));
            }
            other => panic!("tool call: {other:?}"),
        }
    }

    #[test]
    fn converter_maps_error_event() {
        let model = test_model();
        let mut converter = Converter::new(&model);
        let out = converter
            .convert(&json!({ "type": "error", "reason": "error", "errorMessage": "boom" }))
            .into_iter()
            .last()
            .expect("error event");
        let AssistantMessageEvent::Error { reason, error } = out else {
            panic!("expected error: {out:?}")
        };
        assert_eq!(reason, StopReason::Error);
        assert_eq!(error.error_message.as_deref(), Some("boom"));
    }

    /// A malformed/malicious server can send a huge `contentIndex`; the
    /// converter must ignore it instead of trying to resize content to
    /// `index + 1` (capacity-overflow panic / OOM).
    #[test]
    fn converter_ignores_huge_content_index() {
        let model = test_model();
        let mut converter = Converter::new(&model);
        converter.convert(&json!({ "type": "start" }));
        converter.convert(&json!({ "type": "text_start", "contentIndex": 0 }));
        // u64::MAX would overflow `index + 1` in `resize_with`.
        converter.convert(&json!({ "type": "text_start", "contentIndex": u64::MAX }));
        converter.convert(&json!({ "type": "text_delta", "contentIndex": u64::MAX, "delta": "x" }));
        // Content stays bounded; the legit block is untouched.
        assert_eq!(converter.partial.content.len(), 1);
    }
}
