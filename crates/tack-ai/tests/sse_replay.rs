#![allow(clippy::unwrap_used)]
//! SSE fixture replay tests: a tiny local HTTP server replays a recorded
//! provider stream; the adapter must translate it into the pi event protocol.

use serde_json::json;
use tack_ai::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one HTTP response regardless of request; returns the base URL.
async fn serve_once(content_type: &'static str, body: &'static str) -> String {
    serve_once_with_capture(content_type, body).await.0
}

/// Like `serve_once`, but also returns the captured request body.
async fn serve_once_with_capture(
    content_type: &'static str,
    body: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        // Read until end of headers, then keep reading the JSON body per
        // content-length.
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
                        let lower = l.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() - (head_end + 4) >= content_length {
                    break;
                }
            }
        }
        let request_body = request
            .find("\r\n\r\n")
            .map(|i| request[i + 4..].to_string())
            .unwrap_or_default();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = tx.send(request_body);
    });
    (format!("http://{addr}"), rx)
}

fn anthropic_model(base_url: &str) -> Model {
    Model {
        id: "claude-test".to_string(),
        name: "Claude Test".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: None,
        },
        context_window: 200_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn openai_model(base_url: &str) -> Model {
    Model {
        id: "gpt-test".to_string(),
        name: "GPT Test".to_string(),
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
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

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":5}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":12}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Regression test for the per-delta full-message clone (O(deltas x
/// message) copy churn): thousands of tiny SSE deltas must be coalesced
/// into a handful of merged delta events — with zero text loss and block
/// boundaries still flushing in order.
#[tokio::test]
async fn anthropic_many_deltas_are_coalesced_without_loss() {
    const DELTAS: usize = 2000;
    let mut body = String::new();
    body.push_str(
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\n",
    );
    body.push_str(
        "event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\n",
    );
    let mut expected = String::new();
    for i in 0..DELTAS {
        let word = format!("w{i} ");
        expected.push_str(&word);
        body.push_str(&format!(
            "event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{word}\"}}}}\n\n\n",
        ));
    }
    body.push_str(
        "event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\n",
    );
    body.push_str(
        "event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\n",
    );
    body.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n\n");

    let base_url = serve_once("text/event-stream", Box::leak(body.into_boxed_str())).await;
    let model = anthropic_model(&base_url);
    let provider = provider_for(&model).unwrap();
    let mut stream = provider.stream(&model, &test_context(), test_options());

    let mut accumulated = String::new();
    let mut delta_events = 0usize;
    let mut saw_text_start = false;
    let mut saw_text_end = false;
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        match &event {
            AssistantMessageEvent::TextStart { .. } => {
                saw_text_start = true;
                // No delta may arrive before the block opens.
                assert_eq!(delta_events, 0);
            }
            AssistantMessageEvent::TextDelta { delta, partial, .. } => {
                delta_events += 1;
                accumulated.push_str(delta);
                // Snapshot consistency: the partial's text is a prefix of
                // the final text and covers everything emitted so far.
                let partial_text = partial.text();
                assert!(expected.starts_with(&partial_text));
                assert!(partial_text.len() >= accumulated.len());
            }
            AssistantMessageEvent::TextEnd { content, .. } => {
                saw_text_end = true;
                assert_eq!(*content, expected);
                // The stop boundary flushed the window: every byte arrived
                // via delta events, none only in the final message.
                assert_eq!(accumulated, expected);
            }
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    assert!(saw_text_start && saw_text_end);
    // 2000 raw deltas coalesce to ~4 (16KB / 4KB window) — never per-token.
    assert!(
        delta_events <= 10,
        "expected heavy coalescing, got {delta_events} delta events"
    );
    let final_message = stream.result().await;
    assert_eq!(final_message.text(), expected);
}

#[tokio::test]
async fn anthropic_sse_replay_produces_pi_events() {
    let (base_url, request_rx) = serve_once_with_capture("text/event-stream", ANTHROPIC_SSE).await;
    let model = anthropic_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());

    let mut text_deltas = String::new();
    let mut saw_tool_call_end = false;
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        match &event {
            AssistantMessageEvent::TextDelta { delta, .. } => text_deltas.push_str(delta),
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                saw_tool_call_end = true;
                let ContentBlock::ToolCall {
                    name, arguments, ..
                } = tool_call
                else {
                    panic!()
                };
                assert_eq!(name, "read");
                assert_eq!(&arguments["path"], &json!("a.rs"));
            }
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }

    let final_message = stream.result().await;
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    assert_eq!(text_deltas, "Hello world");
    assert!(saw_tool_call_end);
    assert_eq!(final_message.usage.input, 25);
    assert_eq!(final_message.usage.output, 12);
    assert_eq!(final_message.usage.cache_read, 10);
    assert_eq!(final_message.usage.cache_write, 5);
    assert_eq!(final_message.usage.total_tokens, 52);
    assert!(final_message.usage.cost.total > 0.0);

    // Verify the request body shape.
    let body = request_rx.await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["model"], json!("claude-test"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["max_tokens"], json!(8192));
    assert_eq!(body["system"][0]["text"], json!("You are a test bot."));
    assert_eq!(body["messages"][0]["role"], json!("user"));
    assert_eq!(body["tools"][0]["name"], json!("read"));
    assert_eq!(body["tools"][0]["input_schema"]["type"], json!("object"));
    // Short cache retention default: cache_control on system + last user block + last tool.
    assert_eq!(
        body["system"][0]["cache_control"]["type"],
        json!("ephemeral")
    );
}

const OPENAI_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\": \\\"a.rs\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":8},\"completion_tokens_details\":{\"reasoning_tokens\":3}}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn openai_sse_replay_produces_pi_events() {
    let (base_url, request_rx) = serve_once_with_capture("text/event-stream", OPENAI_SSE).await;
    let model = openai_model(&base_url);
    let provider = provider_for(&model).unwrap();

    let mut stream = provider.stream(&model, &test_context(), test_options());

    let mut text_deltas = String::new();
    loop {
        let Some(event) = stream.next().await else {
            break;
        };
        if let AssistantMessageEvent::TextDelta { delta, .. } = &event {
            text_deltas.push_str(delta);
        }
        if event.is_terminal() {
            break;
        }
    }

    let final_message = stream.result().await;
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    assert_eq!(text_deltas, "Hello world");
    assert_eq!(final_message.response_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(final_message.usage.input, 22); // 30 - 8 cached
    assert_eq!(final_message.usage.output, 15);
    assert_eq!(final_message.usage.cache_read, 8);
    assert_eq!(final_message.usage.reasoning, Some(3));

    let calls: Vec<_> = final_message.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "call_1");
    assert_eq!(calls[0].1, "read");
    assert_eq!(calls[0].2["path"], json!("a.rs"));

    // Request body: openai.com is not in the URL, so no prompt_cache_key; but
    // store/stream_options apply (standard provider detection).
    let body = request_rx.await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["model"], json!("gpt-test"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][1]["role"], json!("user"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("read"));
}

#[tokio::test]
async fn http_error_is_in_band() {
    let base_url = serve_once("application/json", "{\"error\":\"nope\"}").await;
    // serve_once returns 200; emulate an error by pointing at a closed port instead.
    let model = anthropic_model("http://127.0.0.1:9");
    let _ = base_url;
    let provider = provider_for(&model).unwrap();
    let stream = provider.stream(&model, &test_context(), test_options());
    let message = stream.result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message.error_message.is_some());
}

/// TS #9188: when the provider reports a renamed model in `message_start`,
/// the assistant message keeps the *requested* model id (thinking replay
/// stays consistent) and records the reported id in `response_model`.
#[tokio::test]
async fn anthropic_renamed_model_goes_to_response_model() {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-renamed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let base_url = serve_once("text/event-stream", body).await;
    let model = anthropic_model(&base_url); // requested id: "claude-test"
    let provider = provider_for(&model).unwrap();
    let message = provider
        .stream(&model, &test_context(), test_options())
        .result()
        .await;
    assert_eq!(message.model, "claude-test");
    assert_eq!(message.response_model.as_deref(), Some("claude-renamed"));
}
