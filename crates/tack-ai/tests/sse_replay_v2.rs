//! SSE fixture replay tests for the Responses/Google/Mistral adapters.
#![allow(clippy::unwrap_used)]

use serde_json::json;
use tack_ai::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one 200 OK SSE response; returns (base_url, request-body receiver).
async fn serve_once(
    body: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    serve_with_status("200 OK", body).await
}

/// Serve one HTTP response with an explicit status line; returns
/// (base_url, request-body receiver).
async fn serve_with_status(
    status: &'static str,
    body: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut request = String::new();
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            request = String::from_utf8_lossy(&buf).to_string();
            if let Some(head_end) = request.find("\r\n\r\n") {
                let headers = &request[..head_end];
                let content_length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() - (head_end + 4) >= content_length {
                    break;
                }
            }
        }
        let head_end = request.find("\r\n\r\n").unwrap();
        let path = request.lines().next().unwrap_or("").to_string();
        let body_received = request[head_end + 4..].to_string();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = tx.send((path, body_received));
    });
    (format!("http://{addr}"), rx)
}

fn make_model(api: &str, provider: &str, base_url: &str) -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test".to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: ModelCost {
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

fn test_context() -> Context {
    Context {
        system_prompt: Some("You are a test bot.".to_string()),
        messages: vec![Message::user("hi")],
        tools: vec![ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            defer_loading: false,
            constrained_sampling: None,
        }],
    }
}

fn test_options() -> StreamOptions {
    StreamOptions {
        api_key: Some("test-key".to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// OpenAI Responses
// ---------------------------------------------------------------------------

const RESPONSES_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\" world\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\"}]}}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\"}\n\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"\\\"a.rs\\\"}\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":30,\"output_tokens\":12,\"total_tokens\":42,\"input_tokens_details\":{\"cached_tokens\":8},\"output_tokens_details\":{\"reasoning_tokens\":2}}}}\n\n",
);

#[tokio::test]
async fn openai_responses_sse_replay() {
    let (base_url, request_rx) = serve_once(RESPONSES_SSE).await;
    let model = make_model("openai-responses", "openai", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut text = String::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        if let AssistantMessageEvent::TextDelta { delta, .. } = &event {
            text.push_str(delta);
        }
        if event.is_terminal() {
            break;
        }
    }
    let message = stream.result().await;

    assert_eq!(text, "Hello world");
    // completed + tool call present → toolUse.
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(message.usage.input, 22); // 30 - 8 cached
    assert_eq!(message.usage.output, 12);
    assert_eq!(message.usage.cache_read, 8);
    assert_eq!(message.usage.reasoning, Some(2));

    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "call_1|fc_1");
    assert_eq!(calls[0].1, "read");
    assert_eq!(calls[0].2["path"], json!("a.rs"));

    let (path, body) = request_rx.await.unwrap();
    assert!(path.contains("/responses"), "{path}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["model"], json!("test-model"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["input"][0]["role"], json!("system")); // reasoning: false → system role
}

// ---------------------------------------------------------------------------
// Google Generative AI
// ---------------------------------------------------------------------------

const GOOGLE_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]}}],\"responseId\":\"resp-g1\"}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" world\"}]}}]}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a.rs\"}}}]}}]}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":4,\"totalTokenCount\":30}}\n\n",
);

#[tokio::test]
async fn google_sse_replay() {
    let (base_url, request_rx) = serve_once(GOOGLE_SSE).await;
    let model = make_model("google-generative-ai", "google", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut text = String::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        if let AssistantMessageEvent::TextDelta { delta, .. } = &event {
            text.push_str(delta);
        }
        if event.is_terminal() {
            break;
        }
    }
    let message = stream.result().await;

    assert_eq!(text, "Hello world");
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 16);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.output, 10);

    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "read");
    assert_eq!(calls[0].2["path"], json!("a.rs"));

    let (path, body) = request_rx.await.unwrap();
    assert!(path.contains(":streamGenerateContent"), "{path}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["systemInstruction"], json!("You are a test bot."));
    assert_eq!(body["contents"][0]["role"], json!("user"));
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        json!("read")
    );
}

// ---------------------------------------------------------------------------
// Mistral
// ---------------------------------------------------------------------------

const MISTRAL_SSE: &str = concat!(
    "data: {\"id\":\"m1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
    "data: {\"id\":\"m1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"text\":\"hmm\"}]}]}}]}\n\n",
    "data: {\"id\":\"m1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call123ab\",\"index\":0,\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\": \\\"a.rs\\\"}\"}}]}}]}\n\n",
    "data: {\"id\":\"m1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":18,\"completion_tokens\":9,\"total_tokens\":27}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn mistral_sse_replay() {
    let (base_url, request_rx) = serve_once(MISTRAL_SSE).await;
    let model = make_model("mistral-conversations", "mistral", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut text = String::new();
    let mut thinking = String::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(delta),
            AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push_str(delta),
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    let message = stream.result().await;

    assert_eq!(text, "Hello");
    assert_eq!(thinking, "hmm");
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 18);
    assert_eq!(message.usage.output, 9);

    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "read");
    assert_eq!(calls[0].2["path"], json!("a.rs"));

    let (path, body) = request_rx.await.unwrap();
    assert!(path.contains("/v1/chat/completions"), "{path}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["model"], json!("test-model"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("read"));
}

// ---------------------------------------------------------------------------
// OpenAI Responses: [DONE] termination + malformed-event error path
// ---------------------------------------------------------------------------

/// A full valid stream, then `[DONE]`, then more events. The shared
/// `SseStream` must terminate at the `[DONE]` marker (new behavior since the
/// migration): everything after it — including a second `response.completed`
/// with different usage — must be ignored.
const RESPONSES_DONE_MARKER_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_done\"}}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"done text\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"done text\"}]}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_done\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":3,\"total_tokens\":13}}}\n\n",
    "data: [DONE]\n\n",
    // Post-[DONE] traffic: would corrupt the result if still processed.
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_done\",\"status\":\"completed\",\"usage\":{\"input_tokens\":999,\"output_tokens\":999,\"total_tokens\":1998}}}\n\n",
);

#[tokio::test]
async fn openai_responses_done_marker_terminates_stream() {
    let (base_url, _rx) = serve_once(RESPONSES_DONE_MARKER_SSE).await;
    let model = make_model("openai-responses", "openai", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut text = String::new();
    let mut saw_done_event = false;
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(delta),
            AssistantMessageEvent::Done { reason, .. } => {
                saw_done_event = true;
                assert_eq!(*reason, StopReason::Stop);
            }
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    assert!(saw_done_event, "stream must end with Done, not Error");
    let message = stream.result().await;

    assert_eq!(text, "done text");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("resp_done"));
    // Usage from the pre-[DONE] completed event; the trailing 999s are ignored.
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 3);
    assert_eq!(message.usage.total_tokens, 13);
}

/// A malformed JSON data payload terminates the stream with an in-band
/// error; the offending payload embedded in the message is capped at 4000
/// chars (new shared `parse_sse_json` behavior).
#[tokio::test]
async fn openai_responses_malformed_event_error_is_truncated() {
    // 5000-char garbage payload: 4000 retained + "[truncated 1000 chars]".
    let garbage = format!("{{{}", "x".repeat(4999));
    let created = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_bad\"}}\n\n";
    let body: &'static str = Box::leak(format!("{created}data: {garbage}\n\n").into_boxed_str());
    let (base_url, _rx) = serve_once(body).await;
    let model = make_model("openai-responses", "openai", &base_url);
    let provider = provider_for(&model).unwrap();

    let stream = provider.stream(&model, &test_context(), test_options());
    let message = stream.result().await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let err = message.error_message.unwrap();
    assert!(err.starts_with("Could not parse Responses event:"), "{err}");
    assert!(err.contains("... [truncated 1000 chars]"), "{err}");
    // 4000 payload chars + fixed framing + serde message; far below the raw
    // 5000-char payload.
    assert!(
        err.len() < 4400,
        "error embeds too much of the payload: {} chars",
        err.len()
    );
    assert!(!err.contains(&"x".repeat(4001)), "payload not capped");
}

// ---------------------------------------------------------------------------
// Mistral: multi-block stream, usage frame, HTTP + mid-stream errors
// ---------------------------------------------------------------------------

/// Text → thinking → text block transitions from mixed string/array
/// `delta.content`, terminated by a usage frame with cached-token detail.
const MISTRAL_MULTI_BLOCK_SSE: &str = concat!(
    "data: {\"id\":\"m2\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
    "data: {\"id\":\"m2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"hmm\"}]}]}}]}\n\n",
    "data: {\"id\":\"m2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"...\"}]}]}}]}\n\n",
    "data: {\"id\":\"m2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\" world\"}]}}]}\n\n",
    "data: {\"id\":\"m2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":8,\"total_tokens\":28,\"prompt_tokens_details\":{\"cached_tokens\":6}}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn mistral_multi_block_stream_with_usage_frame() {
    let (base_url, _rx) = serve_once(MISTRAL_MULTI_BLOCK_SSE).await;
    let model = make_model("mistral-conversations", "mistral", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut text_starts: Vec<usize> = Vec::new();
    let mut thinking_starts: Vec<usize> = Vec::new();
    let mut thinking = String::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        match &event {
            AssistantMessageEvent::TextStart { content_index, .. } => {
                text_starts.push(*content_index);
            }
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                thinking_starts.push(*content_index);
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push_str(delta),
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    let message = stream.result().await;

    // Three blocks in order: text(0), thinking(1), text(2).
    assert_eq!(text_starts, vec![0, 2]);
    assert_eq!(thinking_starts, vec![1]);
    assert_eq!(thinking, "hmm...");
    assert_eq!(message.text(), "Hello world");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("m2"));
    // Cached tokens are split out of the input count.
    assert_eq!(message.usage.input, 14); // 20 - 6 cached
    assert_eq!(message.usage.cache_read, 6);
    assert_eq!(message.usage.output, 8);
    assert_eq!(message.usage.total_tokens, 28);
}

/// Non-2xx responses surface as in-band Error events (400 is not retried).
#[tokio::test]
async fn mistral_http_error_is_in_band() {
    let (base_url, _rx) = serve_with_status(
        "400 Bad Request",
        "{\"error\":{\"message\":\"invalid model\"}}",
    )
    .await;
    let model = make_model("mistral-conversations", "mistral", &base_url);
    let provider = provider_for(&model).unwrap();

    let stream = provider.stream(&model, &test_context(), test_options());
    let message = stream.result().await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let err = message.error_message.unwrap();
    assert!(err.contains("Mistral API error"), "{err}");
    assert!(err.contains("400"), "{err}");
    assert!(err.contains("invalid model"), "{err}");
}

/// A malformed SSE data event mid-stream terminates with an in-band error;
/// deltas received before the failure are preserved.
#[tokio::test]
async fn mistral_malformed_sse_event_mid_stream() {
    const BODY: &str = concat!(
        "data: {\"id\":\"m3\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"}}]}\n\n",
        "data: {this is not json}\n\n",
    );
    let (base_url, _rx) = serve_once(BODY).await;
    let model = make_model("mistral-conversations", "mistral", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut text = String::new();
    let mut saw_error_event = false;
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(delta),
            AssistantMessageEvent::Error { reason, .. } => {
                saw_error_event = true;
                assert_eq!(*reason, StopReason::Error);
            }
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    assert!(saw_error_event);
    let message = stream.result().await;

    assert_eq!(text, "partial");
    assert_eq!(message.stop_reason, StopReason::Error);
    let err = message.error_message.unwrap();
    assert!(err.contains("Could not parse Mistral SSE event"), "{err}");
}

// ---------------------------------------------------------------------------
// Google: thought/text stream with thoughts usage, HTTP error, no finish
// ---------------------------------------------------------------------------

/// Thinking parts (with a thought signature) followed by text, closed by a
/// STOP frame whose usage splits candidate vs thoughts tokens.
const GOOGLE_THOUGHT_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"thought\":true,\"text\":\"let me think\",\"thoughtSignature\":\"sig_abcd\"}]}}],\"responseId\":\"resp-g2\"}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"thought\":true,\"text\":\" more\"}]}}]}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"the answer\"}]}}]}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":30,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":7,\"cachedContentTokenCount\":10,\"totalTokenCount\":42}}\n\n",
);

#[tokio::test]
async fn google_thought_and_text_stream_with_thoughts_usage() {
    let (base_url, _rx) = serve_once(GOOGLE_THOUGHT_SSE).await;
    let model = make_model("google-generative-ai", "google", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut thinking = String::new();
    let mut text = String::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        match &event {
            AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push_str(delta),
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(delta),
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    let message = stream.result().await;

    assert_eq!(thinking, "let me think more");
    assert_eq!(text, "the answer");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("resp-g2"));
    // The thought signature is captured on the thinking block.
    match &message.content[0] {
        ContentBlock::Thinking {
            thinking_signature, ..
        } => assert_eq!(thinking_signature.as_deref(), Some("sig_abcd")),
        other => panic!("expected thinking block, got {other:?}"),
    }
    // thoughtsTokenCount folds into output + reasoning.
    assert_eq!(message.usage.input, 20); // 30 - 10 cached
    assert_eq!(message.usage.cache_read, 10);
    assert_eq!(message.usage.output, 12); // 5 candidates + 7 thoughts
    assert_eq!(message.usage.reasoning, Some(7));
    assert_eq!(message.usage.total_tokens, 42);
}

/// Non-2xx responses surface as in-band Error events (400 is not retried).
#[tokio::test]
async fn google_http_error_is_in_band() {
    let (base_url, _rx) = serve_with_status(
        "400 Bad Request",
        "{\"error\":{\"message\":\"API key not valid\"}}",
    )
    .await;
    let model = make_model("google-generative-ai", "google", &base_url);
    let provider = provider_for(&model).unwrap();

    let stream = provider.stream(&model, &test_context(), test_options());
    let message = stream.result().await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let err = message.error_message.unwrap();
    assert!(err.contains("Google API error"), "{err}");
    assert!(err.contains("400"), "{err}");
    assert!(err.contains("API key not valid"), "{err}");
}

/// A stream that ends (connection close) without any finishReason frame is
/// an in-band error, not a silent success.
#[tokio::test]
async fn google_stream_without_finish_reason_errors() {
    const BODY: &str = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]}}]}\n\n";
    let (base_url, _rx) = serve_once(BODY).await;
    let model = make_model("google-generative-ai", "google", &base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());
    let mut text = String::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        if let AssistantMessageEvent::TextDelta { delta, .. } = &event {
            text.push_str(delta);
        }
        if event.is_terminal() {
            break;
        }
    }
    let message = stream.result().await;

    assert_eq!(text, "hi");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Google stream ended without a finish reason")
    );
}
